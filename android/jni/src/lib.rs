//! Android JNI 桥：把 core 的 iroh 长连接包成 Kotlin 可直接调用的同步接口。
//!
//! 设计取向（M1 可用优先）：
//! - 手机端永远是「主动拨号方」，Windows 端 `serve` 是监听方，不需要 NAT 穿透反向打洞；
//! - 一条 QUIC 连接常驻，收 PC 推送用 `accept_bi()`，发文本用 `open_bi()`；
//! - 断线由 supervisor 循环重连（3s 退避），重连后发 HELLO 带本地 acked_seq，触发补发。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use clipbridge_core::{net, protocol, transfer, ALPN};
use iroh::endpoint::Connection;
use iroh::{Endpoint, TransportAddr};
use jni::objects::{GlobalRef, JByteArray, JObject, JString, JValue};
use jni::sys::{jboolean, jlong, jstring, JNI_FALSE, JNI_TRUE};
use jni::{JNIEnv, JavaVM};
use tokio::runtime::Runtime;

static RT: OnceLock<Runtime> = OnceLock::new();
static SESSION: OnceLock<Mutex<Option<Arc<Session>>>> = OnceLock::new();

/// 会话代号：nativeStart 每建一次会话就 +1，nativeStop 也 +1。
/// 老会话（已经被关掉的那个）如果还在写 UI，会把「未连接」盖到新会话的「已连接」上，
/// 所以回调一律先比代号，不是当前会话就闭嘴。
static SESSION_GEN: AtomicU64 = AtomicU64::new(0);

/// App 是否在前台。前台探活更勤（8s、两次失败就断），后台省电（20s、三次）。
static FOREGROUND: AtomicBool = AtomicBool::new(false);

/// 「现在就探一次」的唤醒闸门：进前台时踢一下，不用等到下一个 20s 周期。
static KICK: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// 上一次确认「链路真的活着」的时刻（epoch ms）。进前台时用它判断是不是该立刻重建。
static LAST_ALIVE: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 手机上没有 stdout，logcat 里看不到 Rust 的 eprintln。
/// 建连/断链这种「事后要追溯」的事件统一写进 files/link.log。
fn log_line(dir: &std::path::Path, msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("link.log"))
    {
        let _ = writeln!(f, "{} {msg}", now_ms());
    }
}

/// 当前链路，形如 `direct|12` 或 `relay|312`（毫秒），空串表示没连上。
/// UI 每 2 秒通过 `nativePathStatus` 读一次。
static LINK: Mutex<String> = Mutex::new(String::new());

/// 对端（电脑）的显示名，来自电脑连上来后发的 HELLO。UI 每 2 秒读一次。
static PEER_NAME: Mutex<String> = Mutex::new(String::new());

fn rt() -> &'static Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("clipbridge-net")
            .build()
            .expect("build tokio runtime")
    })
}

fn slot() -> &'static Mutex<Option<Arc<Session>>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

struct Session {
    ep: Endpoint,
    conn: Mutex<Option<Connection>>,
    vm: JavaVM,
    cb: GlobalRef,
    dir: PathBuf,
    peer: String,
    relay: String,
    /// 本机显示名（HELLO 里带给电脑识别用）
    device: String,
    /// 会话代号，见 SESSION_GEN
    gen: u64,
    acked: AtomicU64,
    /// 已报价、等待数据流的文件：id → (offer, .part 路径)
    pending: Mutex<std::collections::HashMap<String, (transfer::Offer, PathBuf)>>,
    /// 手机发出去的文件：id → 文件名，等 COMPLETE 时回调
    outgoing: Mutex<std::collections::HashMap<String, String>>,
    /// 正在接收的传输：id + 控制位，供「取消」按钮踩刹车
    recv_flags: Mutex<Option<(String, Arc<transfer::TransferFlags>)>>,
}

impl Session {
    /// 这一代会话还是不是当前的那个：老会话被关掉后不该再往 UI 写状态/条目。
    fn current(&self) -> bool {
        self.gen == SESSION_GEN.load(Ordering::SeqCst)
    }

    fn state(&self, text: &str) {
        if !self.current() {
            return;
        }
        let Ok(mut env) = self.vm.attach_current_thread() else {
            return;
        };
        let Ok(obj) = env.new_string(text) else { return };
        let _ = env.call_method(
            self.cb.as_obj(),
            "onState",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&JObject::from(obj))],
        );
    }

    fn notify_text(&self, seq: u64, text: &str) {
        if !self.current() {
            return;
        }
        let Ok(mut env) = self.vm.attach_current_thread() else {
            return;
        };
        let Ok(obj) = env.new_string(text) else { return };
        let _ = env.call_method(
            self.cb.as_obj(),
            "onClipText",
            "(JLjava/lang/String;)V",
            &[JValue::Long(seq as jlong), JValue::Object(&JObject::from(obj))],
        );
    }

    /// 电脑发来一张图：PNG 已落地，交给 Kotlin 进时间线（缩略图由 Kotlin 自己解码）。
    fn notify_image(&self, seq: u64, path: &str, w: u32, h: u32) {
        if !self.current() {
            return;
        }
        let Ok(mut env) = self.vm.attach_current_thread() else {
            return;
        };
        let Ok(p) = env.new_string(path) else { return };
        let _ = env.call_method(
            self.cb.as_obj(),
            "onImage",
            "(JLjava/lang/String;JJ)V",
            &[
                JValue::Long(seq as jlong),
                JValue::Object(&JObject::from(p)),
                JValue::Long(w as jlong),
                JValue::Long(h as jlong),
            ],
        );
    }

    fn notify_history(&self, json: &str) {
        if !self.current() {
            return;
        }
        let Ok(mut env) = self.vm.attach_current_thread() else {
            return;
        };
        let Ok(s) = env.new_string(json) else { return };
        let _ = env.call_method(
            self.cb.as_obj(),
            "onHistory",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&JObject::from(s))],
        );
    }

    fn notify_file(&self, name: &str, path: &str, size: u64) {
        if !self.current() {
            return;
        }
        let Ok(mut env) = self.vm.attach_current_thread() else {
            return;
        };
        let (Ok(n), Ok(p)) = (env.new_string(name), env.new_string(path)) else {
            return;
        };
        let _ = env.call_method(
            self.cb.as_obj(),
            "onFile",
            "(Ljava/lang/String;Ljava/lang/String;J)V",
            &[
                JValue::Object(&JObject::from(n)),
                JValue::Object(&JObject::from(p)),
                JValue::Long(size as jlong),
            ],
        );
    }

    fn notify_progress(&self, name: &str, done: u64, total: u64) {
        if !self.current() {
            return;
        }
        let Ok(mut env) = self.vm.attach_current_thread() else {
            return;
        };
        let Ok(n) = env.new_string(name) else { return };
        let _ = env.call_method(
            self.cb.as_obj(),
            "onFileProgress",
            "(Ljava/lang/String;JJ)V",
            &[
                JValue::Object(&JObject::from(n)),
                JValue::Long(done as jlong),
                JValue::Long(total as jlong),
            ],
        );
    }

    /// 发送结果（PC 回 COMPLETE 时触发）：ok 为校验是否通过。
    fn notify_sent(&self, name: &str, ok: bool) {
        let Ok(mut env) = self.vm.attach_current_thread() else {
            return;
        };
        let Ok(n) = env.new_string(name) else { return };
        let _ = env.call_method(
            self.cb.as_obj(),
            "onFileSent",
            "(Ljava/lang/String;Z)V",
            &[JValue::Object(&JObject::from(n)), JValue::Bool(ok as u8)],
        );
    }

    fn persist_acked(&self, seq: u64) {
        self.acked.store(seq, Ordering::Relaxed);
        let _ = std::fs::write(self.dir.join("last_seq"), seq.to_string());
    }

    async fn connect_once(&self) -> Result<Connection> {
        // 先带上一批「上次连成功时记下来的地址」：能跳过 DNS/pkarr 发现就直接连，
        // 同一局域网里这一步能把重连从「看运气」变成「必成」。
        let cached = self.cached_addrs();
        let addr = net::parse_peer_extra(&self.peer, &self.relay, &cached)?;
        // 连接一定要有上限：没有超时的 connect 会在弱中继/待机断网时一挂几分钟，
        // supervisor 就卡在这里不动，看起来就是「手机一直在连、电脑端一直收不到」。
        tokio::time::timeout(Duration::from_secs(12), self.ep.connect(addr, ALPN))
            .await
            .map_err(|_| anyhow::anyhow!("connect timeout (12s)"))?
            .context("connect to windows")
    }

    /// 上次连成功时的对端地址缓存（`files/peer_addrs.json`）。
    fn cached_addrs(&self) -> Vec<String> {
        let raw = std::fs::read_to_string(self.dir.join("peer_addrs.json")).unwrap_or_default();
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(url) = v["relay"].as_str() {
            if !url.is_empty() {
                out.push(format!("relay:{url}"));
            }
        }
        if let Some(list) = v["ips"].as_array() {
            for ip in list.iter().filter_map(|x| x.as_str()).take(8) {
                out.push(format!("ip:{ip}"));
            }
        }
        out
    }

    /// 连上之后把这个对端现在可用的地址记下来，下次重连直接用。
    fn save_addrs(&self, conn: &Connection) {
        let mut relay = String::new();
        let mut ips: Vec<String> = Vec::new();
        for p in conn.paths().iter() {
            match p.remote_addr() {
                TransportAddr::Relay(url) => {
                    if relay.is_empty() {
                        relay = url.to_string();
                    }
                }
                TransportAddr::Ip(addr) => {
                    let s = addr.to_string();
                    if !ips.contains(&s) {
                        ips.push(s);
                    }
                }
                _ => {}
            }
        }
        ips.truncate(8);
        if relay.is_empty() && ips.is_empty() {
            return;
        }
        let json = serde_json::json!({ "relay": relay, "ips": ips });
        let _ = std::fs::write(self.dir.join("peer_addrs.json"), json.to_string());
    }

    /// 连上后先发 HELLO（带已确认 seq + 本机名），让 Windows 补发错过的条目、
    /// 并在电脑端显示出「已连接 <手机名>」。
    async fn hello(&self, conn: &Connection) -> Result<()> {
        let (mut send, _recv) = conn.open_bi().await.context("open hello stream")?;
        let name = if self.device.trim().is_empty() {
            "Android".to_string()
        } else {
            self.device.clone()
        };
        let payload = format!("v1|{}|{}", self.acked.load(Ordering::Relaxed), name);
        protocol::write_frame(&mut send, protocol::MSG_HELLO, payload.as_bytes()).await?;
        send.finish().ok();
        Ok(())
    }

    /// 发出去的文件记一笔：手机本地不保留文件（发完就删了），
    /// 列表要能显示「发送给 xxx」，只能靠这份流水。
    fn record_sent_file(&self, path: &PathBuf, size: u64) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let ledger = self.dir.join("sent_files.json");
        let mut list: Vec<serde_json::Value> = std::fs::read_to_string(&ledger)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        list.retain(|v| v["name"].as_str() != Some(name.as_str()));
        list.insert(
            0,
            serde_json::json!({
                "name": name,
                "size": size,
                "mtime": now,
                "path": "",
                "outgoing": true,
            }),
        );
        list.truncate(200);
        if let Ok(json) = serde_json::to_string(&list) {
            let _ = std::fs::write(&ledger, json);
        }
    }
}

/// 探一次活：往电脑发 PING，等它把 PING 原样回回来，6 秒没回就算这次失败。
/// 结果写进 `files/probe.log`（手机上没有 stdout，logcat 里看不到 Rust 的 eprintln）。
async fn probe_alive(conn: &Connection, dir: &std::path::Path, budget: Duration) -> bool {
    let t0 = std::time::Instant::now();
    let res = tokio::time::timeout(budget, async {
        let (mut s, mut r) = conn.open_bi().await?;
        protocol::write_frame(&mut s, protocol::MSG_PING, b"").await?;
        s.finish().ok();
        let got = protocol::read_frame(&mut r).await?;
        anyhow::ensure!(matches!(got, Some((protocol::MSG_PING, _))), "no ping echo");
        Ok::<(), anyhow::Error>(())
    })
    .await;
    let ok = matches!(&res, Ok(Ok(())));
    let verdict = match &res {
        Ok(Ok(())) => "ok".to_string(),
        Ok(Err(err)) => format!("err: {err:#}"),
        Err(_) => "timeout".to_string(),
    };
    {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("probe.log"))
        {
            let _ = writeln!(
                f,
                "{} {} {}ms",
                now_ms(),
                verdict,
                t0.elapsed().as_millis()
            );
        }
    }
    ok
}

/// 收连接上的所有推送帧，直到连接断开。
async fn pump_conn(sess: Arc<Session>, conn: Connection) {
    {
        let mut guard = sess.conn.lock().unwrap();
        *guard = Some(conn.clone());
    }
    sess.state("connected");
    LAST_ALIVE.store(now_ms(), Ordering::SeqCst);

    // 链路状态：每 2 秒看一眼选中的路径（打洞成功 iroh 会新增 IP 路径并切过去），
    // 写进 LINK 供 UI 显示「直连 / 中继 + 延迟」。连接断开就清空。
    tokio::spawn({
        let conn = conn.clone();
        let sess = Arc::clone(&sess);
        async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
            // 直连路径往往比「连上」晚几秒才出现，地址缓存要等它出现后再落盘一次，
            // 否则记下来的只有 relay，下次还是得走公网发现。
            let mut saved = false;
            loop {
                tick.tick().await;
                if conn.close_reason().is_some() {
                    // 只有当前会话才有资格清空链路状态：被重建的老会话不能把新会话的显示抹掉
                    if sess.current() {
                        *LINK.lock().unwrap() = String::new();
                    }
                    break;
                }
                // 和中继/直连两条路并存：取「有直连且不慢于中继」的那条，
                // 别只看 is_selected，iroh 切过去有滞后，会把 4ms 的局域网显示成 300ms。
                let mut best: Option<(bool, u64)> = None;
                for p in conn.paths().iter() {
                    let pair = (p.is_ip(), p.rtt().as_millis() as u64);
                    best = Some(match best {
                        None => pair,
                        Some(cur) => {
                            let better = match (cur.0, pair.0) {
                                (false, true) => pair.1 + 20 <= cur.1,
                                (true, false) => false,
                                _ => pair.1 < cur.1,
                            };
                            if better { pair } else { cur }
                        }
                    });
                }
                let text = match best {
                    Some((direct, rtt)) => {
                        let kind = if direct { "direct" } else { "relay" };
                        format!("{kind}|{rtt}")
                    }
                    None => String::new(),
                };
                if sess.current() {
                    *LINK.lock().unwrap() = text;
                }
                // 直连路径出现后再把地址存一次（这时 ips 才不是空的）
                if !saved && matches!(best, Some((true, _))) {
                    sess.save_addrs(&conn);
                    saved = true;
                }
            }
        }
    });

    // 心跳：探活靠电脑端把 PING 原样回回来。这一步不能省：电脑端进程被关掉（或换了新实例）
    // 时这条 QUIC 会变成半开，手机侧读流不报错，UI 就一直显示「已连接」，而电脑端早就不认识它了。
    //
    // 节奏分前台/后台：
    // - 前台 8s 一次、两次失败（≈16s）就断，用户盯着屏幕时绝不拖一分钟；
    // - 后台 20s 一次、三次失败才断，让无线少被叫醒，省电优先。
    let mut hb_fails = 0u32;
    loop {
        // 会话被换掉之后这条连接已经作废，老的心跳不能再发探活、更不能写 UI
        if !sess.current() || conn.close_reason().is_some() {
            log_line(&sess.dir, &format!("pump: exit {:?}", conn.close_reason()));
            break;
        }
        let fg = FOREGROUND.load(Ordering::Relaxed);
        let wait = if fg { Duration::from_secs(5) } else { Duration::from_secs(15) };
        // 前台这一轮里失败会立刻补一次短探活（见下），所以「这一次失败」就等于
        // 「两次都没通」——不必再等下一个 5s 周期，用户盯着屏幕时那一轮太贵了。
        // 后台容错一次：弱中继偶发丢一个包不该把连接拆掉，两次连续不通才断（≈30s 内总会有结论）。
        let need = if fg { 1 } else { 2 };
        let pair = tokio::select! {
            // 到点了
            _ = tokio::time::sleep(wait) => None,
            // 用户把 app 切回前台：短超时探一次（2.5s），探不通就当这条连接已经死了，
            // 立刻断开让 supervisor 重连——后台那种「等三次失败」在用户盯着屏幕时太慢。
            _ = KICK.notified() => {
                if probe_alive(&conn, &sess.dir, Duration::from_millis(2500)).await {
                    hb_fails = 0;
                    LAST_ALIVE.store(now_ms(), Ordering::SeqCst);
                    continue;
                }
                sess.state("disconnected");
                conn.close(0u32.into(), b"link dead");
                break;
            }
            pair = conn.accept_bi() => Some(pair),
        };
        let Some(pair) = pair else {
            // 正在传文件（收或发）时不探活：数据流会把带宽占满，探活超时是误判
            if sess.recv_flags.lock().unwrap().is_some()
                || !sess.outgoing.lock().unwrap().is_empty()
            {
                continue;
            }
            let budget = if fg {
                Duration::from_millis(2500)
            } else {
                // 后台也别给 6 秒：一次探活最多 4 秒，两次失败加起来二十多秒就能有结论
                Duration::from_secs(4)
            };
            let mut alive = probe_alive(&conn, &sess.dir, budget).await;
            // 前台再补一次短探活：偶发丢包不该把好链路判死，但真死了也不该拖两轮
            if !alive && fg {
                tokio::time::sleep(Duration::from_millis(700)).await;
                alive = probe_alive(&conn, &sess.dir, Duration::from_secs(2)).await;
            }
            if alive {
                hb_fails = 0;
                LAST_ALIVE.store(now_ms(), Ordering::SeqCst);
            } else {
                hb_fails += 1;
                // 一旦开始怀疑链路，就别再等 15s 的下一个周期：3 秒后再确认一次，
                // 真死能快 10 秒发现，只是丢一个包也还来得及在第 2 次自证清白。
                if !fg && hb_fails < need {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
            if hb_fails >= need {
                log_line(&sess.dir, &format!("pump: link dead ({hb_fails} missed pings)"));
                sess.state("disconnected");
                conn.close(0u32.into(), b"link dead");
                break;
            }
            continue;
        };
        let (mut send, mut recv) = match pair {
            Ok(pair) => pair,
            Err(err) => {
                log_line(&sess.dir, &format!("pump: accept_bi err {err:?}"));
                break;
            }
        };
        // 收到电脑主动开的流本身就说明链路活着，记一笔（进前台时据此判断要不要重建）
        LAST_ALIVE.store(now_ms(), Ordering::SeqCst);
        let sess = Arc::clone(&sess);
        tokio::spawn(async move {
            let Ok(Some((ty, payload))) = protocol::read_frame(&mut recv).await else {
                return;
            };
            match ty {
                protocol::MSG_HELLO => {
                    // "v1|<seq>|<device_name>"：电脑连上来后发的自我介绍，UI 用来显示设备名
                    let body = String::from_utf8_lossy(&payload).to_string();
                    let name = body.split('|').nth(2).unwrap_or("").trim().to_string();
                    if !name.is_empty() {
                        *PEER_NAME.lock().unwrap() = name;
                    }
                    let _ = send.finish();
                }
                protocol::MSG_CLIP_TEXT => {
                    let body = String::from_utf8_lossy(&payload).to_string();
                    let (seq, text) = body.split_once('|').unwrap_or(("0", &body));
                    let seq = seq.parse::<u64>().unwrap_or(0);
                    let _ = protocol::write_frame(&mut send, protocol::MSG_ACK, b"").await;
                    let _ = send.finish();
                    if seq > 0 {
                        sess.persist_acked(seq);
                    }
                    sess.notify_text(seq, text);
                }
                protocol::MSG_CLIP_IMAGE_META => {
                    // "<seq>|<PNG 字节>"：落地到 inbox（和收到的文件同一个地方），再交给 Kotlin 进时间线
                    let Some(pos) = payload.iter().position(|b| *b == b'|') else {
                        eprintln!("[jni] malformed image payload");
                        return;
                    };
                    let seq: u64 = String::from_utf8_lossy(&payload[..pos])
                        .trim()
                        .parse()
                        .unwrap_or(0);
                    let png = &payload[pos + 1..];
                    let inbox = sess.dir.join("inbox");
                    let _ = std::fs::create_dir_all(&inbox);
                    let ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let path = inbox.join(format!("clip_{ms}.png"));
                    let size = match png_size(png) {
                        Some((w, h)) => Some((w, h)),
                        None => None,
                    };
                    if std::fs::write(&path, png).is_ok() {
                        if let Some((w, h)) = size {
                            let _ = protocol::write_frame(&mut send, protocol::MSG_ACK, b"").await;
                            let _ = send.finish();
                            sess.notify_image(seq, &path.to_string_lossy(), w, h);
                            return;
                        }
                    }
                    eprintln!("[jni] bad png ({} bytes)", png.len());
                    let _ = send.finish();
                }
                protocol::MSG_HISTORY => {
                    // 电脑握手后甩来的历史快照：整包交给 Kotlin 静默合并，不弹通知
                    let body = String::from_utf8_lossy(&payload).to_string();
                    sess.notify_history(&body);
                    let _ = send.finish();
                }
                protocol::MSG_PING => {
                    let _ = protocol::write_frame(&mut send, protocol::MSG_PING, b"").await;
                    let _ = send.finish();
                }
                protocol::MSG_FILE_OFFER => match serde_json::from_slice::<transfer::Offer>(&payload) {
                    Ok(offer) => {
                        let name = transfer::safe_name(&offer.name);
                        if offer.size == 0 || offer.size > transfer::MAX_FILE_SIZE {
                            eprintln!("[jni] reject file: bad size {}", offer.size);
                            return;
                        }
                        let inbox = sess.dir.join("inbox");
                        let _ = std::fs::create_dir_all(&inbox);
                        let part = inbox.join(format!("{}.part", offer.id));
                        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                        sess.pending
                            .lock()
                            .unwrap()
                            .insert(offer.id.clone(), (offer.clone(), part));
                        let accept = transfer::Accept {
                            id: offer.id.clone(),
                            offset: have,
                        };
                        if let Ok(json) = serde_json::to_vec(&accept) {
                            let _ = protocol::write_frame(&mut send, protocol::MSG_FILE_ACCEPT, &json).await;
                        }
                        let _ = send.finish();
                        eprintln!(
                            "[jni] accepting file {} ({} bytes, from {})",
                            name, offer.size, accept.offset
                        );
                    }
                    Err(err) => eprintln!("[jni] bad offer: {err:#}"),
                },
                protocol::MSG_FILE_DATA => {
                    let id = String::from_utf8_lossy(&payload).trim().to_string();
                    // 同一时刻只允许一条写入流：发送端在 relay 上超时后会重开一条流续传，
                    // 两条流同时往同一个 .part 追加会把文件写坏（尺寸比源文件还大）。
                    if sess.recv_flags.lock().unwrap().is_some() {
                        eprintln!("[jni] already receiving, ignoring duplicate data stream {id}");
                        return;
                    }
                    let Some((offer, part)) = sess.pending.lock().unwrap().remove(&id) else {
                        eprintln!("[jni] data for unknown id {id}");
                        return;
                    };
                    let flags = Arc::new(transfer::TransferFlags::new());
                    *sess.recv_flags.lock().unwrap() = Some((id.clone(), Arc::clone(&flags)));
                    let name = transfer::safe_name(&offer.name);
                    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                    sess.notify_progress(&name, have, offer.size);
                    // 每 0.5s 把 flags 里的进度回灌到通知，否则进度条一直停在 0%
                    let watch_flags = Arc::clone(&flags);
                    let watch_name = name.clone();
                    let watch_total = offer.size;
                    let watch_sess = Arc::clone(&sess);
                    let watcher = tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let done = watch_flags.offset();
                            if watch_flags.state() == transfer::ST_CANCEL || done >= watch_total {
                                break;
                            }
                            watch_sess.notify_progress(&watch_name, done, watch_total);
                        }
                    });
                    let received = match transfer::recv_stream(&mut recv, &part, have, &flags, offer.size).await {
                        Ok(n) => n,
                        Err(err) => {
                            watcher.abort();
                            eprintln!("[jni] recv failed: {err:#}");
                            return;
                        }
                    };
                    watcher.abort();
                    *sess.recv_flags.lock().unwrap() = None;
                    if flags.state() == transfer::ST_CANCEL {
                        // 用户点了「取消」：删掉半截文件，并让 PC 别再重试
                        let _ = std::fs::remove_file(&part);
                        sess.notify_progress(&name, 0, 0);
                        eprintln!("[jni] receive canceled by user: {name}");
                        let conn = sess.conn.lock().unwrap().clone();
                        if let Some(conn) = conn {
                            if let Ok((mut s, _r)) = conn.open_bi().await {
                                let _ = protocol::write_frame(&mut s, protocol::MSG_FILE_CANCEL, id.as_bytes()).await;
                                let _ = s.finish();
                            }
                        }
                        return;
                    }
                    let digest = transfer::sha256_file(&part).unwrap_or_default();
                    if received != offer.size
                        || (!offer.sha256.is_empty() && !digest.eq_ignore_ascii_case(&offer.sha256))
                    {
                        eprintln!("[jni] file incomplete/mismatch, part kept {}", part.display());
                        return;
                    }
                    let final_path = transfer::unique_path(&sess.dir.join("inbox"), &name);
                    if let Err(err) = std::fs::rename(&part, &final_path) {
                        eprintln!("[jni] rename failed: {err:#}");
                        return;
                    }
                    let path_text = final_path.display().to_string();
                    sess.notify_file(&name, &path_text, received);
                    eprintln!("[jni] file saved {path_text}");

                    // 回 COMPLETE：PC 侧靠这条新流确认校验通过
                    let done = transfer::Complete {
                        id: id.clone(),
                        ok: true,
                        sha256: digest.clone(),
                        size: received,
                    };
                    let conn = sess.conn.lock().unwrap().clone();
                    if let Some(conn) = conn {
                        if let Ok((mut s, _r)) = conn.open_bi().await {
                            if let Ok(json) = serde_json::to_vec(&done) {
                                let _ = protocol::write_frame(&mut s, protocol::MSG_FILE_COMPLETE, &json).await;
                            }
                            let _ = s.finish();
                        }
                    }
                }
                protocol::MSG_FILE_COMPLETE => {
                    // PC 侧校验结果：手机发出去的文件到这一步才算真完成
                    if let Ok(done) = serde_json::from_slice::<transfer::Complete>(&payload) {
                        eprintln!("[jni] complete ok={} size={}", done.ok, done.size);
                        let name = sess
                            .outgoing
                            .lock()
                            .unwrap()
                            .remove(&done.id)
                            .unwrap_or_else(|| done.id.clone());
                        sess.notify_sent(&name, done.ok);
                    }
                }
                protocol::MSG_FILE_CANCEL => {
                    let id = String::from_utf8_lossy(&payload).trim().to_string();
                    if let Some((_, part)) = sess.pending.lock().unwrap().remove(&id) {
                        let _ = std::fs::remove_file(part);
                    }
                }
                other => eprintln!("[jni] unsupported frame {other:#04x}"),
            }
        });
    }

    sess.conn.lock().unwrap().take();
    sess.state("disconnected");
}

/// 常驻监督：连上 → 收推送 → 断了退避重连。
/// 退避是 1s→2s→4s→封顶 8s：断链刚一发生就连着试几次（打洞/relay 一般都是瞬间恢复），
/// 一直连不上再慢慢放长，别在没网的时候把无线一直叫醒。
async fn supervise(sess: Arc<Session>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        // 会话被换掉（nativeStart 重建 / nativeStop）之后这个循环就该退出，
        // 否则老会话会拿着已经关掉的 endpoint 一直重连，和新会话抢同一个 key。
        if !sess.current() {
            break;
        }
        sess.state("connecting");
        log_line(&sess.dir, "connect: try");
        let recovered = match sess.connect_once().await {
            Ok(conn) => {
                log_line(&sess.dir, "connect: ok");
                sess.save_addrs(&conn);
                if let Err(err) = sess.hello(&conn).await {
                    eprintln!("[jni] hello failed: {err:#}");
                    log_line(&sess.dir, &format!("hello failed: {err:#}"));
                }
                backoff = Duration::from_secs(1);
                pump_conn(Arc::clone(&sess), conn).await;
                log_line(&sess.dir, "pump: ended");
                true
            }
            Err(err) => {
                log_line(&sess.dir, &format!("connect: failed {err:#}"));
                sess.state("disconnected");
                false
            }
        };
        if recovered {
            // 刚刚还连上过，说明环境是通的，快速重试
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(8));
    }
}

impl Session {
    /// 发一条文本到 PC；没连接就先拨一次。返回是否收到 ACK。
    fn push_text(&self, text: &str) -> bool {
        let sess_rt = rt();
        sess_rt.block_on(async {
            let conn = {
                let guard = self.conn.lock().unwrap();
                guard.clone()
            };
            let conn = match conn {
                Some(c) => c,
                None => match tokio::time::timeout(Duration::from_secs(8), self.connect_once()).await
                {
                    Ok(Ok(c)) => c,
                    _ => return false,
                },
            };
            let Ok((mut send, mut recv)) = conn.open_bi().await else {
                return false;
            };
            let payload = format!("0|{text}");
            if protocol::write_frame(&mut send, protocol::MSG_CLIP_TEXT, payload.as_bytes())
                .await
                .is_err()
            {
                return false;
            }
            send.finish().ok();
            matches!(
                tokio::time::timeout(Duration::from_secs(5), protocol::read_frame(&mut recv)).await,
                Ok(Ok(Some((protocol::MSG_ACK, _))))
            )
        })
    }

    /// 发一张图到 PC（Kotlin 侧已经把剪贴板图缩成 PNG）：写一帧 0x03，等 ACK。
    fn push_image(&self, png: Vec<u8>) -> bool {
        rt().block_on(async {
            let conn = {
                let guard = self.conn.lock().unwrap();
                guard.clone()
            };
            let conn = match conn {
                Some(c) => c,
                None => match tokio::time::timeout(Duration::from_secs(8), self.connect_once()).await
                {
                    Ok(Ok(c)) => c,
                    _ => return false,
                },
            };
            let Ok((mut send, mut recv)) = conn.open_bi().await else {
                return false;
            };
            let mut payload = Vec::with_capacity(png.len() + 16);
            payload.extend_from_slice(b"0|");
            payload.extend_from_slice(&png);
            if protocol::write_frame(&mut send, protocol::MSG_CLIP_IMAGE_META, &payload)
                .await
                .is_err()
            {
                return false;
            }
            send.finish().ok();
            matches!(
                tokio::time::timeout(Duration::from_secs(5), protocol::read_frame(&mut recv)).await,
                Ok(Ok(Some((protocol::MSG_ACK, _))))
            )
        })
    }

    /// 发一个文件到 PC：OFFER → ACCEPT → 数据流；COMPLETE 由 pump 侧回调。
    fn send_file(&self, path: &str) -> bool {
        let p = PathBuf::from(path);
        let meta = match std::fs::metadata(&p) {
            Ok(m) if m.is_file() => m,
            _ => {
                eprintln!("[jni] not a file: {path}");
                return false;
            }
        };
        let size = meta.len();
        if size == 0 || size > transfer::MAX_FILE_SIZE {
            eprintln!("[jni] bad size {size}");
            return false;
        }
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let id = transfer::new_id(&p.display().to_string(), size);
        let sha = match transfer::sha256_file(&p) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("[jni] sha: {err:#}");
                return false;
            }
        };
        let offer = transfer::Offer {
            id: id.clone(),
            name: name.clone(),
            size,
            sha256: sha,
            mime: String::new(),
        };
        self.outgoing
            .lock()
            .unwrap()
            .insert(id.clone(), name.clone());

        let ok = rt().block_on(async {
            let conn = {
                let guard = self.conn.lock().unwrap();
                guard.clone()
            };
            let conn = match conn {
                Some(c) => c,
                None => match tokio::time::timeout(Duration::from_secs(8), self.connect_once()).await
                {
                    Ok(Ok(c)) => c,
                    _ => return false,
                },
            };
            let Ok((mut ctrl_send, mut ctrl_recv)) = conn.open_bi().await else {
                return false;
            };
            let Ok(json) = serde_json::to_vec(&offer) else {
                return false;
            };
            if protocol::write_frame(&mut ctrl_send, protocol::MSG_FILE_OFFER, &json)
                .await
                .is_err()
            {
                return false;
            }
            ctrl_send.finish().ok();
            let offset = match tokio::time::timeout(
                Duration::from_secs(15),
                protocol::read_frame(&mut ctrl_recv),
            )
            .await
            {
                Ok(Ok(Some((protocol::MSG_FILE_ACCEPT, payload)))) => {
                    serde_json::from_slice::<transfer::Accept>(&payload)
                        .map(|a| a.offset.min(size))
                        .unwrap_or(0)
                }
                _ => {
                    eprintln!("[jni] offer not accepted");
                    return false;
                }
            };

            self.notify_progress(&name, offset, size);
            let flags = transfer::TransferFlags::new();
            let Ok((mut send, _recv)) = conn.open_bi().await else {
                return false;
            };
            if protocol::write_frame(&mut send, protocol::MSG_FILE_DATA, id.as_bytes())
                .await
                .is_err()
            {
                return false;
            }
            match transfer::send_stream(&mut send, &p, offset, &flags).await {
                Ok(outcome) => {
                    eprintln!("[jni] send {name} → {outcome:?}");
                    true
                }
                Err(err) => {
                    eprintln!("[jni] send failed: {err:#}");
                    false
                }
            }
        });
        if ok {
            // 文件已经落到电脑上了：手机端只留一条流水，供列表显示「发送给 xxx」
            self.record_sent_file(&p, size);
        }
        ok
    }
}

fn read_last_seq(dir: &PathBuf) -> u64 {
    std::fs::read_to_string(dir.join("last_seq"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn jstring_to_string(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s)
        .map(|v| v.into())
        .unwrap_or_else(|_| String::new())
}

/// 只读 PNG 头拿宽高，不解像素（时间线里显示「图片 1080×2400」用）。
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let dec = png::Decoder::new(std::io::Cursor::new(bytes));
    let reader = dec.read_info().ok()?;
    Some((reader.info().width, reader.info().height))
}

/// `Native.start(dir, peerId, relayUrl, deviceName, listener)`
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeStart(
    mut env: JNIEnv,
    _class: JObject,
    dir: JString,
    peer: JString,
    relay: JString,
    device: JString,
    listener: JObject,
) -> jboolean {
    let dir_s = jstring_to_string(&mut env, &dir);
    let peer_s = jstring_to_string(&mut env, &peer);
    let relay_s = jstring_to_string(&mut env, &relay);
    let device_s = jstring_to_string(&mut env, &device);
    if peer_s.trim().is_empty() {
        return JNI_FALSE;
    }

    let vm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(_) => return JNI_FALSE,
    };
    let cb = match env.new_global_ref(listener) {
        Ok(cb) => cb,
        Err(_) => return JNI_FALSE,
    };

    // 已有会话先关掉，避免双端点抢同一个 key。
    // 代号先 +1：老会话哪怕还有线程在跑，它的回调也会因为代号过气而全部失效。
    let gen = SESSION_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(old) = slot().lock().unwrap().take() {
        let old_conn = old.conn.lock().unwrap().take();
        let ep = old.ep.clone();
        rt().block_on(async move {
            if let Some(c) = old_conn {
                c.close(0u32.into(), b"restart");
            }
            ep.close().await;
        });
    }

    let dir_path = PathBuf::from(&dir_s);
    let _ = std::fs::create_dir_all(&dir_path);
    let secret = match net::load_or_create_secret(&dir_path.join("endpoint.key")) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("[jni] key: {err:#}");
            return JNI_FALSE;
        }
    };

    let ep = match rt().block_on(net::build_endpoint_with_relay(secret, &relay_s)) {
        Ok(ep) => ep,
        Err(err) => {
            eprintln!("[jni] endpoint: {err:#}");
            return JNI_FALSE;
        }
    };

    let sess = Arc::new(Session {
        ep,
        conn: Mutex::new(None),
        vm,
        cb,
        dir: dir_path.clone(),
        peer: peer_s,
        relay: relay_s,
        device: device_s,
        gen,
        acked: AtomicU64::new(read_last_seq(&dir_path)),
        pending: Mutex::new(std::collections::HashMap::new()),
        outgoing: Mutex::new(std::collections::HashMap::new()),
        recv_flags: Mutex::new(None),
    });

    *slot().lock().unwrap() = Some(Arc::clone(&sess));
    *PEER_NAME.lock().unwrap() = String::new();
    let handle = rt().handle().clone();
    // 手机侧没有任何 stdout：QAD/NAT 结论只能写进 files/diag.log 再 adb 拉出来。
    // 这是判断「打洞为什么不成」的唯一现场证据（PC 侧只看得到手机发布的候选地址）。
    {
        let sess = Arc::clone(&sess);
        handle.spawn(async move { diag_loop(sess).await });
    }
    handle.spawn(supervise(sess));
    JNI_TRUE
}

/// 等 net_report 出来，把 NAT 结论追加到 `files/diag.log`，之后每 30s 记一次对端候选地址。
async fn diag_loop(sess: Arc<Session>) {
    let path = sess.dir.join("diag.log");
    let mut w = sess.ep.net_report();
    let mut report = None;
    for _ in 0..60 {
        if let Some(r) = iroh::Watcher::get(&mut w) {
            let done = r.global_v4.is_some() || r.global_v6.is_some();
            report = Some(r);
            if done {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut text = format!("=== diag @ {stamp} ===\n");
    match report {
        Some(r) => {
            text.push_str(&format!(
                "global_v4={:?} global_v6={:?} udp_v4={} udp_v6={} mapping_varies_v4={:?} preferred_relay={:?}\n",
                r.global_v4.map(|a| a.to_string()),
                r.global_v6.map(|a| a.to_string()),
                r.udp_v4,
                r.udp_v6,
                r.mapping_varies_by_dest_ipv4,
                r.preferred_relay.map(|u| u.to_string()),
            ));
            if r.mapping_varies_by_dest_ipv4 == Some(true) {
                text.push_str("NAT: 对称（打洞很难）\n");
            } else if r.mapping_varies_by_dest_ipv4 == Some(false) {
                text.push_str("NAT: 锥形（端口稳定）\n");
            } else {
                text.push_str("NAT: 未知\n");
            }
        }
        None => text.push_str("net_report 未完成（relay 不可达）\n"),
    }

    // 对端（电脑）发布了哪些候选地址：同时能看出自己有没有被对端当成直连目标
    if let Ok(peer_id) = sess.peer.parse::<iroh::EndpointId>() {
        if let Some(info) = sess.ep.remote_info(peer_id).await {
            let addrs: Vec<String> = info.addrs().map(|a| format!("{:?}", a.addr())).collect();
            text.push_str(&format!("peer_addrs={}\n", addrs.join(", ")));
            text.push_str(&format!("self_addrs={:?}\n", sess.ep.addr()));
        }
    }
    // 诊断日志轮转：超过 1MB 就挪成 diag.log.1，只留一份
    if std::fs::metadata(&path).map(|m| m.len() > 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&path, sess.dir.join("diag.log.1"));
    }
    let _ = std::fs::write(&path, &text);
}

/// `Native.stop()`
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeStop(
    _env: JNIEnv,
    _class: JObject,
) {
    // 代号 +1：这条会话的回调从此一律失效，UI 不会被它写回「未连接」。
    SESSION_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(old) = slot().lock().unwrap().take() {
        let old_conn = old.conn.lock().unwrap().take();
        rt().block_on(async move {
            if let Some(c) = old_conn {
                c.close(0u32.into(), b"stop");
            }
            old.ep.close().await;
        });
    }
    *LINK.lock().unwrap() = String::new();
}

/// `Native.setForeground(true|false)`：进前台立刻踢一次探活，后台把节奏放慢省电。
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeSetForeground(
    _env: JNIEnv,
    _class: JObject,
    fg: jboolean,
) {
    let fg = fg != JNI_FALSE;
    let was = FOREGROUND.swap(fg, Ordering::SeqCst);
    if fg && !was {
        KICK.notify_waiters();
    }
}

/// `Native.idleMs()`：距离上次确认链路活着过了多久（毫秒）；-1 表示这次会话还没连过。
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeIdleMs(
    _env: JNIEnv,
    _class: JObject,
) -> jlong {
    let last = LAST_ALIVE.load(Ordering::SeqCst);
    if last == 0 {
        return -1;
    }
    now_ms().saturating_sub(last) as jlong
}

/// `Native.sendText(text)`：把手机剪贴板推给 PC。
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeSendText(
    mut env: JNIEnv,
    _class: JObject,
    text: JString,
) -> jboolean {
    let text = jstring_to_string(&mut env, &text);
    if text.trim().is_empty() {
        return JNI_FALSE;
    }
    let sess = slot().lock().unwrap().clone();
    match sess {
        Some(sess) if sess.push_text(&text) => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

/// `Native.sendImage(pngBytes)`：把手机剪贴板里的图（已编码成 PNG）推给 PC。
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeSendImage(
    mut env: JNIEnv,
    _class: JObject,
    bytes: JByteArray,
) -> jboolean {
    let Ok(png) = env.convert_byte_array(&bytes) else {
        return JNI_FALSE;
    };
    if png.is_empty() || png.len() > 12 * 1024 * 1024 {
        return JNI_FALSE;
    }
    let sess = slot().lock().unwrap().clone();
    match sess {
        Some(sess) if sess.push_image(png) => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

/// `Native.cancelTransfer()`：取消当前正在接收的传输。
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeCancelTransfer(
    _env: JNIEnv,
    _class: JObject,
) -> jboolean {
    let sess = slot().lock().unwrap().clone();
    let Some(sess) = sess else { return JNI_FALSE };
    let flags = sess.recv_flags.lock().unwrap().as_ref().map(|(_, f)| Arc::clone(f));
    match flags {
        Some(flags) => {
            flags.cancel();
            JNI_TRUE
        }
        None => JNI_FALSE,
    }
}

/// `Native.sendFile(path)`：把手机上的文件推给 PC，返回是否发完。
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeSendFile(
    mut env: JNIEnv,
    _class: JObject,
    path: JString,
) -> jboolean {
    let path = jstring_to_string(&mut env, &path);
    let sess = slot().lock().unwrap().clone();
    match sess {
        Some(sess) if sess.send_file(&path) => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

/// `Native.selfInfo()` → `"<id>|<relay>|<state>"`，主界面展示用。
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeSelfInfo(
    env: JNIEnv,
    _class: JObject,
) -> jstring {
    let sess = slot().lock().unwrap().clone();
    let info = match sess {
        Some(sess) => {
            let relay = sess
                .ep
                .addr()
                .relay_urls()
                .next()
                .map(|u| u.to_string())
                .unwrap_or_default();
            let state = if sess.conn.lock().unwrap().is_some() {
                "connected"
            } else {
                "disconnected"
            };
            format!("{}|{}|{}", sess.ep.id(), relay, state)
        }
        None => "||idle".to_string(),
    };
    match env.new_string(info) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// `Native.ackedSeq()`：上次同步到的 seq，调试/展示用。
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeAckedSeq(
    _env: JNIEnv,
    _class: JObject,
) -> jlong {
    slot()
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.acked.load(Ordering::Relaxed) as jlong)
        .unwrap_or(0)
}

/// `Native.pathStatus()` → `"direct|12"` / `"relay|312"`，空串表示未连接。
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativePathStatus(
    mut env: JNIEnv,
    _class: JObject,
) -> jstring {
    let text = LINK.lock().unwrap().clone();
    match env.new_string(text) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// `Native.listFiles()` → JSON 数组：手机收到的文件（主界面文件区、长按分享用）。
/// `Native.listFiles()` → JSON 数组，收发两个方向都列：
///   * `inbox/` 里落地的 = 电脑发过来的（outgoing=false）
///   * `sent_files.json` 流水 = 手机发出去的（outgoing=true，文件本体在电脑上）
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeListFiles(
    mut env: JNIEnv,
    _class: JObject,
) -> jstring {
    let dir = slot().lock().unwrap().as_ref().map(|s| s.dir.clone());
    let mut rows = Vec::new();
    if let Some(dir) = dir {
        // 收到的
        if let Ok(entries) = std::fs::read_dir(dir.join("inbox")) {
            for e in entries.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if name.ends_with(".part") {
                    continue;
                }
                let Ok(meta) = e.metadata() else { continue };
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                rows.push(serde_json::json!({
                    "name": name,
                    "size": meta.len(),
                    "mtime": mtime,
                    "path": p.to_string_lossy(),
                    "outgoing": false,
                }));
            }
        }
        // 发出去的
        if let Ok(text) = std::fs::read_to_string(dir.join("sent_files.json")) {
            if let Ok(list) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                for v in list {
                    let mut v = v;
                    v["outgoing"] = serde_json::json!(true);
                    if v.get("path").is_none() {
                        v["path"] = serde_json::json!("");
                    }
                    rows.push(v);
                }
            }
        }
    }
    rows.sort_by_key(|v| std::cmp::Reverse(v["mtime"].as_u64().unwrap_or(0)));
    rows.truncate(200);
    let json = serde_json::Value::Array(rows).to_string();
    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// `Native.peerName()`：电脑的显示名（来自电脑的 HELLO），没连过就是空串。
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativePeerName(
    mut env: JNIEnv,
    _class: JObject,
) -> jstring {
    let name = PEER_NAME.lock().unwrap().clone();
    match env.new_string(name) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}



