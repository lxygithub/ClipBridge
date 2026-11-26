//! Windows 端常驻界面：托盘图标 + 全局热键 + 历史管理窗口。
//!
//! 刻意不引入 GUI 框架：只用 user32 的内置控件类（EDIT / LISTBOX / BUTTON / STATIC），
//! 这样 `clipbridge.exe` 仍然是单文件原生程序，托盘和热键也不用适配事件循环。
//!
//! 线程模型：Win32 消息循环必须在主线程；iroh 网络任务跑在独立 tokio 线程，
//! 两边只通过 `App`（Mutex + Atomic）和 `PostMessageW(WM_APP_REFRESH)` 交互。

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, HBRUSH, HDC, SetBkColor, SetTextColor,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, SetFocus, MOD_ALT, MOD_CONTROL,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::queue::Store;

const WM_APP_REFRESH: u32 = WM_APP + 10;
const WM_TRAY: u32 = WM_APP + 1;

const ID_SEARCH: i32 = 100;
const ID_LIST: i32 = 101;
const ID_COPY: i32 = 102;
const ID_DELETE: i32 = 103;
const ID_CLEAR: i32 = 104;
const ID_STATUS: i32 = 105;

const CMD_OPEN: usize = 2001;
const CMD_PAUSE: usize = 2002;
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

/// 窗口与控件共用的暗底画刷（进程生命周期内复用）。
static BG_BRUSH: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
static UI_FONT: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

fn bg_brush() -> HBRUSH {
    let raw = *BG_BRUSH.get_or_init(|| unsafe { CreateSolidBrush(C_BG) as isize });
    raw as *mut core::ffi::c_void
}

unsafe fn ui_font() -> WPARAM {
    *UI_FONT.get_or_init(|| {
        // 10pt 微软雅黑：中文清晰，行距也比默认字体松
        let face = wide("Microsoft YaHei UI");
        CreateFontW(
            16,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            1u32,
            0,
            0,
            0,
            0u32,
            face.as_ptr(),
        ) as isize
    }) as WPARAM
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

pub struct App {
    pub store: Arc<Store>,
    pub paused: AtomicBool,
    items: Mutex<Vec<HistoryItem>>,
    shown: Mutex<Vec<u64>>,
    filter: Mutex<String>,
    msg_hwnd: AtomicIsize,
    hist_hwnd: AtomicIsize,
}

impl App {
    pub fn new(store: Arc<Store>) -> Self {
        let items = store
            .all()
            .unwrap_or_default()
            .into_iter()
            .map(|(seq, text, mtime)| HistoryItem { seq, text, mtime })
            .collect();
        Self {
            store,
            paused: AtomicBool::new(false),
            items: Mutex::new(items),
            shown: Mutex::new(Vec::new()),
            filter: Mutex::new(String::new()),
            msg_hwnd: AtomicIsize::new(0),
            hist_hwnd: AtomicIsize::new(0),
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
}

/// 启动托盘 + 热键 + 历史窗口，阻塞在消息循环里直到用户退出。
pub fn run(app: Arc<App>) -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(ptr::null());
        let class = wide(CLASS_NAME);

        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(ptr::null_mut(), IDC_ARROW);
        wc.hbrBackground = bg_brush() as _;
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

        add_tray(msg);
        RegisterHotKey(msg, HOTKEY_ID, MOD_CONTROL | MOD_ALT, b'H' as u32);

        // 历史窗口先建好再隐藏：打开时只 ShowWindow，首次弹出不会卡
        let hist = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("ClipBridge 历史").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            240,
            120,
            640,
            720,
            ptr::null_mut(),
            ptr::null_mut(),
            hinst,
            app_ptr,
        );
        if hist.is_null() {
            anyhow::bail!("create history window failed");
        }
        app.hist_hwnd.store(hist as isize, Ordering::Relaxed);

        create_controls(hist);
        darken(hist);
        layout(hist);
        refresh_list(&app, hist);

        let mut m: MSG = std::mem::zeroed();
        while GetMessageW(&mut m, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
    Ok(())
}

unsafe fn add_tray(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = LoadIconW(ptr::null_mut(), IDI_APPLICATION);
    let tip = wide("ClipBridge：剪贴板同步");
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
        WM_APP_REFRESH => {
            if is_hist {
                refresh_list(&*app, hwnd);
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
        WM_COMMAND => {
            if app.is_null() {
                return 0;
            }
            let id = (wp & 0xFFFF) as usize;
            let code = ((wp >> 16) & 0xFFFF) as u32;
            match id {
                CMD_OPEN => show_history(&*app),
                CMD_PAUSE => {
                    let paused = !(*app).paused.load(Ordering::Relaxed);
                    (*app).paused.store(paused, Ordering::Relaxed);
                    set_status(hwnd, if paused { "已暂停监听" } else { "监听中" });
                }
                CMD_AUTOSTART => {
                    let on = !autostart_enabled();
                    set_autostart(on);
                    set_status(hwnd, if on { "已设置开机自启" } else { "已取消开机自启" });
                }
                CMD_QUIT => {
                    remove_tray((*app).msg_hwnd.load(Ordering::Relaxed) as HWND);
                    DestroyWindow((*app).msg_hwnd.load(Ordering::Relaxed) as HWND);
                }
                _ if id == ID_COPY as usize || (id == ID_LIST as usize && code == LBN_DBLCLK) => {
                    copy_selected(&*app, hwnd);
                }
                _ if id == ID_DELETE as usize => delete_selected(&*app, hwnd),
                _ if id == ID_CLEAR as usize => clear_all(&*app, hwnd),
                _ if id == ID_SEARCH as usize && code == EN_CHANGE => {
                    let text = get_text(GetDlgItem(hwnd, ID_SEARCH));
                    *(*app).filter.lock().unwrap() = text;
                    refresh_list(&*app, hwnd);
                }
                _ => {}
            }
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => {
            // 内置控件默认浅色，这里统一压成暗底浅字
            let hdc = wp as HDC;
            SetBkColor(hdc, C_BG);
            SetTextColor(hdc, C_INK);
            bg_brush() as LRESULT        }
        WM_CLOSE => {
            // 历史窗口只隐藏；消息窗口才是退出入口
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

    let edit = make("EDIT", (ES_AUTOHSCROLL as u32) | WS_BORDER, ID_SEARCH);
    // 空编辑框太隐形，加个提示文案（EM_SETCUEBANNER，Windows 通用控件 6 起支持）
    let cue = wide("按内容搜索…");
    SendMessageW(edit, 0x1501, 1, cue.as_ptr() as LPARAM);
    make(
        "LISTBOX",
        LBS_NOTIFY as u32 | WS_VSCROLL | WS_BORDER,
        ID_LIST,
    );
    make("BUTTON", BS_PUSHBUTTON as u32, ID_COPY);
    make("BUTTON", BS_PUSHBUTTON as u32, ID_DELETE);
    make("BUTTON", BS_PUSHBUTTON as u32, ID_CLEAR);
    make("STATIC", 0, ID_STATUS);

    set_text(GetDlgItem(hwnd, ID_COPY), "复制");
    set_text(GetDlgItem(hwnd, ID_DELETE), "删除");
    set_text(GetDlgItem(hwnd, ID_CLEAR), "清空");
    set_text(GetDlgItem(hwnd, ID_STATUS), "监听中");
}

/// 暗色化：标题栏 + 各控件滚动条 + 编辑框/列表背景。
unsafe fn darken(hwnd: HWND) {
    // DWMWA_USE_IMMERSIVE_DARK_MODE = 20（Win10 1809+ 用 19，两个都试一次）
    let on: u32 = 1;
    let p = &on as *const u32 as *const core::ffi::c_void;
    if DwmSetWindowAttribute(hwnd, 20, p, 4) != 0 {
        DwmSetWindowAttribute(hwnd, 19, p, 4);
    }

    let dark = wide("DarkMode_Explorer");
    for id in [ID_SEARCH, ID_LIST] {
        let ctl = GetDlgItem(hwnd, id);
        if !ctl.is_null() {
            SetWindowTheme(ctl, dark.as_ptr(), ptr::null());
        }
    }
}

unsafe fn set_text(hwnd: HWND, text: &str) {
    let w = wide(text);
    SetWindowTextW(hwnd, w.as_ptr());
}

unsafe fn layout(hwnd: HWND) {
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let w = rc.right;
    let h = rc.bottom;
    let pad = 10;
    MoveWindow(GetDlgItem(hwnd, ID_SEARCH), pad, pad, w - pad * 2, 28, 1);
    MoveWindow(
        GetDlgItem(hwnd, ID_LIST),
        pad,
        48,
        w - pad * 2,
        h - 48 - 48,
        1,
    );
    MoveWindow(GetDlgItem(hwnd, ID_COPY), pad, h - 38, 90, 28, 1);
    MoveWindow(GetDlgItem(hwnd, ID_DELETE), pad + 98, h - 38, 90, 28, 1);
    MoveWindow(GetDlgItem(hwnd, ID_CLEAR), pad + 196, h - 38, 90, 28, 1);
    MoveWindow(
        GetDlgItem(hwnd, ID_STATUS),
        pad + 300,
        h - 34,
        w - pad - 300,
        20,
        1,
    );
}

unsafe fn refresh_list(app: &App, hwnd: HWND) {
    let lb = GetDlgItem(hwnd, ID_LIST);
    SendMessageW(lb, LB_RESETCONTENT, 0, 0);

    let filter = app.filter.lock().unwrap().to_lowercase();
    let mut shown = app.shown.lock().unwrap();
    shown.clear();
    for it in app.items.lock().unwrap().iter() {
        if !filter.is_empty() && !it.text.to_lowercase().contains(&filter) {
            continue;
        }
        let head: String = it
            .text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let line = format!("{}  #{}  {}", fmt_time(it.mtime), it.seq, head);
        let w = wide(&line);
        SendMessageW(lb, LB_ADDSTRING, 0, w.as_ptr() as LPARAM);
        shown.push(it.seq);
    }
}

unsafe fn selected_seq(app: &App, hwnd: HWND) -> Option<u64> {
    let idx = SendMessageW(GetDlgItem(hwnd, ID_LIST), LB_GETCURSEL, 0, 0);
    if idx < 0 {
        return None;
    }
    app.shown.lock().unwrap().get(idx as usize).copied()
}

unsafe fn copy_selected(app: &App, hwnd: HWND) {
    let Some(seq) = selected_seq(app, hwnd) else {
        set_status(hwnd, "先选一条");
        return;
    };
    let text = app
        .items
        .lock()
        .unwrap()
        .iter()
        .find(|i| i.seq == seq)
        .map(|i| i.text.clone());
    match text {
        Some(t) if write_clipboard(&t) => set_status(hwnd, "已复制到剪贴板"),
        Some(_) => set_status(hwnd, "复制失败（剪贴板被占用）"),
        None => set_status(hwnd, "条目已不存在"),
    }
}

unsafe fn delete_selected(app: &App, hwnd: HWND) {
    let Some(seq) = selected_seq(app, hwnd) else {
        set_status(hwnd, "先选一条");
        return;
    };
    if let Err(err) = app.store.remove(seq) {
        set_status(hwnd, &format!("删除失败：{err}"));
        return;
    }
    app.items.lock().unwrap().retain(|i| i.seq != seq);
    refresh_list(app, hwnd);
    set_status(hwnd, "已删除");
}

unsafe fn clear_all(app: &App, hwnd: HWND) {
    if MessageBoxW(
        hwnd,
        wide("清空全部历史？文件会从磁盘删除。").as_ptr(),
        wide("ClipBridge").as_ptr(),
        MB_YESNO | MB_ICONQUESTION,
    ) != IDYES
    {
        return;
    }
    if let Err(err) = app.store.clear() {
        set_status(hwnd, &format!("清空失败：{err}"));
        return;
    }
    app.items.lock().unwrap().clear();
    refresh_list(app, hwnd);
    set_status(hwnd, "已清空");
}

unsafe fn show_tray_menu(hwnd: HWND, app: &App) {
    let menu = CreatePopupMenu();
    let paused = app.paused.load(Ordering::Relaxed);
    AppendMenuW(menu, MF_STRING, CMD_OPEN, wide("打开历史 (Ctrl+Alt+H)").as_ptr());
    AppendMenuW(
        menu,
        MF_STRING,
        CMD_PAUSE,
        wide(if paused {
            "继续监听剪贴板"
        } else {
            "暂停监听剪贴板"
        })
        .as_ptr(),
    );
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
    SetFocus(GetDlgItem(hwnd, ID_SEARCH));
}

unsafe fn set_status(hwnd: HWND, text: &str) {
    set_text(GetDlgItem(hwnd, ID_STATUS), text);
}

unsafe fn get_text(hwnd: HWND) -> String {
    if hwnd.is_null() {
        return String::new();
    }
    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
    String::from_utf16_lossy(&buf[..len])
}

/// 写系统剪贴板（历史窗口「复制」用）。
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
        if ok {
            crate::clipboard::note_self_write(text);
        }
        ok
    }
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





