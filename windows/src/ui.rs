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
    GetStockObject, InvalidateRect, ReleaseDC, RoundRect, SelectObject, SetBkColor, NULL_BRUSH, NULL_PEN, SetBkMode, SetTextColor, DT_CENTER,
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

const ID_LOG: i32 = 101;

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
const C_ZONE_TEXT_EDGE: u32 = 0x00353A1D; // #1D3A35
const C_ZONE_FILE_BG: u32 = 0x000E181E; // #1E180E 偏暖
const C_ZONE_FILE_EDGE: u32 = 0x001A2F3A; // #3A2F1A
const C_ROW_HOVER: u32 = 0x00281D12; // #121D28
const C_ROW_EDGE: u32 = 0x001C2A36; // #362A1C
const C_TEXT_FG: u32 = 0x00D8_C4B0; // #B0C4D8 正文偏冷灰

/// 窗口与控件共用的画刷/字体（进程生命周期内复用）。
static BG_BRUSH: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
static PANEL_BRUSH: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
static UI_FONT: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
static TITLE_FONT: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
static SMALL_FONT: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

fn bg_brush() -> HBRUSH {
    let raw = *BG_BRUSH.get_or_init(|| unsafe { CreateSolidBrush(C_BG) as isize });
    raw as *mut core::ffi::c_void
}

fn panel_brush() -> HBRUSH {
    let raw = *PANEL_BRUSH.get_or_init(|| unsafe { CreateSolidBrush(C_PANEL) as isize });
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

#[link(name = "uxtheme")]
extern "system" {
    fn SetWindowTheme(hwnd: HWND, app: *const u16, id: *const u16) -> i32;
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
    File { path: PathBuf, outgoing: bool },
}

pub struct Row {
    pub kind: RowKind,
    pub mtime: u64,
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
    inbox: PathBuf,
    outbox: PathBuf,
    sender: Mutex<Option<UnboundedSender<String>>>,
    pub paused: AtomicBool,
    msg_hwnd: AtomicIsize,
    hist_hwnd: AtomicIsize,
    log_hwnd: AtomicIsize,
    scroll: AtomicIsize,
    hover: AtomicIsize,
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
            inbox: dir.join("inbox"),
            outbox: dir.join("outbox"),
            sender: Mutex::new(None),
            paused: AtomicBool::new(false),
            msg_hwnd: AtomicIsize::new(0),
            hist_hwnd: AtomicIsize::new(0),
            log_hwnd: AtomicIsize::new(0),
            scroll: AtomicIsize::new(0),
            hover: AtomicIsize::new(-1),
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

        // 日志窗口：平时不显示，点标题栏「日志」才弹出来
        let log = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("ClipBridge 日志").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            960,
            160,
            620,
            520,
            ptr::null_mut(),
            ptr::null_mut(),
            hinst,
            app_ptr,
        );
        if !log.is_null() {
            app.log_hwnd.store(log as isize, Ordering::Relaxed);
            SendMessageW(log, WM_SETICON, 1, make_icon(32) as LPARAM);
            create_log_controls(log);
            darken(log);
            layout_log(log);
        }

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
    let is_log = !app.is_null() && (*app).log_hwnd.load(Ordering::Relaxed) == hwnd as isize;

    match msg {
        WM_CREATE => {
            let cs = lp as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            0
        }
        WM_SIZE => {
            if is_hist {
                layout(hwnd);
            } else if is_log {
                layout_log(hwnd);
            }
            0
        }
        WM_TIMER => {
            if is_hist && wp == TIMER_TICK {
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
                refresh_log(&*app);
                InvalidateRect(hwnd, ptr::null(), 0);
            }
            0
        }
        WM_PAINT => {
            if is_hist {
                paint(hwnd, &*app);
            } else if is_log {
                paint_log(hwnd, &*app);
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
                let prev = (*app).hover.swap(idx, Ordering::Relaxed);
                if prev != idx {
                    invalidate_rows(hwnd, rc.right);
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
                let (text_r, file_r) = zone_rects(rc.right);
                let (x, y) = (((lp & 0xFFFF) as i16) as i32, (((lp >> 16) & 0xFFFF) as i16) as i32);
                let pt = POINT { x, y };
                if hit(&text_r, pt) {
                    send_clipboard(&*app, hwnd);
                } else if hit(&file_r, pt) {
                    pick_file(&*app);
                } else if hit(&log_chip_rect(rc.right), pt) {
                    show_log(&*app);
                } else {
                    activate_row(&*app, hwnd, pt);
                }
            }
            0
        }
        WM_KEYDOWN => {
            if wp == VK_V as usize && GetKeyState(VK_CONTROL as i32) < 0 {
                if !app.is_null() {
                    send_clipboard(&*app, hwnd);
                }
            }
            0
        }
        WM_COMMAND => {
            if app.is_null() {
                return 0;
            }
            let id = (wp & 0xFFFF) as usize;
            let code = ((wp >> 16) & 0xFFFF) as u32;
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
                _ if id == ID_LOG as usize && code == LBN_DBLCLK => {
                    copy_log_item(&*app, hwnd);
                }
                _ => {}
            }
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => {
            // 内置控件默认浅色，这里统一压成暗底浅字
            let hdc = wp as HDC;
            SetBkColor(hdc, C_PANEL);
            SetTextColor(hdc, C_INK);
            panel_brush() as LRESULT
        }
        WM_CLOSE => {
            // 主窗口、日志窗口都只隐藏；消息窗口才是退出入口
            if is_hist || is_log {
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

unsafe fn window_focused(hwnd: HWND) -> bool {
    let fg = GetForegroundWindow();
    !fg.is_null() && (fg == hwnd || GetAncestor(fg, GA_ROOT) == hwnd)
}

fn hit(rc: &RECT, pt: POINT) -> bool {
    pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom
}

/// 顶部两块热区：左边粘贴文本、右边选择文件。各是一张独立卡片，留 10px 缝。
fn zone_rects(width: i32) -> (RECT, RECT) {
    let pad = 16;
    let top = 62;
    let h = 86;
    let gap = 10;
    let card_w = (width - pad * 2 - gap) / 2;
    let left = RECT {
        left: pad,
        top,
        right: pad + card_w,
        bottom: top + h,
    };
    let right = RECT {
        left: pad + card_w + gap,
        top,
        right: width - pad,
        bottom: top + h,
    };
    (left, right)
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

/// 主窗口不再有子控件（列表是自绘的），只有日志窗口里有一个 LISTBOX。
unsafe fn create_log_controls(hwnd: HWND) {
    let font = ui_font();
    let c = wide("LISTBOX");
    let t = wide("");
    let h = CreateWindowExW(
        0,
        c.as_ptr(),
        t.as_ptr(),
        WS_CHILD | WS_VISIBLE | LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32 | WS_VSCROLL,
        0,
        0,
        100,
        24,
        hwnd,
        ID_LOG as usize as *mut core::ffi::c_void,
        ptr::null_mut(),
        ptr::null_mut(),
    );
    SendMessageW(h, WM_SETFONT, font, 1);
}

/// 暗色化：标题栏 + 日志列表滚动条。
unsafe fn darken(hwnd: HWND) {
    let on: u32 = 1;
    let p = &on as *const u32 as *const core::ffi::c_void;
    if DwmSetWindowAttribute(hwnd, 20, p, 4) != 0 {
        DwmSetWindowAttribute(hwnd, 19, p, 4);
    }

    let dark = wide("DarkMode_Explorer");
    let ctl = GetDlgItem(hwnd, ID_LOG);
    if !ctl.is_null() {
        SetWindowTheme(ctl, dark.as_ptr(), ptr::null());
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

unsafe fn layout_log(hwnd: HWND) {
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    MoveWindow(
        GetDlgItem(hwnd, ID_LOG),
        16,
        56,
        rc.right - 32,
        (rc.bottom - 56 - 16).max(40),
        1,
    );
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

unsafe fn invalidate_rows(hwnd: HWND, width: i32) {
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let area = RECT {
        left: 0,
        top: LIST_TOP - 4,
        right: width,
        bottom: rc.bottom,
    };
    InvalidateRect(hwnd, &area, 0);
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
            top: 12,
            right: w - 260,
            bottom: 52,
        },
        title_font(),
        C_INK,
        DT_SINGLELINE | DT_VCENTER,
    );

    // 日志入口：平时收起，点了才弹日志窗口
    let lg = log_chip_rect(w);
    fill_round(hdc, lg, C_PANEL, C_BORDER, 14);
    text_in(
        hdc,
        "日志",
        lg,
        small_font(),
        C_DIM,
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

    // 两块热区：各是一张卡片，靠底色区分（左青右暖），不再共用一个大框
    let (text_z, file_z) = zone_rects(w);
    paint_zone(
        hdc,
        text_z,
        C_ZONE_TEXT_BG,
        C_ZONE_TEXT_EDGE,
        C_TEAL,
        "粘贴文本",
        "Ctrl+V · 点这里直接发送",
        true,
    );
    paint_zone(
        hdc,
        file_z,
        C_ZONE_FILE_BG,
        C_ZONE_FILE_EDGE,
        C_AMBER,
        "选择文件",
        "也可以把文件拖进窗口",
        false,
    );

    // 列表标题
    let count = app.rows.lock().unwrap().len();
    text_in(
        hdc,
        "全部条目",
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
    text_in(
        hdc,
        &format!("共 {count} 条"),
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
            "还没有条目",
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
            let accent = match row.kind {
                RowKind::Text { .. } => C_TEAL,
                RowKind::File { outgoing: true, .. } => C_AMBER,
                RowKind::File { outgoing: false, .. } => C_TEAL,
            };
            fill_round(
                hdc,
                r,
                if hot { C_ROW_HOVER } else { C_PANEL },
                if hot { accent } else { C_ROW_EDGE },
                8,
            );

            // 左侧竖条：一眼看出方向
            let bar = RECT {
                left: r.left + 1,
                top: r.top + 12,
                right: r.left + 4,
                bottom: r.bottom - 12,
            };
            let bb = CreateSolidBrush(accent);
            FillRect(hdc, &bar, bb);
            DeleteObject(bb as _);

            let icon_x = r.left + 18;
            let icon_y = r.top + (ROW_H - 14) / 2;
            draw_arrow(hdc, icon_x, icon_y, matches!(row.kind, RowKind::File { outgoing: true, .. }));

            // 文件行右侧留出「打开所在目录」图标的位置
            let is_file = matches!(row.kind, RowKind::File { .. });
            let folder_x = r.right - 16 - 18;
            let meta_right = if is_file { folder_x - 10 } else { r.right - 16 };

            text_in(
                hdc,
                &row.title,
                RECT {
                    left: r.left + 44,
                    top: r.top + 8,
                    right: meta_right - 90,
                    bottom: r.bottom - 8,
                },
                ui_font(),
                C_TEXT_FG,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
            text_in(
                hdc,
                &row.meta,
                RECT {
                    left: meta_right - 160,
                    top: r.top + 8,
                    right: meta_right,
                    bottom: r.bottom - 8,
                },
                small_font(),
                C_FAINT,
                DT_SINGLELINE | DT_VCENTER | DT_RIGHT,
            );
            if is_file {
                draw_folder(
                    hdc,
                    folder_x,
                    r.top + (ROW_H - 14) / 2,
                    if hot { C_AMBER } else { C_DIM },
                );
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
}

/// 一块热区卡片：底色 + 边框 + 左侧图标 + 两行文案。
#[allow(clippy::too_many_arguments)]
unsafe fn paint_zone(
    hdc: HDC,
    rc: RECT,
    bg: u32,
    edge: u32,
    accent: u32,
    title: &str,
    hint: &str,
    clipboard_icon: bool,
) {
    fill_round(hdc, rc, bg, edge, 10);
    let ix = rc.left + 18;
    let iy = rc.top + 22;
    if clipboard_icon {
        draw_clipboard(hdc, ix, iy, accent);
    } else {
        draw_folder_big(hdc, ix, iy, accent);
    }
    text_in(
        hdc,
        title,
        RECT {
            left: ix + 34,
            top: rc.top + 16,
            right: rc.right - 12,
            bottom: rc.top + 48,
        },
        ui_font(),
        C_INK,
        DT_SINGLELINE | DT_VCENTER,
    );
    text_in(
        hdc,
        hint,
        RECT {
            left: ix + 34,
            top: rc.top + 46,
            right: rc.right - 12,
            bottom: rc.top + 72,
        },
        small_font(),
        C_FAINT,
        DT_SINGLELINE | DT_VCENTER,
    );
}

unsafe fn paint_log(hwnd: HWND, app: &App) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    FillRect(hdc, &rc, bg_brush());
    SetBkMode(hdc, TRANSPARENT as i32);
    text_in(
        hdc,
        "交换日志",
        RECT {
            left: 16,
            top: 12,
            right: rc.right - 16,
            bottom: 48,
        },
        title_font(),
        C_INK,
        DT_SINGLELINE | DT_VCENTER,
    );
    text_in(
        hdc,
        &format!("共 {} 条文本 · 双击复制", app.items.lock().unwrap().len()),
        RECT {
            left: rc.right - 260,
            top: 16,
            right: rc.right - 16,
            bottom: 44,
        },
        small_font(),
        C_FAINT,
        DT_SINGLELINE | DT_VCENTER | DT_RIGHT,
    );
    EndPaint(hwnd, &ps);
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
unsafe fn draw_arrow(hdc: HDC, x: i32, y: i32, up: bool) {
    let color = if up { C_AMBER } else { C_TEAL };
    let brush = CreateSolidBrush(color);
    let pen = CreatePen(PS_SOLID, 1, color);
    let ob = SelectObject(hdc, brush as _);
    let op = SelectObject(hdc, pen as _);
    let mut tri = if up {
        [
            POINT { x: x - 4, y: y + 10 },
            POINT { x, y },
            POINT { x: x + 4, y: y + 10 },
        ]
    } else {
        [
            POINT { x: x - 4, y },
            POINT { x, y: y + 10 },
            POINT { x: x + 4, y },
        ]
    };
    Polygon(hdc, tri.as_mut_ptr(), 3);
    SelectObject(hdc, op);
    SelectObject(hdc, ob);
    DeleteObject(pen as _);
    DeleteObject(brush as _);
}

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

/// 主界面列表 = 文本条目 + 文件条目，按时间倒序合并。返回是否有变化。
unsafe fn refresh_rows(app: &App, hwnd: HWND) -> bool {
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
            title: head.trim().replace('\t', " "),
            meta: fmt_time(it.mtime),
        });
    }

    for (dir, outgoing) in [(&app.outbox, true), (&app.inbox, false)] {
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
                kind: RowKind::File {
                    path: path.clone(),
                    outgoing,
                },
                mtime,
                title: name,
                meta: format!("{} · {}", human_size(meta.len()), if outgoing { "已发" } else { "已收" }),
            });
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
            .all(|(a, b)| a.title == b.title && a.mtime == b.mtime);
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
unsafe fn refresh_log(app: &App) {
    let hwnd = app.log_hwnd.load(Ordering::Relaxed) as HWND;
    if hwnd.is_null() {
        return;
    }
    let lb = GetDlgItem(hwnd, ID_LOG);
    if lb.is_null() {
        return;
    }
    SendMessageW(lb, LB_RESETCONTENT, 0, 0);
    for it in app.items.lock().unwrap().iter() {
        let head: String = it
            .text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let line = format!("{}  {}", fmt_time(it.mtime), head.replace('\t', " "));
        let w = wide(&line);
        SendMessageW(lb, LB_ADDSTRING, 0, w.as_ptr() as LPARAM);
    }
    InvalidateRect(hwnd, ptr::null(), 0);
}

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

unsafe fn copy_log_item(app: &App, hwnd: HWND) {
    let idx = SendMessageW(GetDlgItem(hwnd, ID_LOG), LB_GETCURSEL, 0, 0);
    if idx < 0 {
        return;
    }
    let text = app
        .items
        .lock()
        .unwrap()
        .get(idx as usize)
        .map(|i| i.text.clone());
    if let Some(t) = text {
        write_clipboard(&t);
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
            write_clipboard(&t);
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

/// 日志窗口按需弹出（启动时就已建好，这里只负责显示与刷新）。
unsafe fn show_log(app: &App) {
    let hwnd = app.log_hwnd.load(Ordering::Relaxed) as HWND;
    if hwnd.is_null() {
        return;
    }
    refresh_log(app);
    ShowWindow(hwnd, SW_SHOW);
    ShowWindow(hwnd, SW_RESTORE);
    SetForegroundWindow(hwnd);
    InvalidateRect(hwnd, ptr::null(), 0);
}

/// 把剪贴板里的文本发出去（不做后台监听，只有用户明确粘贴才发）。
unsafe fn send_clipboard(app: &App, hwnd: HWND) {
    let Some(text) = read_clipboard_text() else {
        return;
    };
    if text.trim().is_empty() {
        MessageBoxW(
            hwnd,
            wide("剪贴板里没有文本。").as_ptr(),
            wide("ClipBridge").as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }
    app.send_text(text);
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
    ofn.lpstrFilter = wide_embed(filter).as_ptr();
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

/// 自绘图标：深底圆角方块 + 上下两条箭头，配色跟界面一致。
unsafe fn make_icon(size: i32) -> HICON {
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

    let pen = CreatePen(PS_SOLID, 2, C_TEAL);
    SelectObject(hdc, pen as _);
    let null_brush = GetStockObject(NULL_BRUSH as i32);
    SelectObject(hdc, null_brush as _);
    RoundRect(hdc, 2, 2, size - 2, size - 2, 9, 9);

    // 向上的箭头
    let cx = size / 2;
    MoveToEx(hdc, cx, size * 62 / 100, ptr::null_mut());
    LineTo(hdc, cx, size * 22 / 100);
    let mut up = [
        POINT { x: cx - size / 8, y: size * 30 / 100 },
        POINT { x: cx, y: size * 18 / 100 },
        POINT { x: cx + size / 8, y: size * 30 / 100 },
    ];
    let amber = CreateSolidBrush(C_AMBER);
    let old_brush2 = SelectObject(hdc, amber as _);
    Polygon(hdc, up.as_mut_ptr(), 3);
    SelectObject(hdc, old_brush2);

    // 向下的箭头，稍微错开，形成「双向」语义
    MoveToEx(hdc, cx + size * 18 / 100, size * 34 / 100, ptr::null_mut());
    LineTo(hdc, cx + size * 18 / 100, size * 72 / 100);
    let mut down = [
        POINT { x: cx + size * 6 / 100, y: size * 62 / 100 },
        POINT { x: cx + size * 18 / 100, y: size * 80 / 100 },
        POINT { x: cx + size * 30 / 100, y: size * 62 / 100 },
    ];
    let old_brush3 = SelectObject(hdc, amber as _);
    SelectObject(hdc, pen as _);
    Polygon(hdc, down.as_mut_ptr(), 3);
    SelectObject(hdc, old_brush3);

    SelectObject(hdc, old);
    DeleteObject(amber as _);
    DeleteObject(pen as _);

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
