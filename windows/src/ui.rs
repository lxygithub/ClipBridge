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
static LAST_V: AtomicBool = AtomicBool::new(false);

const ID_LOG: i32 = 101;
const ID_FILES: i32 = 102;

const CMD_OPEN: usize = 2001;
const CMD_AUTOSTART: usize = 2003;
const CMD_QUIT: usize = 2004;

const CLASS_NAME: &str = "ClipBridgeWnd";
const TRAY_UID: u32 = 1;
const HOTKEY_ID: i32 = 1;

// 和手机端「一条通道」同一套色板；COLORREF 是 0x00BBGGRR，别写反
const C_BG: u32 = 0x0015_0E07; // #070E15
const C_PANEL: u32 = 0x001C_140B; // #0B141C
const C_INK: u32 = 0x00F2_EBE4; // #E4EBF2
const C_DIM: u32 = 0x00B3_A496; // #96A4B3
const C_FAINT: u32 = 0x0066_5B52; // #525B66
const C_TEAL: u32 = 0x00C0_D37B; // #7BD3C0
const C_AMBER: u32 = 0x005B_B9F5; // #F5B95B
const C_BORDER: u32 = 0x0032_2318; // #182332

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

struct FileRow {
    name: String,
    size: u64,
    outgoing: bool,
    path: PathBuf,
}

pub struct App {
    pub store: Arc<Store>,
    items: Mutex<Vec<HistoryItem>>,
    shown: Mutex<Vec<u64>>,
    file_rows: Mutex<Vec<FileRow>>,
    shown_files: Mutex<Vec<PathBuf>>,
    link: Mutex<LinkInfo>,
    inbox: PathBuf,
    outbox: PathBuf,
    sender: Mutex<Option<UnboundedSender<String>>>,
    pub paused: AtomicBool,
    msg_hwnd: AtomicIsize,
    hist_hwnd: AtomicIsize,
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
            shown: Mutex::new(Vec::new()),
            file_rows: Mutex::new(Vec::new()),
            shown_files: Mutex::new(Vec::new()),
            link: Mutex::new(LinkInfo::default()),
            inbox: dir.join("inbox"),
            outbox: dir.join("outbox"),
            sender: Mutex::new(None),
            paused: AtomicBool::new(false),
            msg_hwnd: AtomicIsize::new(0),
            hist_hwnd: AtomicIsize::new(0),
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

        create_controls(hist);
        darken(hist);
        layout(hist);
        DragAcceptFiles(hist, 1);
        SetTimer(hist, TIMER_TICK, 2000, None);
        refresh_lists(&app, hist);

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
            if is_hist && wp == TIMER_TICK {
                refresh_files(&*app, hwnd);
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                let head = zone_rects(rc.right);
                InvalidateRect(hwnd, &head.0, 0);

                // 焦点常落在列表子控件上，Ctrl+V 不会冒泡到主窗口，所以这里轮询一次按键：
                // 只有本窗口在前台时才处理，避免全局误触。
                if !app.is_null() && window_focused(hwnd) {
                    let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                    let vdown = GetKeyState(VK_V as i32) < 0;
                    let was = LAST_V.swap(vdown, Ordering::Relaxed);
                    if ctrl && vdown && !was {
                        send_clipboard(&*app, hwnd);
                    }
                }
            }
            0
        }
        WM_APP_REFRESH => {
            if is_hist {
                refresh_lists(&*app, hwnd);
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                let head = zone_rects(rc.right);
                InvalidateRect(hwnd, &head.0, 0);
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
                refresh_files(&*app, hwnd);
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
                let (_, paste_r, pick_r) = zone_rects(rc.right);
                let (x, y) = (((lp & 0xFFFF) as i16) as i32, (((lp >> 16) & 0xFFFF) as i16) as i32);
                let pt = POINT { x, y };
                if hit(&paste_r, pt) {
                    send_clipboard(&*app, hwnd);
                } else if hit(&pick_r, pt) {
                    pick_file(&*app);
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
                    // 没有底部按钮了，双击一条日志 = 复制到剪贴板
                    copy_log_item(&*app, hwnd);
                }
                _ if id == ID_FILES as usize && code == LBN_DBLCLK => open_selected_file(&*app, hwnd),
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
            // 主窗口只隐藏；消息窗口才是退出入口
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

unsafe fn window_focused(hwnd: HWND) -> bool {
    let fg = GetForegroundWindow();
    !fg.is_null() && (fg == hwnd || GetAncestor(fg, GA_ROOT) == hwnd)
}

fn hit(rc: &RECT, pt: POINT) -> bool {
    pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom
}

/// 顶部两块热区（粘贴 / 选择文件）与整块粘贴区。
fn zone_rects(width: i32) -> (RECT, RECT, RECT) {
    let pad = 16;
    let top = 62;
    let bottom = top + 86;
    let mid = pad + (width - pad * 2) / 2;
    let zone = RECT {
        left: pad,
        top,
        right: width - pad,
        bottom,
    };
    let left = RECT {
        left: pad,
        top,
        right: mid,
        bottom,
    };
    let right = RECT {
        left: mid,
        top,
        right: width - pad,
        bottom,
    };
    (zone, left, right)
}

unsafe fn create_controls(hwnd: HWND) {
    let font = ui_font();
    let make = |class: &str, style: u32, id: i32| -> HWND {
        let c = wide(class);
        let t = wide("");
        let h = CreateWindowExW(
            0,
            c.as_ptr(),
            t.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            100,
            24,
            hwnd,
            id as usize as *mut core::ffi::c_void,
            ptr::null_mut(),
            ptr::null_mut(),
        );
        SendMessageW(h, WM_SETFONT, font, 1);
        h
    };

    make(
        "LISTBOX",
        LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32 | WS_VSCROLL,
        ID_LOG,
    );
    make(
        "LISTBOX",
        LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32 | WS_VSCROLL,
        ID_FILES,
    );
}

/// 暗色化：标题栏 + 列表滚动条。
unsafe fn darken(hwnd: HWND) {
    let on: u32 = 1;
    let p = &on as *const u32 as *const core::ffi::c_void;
    if DwmSetWindowAttribute(hwnd, 20, p, 4) != 0 {
        DwmSetWindowAttribute(hwnd, 19, p, 4);
    }

    let dark = wide("DarkMode_Explorer");
    for id in [ID_LOG, ID_FILES] {
        let ctl = GetDlgItem(hwnd, id);
        if !ctl.is_null() {
            SetWindowTheme(ctl, dark.as_ptr(), ptr::null());
        }
    }
}

unsafe fn layout(hwnd: HWND) {
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let w = rc.right;
    let h = rc.bottom;
    let pad = 16;
    let top = 62 + 86 + 34;
    let left_w = ((w - pad * 3) as f64 * 0.56) as i32;

    MoveWindow(GetDlgItem(hwnd, ID_LOG), pad, top, left_w, h - top - pad, 1);
    MoveWindow(
        GetDlgItem(hwnd, ID_FILES),
        pad * 2 + left_w,
        top,
        w - pad * 3 - left_w,
        h - top - pad,
        1,
    );
}

unsafe fn paint(hwnd: HWND, app: &App) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);

    FillRect(hdc, &rc, bg_brush());
    SetBkMode(hdc, TRANSPARENT as i32);

    // 标题
    SelectObject(hdc, title_font() as _);
    SetTextColor(hdc, C_INK);
    let mut title = RECT {
        left: 16,
        top: 16,
        right: rc.right - 200,
        bottom: 50,
    };
    let t = wide("ClipBridge");
    DrawTextW(hdc, t.as_ptr(), -1, &mut title, DT_SINGLELINE | DT_VCENTER);

    // 状态徽标：直连/中继 + 延迟；颜色按 RTT 分档
    let link = *app.link.lock().unwrap();
    let (label, fg, bg) = if !link.connected {
        ("未连接".to_string(), 0x00A4_9587u32, 0x0039_2C1Eu32)
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
        (format!("中继 · {}", fmt_rtt(link.rtt_ms)), 0x00B3_A496u32, 0x0023_1B14u32)
    };
    let chip_w = 132;
    let chip = RECT {
        left: rc.right - 16 - chip_w,
        top: 18,
        right: rc.right - 16,
        bottom: 46,
    };
    let brush = CreateSolidBrush(bg);
    let old_brush = SelectObject(hdc, brush as _);
    let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN as i32) as _);
    RoundRect(hdc, chip.left, chip.top, chip.right, chip.bottom, 14, 14);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, ui_font() as _);
    SetTextColor(hdc, fg);
    let mut cr = chip;
    let ct = wide(&label);
    DrawTextW(hdc, ct.as_ptr(), -1, &mut cr, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    DeleteObject(brush as _);

    // 粘贴 / 选择文件区
    let (zone, left, right) = zone_rects(rc.right);
    let zone_brush = CreateSolidBrush(C_PANEL);
    FillRect(hdc, &zone, zone_brush);
    DeleteObject(zone_brush as _);
    let pen = CreatePen(PS_SOLID, 1, C_BORDER);
    let old = SelectObject(hdc, pen as _);
    let null_brush = GetStockObject(NULL_BRUSH as i32);
    SelectObject(hdc, null_brush as _);
    RoundRect(hdc, zone.left, zone.top, zone.right, zone.bottom, 10, 10);
    SelectObject(hdc, old);
    // 两块热区之间的分隔线
    let pen2 = CreatePen(PS_SOLID, 1, C_BORDER);
    let old2 = SelectObject(hdc, pen2 as _);
    MoveToEx(hdc, left.right, zone.top + 12, ptr::null_mut());
    LineTo(hdc, left.right, zone.bottom - 12);
    SelectObject(hdc, old2);
    DeleteObject(pen2 as _);

    SelectObject(hdc, ui_font() as _);
    SetTextColor(hdc, C_DIM);
    let mut lr = left;
    lr.bottom -= 20;
    let a = wide("粘贴文本");
    DrawTextW(hdc, a.as_ptr(), -1, &mut lr, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    SelectObject(hdc, small_font() as _);
    SetTextColor(hdc, C_FAINT);
    let mut lr2 = left;
    lr2.top += 34;
    let a2 = wide("Ctrl+V 或点这里");
    DrawTextW(hdc, a2.as_ptr(), -1, &mut lr2, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    SelectObject(hdc, ui_font() as _);
    SetTextColor(hdc, C_DIM);
    let mut rr = right;
    rr.bottom -= 20;
    let b = wide("选择文件");
    DrawTextW(hdc, b.as_ptr(), -1, &mut rr, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    SelectObject(hdc, small_font() as _);
    SetTextColor(hdc, C_FAINT);
    let mut rr2 = right;
    rr2.top += 34;
    let b2 = wide("或把文件拖进窗口");
    DrawTextW(hdc, b2.as_ptr(), -1, &mut rr2, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    // 分栏标题
    let pad = 16;
    let head_top = 62 + 86 + 8;
    SelectObject(hdc, small_font() as _);
    SetTextColor(hdc, C_FAINT);
    let mut lh = RECT {
        left: pad,
        top: head_top,
        right: rc.right,
        bottom: head_top + 20,
    };
    let hl = wide("交换日志");
    DrawTextW(hdc, hl.as_ptr(), -1, &mut lh, DT_SINGLELINE);
    let left_w = ((rc.right - pad * 3) as f64 * 0.56) as i32;
    let mut rh = RECT {
        left: pad * 2 + left_w,
        top: head_top,
        right: rc.right,
        bottom: head_top + 20,
    };
    let hr = wide("文件");
    DrawTextW(hdc, hr.as_ptr(), -1, &mut rh, DT_SINGLELINE);

    EndPaint(hwnd, &ps);
}

fn fmt_rtt(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

unsafe fn refresh_lists(app: &App, hwnd: HWND) {
    refresh_log(app, hwnd);
    refresh_files(app, hwnd);
}

unsafe fn refresh_log(app: &App, hwnd: HWND) {
    let lb = GetDlgItem(hwnd, ID_LOG);
    SendMessageW(lb, LB_RESETCONTENT, 0, 0);

    let mut shown = app.shown.lock().unwrap();
    shown.clear();
    for it in app.items.lock().unwrap().iter() {
        let head: String = it
            .text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        let line = format!("{}  {}", fmt_time(it.mtime), head.replace('\n', " "));
        let w = wide(&line);
        SendMessageW(lb, LB_ADDSTRING, 0, w.as_ptr() as LPARAM);
        shown.push(it.seq);
    }
}

unsafe fn refresh_files(app: &App, hwnd: HWND) {
    let lb = GetDlgItem(hwnd, ID_FILES);
    let mut rows = gather_files(app);

    let shown_now = app.shown_files.lock().unwrap().clone();
    let paths_now: Vec<PathBuf> = rows.iter().map(|r| r.path.clone()).collect();
    if shown_now == paths_now {
        return;
    }

    SendMessageW(lb, LB_RESETCONTENT, 0, 0);
    for r in rows.iter() {
        let arrow = if r.outgoing { "↑" } else { "↓" };
        let line = format!("{} {}  {}", arrow, r.name, human_size(r.size));
        let w = wide(&line);
        SendMessageW(lb, LB_ADDSTRING, 0, w.as_ptr() as LPARAM);
    }
    let mut shown = app.shown_files.lock().unwrap();
    shown.clear();
    shown.extend(paths_now);
    rows.clear();
}

/// inbox（对方发来）与 outbox（待发/在发）合并成一个文件列表。
fn gather_files(app: &App) -> Vec<FileRow> {
    let mut out: Vec<FileRow> = Vec::new();
    for (dir, outgoing) in [(&app.inbox, false), (&app.outbox, true)] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_file() {
                continue;
            }
            if let Ok(meta) = e.metadata() {
                if meta.is_file() {
                    out.push(FileRow {
                        name: path
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        size: meta.len(),
                        outgoing,
                        path,
                    });
                }
            }
        }
    }
    out.sort_by_key(|r| {
        std::cmp::Reverse(
            std::fs::metadata(&r.path)
                .and_then(|m| m.modified())
                .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
                .unwrap_or(0),
        )
    });
    out.truncate(200);
    out
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
    let seq = app.shown.lock().unwrap().get(idx as usize).copied();
    let Some(seq) = seq else { return };
    let text = app
        .items
        .lock()
        .unwrap()
        .iter()
        .find(|i| i.seq == seq)
        .map(|i| i.text.clone());
    if let Some(t) = text {
        write_clipboard(&t);
    }
}

unsafe fn open_selected_file(app: &App, hwnd: HWND) {
    let idx = SendMessageW(GetDlgItem(hwnd, ID_FILES), LB_GETCURSEL, 0, 0);
    if idx < 0 {
        return;
    }
    let path = app.shown_files.lock().unwrap().get(idx as usize).cloned();
    let Some(path) = path else { return };
    let open = wide("open");
    ShellExecuteW(
        hwnd,
        open.as_ptr(),
        wide(&path.display().to_string()).as_ptr(),
        ptr::null(),
        ptr::null(),
        SW_SHOWNORMAL,
    );
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
