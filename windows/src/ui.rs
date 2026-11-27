//! Windows 端常驻界面：托盘 + 全局热键 + 主窗口（粘贴/拖入区 + 日志与文件双栏）。
//!
//! 刻意不引入 GUI 框架：标题、粘贴区、状态徽标都用 GDI 自绘，
//! 只有两个列表用内置 LISTBOX（自带滚动条，省得自己写虚拟化）。
//!
//! 线程模型：Win32 消息循环必须在主线程；iroh 网络任务跑在独立 tokio 线程，
//! 两边只通过 `App`（Mutex + Atomic）、发送通道和 `PostMessageW(WM_APP_REFRESH)` 交互。

use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateBitmap, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC, LineTo, MoveToEx, Polygon,
    GetStockObject, InvalidateRect, ReleaseDC, RoundRect, SelectObject, NULL_BRUSH, NULL_PEN, SetBkMode, SetTextColor, DT_CENTER,
    DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
    DT_RIGHT, DT_END_ELLIPSIS, BitBlt, SRCCOPY,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, RegisterHotKey, SetFocus, MOD_ALT, MOD_CONTROL, VK_CONTROL, VK_V,
};
use windows_sys::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, Shell_NotifyIconW, ShellExecuteW, NIF_ICON,
    NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::queue::Store;

const WM_APP_REFRESH: u32 = WM_APP + 10;
const WM_TRAY: u32 = WM_APP + 1;
const TIMER_TICK: usize = 1;
/// 复制/发送后的浮动提示，1.2 秒后自动消失
const TIMER_HINT: usize = 2;

const CMD_OPEN: usize = 2001;
const CMD_AUTOSTART: usize = 2003;
const CMD_QUIT: usize = 2004;

const CLASS_NAME: &str = "ClipBridgeWnd";
const TRAY_UID: u32 = 1;
const HOTKEY_ID: i32 = 1;

// 版面：标题 0..56，两块热区 62..148，列表标题 160..180，条目从这里开始
const LIST_TOP: i32 = 196;
const ROW_H: i32 = 46;
const ROW_GAP: i32 = 6;
const ROW_STRIDE: i32 = ROW_H + ROW_GAP;

// 和手机端「一条通道」同一套色板；COLORREF 是 0x00BBGGRR，别写反
const C_BG: u32 = 0x0015_0E07; // #070E15
const C_PANEL: u32 = 0x001C_140B; // #0B141C
const C_INK: u32 = 0x00F2_EBE4; // #E4EBF2
const C_DIM: u32 = 0x00B3_A496; // #96A4B3
const C_FAINT: u32 = 0x0066_5B52; // #525B66
const C_TEAL: u32 = 0x00C0_D37B; // #7BD3C0
const C_AMBER: u32 = 0x005B_B9F5; // #F5B95B
const C_BORDER: u32 = 0x0032_2318; // #182332

// 两块热区各自的底色，靠色相区分，不再用一条分隔线硬切
const C_ZONE_TEXT_BG: u32 = 0x001C1E0E; // #0E1E1C 偏青
const C_ROW_HOVER: u32 = 0x00281D12; // #121D28

// 列表行按方向上色，口径与手机端时间线完全一致：
//   发 = 琥珀 #F5B95B，收 = 青 #7BD3C0
// 之前用「蓝 vs 青」，两个色相太近，扫一眼分不出来；手机端用的就是琥珀/青，蓝色不再参与方向语义。
// 静默时走 *_DIM（暗一档，1px 边框不刺眼），悬浮时才提亮到满色。
const C_SEND: u32 = 0x005B_B9F5; // #F5B95B 琥珀
const C_SEND_DIM: u32 = 0x0033_668A; // #8A6633 暗琥珀
const C_RECV: u32 = 0x00C0_D37B; // #7BD3C0 青
const C_RECV_DIM: u32 = 0x006B_7644; // #44766B 暗青

/// 窗口与控件共用的画刷/字体（进程生命周期内复用）。
static BG_BRUSH: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
static UI_FONT: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
static TITLE_FONT: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
static SMALL_FONT: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

fn bg_brush() -> HBRUSH {
    let raw = *BG_BRUSH.get_or_init(|| unsafe { CreateSolidBrush(C_BG) as isize });
    raw as *mut core::ffi::c_void
}

unsafe fn font_of(px: i32, weight: i32) -> WPARAM {
    let face = wide("Microsoft YaHei UI");
    CreateFontW(
        px,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        1u32,
        0,
        0,
        0,
        0u32,
        face.as_ptr(),
    ) as isize as WPARAM
}

unsafe fn ui_font() -> WPARAM {
    *UI_FONT.get_or_init(|| font_of(16, 400) as isize) as WPARAM
}

unsafe fn title_font() -> WPARAM {
    *TITLE_FONT.get_or_init(|| font_of(26, 600) as isize) as WPARAM
}

unsafe fn small_font() -> WPARAM {
    *SMALL_FONT.get_or_init(|| font_of(14, 400) as isize) as WPARAM
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(hwnd: HWND, attr: u32, pv: *const core::ffi::c_void, cb: u32) -> i32;
}

pub struct HistoryItem {
    pub seq: u64,
    pub text: String,
    pub mtime: u64,
}

/// 一条连接当前的链路状态，给标题栏那枚徽标用。
#[derive(Clone, Copy, Default)]
pub struct LinkInfo {
    pub connected: bool,
    /// true = 打洞成功走 IP 直连；false = 走 relay
    pub direct: bool,
    pub rtt_ms: u64,
}

/// 列表里的一行：要么是一条文本，要么是一个文件。
pub enum RowKind {
    Text { text: String },
    File { path: PathBuf },
}

pub struct Row {
    pub kind: RowKind,
    pub mtime: u64,
    /// true = 电脑发出去的，false = 手机发过来的；决定行边框配色
    pub outgoing: bool,
    /// 预渲染的一行主文案
    pub title: String,
    /// 右侧元信息（大小 / 时间）
    pub meta: String,
}

pub struct App {
    pub store: Arc<Store>,
    items: Mutex<Vec<HistoryItem>>,
    rows: Mutex<Vec<Row>>,
    link: Mutex<LinkInfo>,
    /// 已连接设备（手机）的显示名，来自手机 HELLO；用来在标题下和列表行里标"发送给/来自 xxx"
    peer_name: Mutex<String>,
    inbox: PathBuf,
    outbox: PathBuf,
    sender: Mutex<Option<UnboundedSender<String>>>,
    pub paused: AtomicBool,
    msg_hwnd: AtomicIsize,
    hist_hwnd: AtomicIsize,
    /// 0 = 条目页，1 = 日志页（同一个窗口里翻页，不再单开窗口）
    page: AtomicIsize,
    scroll: AtomicIsize,
    hover: AtomicIsize,
    /// 悬浮在哪个热区：0 无、1 粘贴、2 选文件
    zone_hover: AtomicIsize,
    /// 底部浮动提示，1.2 秒后清掉
    hint: Mutex<Option<String>>,
}

impl App {
    pub fn new(store: Arc<Store>, dir: &Path) -> Self {
        let items = store
            .all()
            .unwrap_or_default()
            .into_iter()
            .map(|(seq, text, mtime)| HistoryItem { seq, text, mtime })
            .collect();
        Self {
            store,
            items: Mutex::new(items),
            rows: Mutex::new(Vec::new()),
            link: Mutex::new(LinkInfo::default()),
            peer_name: Mutex::new(String::new()),
            inbox: dir.join("inbox"),
            outbox: dir.join("outbox"),
            sender: Mutex::new(None),
            paused: AtomicBool::new(false),
            msg_hwnd: AtomicIsize::new(0),
            hist_hwnd: AtomicIsize::new(0),
            page: AtomicIsize::new(0),
            scroll: AtomicIsize::new(0),
            hover: AtomicIsize::new(-1),
            zone_hover: AtomicIsize::new(0),
            hint: Mutex::new(None),
        }
    }

    /// 网络线程把"发一段文本"的通道交进来（手动粘贴走这里）。
    pub fn set_sender(&self, tx: UnboundedSender<String>) {
        *self.sender.lock().unwrap() = Some(tx);
    }

    pub fn send_text(&self, text: String) -> bool {
        match self.sender.lock().unwrap().as_ref() {
            Some(tx) => tx.send(text).is_ok(),
            None => false,
        }
    }

    pub fn set_link(&self, info: LinkInfo) {
        let mut cur = self.link.lock().unwrap();
        // 连接状态变了才重绘，免得每秒都刷一次整个窗口
        let changed = cur.connected != info.connected
            || cur.direct != info.direct
            || cur.rtt_ms / 50 != info.rtt_ms / 50;
        *cur = info;
        drop(cur);
        if changed {
            self.refresh();
        }
    }

    /// 手机连上来时报上自己的名字；变了就重绘（标题下的设备名 + 列表行的「发送给/来自」都要跟着变）。
    pub fn set_peer_name(&self, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let changed = {
            let mut cur = self.peer_name.lock().unwrap();
            let changed = *cur != name;
            *cur = name;
            changed
        };
        if changed {
            self.refresh();
        }
    }

    fn peer_label(&self) -> String {
        let name = self.peer_name.lock().unwrap().clone();
        if name.is_empty() {
            "手机".to_string()
        } else {
            name
        }
    }

    /// 新条目：进内存 + 刷新窗口（网络线程调用）。
    pub fn add(&self, seq: u64, text: &str) {
        let mtime = crate::queue::now_secs();
        self.items.lock().unwrap().insert(
            0,
            HistoryItem {
                seq,
                text: text.to_string(),
                mtime,
            },
        );
        self.refresh();
    }

    pub fn refresh(&self) {
        let hwnd = self.hist_hwnd.load(Ordering::Relaxed);
        if hwnd != 0 {
            unsafe { PostMessageW(hwnd as HWND, WM_APP_REFRESH, 0, 0) };
        }
    }

    /// 翻到条目页 / 日志页；两页共用一块列表区，只换数据源。
    pub fn set_page(&self, page: isize) {
        self.page.store(page, Ordering::Relaxed);
        self.scroll.store(0, Ordering::Relaxed);
        self.hover.store(-1, Ordering::Relaxed);
        let hwnd = self.hist_hwnd.load(Ordering::Relaxed) as HWND;
        if !hwnd.is_null() {
            unsafe {
                refresh_rows(self, hwnd);
                InvalidateRect(hwnd, ptr::null(), 0);
            }
        }
    }

    pub fn page(&self) -> isize {
        self.page.load(Ordering::Relaxed)
    }

    /// 底部浮动提示（复制成功、发送等）。窗口在的话顺手起个 1.2 秒的定时器收尾。
    pub fn set_hint(&self, text: &str) {
        *self.hint.lock().unwrap() = Some(text.to_string());
        let hwnd = self.hist_hwnd.load(Ordering::Relaxed) as HWND;
        if !hwnd.is_null() {
            unsafe {
                InvalidateRect(hwnd, ptr::null(), 0);
                SetTimer(hwnd, TIMER_HINT, 1200, None);
            }
        }
    }

    fn clear_hint(&self) {
        *self.hint.lock().unwrap() = None;
    }

    /// 把文件塞进 outbox，交给 outbox 监视器发出去。
    /// 同一卷优先做硬链接（大文件不复制一份），跨卷才真拷贝。
    pub fn queue_file(&self, src: &Path) -> std::io::Result<PathBuf> {
        let name = src
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let mut dst = self.outbox.join(&name);
        let mut n = 1;
        while dst.exists() {
            let stem = Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| name.clone());
            let ext = Path::new(&name)
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy()))
                .unwrap_or_default();
            dst = self.outbox.join(format!("{stem}({n}){ext}"));
            n += 1;
        }
        std::fs::create_dir_all(&self.outbox)?;
        if std::fs::hard_link(src, &dst).is_err() {
            std::fs::copy(src, &dst)?;
        }
        self.refresh();
        Ok(dst)
    }
}

/// 启动托盘 + 热键 + 主窗口，阻塞在消息循环里直到用户退出。
pub fn run(app: Arc<App>) -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(ptr::null());
        let class = wide(CLASS_NAME);
        let icon = make_icon(32);

        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.style = CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(ptr::null_mut(), IDC_ARROW);
        wc.hbrBackground = bg_brush() as _;
        wc.hIcon = icon;
        wc.hIconSm = make_icon(16);
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);

        let app_ptr = Arc::as_ptr(&app) as *mut core::ffi::c_void;

        // 消息窗口：只收托盘回调与热键，永远不显示
        let msg = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("ClipBridge").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            ptr::null_mut(),
            hinst,
            app_ptr,
        );
        if msg.is_null() {
            anyhow::bail!("create message window failed");
        }
        app.msg_hwnd.store(msg as isize, Ordering::Relaxed);

        add_tray(msg, icon);
        RegisterHotKey(msg, HOTKEY_ID, MOD_CONTROL | MOD_ALT, b'H' as u32);

        // 主窗口先建好再隐藏：打开时只 ShowWindow，首次弹出不会卡
        let hist = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("ClipBridge").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            240,
            120,
            820,
            640,
            ptr::null_mut(),
            ptr::null_mut(),
            hinst,
            app_ptr,
        );
        if hist.is_null() {
            anyhow::bail!("create main window failed");
        }
        app.hist_hwnd.store(hist as isize, Ordering::Relaxed);
        SendMessageW(hist, WM_SETICON, 1, icon as LPARAM);
        SendMessageW(hist, WM_SETICON, 0, make_icon(16) as LPARAM);

        darken(hist);
        layout(hist);
        DragAcceptFiles(hist, 1);
        SetTimer(hist, TIMER_TICK, 2000, None);
        refresh_rows(&app, hist);

        let mut m: MSG = std::mem::zeroed();
        while GetMessageW(&mut m, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
    Ok(())
}

unsafe fn add_tray(hwnd: HWND, icon: HICON) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = icon;
    let tip = wide("ClipBridge");
    let n = tip.len().min(127);
    nid.szTip[..n].copy_from_slice(&tip[..n]);
    Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn remove_tray(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App;
    let is_hist = !app.is_null() && (*app).hist_hwnd.load(Ordering::Relaxed) == hwnd as isize;

    match msg {
        WM_CREATE => {
            let cs = lp as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            0
        }
        WM_SIZE => {
            if is_hist {
                layout(hwnd);
            }
            0
        }
        WM_TIMER => {
            if !is_hist {
                return 0;
            }
            if wp == TIMER_HINT {
                (*app).clear_hint();
                KillTimer(hwnd, TIMER_HINT);
                InvalidateRect(hwnd, ptr::null(), 0);
            } else if wp == TIMER_TICK {
                let changed = refresh_rows(&*app, hwnd);
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                // 头部（标题/日志入口/状态徽标/热区）每 2 秒重画一次，
                // 徽标不会因为没失效而一直显示「未连接」
                let head = RECT {
                    left: 0,
                    top: 0,
                    right: rc.right,
                    bottom: LIST_TOP - 8,
                };
                InvalidateRect(hwnd, &head, 0);
                if changed {
                    let rows_area = RECT {
                        left: 0,
                        top: LIST_TOP - 8,
                        right: rc.right,
                        bottom: rc.bottom,
                    };
                    InvalidateRect(hwnd, &rows_area, 0);
                }
            }
            0
        }
        WM_APP_REFRESH => {
            if is_hist {
                refresh_rows(&*app, hwnd);
                InvalidateRect(hwnd, ptr::null(), 0);
            }
            0
        }
        WM_PAINT => {
            if is_hist {
                paint(hwnd, &*app);
            } else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEWHEEL => {
            if is_hist {
                let delta = ((wp >> 16) & 0xFFFF) as i16 as i32;
                scroll_by(&*app, hwnd, -(delta / 120) * 3);
            }
            0
        }
        WM_MOUSEMOVE => {
            if is_hist {
                let pt = POINT {
                    x: (((lp & 0xFFFF) as i16) as i32),
                    y: (((lp >> 16) & 0xFFFF) as i16) as i32,
                };
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                let idx = row_at(&*app, rc.right, pt);
                let zone = {
                    let (_, text_r, file_r) = zone_rects(rc.right);
                    if hit(&text_r, pt) {
                        1
                    } else if hit(&file_r, pt) {
                        2
                    } else {
                        0
                    }
                };
                let prev = (*app).hover.swap(idx, Ordering::Relaxed);
                let pz = (*app).zone_hover.swap(zone, Ordering::Relaxed);
                if prev != idx || pz != zone {
                    InvalidateRect(hwnd, ptr::null(), 0);
                }
            }
            0
        }
        WM_DROPFILES => {
            if !app.is_null() {
                let hdrop = wp as *mut core::ffi::c_void;
                let n = DragQueryFileW(hdrop, u32::MAX, ptr::null_mut(), 0);
                for i in 0..n {
                    let len = DragQueryFileW(hdrop, i, ptr::null_mut(), 0);
                    let mut buf = vec![0u16; len as usize + 1];
                    DragQueryFileW(hdrop, i, buf.as_mut_ptr(), len + 1);
                    let path = String::from_utf16_lossy(&buf[..len as usize]);
                    (*app).queue_file(Path::new(&path)).ok();
                }
                DragFinish(hdrop);
                (*app).set_hint("已加入发送队列");
                refresh_rows(&*app, hwnd);
                InvalidateRect(hwnd, ptr::null(), 0);
            }
            0
        }
        WM_HOTKEY => {
            if !app.is_null() {
                show_history(&*app);
            }
            0
        }
        WM_TRAY => {
            if app.is_null() {
                return 0;
            }
            match lp as u32 {
                WM_LBUTTONDBLCLK | WM_LBUTTONUP => show_history(&*app),
                WM_RBUTTONUP => show_tray_menu(hwnd, &*app),
                _ => {}
            }
            0
        }
        WM_LBUTTONUP => {
            if is_hist {
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                let (_, text_r, file_r) = zone_rects(rc.right);
                let (x, y) = (((lp & 0xFFFF) as i16) as i32, (((lp >> 16) & 0xFFFF) as i16) as i32);
                let pt = POINT { x, y };
                if hit(&text_r, pt) {
                    send_clipboard(&*app, hwnd);
                } else if hit(&file_r, pt) {
                    pick_file(&*app);
                } else if hit(&log_chip_rect(rc.right), pt) {
                    let next = if (*app).page() == 0 { 1 } else { 0 };
                    (*app).set_page(next);
                } else {
                    activate_row(&*app, hwnd, pt);
                }
            }
            0
        }
        WM_KEYDOWN => {
            if is_hist && wp == VK_V as usize && GetKeyState(VK_CONTROL as i32) < 0 {
                // 悬浮在哪块都行，Ctrl+V 直接发剪贴板里的文本
                send_clipboard(&*app, hwnd);
            }
            0
        }
        WM_COMMAND => {
            if app.is_null() {
                return 0;
            }
            let id = (wp & 0xFFFF) as usize;
            let _code = ((wp >> 16) & 0xFFFF) as u32;
            match id {
                CMD_OPEN => show_history(&*app),
                CMD_AUTOSTART => {
                    let on = !autostart_enabled();
                    set_autostart(on);
                }
                CMD_QUIT => {
                    remove_tray((*app).msg_hwnd.load(Ordering::Relaxed) as HWND);
                    DestroyWindow((*app).msg_hwnd.load(Ordering::Relaxed) as HWND);
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            // 窗口只隐藏；消息窗口才是退出入口
            if is_hist {
                ShowWindow(hwnd, SW_HIDE);
                return 0;
            }
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn hit(rc: &RECT, pt: POINT) -> bool {
    pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom
}

/// 顶部一张卡片，左右两半：左半粘贴文本（接 Ctrl+V），右半选择文件。
/// 返回（整卡、左半、右半）。
fn zone_rects(width: i32) -> (RECT, RECT, RECT) {
    let pad = 16;
    let top = 78;
    let h = 82;
    let card = RECT {
        left: pad,
        top,
        right: width - pad,
        bottom: top + h,
    };
    let mid = pad + (width - pad * 2) / 2;
    let text = RECT {
        left: pad,
        top,
        right: mid,
        bottom: top + h,
    };
    let file = RECT {
        left: mid,
        top,
        right: width - pad,
        bottom: top + h,
    };
    (card, text, file)
}

fn badge_rect(width: i32) -> RECT {
    RECT {
        left: width - 16 - 132,
        top: 18,
        right: width - 16,
        bottom: 46,
    }
}

fn log_chip_rect(width: i32) -> RECT {
    RECT {
        left: width - 16 - 132 - 10 - 62,
        top: 18,
        right: width - 16 - 132 - 10,
        bottom: 46,
    }
}

/// 第 i 行（已扣掉滚动偏移）在客户区里的矩形。
fn row_rect(width: i32, i: i32, scroll: i32) -> RECT {
    let top = LIST_TOP + (i - scroll) * ROW_STRIDE;
    RECT {
        left: 16,
        top,
        right: width - 16,
        bottom: top + ROW_H,
    }
}

fn visible_rows(height: i32) -> i32 {
    ((height - LIST_TOP - 8) / ROW_STRIDE).max(0)
}

/// 鼠标落在第几行（考虑滚动），不在任何行上返回 -1。
fn row_at(app: &App, width: i32, pt: POINT) -> isize {
    if pt.y < LIST_TOP || pt.x < 16 || pt.x >= width - 16 {
        return -1;
    }
    let scroll = app.scroll.load(Ordering::Relaxed) as i32;
    let slot = (pt.y - LIST_TOP) / ROW_STRIDE;
    let in_gap = (pt.y - LIST_TOP) % ROW_STRIDE >= ROW_H;
    if in_gap {
        return -1;
    }
    let idx = scroll + slot;
    if (idx as usize) < app.rows.lock().unwrap().len() {
        idx as isize
    } else {
        -1
    }
}

/// 暗色化标题栏。
unsafe fn darken(hwnd: HWND) {
    let on: u32 = 1;
    let p = &on as *const u32 as *const core::ffi::c_void;
    if DwmSetWindowAttribute(hwnd, 20, p, 4) != 0 {
        DwmSetWindowAttribute(hwnd, 19, p, 4);
    }
}

/// 主窗口没有子控件，布局只负责夹住滚动位置并整窗重绘。
unsafe fn layout(hwnd: HWND) {
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let app = get_app(hwnd);
    if app.is_null() {
        return;
    }
    let max_scroll = max_scroll_for(&*app, rc.right, rc.bottom);
    let scroll = (*app).scroll.load(Ordering::Relaxed).min(max_scroll as isize);
    (*app).scroll.store(scroll, Ordering::Relaxed);
    InvalidateRect(hwnd, ptr::null(), 0);
}

unsafe fn get_app(hwnd: HWND) -> *const App {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App
}

fn max_scroll_for(app: &App, _width: i32, height: i32) -> i32 {
    let n = app.rows.lock().unwrap().len() as i32;
    (n - visible_rows(height)).max(0)
}

unsafe fn scroll_by(app: &App, hwnd: HWND, delta: i32) {
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let max = max_scroll_for(app, rc.right, rc.bottom) as isize;
    let cur = app.scroll.load(Ordering::Relaxed);
    let next = (cur + delta as isize).clamp(0, max);
    app.scroll.store(next, Ordering::Relaxed);
    InvalidateRect(hwnd, ptr::null(), 0);
}


unsafe fn paint(hwnd: HWND, app: &App) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let w = rc.right.max(1);
    let h = rc.bottom.max(1);

    // 双缓冲：自绘内容多，直接画到窗口上会闪
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(mem, bmp as _);
    draw_main(mem, app, w, h);
    BitBlt(hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old);
    DeleteObject(bmp as _);
    DeleteDC(mem);

    EndPaint(hwnd, &ps);
}

unsafe fn draw_main(hdc: HDC, app: &App, w: i32, h: i32) {
    let full = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    FillRect(hdc, &full, bg_brush());
    SetBkMode(hdc, TRANSPARENT as i32);

    // 标题
    text_in(
        hdc,
        "ClipBridge",
        RECT {
            left: 16,
            top: 8,
            right: w - 260,
            bottom: 44,
        },
        title_font(),
        C_INK,
        DT_SINGLELINE | DT_VCENTER,
    );

    // 副标题：已连接的设备名（手机报上来的名字；没连就写未连接）
    let (sub_text, sub_color) = {
        let connected = app.link.lock().unwrap().connected;
        if connected {
            (format!("已连接 · {}", app.peer_label()), C_DIM)
        } else {
            ("未连接（手机连上后自动显示设备名）".to_string(), C_FAINT)
        }
    };
    text_in(
        hdc,
        &sub_text,
        RECT {
            left: 18,
            top: 44,
            right: w - 260,
            bottom: 72,
        },
        small_font(),
        sub_color,
        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );

    // 页面切换：条目页 ↔ 日志页，同一个窗口里翻
    let on_log = app.page() == 1;
    let lg = log_chip_rect(w);
    fill_round(hdc, lg, C_PANEL, if on_log { C_TEAL } else { C_BORDER }, 14);
    text_in(
        hdc,
        if on_log { "返回" } else { "日志" },
        lg,
        small_font(),
        if on_log { C_TEAL } else { C_DIM },
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    // 状态徽标：直连/中继 + 延迟
    let link = *app.link.lock().unwrap();
    let (label, fg, bg) = if !link.connected {
        ("未连接".to_string(), 0x0087_9584u32, 0x001E_2C39u32)
    } else if link.direct {
        let ms = link.rtt_ms;
        let (fg, bg) = if ms <= 60 {
            (0x00A6_E37Bu32, 0x002B_4314u32)
        } else if ms <= 160 {
            (0x005B_B9F5u32, 0x0012_324Au32)
        } else {
            (0x009A_9AFFu32, 0x001D_1D4Au32)
        };
        (format!("直连 · {}", fmt_rtt(ms)), fg, bg)
    } else {
        (
            format!("中继 · {}", fmt_rtt(link.rtt_ms)),
            0x00B3_A496u32,
            0x0023_1B14u32,
        )
    };
    let chip = badge_rect(w);
    fill_round(hdc, chip, bg, bg, 14);
    text_in(
        hdc,
        &label,
        chip,
        ui_font(),
        fg,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    // 一张卡片，左右两半：左半粘贴文本（悬浮即接收 Ctrl+V），右半选择文件
    let (card, text_z, file_z) = zone_rects(w);
    let zh = app.zone_hover.load(Ordering::Relaxed);
    fill_round(hdc, card, C_PANEL, if zh != 0 { C_TEAL } else { C_BORDER }, 10);
    // 两半之间的细线
    let pen = CreatePen(PS_SOLID, 1, C_BORDER);
    let old_pen = SelectObject(hdc, pen as _);
    MoveToEx(hdc, text_z.right, card.top + 14, ptr::null_mut());
    LineTo(hdc, text_z.right, card.bottom - 14);
    SelectObject(hdc, old_pen);
    DeleteObject(pen as _);

    paint_half(
        hdc,
        text_z,
        zh == 1,
        C_TEAL,
        "粘贴文本",
        "Ctrl+V 或点这里直接发送",
        true,
    );
    paint_half(
        hdc,
        file_z,
        zh == 2,
        C_AMBER,
        "选择文件",
        "也可以把文件拖进窗口",
        false,
    );

    // 列表标题（两页共用同一块列表区）
    let log_page = app.page() == 1;
    let count = app.rows.lock().unwrap().len();
    text_in(
        hdc,
        if log_page { "交换日志" } else { "全部条目" },
        RECT {
            left: 18,
            top: LIST_TOP - 30,
            right: w,
            bottom: LIST_TOP - 8,
        },
        small_font(),
        C_FAINT,
        DT_SINGLELINE,
    );
    let count_label = if log_page {
        format!("共 {count} 条文本")
    } else {
        format!("共 {count} 条")
    };
    text_in(
        hdc,
        &count_label,
        RECT {
            left: 18,
            top: LIST_TOP - 30,
            right: w - 18,
            bottom: LIST_TOP - 8,
        },
        small_font(),
        C_FAINT,
        DT_SINGLELINE | DT_RIGHT,
    );

    // 条目
    let scroll = app.scroll.load(Ordering::Relaxed) as i32;
    let hover = app.hover.load(Ordering::Relaxed) as i32;
    let rows = app.rows.lock().unwrap();
    if rows.is_empty() {
        text_in(
            hdc,
            if log_page {
                "还没有日志"
            } else {
                "还没有条目"
            },
            RECT {
                left: 16,
                top: LIST_TOP + 40,
                right: w - 16,
                bottom: LIST_TOP + 80,
            },
            ui_font(),
            C_FAINT,
            DT_CENTER | DT_SINGLELINE,
        );
        text_in(
            hdc,
            "电脑上粘贴文本发出去，或手机上复制 / 发文件",
            RECT {
                left: 16,
                top: LIST_TOP + 80,
                right: w - 16,
                bottom: LIST_TOP + 110,
            },
            small_font(),
            C_FAINT,
            DT_CENTER | DT_SINGLELINE,
        );
    } else {
        for (i, row) in rows.iter().enumerate().skip(scroll.max(0) as usize) {
            let r = row_rect(w, i as i32, scroll);
            if r.top >= h {
                break;
            }
            let hot = hover == i as i32;
            let is_file = matches!(row.kind, RowKind::File { .. });
            // 方向配色和手机端一致：发=琥珀，收=青。静默走暗档、悬浮提亮，边框 1px。
            let edge = match (row.outgoing, hot) {
                (true, true) => C_SEND,
                (true, false) => C_SEND_DIM,
                (false, true) => C_RECV,
                (false, false) => C_RECV_DIM,
            };
            fill_round(hdc, r, if hot { C_ROW_HOVER } else { C_PANEL }, edge, 8);

            // 类型图标：文本=复制小方块，文件=文档
            let icon_x = r.left + 15;
            let icon_y = r.top + (ROW_H - 15) / 2;
            if is_file {
                draw_doc(hdc, icon_x, icon_y, if hot { C_TEAL } else { C_DIM });
            } else {
                draw_copy(hdc, icon_x, icon_y, if hot { C_TEAL } else { C_DIM });
            }

            // 文件行右侧留出「打开所在目录」图标的位置
            let folder_x = r.right - 16 - 18;
            let meta_right = if is_file { folder_x - 12 } else { r.right - 16 };

            text_in(
                hdc,
                &row.title,
                RECT {
                    left: r.left + 40,
                    top: r.top + 6,
                    right: meta_right - 168,
                    bottom: r.bottom - 6,
                },
                ui_font(),
                C_INK,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
            text_in(
                hdc,
                &row.meta,
                RECT {
                    left: meta_right - 232,
                    top: r.top + 6,
                    right: meta_right,
                    bottom: r.bottom - 6,
                },
                small_font(),
                C_FAINT,
                DT_SINGLELINE | DT_VCENTER | DT_RIGHT,
            );
            if is_file {
                draw_folder(hdc, folder_x, r.top + (ROW_H - 15) / 2, if hot { C_AMBER } else { C_DIM });
            }
        }
    }

    // 细滚动条：内容超出一屏才画
    let max_scroll = (rows.len() as i32 - visible_rows(h)).max(0);
    if max_scroll > 0 {
        let track = RECT {
            left: w - 8,
            top: LIST_TOP,
            right: w - 5,
            bottom: h - 8,
        };
        let tb = CreateSolidBrush(0x0023_2A31u32);
        FillRect(hdc, &track, tb);
        DeleteObject(tb as _);
        let track_h = track.bottom - track.top;
        let thumb_h = (track_h as f32 * visible_rows(h) as f32 / rows.len() as f32).max(28.0) as i32;
        let pos = (track_h - thumb_h) as f32 * scroll as f32 / max_scroll as f32;
        let thumb = RECT {
            left: track.left,
            top: track.top + pos as i32,
            right: track.right,
            bottom: track.top + pos as i32 + thumb_h,
        };
        let th = CreateSolidBrush(0x0050_3A28u32);
        FillRect(hdc, &thumb, th);
        DeleteObject(th as _);
    }
    drop(rows);

    // 浮动提示：复制成功之类，1.2 秒自动消失
    if let Some(text) = app.hint.lock().unwrap().clone() {
        let tw = 200;
        let pill = RECT {
            left: (w - tw) / 2,
            top: h - 52,
            right: (w + tw) / 2,
            bottom: h - 20,
        };
        fill_round(hdc, pill, 0x002B_4314u32, C_TEAL, 16);
        text_in(
            hdc,
            &text,
            pill,
            small_font(),
            C_TEAL,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }
}

/// 卡片的一半：左侧图标 + 两行文案；悬浮时整半泛一点底色。
unsafe fn paint_half(
    hdc: HDC,
    rc: RECT,
    hot: bool,
    accent: u32,
    title: &str,
    hint: &str,
    clipboard_icon: bool,
) {
    if hot {
        // 悬浮高亮：把这一半单独铺一层更亮的底色
        let hl = RECT {
            left: rc.left + 2,
            top: rc.top + 2,
            right: rc.right - 2,
            bottom: rc.bottom - 2,
        };
        fill_round(hdc, hl, 0x0023_2A34u32, 0x0023_2A34u32, 9);
    }
    let ix = rc.left + 20;
    let iy = rc.top + 20;
    if clipboard_icon {
        draw_clipboard(hdc, ix, iy, accent);
    } else {
        draw_folder_big(hdc, ix, iy, accent);
    }
    text_in(
        hdc,
        title,
        RECT {
            left: ix + 32,
            top: rc.top + 14,
            right: rc.right - 12,
            bottom: rc.top + 44,
        },
        ui_font(),
        C_INK,
        DT_SINGLELINE | DT_VCENTER,
    );
    text_in(
        hdc,
        hint,
        RECT {
            left: ix + 32,
            top: rc.top + 44,
            right: rc.right - 12,
            bottom: rc.top + 68,
        },
        small_font(),
        if hot { accent } else { C_FAINT },
        DT_SINGLELINE | DT_VCENTER,
    );
}


fn fmt_rtt(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

// ---------- GDI 自绘小工具 ----------

unsafe fn fill_round(hdc: HDC, rc: RECT, fill: u32, edge: u32, radius: i32) {
    let brush = CreateSolidBrush(fill);
    let pen = CreatePen(PS_SOLID, 1, edge);
    let ob = SelectObject(hdc, brush as _);
    let op = SelectObject(hdc, pen as _);
    RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
    SelectObject(hdc, op);
    SelectObject(hdc, ob);
    DeleteObject(pen as _);
    DeleteObject(brush as _);
}

unsafe fn text_in(hdc: HDC, s: &str, mut rc: RECT, font: WPARAM, color: u32, flags: u32) {
    SelectObject(hdc, font as _);
    SetTextColor(hdc, color);
    let w = wide(s);
    DrawTextW(hdc, w.as_ptr(), -1, &mut rc, flags);
}

/// 行首的方向箭头：向上（发出去）/ 向下（收进来）。

/// 粘贴区图标：一块剪贴板（板 + 顶部夹子 + 两条内容线）。
unsafe fn draw_clipboard(hdc: HDC, x: i32, y: i32, color: u32) {
    let brush = CreateSolidBrush(color);
    let _pb = SelectObject(hdc, brush as _);
    let null_pen = GetStockObject(NULL_PEN as i32);
    SelectObject(hdc, null_pen as _);
    // 板身
    let mut body = [
        POINT { x, y: y + 4 },
        POINT { x: x + 20, y: y + 4 },
        POINT { x: x + 20, y: y + 22 },
        POINT { x, y: y + 22 },
    ];
    Polygon(hdc, body.as_mut_ptr(), 4);
    // 内芯（背景色挖空，做出边框感）
    let inner = CreateSolidBrush(C_ZONE_TEXT_BG);
    SelectObject(hdc, inner as _);
    let mut hole = [
        POINT { x: x + 2, y: y + 6 },
        POINT { x: x + 18, y: y + 6 },
        POINT { x: x + 18, y: y + 20 },
        POINT { x: x + 2, y: y + 20 },
    ];
    Polygon(hdc, hole.as_mut_ptr(), 4);
    SelectObject(hdc, brush as _);
    // 顶部夹子
    let mut tabl = [
        POINT { x: x + 6, y: y },
        POINT { x: x + 14, y: y },
        POINT { x: x + 14, y: y + 4 },
        POINT { x: x + 6, y: y + 4 },
    ];
    Polygon(hdc, tabl.as_mut_ptr(), 4);
    // 两条内容线
    let mut mini = [
        POINT { x: x + 6, y: y + 10 },
        POINT { x: x + 14, y: y + 10 },
        POINT { x: x + 14, y: y + 12 },
        POINT { x: x + 6, y: y + 12 },
    ];
    Polygon(hdc, mini.as_mut_ptr(), 4);
    let mut mini2 = [
        POINT { x: x + 6, y: y + 15 },
        POINT { x: x + 12, y: y + 15 },
        POINT { x: x + 12, y: y + 17 },
        POINT { x: x + 6, y: y + 17 },
    ];
    Polygon(hdc, mini2.as_mut_ptr(), 4);
    DeleteObject(inner as _);
    DeleteObject(brush as _);
}

/// 文本行图标：两张叠纸（点一下就复制）。
unsafe fn draw_copy(hdc: HDC, x: i32, y: i32, color: u32) {
    let pen = CreatePen(PS_SOLID, 1, color);
    let old_pen = SelectObject(hdc, pen as _);
    SelectObject(hdc, GetStockObject(NULL_BRUSH as i32) as _);
    // 后面那张
    RoundRect(hdc, x + 4, y + 1, x + 15, y + 12, 4, 4);
    // 前面那张
    RoundRect(hdc, x + 1, y + 4, x + 12, y + 15, 4, 4);
    SelectObject(hdc, old_pen);
    DeleteObject(pen as _);
}

/// 文件行图标：一张文档（右上角折角）。
unsafe fn draw_doc(hdc: HDC, x: i32, y: i32, color: u32) {
    let pen = CreatePen(PS_SOLID, 1, color);
    let old_pen = SelectObject(hdc, pen as _);
    SelectObject(hdc, GetStockObject(NULL_BRUSH as i32) as _);
    let mut body = [
        POINT { x, y },
        POINT { x: x + 10, y },
        POINT { x: x + 15, y: y + 5 },
        POINT { x: x + 15, y: y + 15 },
        POINT { x, y: y + 15 },
    ];
    Polygon(hdc, body.as_mut_ptr(), 5);
    MoveToEx(hdc, x + 10, y, ptr::null_mut());
    LineTo(hdc, x + 10, y + 5);
    LineTo(hdc, x + 15, y + 5);
    SelectObject(hdc, old_pen);
    DeleteObject(pen as _);
}

/// 小文件夹图标（列表行右侧的「打开所在目录」按钮）。
unsafe fn draw_folder(hdc: HDC, x: i32, y: i32, color: u32) {
    let brush = CreateSolidBrush(color);
    let ob = SelectObject(hdc, brush as _);
    SelectObject(hdc, GetStockObject(NULL_PEN as i32) as _);
    let mut pts = [
        POINT { x, y: y + 2 },
        POINT { x: x + 6, y: y + 2 },
        POINT { x: x + 8, y: y + 5 },
        POINT { x: x + 16, y: y + 5 },
        POINT { x: x + 16, y: y + 14 },
        POINT { x, y: y + 14 },
    ];
    Polygon(hdc, pts.as_mut_ptr(), 6);
    SelectObject(hdc, ob);
    DeleteObject(brush as _);
}

/// 热区里的大文件夹图标（20×20）。
unsafe fn draw_folder_big(hdc: HDC, x: i32, y: i32, color: u32) {
    let brush = CreateSolidBrush(color);
    let ob = SelectObject(hdc, brush as _);
    SelectObject(hdc, GetStockObject(NULL_PEN as i32) as _);
    let mut pts = [
        POINT { x, y: y + 3 },
        POINT { x: x + 8, y: y + 3 },
        POINT { x: x + 10, y: y + 7 },
        POINT { x: x + 20, y: y + 7 },
        POINT { x: x + 20, y: y + 19 },
        POINT { x, y: y + 19 },
    ];
    Polygon(hdc, pts.as_mut_ptr(), 6);
    SelectObject(hdc, ob);
    DeleteObject(brush as _);
}

/// 列表数据：条目页 = 文本 + 文件；日志页 = 只有文本。返回是否有变化。
unsafe fn refresh_rows(app: &App, hwnd: HWND) -> bool {
    let log_page = app.page() == 1;
    let peer = app.peer_label();
    let mut rows: Vec<Row> = Vec::new();

    for it in app.items.lock().unwrap().iter() {
        let head: String = it
            .text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        rows.push(Row {
            kind: RowKind::Text {
                text: it.text.clone(),
            },
            mtime: it.mtime,
            // store 里的文本都是电脑发出去的（手机发来的只写剪贴板、不入队）
            outgoing: true,
            title: head.trim().replace('\t', " "),
            meta: format!("发给 {peer} · {}", fmt_time(it.mtime)),
        });
    }

    if !log_page {
        // 发出去的文件会被 watch_outbox 挪到 outbox/sent/，只扫 outbox 的话
        // 列表里一发完就消失，看起来就是「和手机端不同步」。三个目录都要扫。
        let sent_dir = app.outbox.join("sent");
        for (dir, outgoing) in [
            (&app.outbox, true),
            (&sent_dir, true),
            (&app.inbox, false),
        ] {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in entries.flatten() {
                let path = e.path();
                let Ok(meta) = e.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                // 发送中的临时落盘文件不进列表
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if name.ends_with(".part") {
                    continue;
                }
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                rows.push(Row {
                    kind: RowKind::File { path: path.clone() },
                    mtime,
                    outgoing,
                    title: name,
                    meta: format!(
                        "{} · {}",
                        human_size(meta.len()),
                        if outgoing {
                            format!("发给 {peer}")
                        } else {
                            format!("来自 {peer}")
                        }
                    ),
                });
            }
        }
    }

    rows.sort_by_key(|r| std::cmp::Reverse(r.mtime));
    rows.truncate(200);
    let mut cur = app.rows.lock().unwrap();
    // 条目变化才重排，免得每 2 秒把滚动位置顶掉
    let same = cur.len() == rows.len()
        && cur
            .iter()
            .zip(rows.iter())
            .all(|(a, b)| {
                a.title == b.title
                    && a.mtime == b.mtime
                    && a.meta == b.meta
                    && a.outgoing == b.outgoing
            });
    if same {
        return false;
    }
    *cur = rows;
    drop(cur);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let max = max_scroll_for(app, rc.right, rc.bottom) as isize;
    let s = app.scroll.load(Ordering::Relaxed).min(max);
    app.scroll.store(s, Ordering::Relaxed);
    true
}

/// 日志窗口：文本条目一行一条（进列表前先清空，日志量不大，重排可接受）。

fn human_size(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.1} KB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MB", f / K / K)
    } else {
        format!("{:.2} GB", f / K / K / K)
    }
}


/// 点一行：文本 → 复制到剪贴板；文件 → 点右侧文件夹图标打开所在目录，点其它地方打开文件。
unsafe fn activate_row(app: &App, hwnd: HWND, pt: POINT) {
    let idx = row_at(app, {
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        rc.right
    }, pt);
    if idx < 0 {
        return;
    }
    let action = {
        let rows = app.rows.lock().unwrap();
        match rows.get(idx as usize).map(|r| &r.kind) {
            Some(RowKind::Text { text, .. }) => Some(Action::Copy(text.clone())),
            Some(RowKind::File { path, .. }) => {
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                let r = row_rect(rc.right, idx as i32, app.scroll.load(Ordering::Relaxed) as i32);
                if pt.x >= r.right - 44 {
                    Some(Action::OpenDir(path.clone()))
                } else {
                    Some(Action::OpenFile(path.clone()))
                }
            }
            None => None,
        }
    };
    match action {
        Some(Action::Copy(t)) => {
            if write_clipboard(&t) {
                app.set_hint("已复制到剪贴板");
            } else {
                app.set_hint("复制失败，剪贴板被占用");
            }
        }
        Some(Action::OpenDir(p)) => {
            let explorer = wide("explorer.exe");
            let args = format!("/select,\"{}\"", p.display());
            ShellExecuteW(
                hwnd,
                ptr::null(),
                explorer.as_ptr(),
                wide(&args).as_ptr(),
                ptr::null(),
                SW_SHOWNORMAL,
            );
        }
        Some(Action::OpenFile(p)) => {
            ShellExecuteW(
                hwnd,
                wide("open").as_ptr(),
                wide(&p.display().to_string()).as_ptr(),
                ptr::null(),
                ptr::null(),
                SW_SHOWNORMAL,
            );
        }
        None => {}
    }
}

enum Action {
    Copy(String),
    OpenDir(PathBuf),
    OpenFile(PathBuf),
}

/// 把剪贴板里的文本发出去（不做后台监听，只有用户明确粘贴才发）。
unsafe fn send_clipboard(app: &App, hwnd: HWND) {
    let Some(text) = read_clipboard_text() else {
        app.set_hint("剪贴板里没有文本");
        return;
    };
    if text.trim().is_empty() {
        app.set_hint("剪贴板里没有文本");
        return;
    }
    let chars = text.chars().count();
    if app.send_text(text) {
        app.set_hint(&format!("已发送 {chars} 字到手机"));
    } else {
        app.set_hint("发送通道未就绪，稍后再试");
    }
    let _ = hwnd;
}

pub fn read_clipboard_text() -> Option<String> {
    unsafe {
        if OpenClipboard(ptr::null_mut()) == 0 {
            return None;
        }
        let h = GetClipboardData(CF_UNICODETEXT as u32);
        if h.is_null() {
            CloseClipboard();
            return None;
        }
        let p = GlobalLock(h) as *const u16;
        if p.is_null() {
            CloseClipboard();
            return None;
        }
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        GlobalUnlock(h);
        CloseClipboard();
        Some(text)
    }
}

/// 选文件 → 塞进 outbox 立即发。
unsafe fn pick_file(app: &App) {
    let mut buf = [0u16; 4096];
    let mut ofn: OPENFILENAMEW = std::mem::zeroed();
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    let filter = "所有文件\0*.*\0\0";
    let filter_buf = wide_embed(filter);
    ofn.lpstrFilter = filter_buf.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if GetOpenFileNameW(&mut ofn) == 0 {
        return;
    }
    let len = buf.iter().position(|c| *c == 0).unwrap_or(0);
    let path = String::from_utf16_lossy(&buf[..len]);
    app.queue_file(Path::new(&path)).ok();
}

/// 带 \0 分隔的字符串常量（lpstrFilter 要求 NUL 分隔，末尾双 NUL）。
fn wide_embed(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

unsafe fn show_tray_menu(hwnd: HWND, _app: &App) {
    let menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING, CMD_OPEN, wide("打开窗口 (Ctrl+Alt+H)").as_ptr());
    AppendMenuW(
        menu,
        MF_STRING
            | if autostart_enabled() {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            },
        CMD_AUTOSTART,
        wide("开机自启动").as_ptr(),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
    AppendMenuW(menu, MF_STRING, CMD_QUIT, wide("退出").as_ptr());

    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        ptr::null(),
    );
    DestroyMenu(menu);
    if cmd != 0 {
        PostMessageW(hwnd, WM_COMMAND, cmd as usize, 0);
    }
}

unsafe fn show_history(app: &App) {
    let hwnd = app.hist_hwnd.load(Ordering::Relaxed) as HWND;
    if hwnd.is_null() {
        return;
    }
    ShowWindow(hwnd, SW_SHOW);
    ShowWindow(hwnd, SW_RESTORE);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
}

/// 写系统剪贴板（双击日志用）。
pub fn write_clipboard(text: &str) -> bool {
    unsafe {
        if OpenClipboard(ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let data: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = data.len() * 2;
        let h = GlobalAlloc(GHND, bytes);
        if h.is_null() {
            CloseClipboard();
            return false;
        }
        let p = GlobalLock(h) as *mut u16;
        if p.is_null() {
            CloseClipboard();
            return false;
        }
        ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        GlobalUnlock(h);
        let ok = SetClipboardData(CF_UNICODETEXT as u32, h) != ptr::null_mut();
        CloseClipboard();
        ok
    }
}

/// 自绘图标：和手机端 launcher 同款 —— 蓝色圆角方块 + 白色阶梯块。
/// 手机端 vector 用 108 视口，这里按比例缩放，两边看起来是同一个图标。
unsafe fn make_icon(size: i32) -> HICON {
    const BLUE: u32 = 0x00FF_6B2F; // #2F6BFF
    let screen = GetDC(ptr::null_mut());
    let hdc = CreateCompatibleDC(screen);
    let color = CreateCompatibleBitmap(screen, size, size);
    let old = SelectObject(hdc, color as _);

    let mut rc = RECT {
        left: 0,
        top: 0,
        right: size,
        bottom: size,
    };
    FillRect(hdc, &rc, bg_brush());

    // 蓝色圆角底（矢量里是直角方块，桌面图标按 Windows 习惯收个圆角）
    let bg = CreateSolidBrush(BLUE);
    let pen = CreatePen(PS_SOLID, 1, BLUE);
    let old_brush = SelectObject(hdc, bg as _);
    let old_pen = SelectObject(hdc, pen as _);
    let r = (size * 22 / 108).max(2);
    RoundRect(hdc, 0, 0, size, size, r, r);

    // 白色阶梯块：108 视口下的 (34,38)-(60,38)-(60,48)-(74,48)-(74,70)-(48,70)-(48,60)-(34,60)
    let s = |v: i32| (v * size / 108) as i32;
    let mut shape = [
        POINT { x: s(34), y: s(38) },
        POINT { x: s(60), y: s(38) },
        POINT { x: s(60), y: s(48) },
        POINT { x: s(74), y: s(48) },
        POINT { x: s(74), y: s(70) },
        POINT { x: s(48), y: s(70) },
        POINT { x: s(48), y: s(60) },
        POINT { x: s(34), y: s(60) },
    ];
    let white = CreateSolidBrush(0x00FF_FFFF);
    SelectObject(hdc, white as _);
    SelectObject(hdc, GetStockObject(NULL_PEN as i32) as _);
    Polygon(hdc, shape.as_mut_ptr(), 8);

    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old);
    DeleteObject(white as _);
    DeleteObject(pen as _);
    DeleteObject(bg as _);

    // 单色掩码全 0 = 整块可见
    let mask_bits = vec![0u8; ((size as usize + 15) / 16) * 2 * size as usize];
    let mask = CreateBitmap(size, size, 1, 1, mask_bits.as_ptr() as *const core::ffi::c_void);
    let mut ii: ICONINFO = std::mem::zeroed();
    ii.fIcon = 1;
    ii.hbmColor = color;
    ii.hbmMask = mask;
    let icon = CreateIconIndirect(&ii);
    DeleteObject(color as _);
    DeleteObject(mask as _);
    DeleteDC(hdc);
    ReleaseDC(ptr::null_mut(), screen);
    icon
}

// ---------- 开机自启（HKCU\...\Run） ----------

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "ClipBridge";

unsafe fn open_run_key(write: bool) -> Option<windows_sys::Win32::System::Registry::HKEY> {
    use windows_sys::Win32::System::Registry::{
        RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE,
    };
    let sub = wide(RUN_KEY);
    let mut key: HKEY = ptr::null_mut();
    let access = if write { KEY_SET_VALUE } else { KEY_QUERY_VALUE };
    if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut key) != 0 {
        return None;
    }
    Some(key)
}

pub fn autostart_enabled() -> bool {
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegQueryValueExW, REG_SZ};
    unsafe {
        let Some(key) = open_run_key(false) else {
            return false;
        };
        let name = wide(RUN_VALUE);
        let mut ty = 0u32;
        let mut size = 0u32;
        let rc = RegQueryValueExW(
            key,
            name.as_ptr(),
            ptr::null(),
            &mut ty,
            ptr::null_mut(),
            &mut size,
        );
        RegCloseKey(key);
        rc == 0 && ty == REG_SZ && size > 0
    }
}

pub fn set_autostart(enable: bool) {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegSetValueExW, REG_SZ,
    };
    unsafe {
        let Some(key) = open_run_key(true) else {
            return;
        };
        let name = wide(RUN_VALUE);
        if enable {
            let exe = std::env::current_exe().unwrap_or_default();
            let dir = std::env::var("CLIPBRIDGE_DIR").unwrap_or_else(|_| r"D:\ClipBridge".into());
            let cmd = format!("\"{}\" serve --dir \"{}\"", exe.display(), dir);
            let data = wide(&cmd);
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            );
        } else {
            RegDeleteValueW(key, name.as_ptr());
        }
        RegCloseKey(key);
    }
}

// ---------- 小工具 ----------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 秒级时间戳 → "MM-DD HH:MM"（按东八区，避免引时区库）。
fn fmt_time(secs: u64) -> String {
    let local = secs + 8 * 3600;
    let days = (local / 86400) as i64;
    let sod = local % 86400;
    let (m, d) = civil_md(days);
    format!("{:02}-{:02} {:02}:{:02}", m, d, sod / 3600, (sod % 3600) / 60)
}

fn civil_md(z: i64) -> (u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (m, d)
}