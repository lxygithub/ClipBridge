//! Android JNI 桥：把 core 的 iroh 长连接包成 Kotlin 可直接调用的同步接口。
//!
//! 设计取向（M1 可用优先）：
//! - 手机端永远是「主动拨号方」，Windows 端 `serve` 是监听方，不需要 NAT 穿透反向打洞；
//! - 一条 QUIC 连接常驻，收 PC 推送用 `accept_bi()`，发文本用 `open_bi()`；
//! - 断线由 supervisor 循环重连（3s 退避），重连后发 HELLO 带本地 acked_seq，触发补发。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use clipbridge_core::{net, protocol, transfer, ALPN};
use iroh::endpoint::Connection;
use iroh::Endpoint;
use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::sys::{jboolean, jlong, jstring, JNI_FALSE, JNI_TRUE};
use jni::{JNIEnv, JavaVM};
use tokio::runtime::Runtime;

static RT: OnceLock<Runtime> = OnceLock::new();
static SESSION: OnceLock<Mutex<Option<Arc<Session>>>> = OnceLock::new();

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
    acked: AtomicU64,
    /// 已报价、等待数据流的文件：id → (offer, .part 路径)
    pending: Mutex<std::collections::HashMap<String, (transfer::Offer, PathBuf)>>,
    /// 手机发出去的文件：id → 文件名，等 COMPLETE 时回调
    outgoing: Mutex<std::collections::HashMap<String, String>>,
    /// 正在接收的传输：id + 控制位，供「取消」按钮踩刹车
    recv_flags: Mutex<Option<(String, Arc<transfer::TransferFlags>)>>,
}

impl Session {
    fn state(&self, text: &str) {
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

    fn notify_file(&self, name: &str, path: &str, size: u64) {
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

    fn notify_progress(&self, name: &str, done: u64, total: u64) {        let Ok(mut env) = self.vm.attach_current_thread() else {
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
        let addr = net::parse_peer(&self.peer, &self.relay)?;
        self.ep.connect(addr, ALPN).await.context("connect to windows")
    }

    /// 连上后先发 HELLO（带已确认 seq），让 Windows 补发错过的条目。
    async fn hello(&self, conn: &Connection) -> Result<()> {
        let (mut send, _recv) = conn.open_bi().await.context("open hello stream")?;
        let payload = format!("v1|{}|android", self.acked.load(Ordering::Relaxed));
        protocol::write_frame(&mut send, protocol::MSG_HELLO, payload.as_bytes()).await?;
        send.finish().ok();
        Ok(())
    }
}

/// 收连接上的所有推送帧，直到连接断开。
async fn pump_conn(sess: Arc<Session>, conn: Connection) {
    {
        let mut guard = sess.conn.lock().unwrap();
        *guard = Some(conn.clone());
    }
    sess.state("connected");

    // 心跳：每 20s 探一次活。只在失败时记日志，**不主动关连接** ——
    // 之前直接 close 掉，结果半开检测反而把好连接杀掉了。
    let mut hb = tokio::time::interval(std::time::Duration::from_secs(20));
    let mut hb_fails = 0u32;
    loop {
        let pair = tokio::select! {
            _ = hb.tick() => {
                // 正在收文件时不探活：大文件会把连接占满，探活超时反而误判
                if sess.recv_flags.lock().unwrap().is_some() {
                    continue;
                }
                let probe = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    let (mut s, _r) = conn.open_bi().await?;
                    protocol::write_frame(&mut s, protocol::MSG_PING, b"").await?;
                    s.finish().ok();
                    Ok::<(), anyhow::Error>(())
                })
                .await;
                match &probe {
                    Ok(Ok(())) => hb_fails = 0,
                    _ => hb_fails += 1,
                }
                if hb_fails >= 3 {
                    eprintln!("[jni] heartbeat failed 3 times, dropping conn");
                    conn.close(0u32.into(), b"stale");
                    break;
                }
                continue;
            }
            pair = conn.accept_bi() => pair,
        };
        let (mut send, mut recv) = match pair {
            Ok(pair) => pair,
            Err(_) => break,
        };
        let sess = Arc::clone(&sess);
        tokio::spawn(async move {
            let Ok(Some((ty, payload))) = protocol::read_frame(&mut recv).await else {
                return;
            };
            match ty {
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
async fn supervise(sess: Arc<Session>) {
    loop {
        sess.state("connecting");
        match sess.connect_once().await {
            Ok(conn) => {
                if let Err(err) = sess.hello(&conn).await {
                    eprintln!("[jni] hello failed: {err:#}");
                }
                pump_conn(Arc::clone(&sess), conn).await;
            }
            Err(err) => {
                eprintln!("[jni] connect failed: {err:#}");
                sess.state("disconnected");
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
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
        })
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

/// `Native.start(dir, peerId, relayUrl, listener)`
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeStart(
    mut env: JNIEnv,
    _class: JObject,
    dir: JString,
    peer: JString,
    relay: JString,
    listener: JObject,
) -> jboolean {
    let dir_s = jstring_to_string(&mut env, &dir);
    let peer_s = jstring_to_string(&mut env, &peer);
    let relay_s = jstring_to_string(&mut env, &relay);
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
    if let Some(old) = slot().lock().unwrap().take() {
        let ep = old.ep.clone();
        rt().block_on(async move { ep.close().await });
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

    let ep = match rt().block_on(net::build_endpoint(secret)) {
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
        acked: AtomicU64::new(read_last_seq(&dir_path)),
        pending: Mutex::new(std::collections::HashMap::new()),
        outgoing: Mutex::new(std::collections::HashMap::new()),
        recv_flags: Mutex::new(None),
    });

    *slot().lock().unwrap() = Some(Arc::clone(&sess));
    let handle = rt().handle().clone();
    handle.spawn(supervise(sess));
    JNI_TRUE
}

/// `Native.stop()`
#[no_mangle]
pub extern "system" fn Java_top_ieop_clipbridge_Native_nativeStop(
    _env: JNIEnv,
    _class: JObject,
) {
    if let Some(old) = slot().lock().unwrap().take() {
        rt().block_on(async move { old.ep.close().await });
    }
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



