//! ClipBridge Windows 端（M1：剪贴板监听 + 多设备广播 + 断连补发 + 托盘/热键/历史窗口）。
//!
//! 用法：
//!   clipbridge info
//!   clipbridge serve                        常驻：托盘 + 剪贴板推送 + Ctrl+Alt+H 历史窗口
//!   clipbridge serve --headless             无界面常驻（调试/自动化用）
//!   clipbridge send --peer --relay --text   一次性发一段文本
//!   clipbridge recv --peer --relay          调试用：作为对端接收广播

mod clipboard;
mod queue;
mod ui;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use clipbridge_core::{net, protocol, ALPN, DEFAULT_DIR};
use iroh::endpoint::Connection;
use iroh::EndpointId;
use tokio::sync::Mutex;

use clipboard::ClipboardWatcher;
use queue::Store;

#[derive(Parser)]
#[command(
    name = "clipbridge",
    version,
    about = "ClipBridge：Windows ↔ Android 剪贴板/文件同步"
)]
struct Cli {
    /// 数据目录（密钥、队列、落盘文件）
    #[arg(long, global = true, default_value = DEFAULT_DIR)]
    dir: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 打印本机端点身份
    Info,
    /// 常驻：托盘 + 监听剪贴板 + 广播
    Serve {
        /// 不建托盘/窗口（自动化测试用）
        #[arg(long)]
        headless: bool,
    },
    /// 向对端发送一段文本
    Send {
        #[arg(long)]
        peer: String,
        #[arg(long, default_value = "")]
        relay: String,
        #[arg(long)]
        text: String,
    },
    /// 调试用：作为"接收端"连上来，打印收到的广播
    Recv {
        #[arg(long)]
        peer: String,
        #[arg(long, default_value = "")]
        relay: String,
    },
    /// 开关开机自启（写入 HKCU\...\Run）
    Autostart {
        /// 关闭自启
        #[arg(long)]
        off: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let key_path = cli.dir.join("endpoint.key");
    let secret = net::load_or_create_secret(&key_path)?;

    match cli.cmd {
        Cmd::Serve { headless } => serve(cli.dir, secret, headless),
        Cmd::Info => block_on(info(secret)),
        Cmd::Send { peer, relay, text } => block_on(send(secret, &peer, &relay, &text)),
        Cmd::Recv { peer, relay } => block_on(recv(secret, &peer, &relay)),
        Cmd::Autostart { off } => {
            std::env::set_var("CLIPBRIDGE_DIR", cli.dir.display().to_string());
            ui::set_autostart(!off);
            println!("开机自启：{}", if off { "已关闭" } else { "已开启" });
            Ok(())
        }
    }
}

fn block_on<F: Future<Output = Result<()>>>(fut: F) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build runtime")?
        .block_on(fut)
}

fn build_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("clipbridge-net")
        .build()
        .context("build runtime")
}

async fn info(secret: iroh::SecretKey) -> Result<()> {
    let ep = net::build_endpoint(secret).await?;
    net::print_endpoint_info(&ep);
    ep.close().await;
    Ok(())
}

/// 已连接设备表。断开后由连接关闭事件清理。
type Peers = Arc<Mutex<HashMap<EndpointId, Connection>>>;

fn serve(dir: PathBuf, secret: iroh::SecretKey, headless: bool) -> Result<()> {
    let store = Arc::new(Store::open(dir.join("store"))?);
    let app = Arc::new(ui::App::new(Arc::clone(&store)));

    // 让自启命令写得回正确目录
    std::env::set_var("CLIPBRIDGE_DIR", dir.display().to_string());

    let net_store = Arc::clone(&store);
    let net_app = Arc::clone(&app);
    let net_dir = dir.clone();
    let net_thread = std::thread::Builder::new()
        .name("clipbridge-net".into())
        .spawn(move || {
            let rt = match build_runtime() {
                Ok(rt) => rt,
                Err(err) => {
                    eprintln!("[net] runtime: {err:#}");
                    return;
                }
            };
            if let Err(err) = rt.block_on(serve_net(secret, net_store, net_app, net_dir)) {
                eprintln!("[net] {err:#}");
            }
        })
        .context("spawn net thread")?;

    if headless {
        let _ = net_thread.join();
        return Ok(());
    }

    // 首次启动就把自启登记好，用户能在托盘菜单里关掉
    if !ui::autostart_enabled() {
        ui::set_autostart(true);
    }

    println!("=== clipbridge serve ===");
    println!("托盘已启动；Ctrl+Alt+H 打开历史窗口");
    ui::run(app)?;
    Ok(())
}

async fn serve_net(
    secret: iroh::SecretKey,
    store: Arc<Store>,
    app: Arc<ui::App>,
    dir: PathBuf,
) -> Result<()> {
    let ep = net::build_endpoint(secret).await?;
    let peers: Peers = Arc::new(Mutex::new(HashMap::new()));

    println!("=== clipbridge serve ===");
    net::print_endpoint_info(&ep);
    println!(
        "QUEUE: {} (last seq {})",
        dir.join("store").display(),
        store.last_seq()
    );
    println!("LISTENING");

    // 剪贴板 → 入队 → 历史 → 广播
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let app = Arc::clone(&app);
        ClipboardWatcher::new().spawn_poll(move |text| {
            if app.paused.load(Ordering::Relaxed) {
                return;
            }
            let _ = tx.send(text);
        })?;
    }

    {
        let store = Arc::clone(&store);
        let peers = Arc::clone(&peers);
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                let seq = match store.push(&text) {
                    Ok(seq) => seq,
                    Err(err) => {
                        eprintln!("[queue] push failed: {err:#}");
                        continue;
                    }
                };
                let head: String = text.chars().take(40).collect();
                println!(
                    "[clip] #{seq} {}{}",
                    head.replace('\n', " "),
                    if text.chars().count() > 40 { "…" } else { "" }
                );
                app.add(seq, &text);

                let conns: Vec<Connection> = peers.lock().await.values().cloned().collect();
                if conns.is_empty() {
                    println!("[clip] no peer online, queued");
                    continue;
                }
                for conn in conns {
                    let text = text.clone();
                    tokio::spawn(async move {
                        if let Err(err) = push_text(conn, seq, &text).await {
                            eprintln!("[push] {err:#}");
                        }
                    });
                }
            }
        });
    }

    loop {
        let Some(incoming) = ep.accept().await else { break };
        let peers = Arc::clone(&peers);
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            let Ok(accepting) = incoming.accept() else { return };
            let Ok(conn) = accepting.await else { return };
            let id = conn.remote_id();
            println!("[conn] accepted {}", id.fmt_short());
            peers.lock().await.insert(id, conn.clone());
            if let Err(err) = handle_conn(conn, store).await {
                eprintln!("[conn] {err:#}");
            }
            peers.lock().await.remove(&id);
            println!("[conn] closed {}", id.fmt_short());
        });
    }
    Ok(())
}

async fn handle_conn(conn: Connection, store: Arc<Store>) -> Result<()> {
    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => break,
        };
        let store = Arc::clone(&store);
        let conn2 = conn.clone();
        tokio::spawn(async move {
            let Ok(Some((ty, payload))) = protocol::read_frame(&mut recv).await else {
                return;
            };
            match ty {
                protocol::MSG_HELLO => {
                    // "v1|<last_seq>|<device_name>"
                    let body = String::from_utf8_lossy(&payload).to_string();
                    let last_seq = body
                        .split('|')
                        .nth(1)
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    println!("[hello] since #{last_seq}");
                    match store.since(last_seq) {
                        Ok(items) => {
                            println!("[replay] {} item(s)", items.len());
                            for (seq, text) in items {
                                let c = conn2.clone();
                                tokio::spawn(async move {
                                    let _ = push_text(c, seq, &text).await;
                                });
                            }
                        }
                        Err(err) => eprintln!("[replay] {err:#}"),
                    }
                }
                protocol::MSG_CLIP_TEXT => {
                    // "seq|text"，seq=0 表示手机随手上传
                    let body = String::from_utf8_lossy(&payload).to_string();
                    let (seq, text) = body.split_once('|').unwrap_or(("0", &body));
                    println!("[in] #{seq} {}", text.replace('\n', " "));
                    let _ = protocol::write_frame(&mut send, protocol::MSG_ACK, b"").await;
                    let _ = send.finish();
                }
                other => eprintln!("[conn] unsupported type {other:#04x}"),
            }
        });
    }
    Ok(())
}

/// 开一条新流推一条文本。
async fn push_text(conn: Connection, seq: u64, text: &str) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await.context("open stream")?;
    let payload = format!("{seq}|{text}");
    protocol::write_frame(&mut send, protocol::MSG_CLIP_TEXT, payload.as_bytes()).await?;
    send.finish().ok();
    // 等 ACK 最多 3s，超时就认为离线
    let _ = tokio::time::timeout(Duration::from_secs(3), recv.read_to_end(64)).await;
    Ok(())
}

async fn send(secret: iroh::SecretKey, peer: &str, relay: &str, text: &str) -> Result<()> {
    let addr = net::parse_peer(peer, relay)?;
    let ep = net::build_endpoint(secret).await?;
    let conn = ep.connect(addr, ALPN).await.context("connect failed")?;
    let (mut send, mut recv) = conn.open_bi().await.context("open stream")?;
    let payload = format!("0|{text}");
    protocol::write_frame(&mut send, protocol::MSG_CLIP_TEXT, payload.as_bytes()).await?;
    send.finish().ok();
    match tokio::time::timeout(Duration::from_secs(10), protocol::read_frame(&mut recv)).await {
        Ok(Ok(Some((protocol::MSG_ACK, _)))) => println!("✓ 对方已收到"),
        Ok(_) => println!("⚠ 连接结束但没收到 ACK"),
        Err(_) => println!("⚠ 等待 ACK 超时"),
    }
    conn.close(0u32.into(), b"done");
    ep.close().await;
    Ok(())
}

/// 调试用接收端：模拟手机，验证广播与补发链路。
async fn recv(secret: iroh::SecretKey, peer: &str, relay: &str) -> Result<()> {
    let addr = net::parse_peer(peer, relay)?;
    let ep = net::build_endpoint(secret).await?;
    let conn = ep.connect(addr, ALPN).await.context("connect failed")?;

    // HELLO(last_seq=0) 触发全量补发
    let (mut send, _) = conn.open_bi().await?;
    protocol::write_frame(&mut send, protocol::MSG_HELLO, b"v1|0|debug").await?;
    send.finish().ok();
    println!("HELLO sent, waiting for pushes…");

    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => break,
        };
        tokio::spawn(async move {
            if let Ok(Some((protocol::MSG_CLIP_TEXT, payload))) =
                protocol::read_frame(&mut recv).await
            {
                let body = String::from_utf8_lossy(&payload).to_string();
                let (seq, text) = body.split_once('|').unwrap_or(("0", &body));
                println!("[recv] #{seq} {}", text.replace('\n', " "));
                let _ = protocol::write_frame(&mut send, protocol::MSG_ACK, b"").await;
                let _ = send.finish();
            }
        });
    }
    Ok(())
}
