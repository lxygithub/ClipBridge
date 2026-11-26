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
use clipbridge_core::{net, protocol, ALPN};
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

    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
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
