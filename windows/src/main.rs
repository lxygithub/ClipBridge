#![cfg_attr(windows, windows_subsystem = "windows")]
//! ClipBridge Windows 端（M1：剪贴板监听 + 多设备广播 + 断连补发 + 托盘/热键/历史窗口）。//!
//! 用法：
//!   clipbridge info
//!   clipbridge serve                        常驻：托盘 + 剪贴板推送 + Ctrl+Alt+H 历史窗口
//!   clipbridge serve --headless             无界面常驻（调试/自动化用）
//!   clipbridge send --peer --relay --text   一次性发一段文本
//!   clipbridge recv --peer --relay          调试用：作为对端接收广播

mod clipimg;
mod queue;
mod ui;

/// UTF-16 宽字符串（Win32 入参用），末尾补 0。
fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 拿命名互斥量做单实例：返回 false 表示已有进程持着。
/// 句柄会故意泄漏到进程退出——内核会在进程结束时自动释放，不怕残留。
fn take_single_instance(name: &str) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, wide(name).as_ptr());
        if h.is_null() {
            // 建不出来就别拦用户，宁可多一个实例也别让程序起不来
            return true;
        }
        match WaitForSingleObject(h, 0) {
            WAIT_OBJECT_0 | WAIT_ABANDONED => true,
            _ => {
                CloseHandle(h);
                false
            }
        }
    }
}

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use clipbridge_core::{net, protocol, transfer, ALPN, DEFAULT_DIR};
use iroh::endpoint::Connection;
use iroh::EndpointId;
use iroh::TransportAddr;
use std::path::Path;
use std::sync::Mutex;
use tokio::sync::Mutex as AsyncMutex;

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
    /// 打印本机端点身份（--diag 额外打印 NAT 类型/公网映射，用于排查打洞）
    Info {
        /// 打洞诊断：NAT 类型、公网映射、relay 延迟、实际发布的候选地址
        #[arg(long)]
        diag: bool,
        #[arg(long, default_value = clipbridge_core::net::DEFAULT_RELAY)]
        relay: String,
        /// 固定本地 UDP 端口（0 = 随机）；和 serve 用同一个端口才能判断端口映射是否生效
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// 常驻：托盘 + 监听剪贴板 + 广播
    Serve {
        /// 不建托盘/窗口（自动化测试用）
        #[arg(long)]
        headless: bool,
        /// 钉死 relay；传空串 `--relay ""` 则交回官方默认地图自动选
        #[arg(long, default_value = clipbridge_core::net::DEFAULT_RELAY)]
        relay: String,
        /// 固定本地 UDP 端口（0 = 随机）。固定后才能在路由器上做端口映射，这是提高打洞成功率最有效的一招
        #[arg(long, default_value_t = clipbridge_core::net::DEFAULT_PORT)]
        port: u16,
        /// 只绑这块网卡（IP 或 auto）。绑单卡可避免把 WireGuard/Tailscale/ZeroTier/WSL 的地址一起发布出去
        #[arg(long, default_value = "")]
        bind_ip: String,
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
    /// 向对端发一个文件
    SendFile {
        #[arg(long)]
        peer: String,
        #[arg(long, default_value = "")]
        relay: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// 开关开机自启（写入 HKCU\...\Run）
    Autostart {
        /// 关闭自启
        #[arg(long)]
        off: bool,
    },
}

fn main() -> Result<()> {
    // 默认静默（和以前一样只打自己的 println）；排查问题时设 RUST_LOG 再看 iroh 内部日志
    init_tracing();

    // GUI 子系统下没有控制台可以接住 clap 的报错，所以双击（不带参数）直接当 serve 处理，
    // 用户看到的就是托盘起来，而不是一个无声无息的退出。
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let cli = if args.len() <= 1 {
        let mut with_default = args.clone();
        with_default.push("serve".into());
        Cli::parse_from(with_default)
    } else {
        Cli::parse_from(args)
    };
    // 先定输出出口再谈别的：GUI 子系统下没有控制台，println 写失败会 panic。
    // 只有"托盘模式"要脱离控制台，其余命令/headless 都贴着父终端打印。
    setup_stdio(&cli.dir, matches!(&cli.cmd, Cmd::Serve { headless: false, .. }));
    let key_path = cli.dir.join("endpoint.key");
    let secret = net::load_or_create_secret(&key_path)?;

    match cli.cmd {
        Cmd::Serve {
            headless,
            relay,
            port,
            bind_ip,
        } => {
            let opts = net_opts(relay, port, &bind_ip)?;
            serve(cli.dir, secret, headless, opts)
        }
        Cmd::Info { diag, relay, port } => {
            block_on(info(secret, &net_opts(relay, port, "")?, diag))
        }
        Cmd::Send { peer, relay, text } => block_on(send(secret, &peer, &relay, &text)),
        Cmd::Recv { peer, relay } => block_on(recv(secret, &peer, &relay)),
        Cmd::SendFile { peer, relay, file } => block_on(send_file(secret, &peer, &relay, file)),
        Cmd::Autostart { off } => {
            std::env::set_var("CLIPBRIDGE_DIR", cli.dir.display().to_string());
            ui::set_autostart(!off);
            println!("开机自启：{}", if off { "已关闭" } else { "已开启" });
            Ok(())
        }
    }
}

/// 设了 RUST_LOG 才开 iroh 内部日志；没设就保持安静。
///
/// 排查打洞时用：`$env:RUST_LOG="iroh=debug"; clipbridge.exe info --diag`
fn init_tracing() {
    let Ok(filter) = std::env::var("RUST_LOG") else {
        return;
    };
    if filter.trim().is_empty() {
        return;
    }
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .try_init();
}

fn block_on<F: Future<Output = Result<()>>>(fut: F) -> Result<()> {    tokio::runtime::Builder::new_multi_thread()
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

/// 把 CLI 的 relay/port/bind-ip 折成 [net::NetOptions]；`bind_ip=auto` 交给系统选默认出口网卡。
fn net_opts(relay: String, port: u16, bind_ip: &str) -> Result<net::NetOptions> {
    let bind_ip = match bind_ip.trim() {
        "" => None,
        "auto" | "AUTO" => {
            let ip = net::detect_primary_ip().context(
                "detect primary ip failed（无法连上公网就用具体 IP，别用 auto）",
            )?;
            println!("[bind] 自动选中的主网卡地址：{ip}");
            Some(ip)
        }
        other => Some(other.parse().with_context(|| format!("invalid bind ip: {other}"))?),
    };
    Ok(net::NetOptions {
        relay,
        port,
        bind_ip,
    })
}

async fn info(secret: iroh::SecretKey, opts: &net::NetOptions, diag: bool) -> Result<()> {
    let ep = net::build_endpoint_opts(secret, opts).await?;
    net::print_endpoint_info(&ep);
    if diag {
        net::print_diagnostics(&ep, 15).await;
    }
    ep.close().await;
    Ok(())
}

/// 已连接设备表。断开后由连接关闭事件清理。
type Peers = Arc<AsyncMutex<HashMap<EndpointId, Connection>>>;

/// 已报价、等待数据面流的文件。
struct Pending {
    offer: transfer::Offer,
    part: PathBuf,
    flags: Arc<transfer::TransferFlags>,
}

type Pendings = Arc<Mutex<HashMap<String, Pending>>>;

/// 正在往外发的传输：id → 控制位，收到对端 `FILE_CANCEL` 时用来踩刹车。
type Outgoing = Arc<Mutex<HashMap<String, Arc<transfer::TransferFlags>>>>;

/// 谁在等某个文件 id 的 `FILE_COMPLETE`。
///
/// 对端校验完会**单开一条流**把 COMPLETE 发回来，而一条 QUIC 连接上只能有一个
/// `accept_bi()` 的拥有者：以前 `handle_conn` 和 `push_file` 各自去抢，抢输的那个
/// 把 COMPLETE 当未知帧丢掉（对端日志里的 `unsupported type 0x13`），发送方等 60 秒
/// 超时后被判定"未送达"，于是 outbox 又整份重发一遍。
/// 现在统一由 `handle_conn` 收帧，按 id 投递给正在等的发送方。
type Completions = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<transfer::Complete>>>>;

/// push_file 中途取消/出错时把 completions 里的槽位收回来，避免表里堆垃圾。
struct CompletionSlot<'a> {
    map: &'a Completions,
    id: String,
}

impl Drop for CompletionSlot<'_> {
    fn drop(&mut self) {
        self.map.lock().unwrap().remove(&self.id);
    }
}

/// 把一条 COMPLETE 帧交还给等它的发送方；等不到人（迟到/对端先关连接）就只记一行日志。
fn deliver_complete(completions: &Completions, payload: &[u8]) {
    match serde_json::from_slice::<transfer::Complete>(payload) {
        Ok(done) => {
            let waiter = completions.lock().unwrap().remove(&done.id);
            match waiter {
                Some(tx) => {
                    let _ = tx.send(done);
                }
                None => println!(
                    "[file] 收到没有等待者的 COMPLETE {}",
                    &done.id[..8.min(done.id.len())]
                ),
            }
        }
        Err(err) => eprintln!("[file] bad COMPLETE: {err:#}"),
    }
}

/// 把 stdout/stderr 钉到指定目标（追加写文件，或已存在的控制台）。
#[cfg(windows)]
unsafe fn rebind_std(name: &std::ffi::OsStr, append: bool) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_ALWAYS, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

    let mut wide: Vec<u16> = name.encode_wide().collect();
    wide.push(0);
    let (access, disposition) = if append {
        (FILE_APPEND_DATA, OPEN_ALWAYS)
    } else {
        (GENERIC_READ | GENERIC_WRITE, OPEN_EXISTING)
    };
    let handle = CreateFileW(
        wide.as_ptr(),
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        std::ptr::null(),
        disposition,
        FILE_ATTRIBUTE_NORMAL,
        std::ptr::null_mut(),
    );
    if handle as isize == -1 {
        return;
    }
    SetStdHandle(STD_OUTPUT_HANDLE, handle);
    SetStdHandle(STD_ERROR_HANDLE, handle);
}

/// 进程出口统一在这里定：release 版是 GUI 子系统，双击不会再闪黑窗口。
///
/// 代价是没有控制台，而 `println!` 写失败会 panic，所以要先决定输出去哪：
/// - 已经被重定向到文件/管道（父进程给的句柄有效）：保持原样，别动
/// - 托盘（`serve` 不带 --headless）：脱离控制台 + 追加写 `<dir>/serve.log`
/// - 其它（info/send 这类一次性命令、headless 调试）：贴父控制台打日志，
///   贴不上（比如被计划任务拉起）才落 `serve.log`
///
/// 托盘模式必须脱离，否则那个挂在进程上的控制台窗口一关，Windows 会发
/// CTRL_CLOSE_EVENT 把整个托盘带走。
#[cfg(windows)]
fn setup_stdio(dir: &Path, tray: bool) {
    use std::ffi::OsStr;
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
    };

    unsafe {
        let std_handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if !std_handle.is_null() && std_handle as isize != -1 {
            return;
        }
        if tray {
            FreeConsole();
            rotate_log(&dir.join("serve.log"));
            rebind_std(dir.join("serve.log").as_os_str(), true);
            return;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            rebind_std(OsStr::new("CONOUT$"), false);
            return;
        }
        rotate_log(&dir.join("serve.log"));
        rebind_std(dir.join("serve.log").as_os_str(), true);
    }
}

/// 日志轮转：超过 5MB 就把当前日志挪成 `.1`（只留一份，够定位最近的问题）。
#[cfg(windows)]
fn rotate_log(path: &Path) {
    let too_big = std::fs::metadata(path).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false);
    if too_big {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}

#[cfg(not(windows))]
fn rotate_log(_path: &Path) {}

#[cfg(not(windows))]
fn setup_stdio(_dir: &Path, _tray: bool) {}

fn serve(dir: PathBuf, secret: iroh::SecretKey, headless: bool, opts: net::NetOptions) -> Result<()> {
    // 日志第一行带 pid：同时开过好几个实例时，一眼能看出这条日志是哪个进程写的。
    // 放在单实例检查之前，故意让「第二个实例被挡住」也留痕。
    println!(
        "[boot] pid={} version={} headless={}",
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        headless
    );

    // 单实例：第二个进程既抢不到 4242（会静默退化成「只有窗口、没有连接」的空壳，
    // 界面上永远显示「未连接」），又会往同一份 serve.log 里写。这里用内核命名互斥量挡住。
    if !take_single_instance("ClipBridge-Serve") {
        eprintln!("[serve] 已有实例在运行（托盘里那个），本次退出");
        unsafe {
            // 别再弹模态框：从资源管理器启动时没人点「确定」的话进程会一直挂在那儿，
            // 一个窗口都不建、日志一行不写，看起来就像「启动失败」。直接把老实例的窗口顶上来。
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                FindWindowW, SetForegroundWindow, ShowWindow, SW_SHOWNORMAL,
            };
            let hwnd = FindWindowW(wide("ClipBridgeWnd").as_ptr(), std::ptr::null());
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_SHOWNORMAL);
                SetForegroundWindow(hwnd);
            }
        }
        return Ok(());
    }

    let store = Arc::new(Store::open(dir.join("store"))?);
    let app = Arc::new(ui::App::new(Arc::clone(&store), &dir));

    // 输出出口已经在 main 里的 setup_stdio 定好了（脱离控制台 / 落 serve.log / 贴终端）

    // 让自启命令写得回正确目录
    std::env::set_var("CLIPBRIDGE_DIR", dir.display().to_string());

    let net_store = Arc::clone(&store);
    let net_app = Arc::clone(&app);
    let net_dir = dir.clone();
    let net_opts = opts.clone();
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
            if let Err(err) = rt.block_on(serve_net(secret, net_store, net_app, net_dir, net_opts)) {
                eprintln!("[net] {err:#}");
                let text = format!("{err:#}");
                if text.contains("10048") {
                    eprintln!(
                        "[net] 端口 4242 被占用：大概率已经有一个 clipbridge 实例在跑，\
                         这个进程不会再接任何连接（界面就会一直显示「未连接」）。\
                         请退掉旧实例后只保留一个。"
                    );
                }
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

/// 一条连接现在走的是哪条路径：优先看选中的那条，没有就用第一条。
/// 直连是打洞成功后 iroh 新增并切过去的 IP 路径，中继是兜底。
fn conn_path(conn: &Connection) -> (bool, u64) {
    // 一条连接同时挂着中继和打洞出来的直连两条路，iroh 自己会在里面挑。
    // 但 is_selected 在某些时刻会停在中继上（切换滞后），界面上就会把 4ms 的局域网显示成 300ms。
    // 所以这里按「有效路径里最好的那条」算：有直连、且不比重继慢，就算直连。
    let mut best: Option<(bool, u64)> = None;
    for p in conn.paths().iter() {
        let pair = (p.is_ip(), p.rtt().as_millis() as u64);
        best = Some(match best {
            None => pair,
            Some(cur) => {
                let better = match (cur.0, pair.0) {
                    // 中继换直连：直连得明显更快才换，免得抖动时来回跳
                    (false, true) => pair.1 + 20 <= cur.1,
                    // 已经是直连就别退回中继
                    (true, false) => false,
                    _ => pair.1 < cur.1,
                };
                if better { pair } else { cur }
            }
        });
    }
    best.unwrap_or((false, 0))
}

/// 电脑的显示名：手机端节点卡上要能看到「已连接 <这个名字>」。
/// 取 `COMPUTERNAME`（Windows 默认就是主机名），取不到退回通用名。
fn pc_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Windows PC".to_string())
}

async fn serve_net(
    secret: iroh::SecretKey,
    store: Arc<Store>,
    app: Arc<ui::App>,
    dir: PathBuf,
    opts: net::NetOptions,
) -> Result<()> {
    let ep = net::build_endpoint_opts(secret, &opts).await?;
    let peers: Peers = Arc::new(AsyncMutex::new(HashMap::new()));
    let pending: Pendings = Arc::new(Mutex::new(HashMap::new()));
    let outgoing: Outgoing = Arc::new(Mutex::new(HashMap::new()));
    let completions: Completions = Arc::new(Mutex::new(HashMap::new()));
    let inbox = dir.join("inbox");
    let _ = std::fs::create_dir_all(&inbox);
    let outbox = dir.join("outbox");
    let _ = std::fs::create_dir_all(&outbox);
    tokio::spawn(watch_outbox(
        Arc::clone(&peers),
        Arc::clone(&app),
        Arc::clone(&outgoing),
        Arc::clone(&completions),
        outbox.clone(),
        outbox.join("sent"),
    ));

    println!("=== clipbridge serve ===");
    net::print_endpoint_info(&ep);
    println!(
        "QUEUE: {} (last seq {})",
        dir.join("store").display(),
        store.last_seq()
    );
    println!("LISTENING");

    // 不再实时监听剪贴板：只有用户在窗口里粘贴 / 拖入文件才发。
    // ui 侧通过 App::send_text / send_image 把内容送进来，和真正的广播逻辑共用一条通道。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ui::Outgoing>();
    app.set_sender(tx.clone());

    // 链路状态：每 2 秒看一眼选中路径，算出「直连 / 中继 + RTT」喂给标题栏那枚徽标。
    // 打洞成功时 iroh 会新增一条 IP 路径并切过去，relay 退成兜底，所以看 is_selected()。
    {
        let peers = Arc::clone(&peers);
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tick.tick().await;
                let map = peers.lock().await;
                let mut info = ui::LinkInfo::default();
                let mut direct_n = 0usize;
                let mut rtt_max = 0u64;
                let mut slots = Vec::new();
                for (id, conn) in map.iter() {
                    if conn.close_reason().is_some() {
                        continue;
                    }
                    let (direct, rtt_ms) = conn_path(conn);
                    info.connected = true;
                    if direct {
                        direct_n += 1;
                    }
                    rtt_max = rtt_max.max(rtt_ms);
                    slots.push(ui::PeerSlot {
                        id: id.to_string(),
                        name: String::new(),
                        direct,
                        rtt_ms,
                    });
                }
                drop(map);
                info.peers = slots.len();
                info.direct_n = direct_n;
                info.direct = slots.len() == 1 && direct_n == 1;
                info.rtt_ms = rtt_max;
                app.set_link(info);

                // 顺手把「连着的手机」同步给 UI：副标题、发送目标胶囊、行的「发给 xxx」都靠它
                app.set_peers(slots);
            }
        });
    }

    {
        let store = Arc::clone(&store);
        let peers = Arc::clone(&peers);
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            // 图片不进 store（store 是文本库），seq 只是给手机端显示个编号
            let mut img_seq = store.last_seq();
            while let Some(msg) = rx.recv().await {
                let (seq, text, image) = match msg {
                    ui::Outgoing::Text(text) => {
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
                        (seq, Some(text), None)
                    }
                    ui::Outgoing::Image(png) => {
                        img_seq += 1;
                        println!("[img] #{} {} bytes out", img_seq, png.len());
                        (img_seq, None, Some(png))
                    }
                };

                // 只发「发送目标」选中的那台；没选就是广播（旧行为）
                let conns = target_conns(&peers, &app).await;
                if conns.is_empty() {
                    println!("[clip] no peer online, queued");
                    continue;
                }
                // 记下这条到底给了谁：之后在「发给」行换目标，历史条目也不再跟着变
                if text.is_some() {
                    let label = app.current_target_label();
                    app.note_delivered(&format!("t{seq}"), &label);
                }
                for conn in conns {
                    let text = text.clone();
                    let image = image.clone();
                    tokio::spawn(async move {
                        let res = match (text, image) {
                            (Some(text), _) => push_text(conn, seq, &text).await,
                            (None, Some(png)) => push_image(conn, seq, &png).await,
                            (None, None) => Ok(()),
                        };
                        if let Err(err) = res {
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
        let pending = Arc::clone(&pending);
        let outgoing = Arc::clone(&outgoing);
        let inbox = inbox.clone();
        let completions = Arc::clone(&completions);
        let app_h = Arc::clone(&app);
    let ep_for_peer = ep.clone();
        tokio::spawn(async move {
            let Ok(accepting) = incoming.accept() else { return };
            let Ok(conn) = accepting.await else { return };
            let id = conn.remote_id();
            println!("[conn] accepted {} ({id})", id.fmt_short());
            peers.lock().await.insert(id, conn.clone());

            // 反向自我介绍：手机端的节点卡要显示电脑名，不然只能看到一串 id。
            // 走一条独立 bi 流，和 HELLO/补发逻辑互不干扰。
            {
                let hello_conn = conn.clone();
                tokio::spawn(async move {
                    if let Ok((mut s, _r)) = hello_conn.open_bi().await {
                        let payload = format!("v1|0|{}", pc_device_name());
                        println!("[hello-out] → 手机: {payload}");
                        let _ = protocol::write_frame(&mut s, protocol::MSG_HELLO, payload.as_bytes())
                            .await;
                        let _ = s.finish();
                    }
                });
            }
            // 对端「发布了哪些候选地址」+ 当前实际路径，是判断它那边 QAD 成功没有的直接证据：
            // 只有 relay 一条 = 它没测到自己公网映射，打洞不可能成。
            // 打洞要 1–4 秒，建连瞬间的快照只有 relay；所以打两次，
            // 免得日志把「还没打通」误读成「对端一条 IP 都没发布」。
            let ep2 = ep_for_peer.clone();
            let probe = conn.clone();
            tokio::spawn(async move {
                for (label, wait) in [("+2s", 2u64), ("+8s", 8u64)] {
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    if probe.close_reason().is_some() {
                        break;
                    }
                    match ep2.remote_info(id).await {
                        Some(info) => {
                            let addrs: Vec<String> = info
                                .addrs()
                                .map(|a| {
                                    let used = match a.usage() {
                                        iroh::endpoint::TransportAddrUsage::Active => "*",
                                        _ => "",
                                    };
                                    format!("{used}{:?}", a.addr())
                                })
                                .collect();
                            println!(
                                "[peer{label}] {} 候选地址: {}",
                                id.fmt_short(),
                                addrs.join(", ")
                            );
                        }
                        None => println!("[peer{label}] {} 还没拿到候选地址", id.fmt_short()),
                    }
                    let paths = probe.paths();
                    let mut shown: Vec<String> = Vec::new();
                    for p in paths.iter() {
                        let kind = match p.remote_addr() {
                            TransportAddr::Relay(url) => format!("relay({url})"),
                            TransportAddr::Ip(addr) => format!("direct({addr})"),
                            other => format!("{other:?}"),
                        };
                        shown.push(format!(
                            "{}{} rtt={}ms",
                            if p.is_selected() { "*" } else { "" },
                            kind,
                            p.stats().rtt.as_millis()
                        ));
                    }
                    println!(
                        "[path{label}] {}",
                        if shown.is_empty() {
                            "(空)".to_string()
                        } else {
                            shown.join(" | ")
                        }
                    );
                }
            });
            let tail = conn.clone();
            if let Err(err) =
                handle_conn(conn, id.to_string(), store, pending, outgoing, completions, inbox, app_h).await
            {
                eprintln!("[conn] {err:#}");
            }
            peers.lock().await.remove(&id);
            // 断链要说清是谁断的、为什么断：手机侧 hb 判死会带 "link dead"，
            // 本地主动收尾是 "done"，没 reason 说明是对端进程直接消失。
            println!(
                "[conn] closed {} reason={:?}",
                id.fmt_short(),
                tail.close_reason()
            );
        });
    }
    Ok(())
}

async fn handle_conn(
    conn: Connection,
    peer_id: String,
    store: Arc<Store>,
    pending: Pendings,
    outgoing: Outgoing,
    completions: Completions,
    inbox: PathBuf,
    app: Arc<ui::App>,
) -> Result<()> {
    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => break,
        };
        let store = Arc::clone(&store);
        let conn2 = conn.clone();
        let pending = Arc::clone(&pending);
        let outgoing = Arc::clone(&outgoing);
        let completions = Arc::clone(&completions);
        let inbox = inbox.clone();
        let app = Arc::clone(&app);
        let peer_id = peer_id.clone();
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
                    let name = body.split('|').nth(2).unwrap_or("").trim().to_string();
                    println!("[hello] since #{last_seq} from {}", if name.is_empty() { "?" } else { &name });
                    if !name.is_empty() {
                        // 手机名按 EndpointId 记：两台手机各显示各的名字，不会互相覆盖
                        app.learn_peer_name(&peer_id, &name);
                    }
                    match store.since_outgoing(last_seq) {
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
                    // 补发只覆盖 `>last_seq` 的部分。手机重装/清数据后 last_seq 仍是旧的、
                    // 本地历史却是空的，就会永远停在「通道已建立，还没有条目」。
                    // 所以每次握手都额外甩一份最近的历史快照，手机端静默合并（不弹通知）。
                    {
                        let c = conn2.clone();
                        let st = Arc::clone(&store);
                        tokio::spawn(async move {
                            if let Err(err) = push_history(c, st).await {
                                eprintln!("[history] {err:#}");
                            }
                        });
                    }
                }
                protocol::MSG_CLIP_TEXT => {
                    // "seq|text"，手机发来的 seq 只是它自己的编号，落库走本机的 seq 水位线
                    let body = String::from_utf8_lossy(&payload).to_string();
                    let (seq, text) = body.split_once('|').unwrap_or(("0", &body));
                    let from = app.peer_display(&peer_id);
                    println!("[in] #{seq} from {from}: {}", text.replace('\n', " "));
                    // 以前这里只打印 + 回 ACK，电脑端既不落历史也不进剪贴板，
                    // 看起来就是「手机说已发送，电脑上什么都没有」。
                    match store.push_incoming(text, &from) {
                        Ok(new_seq) => {
                            app.add_in(new_seq, text, &from);
                            let ok = ui::write_clipboard(text);
                            app.set_hint(&format!(
                                "收到 {from} 的 {} 字{}",
                                text.chars().count(),
                                if ok { "，已放进剪贴板" } else { "，剪贴板被占用" }
                            ));
                        }
                        Err(err) => eprintln!("[in] store failed: {err:#}"),
                    }
                    let _ = protocol::write_frame(&mut send, protocol::MSG_ACK, b"").await;
                    let _ = send.finish();
                }
                protocol::MSG_CLIP_IMAGE_META => {
                    // "<seq>|<PNG 字节>"：落盘 + 直接放进电脑剪贴板，用户切到微信就能 Ctrl+V
                    let Some(pos) = payload.iter().position(|b| *b == b'|') else {
                        eprintln!("[img] malformed payload");
                        return;
                    };
                    let seq: u64 = String::from_utf8_lossy(&payload[..pos])
                        .trim()
                        .parse()
                        .unwrap_or(0);
                    let png = &payload[pos + 1..];
                    match clipimg::decode_png(png) {
                        Some(bmp) => {
                            app.save_image(png, bmp.w, bmp.h, false);
                            let ok = clipimg::write_clipboard(&bmp);
                            println!(
                                "[img] #{seq} {}×{} · {} bytes{}",
                                bmp.w,
                                bmp.h,
                                png.len(),
                                if ok { " · 已放入剪贴板" } else { " · 剪贴板被占用" }
                            );
                            app.set_hint(if ok {
                                "手机发来一张图，已放进剪贴板"
                            } else {
                                "收到图片，但剪贴板被别的程序占用"
                            });
                        }
                        None => eprintln!("[img] bad png ({} bytes)", png.len()),
                    }
                    let _ = protocol::write_frame(&mut send, protocol::MSG_ACK, b"").await;
                    let _ = send.finish();
                }
                protocol::MSG_FILE_OFFER => match serde_json::from_slice::<transfer::Offer>(&payload) {
                    Ok(offer) => match prepare_offer(&offer, &inbox, &pending) {
                        Ok(accept) => {
                            if let Ok(json) = serde_json::to_vec(&accept) {
                                let _ = protocol::write_frame(&mut send, protocol::MSG_FILE_ACCEPT, &json).await;
                            }
                            let _ = send.finish();
                            println!(
                                "[file] offer {} · {} bytes · id {}",
                                offer.name,
                                offer.size,
                                &offer.id[..8.min(offer.id.len())]
                            );
                        }
                        Err(err) => {
                            let rej = serde_json::json!({ "id": offer.id, "reason": err.to_string() });
                            let _ = protocol::write_frame(
                                &mut send,
                                protocol::MSG_FILE_REJECT,
                                rej.to_string().as_bytes(),
                            )
                            .await;
                            let _ = send.finish();
                            eprintln!("[file] reject: {err:#}");
                        }
                    },
                    Err(err) => eprintln!("[file] bad offer: {err:#}"),
                },
                protocol::MSG_FILE_DATA => {
                    let id = String::from_utf8_lossy(&payload).trim().to_string();
                    let Some(p) = pending.lock().unwrap().remove(&id) else {
                        eprintln!("[file] data for unknown id {}", &id);
                        return;
                    };
                    println!("[file] receiving {} → {}", p.offer.name, p.part.display());
                    let received =
                        match transfer::recv_stream(&mut recv, &p.part, 0, &p.flags, p.offer.size).await
                        {
                            Ok(n) => n,
                            Err(err) => {
                                eprintln!("[file] recv failed: {err:#}");
                                return;
                            }
                        };
                    println!("[file] got {} / {} bytes", received, p.offer.size);
                    if received != p.offer.size {
                        eprintln!("[file] incomplete, part kept: {}", p.part.display());
                        return;
                    }
                    let digest = match transfer::sha256_file(&p.part) {
                        Ok(d) => d,
                        Err(err) => {
                            eprintln!("[file] hash failed: {err:#}");
                            return;
                        }
                    };
                    if !p.offer.sha256.is_empty() && !digest.eq_ignore_ascii_case(&p.offer.sha256) {
                        eprintln!("[file] sha256 mismatch, dropped");
                        let _ = std::fs::remove_file(&p.part);
                        let _ = send_complete(&conn2, &p.offer.id, false, &digest, received).await;
                        return;
                    }
                    let dir = inbox.clone();
                    let final_path = transfer::unique_path(&dir, &transfer::safe_name(&p.offer.name));
                    match std::fs::rename(&p.part, &final_path) {
                        Ok(()) => {
                            println!("[file] saved {}", final_path.display());
                            let _ = send_complete(&conn2, &p.offer.id, true, &digest, received).await;
                        }
                        Err(err) => eprintln!("[file] rename failed: {err:#}"),
                    }
                }
                protocol::MSG_FILE_CANCEL => {
                    let id = String::from_utf8_lossy(&payload).trim().to_string();
                    if let Some(p) = pending.lock().unwrap().remove(&id) {
                        p.flags.cancel();
                        let _ = std::fs::remove_file(&p.part);
                        println!("[file] canceled {}", &id);
                    }
                    // 也可能是在取消我们发出去的这一笔
                    if let Some(flags) = outgoing.lock().unwrap().get(&id).cloned() {
                        flags.cancel();
                        println!("[file] peer canceled outgoing {}", &id);
                    }
                }
                protocol::MSG_FILE_COMPLETE => {
                    // 对端（可能是正在收我们发的文件的设备）校验完的回执。它只认 id，
                    // 具体交给哪个发送任务由 completions 决定。
                    deliver_complete(&completions, &payload);
                    let _ = send.finish();
                }
                protocol::MSG_PING => {
                    let _ = protocol::write_frame(&mut send, protocol::MSG_PING, b"").await;
                    let _ = send.finish();
                }
                other => eprintln!("[conn] unsupported type {other:#04x}"),
            }
        });
    }
    Ok(())
}

/// 开一条新流推一条文本。
/// 按「发送目标」过滤在线连接：没选目标 = 全部广播；选了就只留那一台。
async fn target_conns(peers: &Peers, app: &std::sync::Arc<ui::App>) -> Vec<Connection> {
    let target = app.target_peer();
    let guard = peers.lock().await;
    guard
        .iter()
        .filter(|(id, _)| match &target {
            None => true,
            Some(t) => &id.to_string() == t,
        })
        .map(|(_, conn)| conn.clone())
        .collect()
}

/// 历史快照：最近 200 条文本，`{"items":[[seq,text,mtime],...]}`。
/// 手机端只用于填时间线，不写剪贴板、不弹通知；文本累计超过 6MB 就截断（帧上限 16MB）。
async fn push_history(conn: Connection, store: Arc<Store>) -> Result<()> {
    let all = store.all_outgoing()?;
    let start = all.len().saturating_sub(200);
    let mut arr: Vec<serde_json::Value> = Vec::new();
    let mut bytes = 0usize;
    for (seq, text, mtime) in all[start..].iter() {
        bytes += text.len();
        if bytes > 6 * 1024 * 1024 {
            break;
        }
        arr.push(serde_json::json!([seq, text, mtime]));
    }
    let body = serde_json::json!({ "items": arr }).to_string();
    let (mut send, _recv) = conn.open_bi().await.context("open history stream")?;
    protocol::write_frame(&mut send, protocol::MSG_HISTORY, body.as_bytes()).await?;
    send.finish().ok();
    println!("[history] sent {} item(s), {} bytes", arr.len(), body.len());
    Ok(())
}

async fn push_text(conn: Connection, seq: u64, text: &str) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await.context("open stream")?;
    let payload = format!("{seq}|{text}");
    protocol::write_frame(&mut send, protocol::MSG_CLIP_TEXT, payload.as_bytes()).await?;
    send.finish().ok();
    // 等 ACK 最多 3s，超时就认为离线
    let _ = tokio::time::timeout(Duration::from_secs(3), recv.read_to_end(64)).await;
    Ok(())
}

/// 推一张图：payload = `<seq>|<PNG 原始字节>`，不套 base64（省一次编解码和 1/3 膨胀）。
async fn push_image(conn: Connection, seq: u64, png: &[u8]) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await.context("open stream")?;
    let mut payload = Vec::with_capacity(png.len() + 16);
    payload.extend_from_slice(format!("{seq}|").as_bytes());
    payload.extend_from_slice(png);
    protocol::write_frame(&mut send, protocol::MSG_CLIP_IMAGE_META, &payload).await?;
    send.finish().ok();
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

/// 报价校验 + 登记待接收的文件（`.part` 用 transfer id 命名，避开怪字符）。
fn prepare_offer(
    offer: &transfer::Offer,
    inbox: &Path,
    pending: &Pendings,
) -> Result<transfer::Accept> {
    anyhow::ensure!(offer.id.len() == 32, "bad transfer id");
    anyhow::ensure!(offer.size > 0, "empty file");
    anyhow::ensure!(
        offer.size <= transfer::MAX_FILE_SIZE,
        "file too large ({} bytes > 2GB)",
        offer.size
    );
    let part = inbox.join(format!("{}.part", offer.id));
    pending.lock().unwrap().insert(
        offer.id.clone(),
        Pending {
            offer: offer.clone(),
            part,
            flags: transfer::TransferFlags::new(),
        },
    );
    Ok(transfer::Accept {
        id: offer.id.clone(),
        offset: 0,
    })
}

async fn send_complete(
    conn: &Connection,
    id: &str,
    ok: bool,
    sha: &str,
    size: u64,
) -> Result<()> {
    let (mut send, _recv) = conn.open_bi().await.context("open control stream")?;
    let body = serde_json::to_vec(&transfer::Complete {
        id: id.to_string(),
        ok,
        sha256: sha.to_string(),
        size,
    })?;
    protocol::write_frame(&mut send, protocol::MSG_FILE_COMPLETE, &body).await?;
    send.finish().ok();
    Ok(())
}

/// 按扩展名猜 MIME（只用于展示，不做严格校验）。
fn mime_of(path: &Path) -> String {
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "zip" => "application/zip",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "apk" => "application/vnd.android.package-archive",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// 每 3s 打一条 `[path]`：当前选中路径是 relay 还是直连、RTT、已发字节、丢包。
///
/// 跨网传输慢的时候，这条日志能直接区分「relay 带宽不行」和「直连被打洞失败」，
/// 光看 `connection lost: timed out` 是分不出来的。传完自动停。
fn spawn_path_probe(
    conn: &Connection,
    name: &str,
    size: u64,
    start_at: u64,
) -> tokio::task::JoinHandle<()> {
    let conn = conn.clone();
    let label = name.to_string();
    let total = size.saturating_sub(start_at);
    let mut last_tx = 0u64;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3));
        let started = std::time::Instant::now();
        loop {
            tick.tick().await;
            if conn.close_reason().is_some() {
                break;
            }
            let paths = conn.paths();
            if paths.is_empty() {
                break;
            }
            for p in paths.iter() {
                let kind = match p.remote_addr() {
                    TransportAddr::Relay(url) => format!("relay({})", url),
                    TransportAddr::Ip(addr) => format!("direct({addr})"),
                    other => format!("{other:?}"),
                };
                let st = p.stats();
                // 选中路径的累计发送字节才是真正在跑的数据量
                let tx = if p.is_selected() {
                    let delta = st.udp_tx.bytes.saturating_sub(last_tx);
                    last_tx = st.udp_tx.bytes;
                    delta
                } else {
                    0
                };
                let speed = if p.is_selected() && started.elapsed().as_secs() > 0 {
                    (st.udp_tx.bytes - start_at) as f64 / 1024.0 / started.elapsed().as_secs_f64()
                } else {
                    0.0
                };
                println!(
                    "[path] {} {}{} rtt={}ms cwnd={} mtu={} tx={}KB(+{}KB/3s ≈ {:.0}KB/s) rx={}KB lost={} 已传 {:.1}%",
                    label,
                    if p.is_selected() { "*" } else { " " },
                    kind,
                    st.rtt.as_millis(),
                    st.cwnd,
                    st.current_mtu,
                    st.udp_tx.bytes / 1024,
                    tx / 1024,
                    speed,
                    st.udp_rx.bytes / 1024,
                    st.lost_packets,
                    (st.udp_tx.bytes.saturating_sub(start_at)) as f64 * 100.0
                        / total.max(1) as f64
                );
            }
        }
    })
}

/// 在一条已有连接上推一个文件：OFFER → ACCEPT → 数据流 → 等 COMPLETE。
///
/// 既被 CLI 用（自己拨号），也被 `outbox` 监视用（复用手机拨进来的那条连接）。
async fn push_file(
    conn: &Connection,
    path: &Path,
    outgoing: &Outgoing,
    completions: &Completions,
) -> Result<()> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    anyhow::ensure!(meta.is_file(), "not a file");
    let size = meta.len();
    anyhow::ensure!(size > 0, "empty file");
    anyhow::ensure!(
        size <= transfer::MAX_FILE_SIZE,
        "file too large: {size} bytes (limit 2GB)"
    );

    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let id = transfer::new_id(&path.display().to_string(), size);
    let sha = {
        let p = path.to_path_buf();
        tokio::task::spawn_blocking(move || transfer::sha256_file(&p)).await??
    };

    let offer = transfer::Offer {
        id: id.clone(),
        name: name.clone(),
        size,
        sha256: sha.clone(),
        mime: mime_of(path),
    };

    // 1) 报价
    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await.context("open control stream")?;
    protocol::write_frame(
        &mut ctrl_send,
        protocol::MSG_FILE_OFFER,
        &serde_json::to_vec(&offer)?,
    )
    .await?;
    ctrl_send.finish().ok();
    let reply = tokio::time::timeout(Duration::from_secs(20), protocol::read_frame(&mut ctrl_recv))
        .await
        .context("offer timeout")??;
    let Some((ty, payload)) = reply else {
        anyhow::bail!("peer closed before accepting");
    };
    let mut start_at = 0u64;
    match ty {
        protocol::MSG_FILE_ACCEPT => {
            let accept: transfer::Accept = serde_json::from_slice(&payload)?;
            start_at = accept.offset.min(size);
            println!(
                "✓ 对方已接受（{}，{} bytes，从 {} 续传）",
                offer.name, offer.size, start_at
            );
        }
        protocol::MSG_FILE_REJECT => {
            anyhow::bail!("peer rejected: {}", String::from_utf8_lossy(&payload));
        }
        other => anyhow::bail!("unexpected reply {other:#04x}"),
    }

    // 2) 数据面；控制位登记到 outgoing，供对端 FILE_CANCEL 踩刹车
    //    等 COMPLETE 的槽位必须赶在发数据之前挂上：对端可能在我们发完最后一块、
    //    还没走到"等待"那一步时就把回执发回来了，晚一步就漏掉。
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    completions.lock().unwrap().insert(id.clone(), done_tx);
    let _slot = CompletionSlot {
        map: completions,
        id: id.clone(),
    };
    let flags = Arc::new(transfer::TransferFlags::new());
    // 用户把 outbox 里的源文件删掉 = 取消这一笔。
    // 注意不能靠 watch_outbox 的 read_dir 判：文件删了就不再出现在目录列表里，
    // 那段检查是死代码。这里在发送期间直接盯着源文件本身。
    let watch = {
        let watch_path = path.to_path_buf();
        let flags = Arc::clone(&flags);
        let conn = conn.clone();
        let id = id.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(800)).await;
                if flags.state() != transfer::ST_RUN {
                    break;
                }
                if !watch_path.exists() {
                    println!("[cancel] 源文件已删除，停止发送 {}", watch_path.display());
                    flags.cancel();
                    // 必须也让对端刹车，否则手机还挂在旧传输上、删不掉半截文件，
                    // 后续 OFFER 也会因为连接被重传占满而超时
                    if let Ok((mut s, _r)) = conn.open_bi().await {
                        let _ = protocol::write_frame(&mut s, protocol::MSG_FILE_CANCEL, id.as_bytes())
                            .await;
                        let _ = s.finish();
                    }
                    break;
                }
            }
        })
    };
    outgoing.lock().unwrap().insert(id.clone(), Arc::clone(&flags));
    let (mut send, _recv) = conn.open_bi().await.context("open data stream")?;
    protocol::write_frame(&mut send, protocol::MSG_FILE_DATA, id.as_bytes()).await?;
    let probe = spawn_path_probe(conn, &name, size, start_at);
    let outcome = transfer::send_stream(&mut send, path, start_at, &flags).await?;
    watch.abort();
    probe.abort();
    println!("→ 发送结果 {:?}", outcome);
    outgoing.lock().unwrap().remove(&id);
    if let transfer::SendOutcome::Canceled(_) = outcome {
        anyhow::bail!("canceled by peer");
    }
    if let transfer::SendOutcome::Paused(_) = outcome {
        anyhow::bail!("paused");
    }

    // 3) 等对端校验结果。**必须拿到 COMPLETE 才算送达**：
    //    之前超时后直接当成功，于是数据其实没到齐（跨网 relay 丢包）的文件也被挪进 sent/，
    //    从此再也不重试。现在改成失败返回，outbox 会用稳定 id 从 .part 续传补齐。
    //    回执由 handle_conn 收（它是这条连接上 accept_bi() 的唯一拥有者）再投递过来。
    let done: transfer::Complete = match tokio::time::timeout(Duration::from_secs(60), done_rx).await
    {
        Ok(Ok(done)) => done,
        Ok(Err(_)) => anyhow::bail!("COMPLETE 通道已关闭"),
        Err(_) => anyhow::bail!("no COMPLETE from peer (data may be incomplete, will resume)"),
    };
    println!(
        "{} 校验 {} · {} bytes · sha256 {}",
        if done.ok { "✓ 已完成" } else { "✗ 失败" },
        if done.ok { "通过" } else { "不通过" },
        done.size,
        &done.sha256[..8.min(done.sha256.len())]
    );
    anyhow::ensure!(done.ok, "peer reported failure");
    Ok(())
}

/// 一次性发文件：自己拨号到对端，再走 [push_file]。
async fn send_file(
    secret: iroh::SecretKey,
    peer: &str,
    relay: &str,
    path: PathBuf,
) -> Result<()> {
    let addr = net::parse_peer(peer, relay)?;
    let ep = net::build_endpoint(secret).await?;
    let conn = ep.connect(addr, ALPN).await.context("connect failed")?;
    let outgoing: Outgoing = Arc::new(Mutex::new(HashMap::new()));
    let completions: Completions = Arc::new(Mutex::new(HashMap::new()));
    // 这条连接只用来发文件，但仍然需要唯一的 accept_bi() 拥有者：
    // 否则 CLI 发送也会和 push_file 抢 COMPLETE 那条流（抢输就判"未送达"）。
    {
        let conn = conn.clone();
        let completions = Arc::clone(&completions);
        tokio::spawn(async move {
            while let Ok((mut s, mut r)) = conn.accept_bi().await {
                let completions = Arc::clone(&completions);
                tokio::spawn(async move {
                    if let Ok(Some((protocol::MSG_FILE_COMPLETE, payload))) =
                        protocol::read_frame(&mut r).await
                    {
                        deliver_complete(&completions, &payload);
                    }
                    let _ = s.finish();
                });
            }
        });
    }
    push_file(&conn, &path, &outgoing, &completions).await?;
    conn.close(0u32.into(), b"done");
    ep.close().await;
    Ok(())
}

/// `outbox/` 监视：放进去的文件会被推给所有在线设备，成功后移到 `outbox/sent/`。
///
/// 取消：传输中把 outbox 里的源文件删掉即可；被对端取消的落到 `outbox/cancelled/`，不再重试。
async fn watch_outbox(
    peers: Peers,
    app: Arc<ui::App>,
    outgoing: Outgoing,
    completions: Completions,
    outbox: PathBuf,
    sent: PathBuf,
) {
    let _ = std::fs::create_dir_all(&sent);
    let cancelled = outbox.join("cancelled");
    let _ = std::fs::create_dir_all(&cancelled);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let entries = match std::fs::read_dir(&outbox) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let conns = target_conns(&peers, &app).await;
            if conns.is_empty() {
                continue;
            }
            let mut all_ok = true;
            // 记下这份文件发给了谁（按文件名记，之后挪到 sent/ 名字不变）
            if let Some(name) = path.file_name().map(|s| s.to_string_lossy().to_string()) {
                let label = app.current_target_label();
                app.note_delivered(&format!("fname:{name}"), &label);
            }
            for conn in conns {
                if let Err(err) = push_file(&conn, &path, &outgoing, &completions).await {
                    let text = format!("{err:#}");
                    eprintln!("[outbox] {text}");
                    if text.contains("canceled by peer") {
                        if let Some(name) = path.file_name() {
                            let _ = std::fs::rename(&path, cancelled.join(name));
                        }
                        all_ok = false;
                        break;
                    }
                    all_ok = false;
                }
            }
            if all_ok {
                if let Some(name) = path.file_name() {
                    let _ = std::fs::rename(&path, sent.join(name));
                }
                println!("[outbox] sent {}", path.display());
            }
        }
    }
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



