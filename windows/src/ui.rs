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
    GetKeyState, RegisterHotKey, SetFocus, MOD_ALT, MOD_CONTROL, VK_A, VK_CONTROL, VK_DELETE,
    VK_ESCAPE, VK_SHIFT, VK_V,
};
use windows_sys::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, Shell_NotifyIconW, ShellExecuteW, NIF_ICON,
    NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::clipimg;
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

// 版面：标题 0..56，两块热区 78..150，发送目标行 152..176，列表标题 200..222，条目从这里开始
const LIST_TOP: i32 = 230;
const PEER_TOP: i32 = 152;
const PEER_H: i32 = 24;
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
const C_ROW_HOVER: u32 = 0x00281D12; // #121D28
/// 选中的行：比悬浮再亮一档，靠「底色 + 满色边框」表示选中
const C_ROW_SEL: u32 = 0x00342A19; // #192A34

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
    /// 收进来的条目记着发件设备名；电脑自己发出去的是 None
    pub from: Option<String>,
}

/// 一条连接当前的链路状态，给标题栏那枚徽标用。
#[derive(Clone, Copy, Default)]
pub struct LinkInfo {
    pub connected: bool,
    /// 只有一台时才有意义：true = 打洞成功走 IP 直连；false = 走 relay
    pub direct: bool,
    /// 单台是它自己的延迟；多台时取最慢的那台（徽标只报汇总，具体每台的延迟在「发给」胶囊上）
    pub rtt_ms: u64,
    /// 在线设备台数
    pub peers: usize,
    /// 其中走直连的台数
    pub direct_n: usize,
}

/// 列表里的一行：文本 / 文件 / 剪贴板图片。
pub enum RowKind {
    Text { seq: u64, text: String },
    File { path: PathBuf },
    Image { path: PathBuf },
}

pub struct Row {
    /// 稳定身份：`t<seq>` / `f<路径>` / `i<路径>`。列表每 2 秒重排，选中状态按它记才不会串行
    pub key: String,
    pub kind: RowKind,
    pub mtime: u64,
    /// true = 电脑发出去的，false = 手机发过来的；决定行边框配色
    pub outgoing: bool,
    /// 预渲染的一行主文案
    pub title: String,
    /// 右侧元信息（大小 / 时间）
    pub meta: String,
}

/// 一台连上来的手机：id 是 EndpointId，name 来自它的 HELLO（还没收到名字时为空）。
#[derive(Clone, Default)]
pub struct PeerSlot {
    pub id: String,
    pub name: String,
    /// 这台现在走没走直连；胶囊上顺带把延迟写出来，多台时才知道「这个 586 ms 是谁的」
    pub direct: bool,
    pub rtt_ms: u64,
}

pub struct App {
    pub store: Arc<Store>,
    items: Mutex<Vec<HistoryItem>>,
    rows: Mutex<Vec<Row>>,
    link: Mutex<LinkInfo>,
    /// 连着的所有手机（网络线程每 2 秒同步一次）
    peers: Mutex<Vec<PeerSlot>>,
    /// 连过的 EndpointId → 名字（名字可能比连接晚到，先记着）
    peer_names: Mutex<std::collections::HashMap<String, String>>,
    /// 只发给这一台；None = 广播给全部
    target: Mutex<Option<String>>,
    /// 每条条目发出去那一刻到底给了谁（Row::key → 标签）。写死在这里，
    /// 之后再点「发给」行也**不会**改写历史条目的副标题。
    delivered: Mutex<std::collections::HashMap<String, String>>,
    /// 上面那份投递记录落盘的位置，重启后历史条目还认得自己当初发给谁
    delivered_path: PathBuf,
    /// 列表里选中的行（存 Row::key）。Ctrl+点追加/取消，Shift+点从锚点连选，Delete 一次性删掉
    selected: Mutex<std::collections::HashSet<String>>,
    /// Shift 连选的起点（行号，-1 = 还没点过任何行）
    anchor: AtomicIsize,
    /// 删掉的东西不直接抹掉，挪到这儿还能捞回来
    trash: PathBuf,
    inbox: PathBuf,
    outbox: PathBuf,
    /// 剪贴板图片落地目录（`in_`/`out_` 前缀区分收发的方向）
    images: PathBuf,
    /// 列表缩略图缓存：路径 → (mtime, 位图)
    thumbs: Mutex<std::collections::HashMap<PathBuf, (u64, clipimg::Thumb)>>,
    sender: Mutex<Option<UnboundedSender<Outgoing>>>,
    pub paused: AtomicBool,
    msg_hwnd: AtomicIsize,
    hist_hwnd: AtomicIsize,
    /// 0 = 条目页，1 = 日志页（同一个窗口里翻页，不再单开窗口）
    page: AtomicIsize,
    scroll: AtomicIsize,
    hover: AtomicIsize,
    /// 鼠标是否停在顶部那张发送卡片上：0 否、1 是
    zone_hover: AtomicIsize,
    /// 底部浮动提示，1.2 秒后清掉
    hint: Mutex<Option<String>>,
}

/// 交给网络线程的一份待发内容：文本走 store 排队，图片直接带着 PNG 字节走。
pub enum Outgoing {
    Text(String),
    Image(Vec<u8>),
}

impl App {
    pub fn new(store: Arc<Store>, dir: &Path) -> Self {
        let items = store
            .all()
            .unwrap_or_default()
            .into_iter()
            .map(|(seq, text, mtime)| HistoryItem {
                seq,
                text,
                mtime,
                from: store.incoming_from(seq),
            })
            .collect();
        Self {
            store,
            items: Mutex::new(items),
            rows: Mutex::new(Vec::new()),
            link: Mutex::new(LinkInfo::default()),
            peers: Mutex::new(Vec::new()),
            peer_names: Mutex::new(std::collections::HashMap::new()),
            target: Mutex::new(None),
            delivered: Mutex::new(load_delivered(&dir.join("delivered.tsv"))),
            delivered_path: dir.join("delivered.tsv"),
            selected: Mutex::new(std::collections::HashSet::new()),
            anchor: AtomicIsize::new(-1),
            trash: dir.join("trash"),
            inbox: dir.join("inbox"),
            outbox: dir.join("outbox"),
            images: dir.join("images"),
            thumbs: Mutex::new(std::collections::HashMap::new()),
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
    pub fn set_sender(&self, tx: UnboundedSender<Outgoing>) {
        *self.sender.lock().unwrap() = Some(tx);
    }

    pub fn send_text(&self, text: String) -> bool {
        match self.sender.lock().unwrap().as_ref() {
            Some(tx) => tx.send(Outgoing::Text(text)).is_ok(),
            None => false,
        }
    }

    /// 发一张已编码好的 PNG。图片不写 store（文本库只存字），只在 images 目录留一份可回看。
    pub fn send_image(&self, png: Vec<u8>, w: u32, h: u32) -> bool {
        if let Some(path) = self.save_image(&png, w, h, true) {
            // 图片按文件名记投递对象（路径会随归档搬家，名字不会）
            if let Some(name) = path.file_name().map(|s| s.to_string_lossy().to_string()) {
                let label = self.current_target_label();
                self.note_delivered(&format!("fname:{name}"), &label);
            }
        }
        match self.sender.lock().unwrap().as_ref() {
            Some(tx) => tx.send(Outgoing::Image(png)).is_ok(),
            None => false,
        }
    }

    /// 落盘一张图片：文件名里带方向和尺寸，列表就不必每次解码 PNG 才知道多大。
    /// `outgoing` = 电脑发出去的（`out_` 前缀），否则是手机发来的（`in_`）。
    pub fn save_image(&self, png: &[u8], w: u32, h: u32, outgoing: bool) -> Option<PathBuf> {
        let _ = std::fs::create_dir_all(&self.images);
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis();
        let name = format!(
            "{}_{ms:013}_{w}x{h}.png",
            if outgoing { "out" } else { "in" }
        );
        let path = self.images.join(name);
        std::fs::write(&path, png).ok()?;
        self.refresh();
        Some(path)
    }

    /// 列表缩略图：按路径缓存，文件变了（mtime 不同）就重建。
    pub fn thumb(&self, path: &Path, mtime: u64) -> Option<Arc<clipimg::Thumb>> {
        {
            let cache = self.thumbs.lock().unwrap();
            if let Some((m, t)) = cache.get(path) {
                if *m == mtime {
                    return Some(Arc::new(clipimg::Thumb {
                        hbmp: t.hbmp,
                        dc: t.dc,
                        w: t.w,
                        h: t.h,
                    }));
                }
            }
        }
        let thumb = unsafe { clipimg::make_thumb(path, 34) }?;
        let mut cache = self.thumbs.lock().unwrap();
        // 缓存别无限长：超过 120 张就整体丢掉重建（列表本身也只留 200 行）
        if cache.len() > 120 {
            for (_, (_, t)) in cache.drain() {
                unsafe { t.destroy() };
            }
        }
        let boxed = Arc::new(thumb);
        cache.insert(
            path.to_path_buf(),
            (
                mtime,
                clipimg::Thumb {
                    hbmp: boxed.hbmp,
                    dc: boxed.dc,
                    w: boxed.w,
                    h: boxed.h,
                },
            ),
        );
        Some(boxed)
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

    /// 手机连上来时报上自己的名字。按 EndpointId 记，多台手机互不覆盖。
    pub fn learn_peer_name(&self, id: &str, name: &str) {
        let name = name.trim();
        if id.is_empty() || name.is_empty() {
            return;
        }
        let changed = {
            let mut names = self.peer_names.lock().unwrap();
            names.insert(id.to_string(), name.to_string()).as_deref() != Some(name)
        };
        if changed {
            self.sync_peer_names();
            self.refresh();
        }
    }

    /// 网络线程每 2 秒把「当前连着的手机」同步过来（顺序按 id，稳定不乱跳）。
    pub fn set_peers(&self, mut list: Vec<PeerSlot>) {
        list.sort_by(|a, b| a.id.cmp(&b.id));
        let changed = {
            let mut cur = self.peers.lock().unwrap();
            let same = cur.len() == list.len()
                && cur.iter().zip(list.iter()).all(|(a, b)| {
                    a.id == b.id
                        && a.name == b.name
                        && a.direct == b.direct
                        // 延迟抖几个 ms 不值得整窗重绘，按 20ms 一档比
                        && a.rtt_ms / 20 == b.rtt_ms / 20
                });
            if same {
                false
            } else {
                *cur = list;
                true
            }
        };
        if changed {
            // 选中的那台断开了就退回「全部」，否则界面显示发了却没人收
            let mut target = self.target.lock().unwrap();
            if let Some(t) = target.as_ref() {
                let alive = self.peers.lock().unwrap().iter().any(|p| &p.id == t);
                if !alive {
                    *target = None;
                }
            }
            drop(target);
            self.refresh();
        }
    }

    fn sync_peer_names(&self) {
        let names = self.peer_names.lock().unwrap().clone();
        let mut peers = self.peers.lock().unwrap();
        for p in peers.iter_mut() {
            if p.name.is_empty() {
                if let Some(n) = names.get(&p.id) {
                    p.name = n.clone();
                }
            }
        }
    }

    pub fn peer_slots(&self) -> Vec<PeerSlot> {
        self.sync_peer_names();
        self.peers.lock().unwrap().clone()
    }

    /// 当前发送目标：None = 全部在线设备。
    pub fn target_peer(&self) -> Option<String> {
        self.target.lock().unwrap().clone()
    }

    /// 点选目标设备（None = 全部）。
    pub fn set_target(&self, id: Option<String>) {
        let mut t = self.target.lock().unwrap();
        if *t == id {
            return;
        }
        *t = id;
        drop(t);
        // 只重绘「发给」那一行和标题，历史条目的副标题是发出去那一刻定下的，不受影响
        let hwnd = self.hist_hwnd.load(Ordering::Relaxed);
        if hwnd != 0 {
            unsafe { PostMessageW(hwnd as HWND, WM_APP_REFRESH, 0, 0) };
        }
    }

    /// 记下某条条目真正发给了谁（网络线程在投递前调用）。写一次就不再改动。
    pub fn note_delivered(&self, key: &str, label: &str) {
        let mut map = self.delivered.lock().unwrap();
        if map.get(key).map(|s| s.as_str()) == Some(label) {
            return;
        }
        map.insert(key.to_string(), label.to_string());
        save_delivered(&self.delivered_path, &map);
        drop(map);
        let hwnd = self.hist_hwnd.load(Ordering::Relaxed);
        if hwnd != 0 {
            unsafe { PostMessageW(hwnd as HWND, WM_APP_REFRESH, 0, 0) };
        }
    }

    /// 这条条目当初发给了谁；没记录（老数据 / 没连上）就返回 None。
    pub fn delivered_of(&self, key: &str) -> Option<String> {
        self.delivered.lock().unwrap().get(key).cloned()
    }

    /// 当前选择下「发给谁」的文字，网络线程发送前拿它写进投递记录。
    pub fn current_target_label(&self) -> String {
        self.peer_label()
    }

    /// 列表行与副标题用的对端标签：选了单台就是它的名字，否则「全部设备」。
    fn peer_label(&self) -> String {
        let peers = self.peer_slots();
        let target = self.target_peer();
        let pick = |p: &PeerSlot| -> String {
            if p.name.is_empty() {
                format!("手机 · {}…", &p.id[..8.min(p.id.len())])
            } else {
                p.name.clone()
            }
        };
        if let Some(t) = target.as_ref() {
            if let Some(p) = peers.iter().find(|p| &p.id == t) {
                return pick(p);
            }
        }
        match peers.len() {
            0 => "手机".to_string(),
            1 => pick(&peers[0]),
            n => format!("全部设备（{n} 台）"),
        }
    }

    /// 收到的条目用：广播模式下没法知道具体是哪台发的，就写「手机」。
    fn peer_label_in(&self) -> String {
        let peers = self.peer_slots();
        if let Some(t) = self.target_peer() {
            if let Some(p) = peers.iter().find(|p| p.id == t) {
                return if p.name.is_empty() {
                    format!("手机 · {}…", &p.id[..8.min(p.id.len())])
                } else {
                    p.name.clone()
                };
            }
        }
        match peers.len() {
            0 => "手机".to_string(),
            1 => {
                let p = &peers[0];
                if p.name.is_empty() {
                    format!("手机 · {}…", &p.id[..8.min(p.id.len())])
                } else {
                    p.name.clone()
                }
            }
            _ => "手机".to_string(),
        }
    }

    /// EndpointId → 给人看的设备名（名字还没到时退回短 id）。
    pub fn peer_display(&self, id: &str) -> String {
        let names = self.peer_names.lock().unwrap();
        match names.get(id) {
            Some(n) if !n.trim().is_empty() => n.clone(),
            _ => format!("手机 · {}…", &id[..8.min(id.len())]),
        }
    }

    /// 新条目：进内存 + 刷新窗口（网络线程调用）。
    pub fn add(&self, seq: u64, text: &str) {
        self.add_item(seq, text, None);
    }

    /// 手机发过来的条目：记下发件设备，列表里显示「来自 xxx」。
    pub fn add_in(&self, seq: u64, text: &str, from: &str) {
        self.add_item(seq, text, Some(from.to_string()));
    }

    fn add_item(&self, seq: u64, text: &str, from: Option<String>) {
        let mtime = crate::queue::now_secs();
        self.items.lock().unwrap().insert(
            0,
            HistoryItem {
                seq,
                text: text.to_string(),
                mtime,
                from,
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

    /// 这一行选没选中。
    pub fn is_selected(&self, key: &str) -> bool {
        self.selected.lock().unwrap().contains(key)
    }

    pub fn selection_len(&self) -> usize {
        self.selected.lock().unwrap().len()
    }

    /// 删除时文件被挪去的地方，提示里要写给用户看。
    pub fn trash_dir(&self) -> &Path {
        &self.trash
    }

    /// Ctrl+点：把这行加进选中或移出去。
    pub fn toggle_selected(&self, key: &str) {
        {
            let mut set = self.selected.lock().unwrap();
            if !set.remove(key) {
                set.insert(key.to_string());
            }
        }
        self.refresh();
    }

    /// Ctrl+A：当前列表全选。
    pub fn select_all_rows(&self) {        {
            let rows = self.rows.lock().unwrap();
            let mut set = self.selected.lock().unwrap();
            set.clear();
            for r in rows.iter() {
                set.insert(r.key.clone());
            }
        }
        self.refresh();
    }

    /// 点空白处 / Esc：清空选中。返回是否真的清掉了东西。
    pub fn clear_selection(&self) -> bool {
        let had = {
            let mut set = self.selected.lock().unwrap();
            let had = !set.is_empty();
            set.clear();
            had
        };
        if had {
            self.refresh();
        }
        had
    }

    /// Shift 连选的锚点：Ctrl+点 / 普通点都会把它挪到当前行。
    pub fn set_anchor(&self, idx: isize) {
        self.anchor.store(idx, Ordering::Relaxed);
    }

    pub fn anchor(&self) -> isize {
        self.anchor.load(Ordering::Relaxed)
    }

    /// Shift+点：把锚点到这一行之间的所有条目选中（替换原有选中）。
    pub fn select_range(&self, from: isize, to: isize) {
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        let keys: Vec<String> = {
            let rows = self.rows.lock().unwrap();
            rows.iter()
                .enumerate()
                .filter(|(i, _)| (*i as isize) >= lo && (*i as isize) <= hi)
                .map(|(_, r)| r.key.clone())
                .collect()
        };
        {
            let mut set = self.selected.lock().unwrap();
            set.clear();
            for k in keys {
                set.insert(k);
            }
        }
        self.refresh();
    }

    /// 删除选中的条目，返回删掉的条数。
    ///
    /// 文本从 store 里摘掉（本来就是我们自己的历史库）；文件和图片不直接删盘，
    /// 挪到 `<数据目录>\trash` 下，误删了还能拖回来。
    pub fn delete_selected(&self) -> usize {
        let keys: std::collections::HashSet<String> =
            std::mem::take(&mut *self.selected.lock().unwrap());
        if keys.is_empty() {
            return 0;
        }
        let mut n = 0usize;
        {
            let rows = self.rows.lock().unwrap();
            for row in rows.iter() {
                if !keys.contains(&row.key) {
                    continue;
                }
                match &row.kind {
                    RowKind::Text { seq, .. } => {
                        // 文本也走回收站：直接 store.remove 就再也找不回来了
                        let p = self.store.item_path(*seq);
                        if self.move_to_trash(&p) {
                            n += 1;
                        }
                    }
                    RowKind::File { path } => {
                        if self.move_to_trash(path) {
                            n += 1;
                        }
                    }
                    RowKind::Image { path } => {
                        if self.move_to_trash(path) {
                            n += 1;
                            self.thumbs.lock().unwrap().remove(path);
                        }
                    }
                }
            }
        }
        // store 变了，内存里的文本列表跟着重建，窗口下一帧就是删干净的样子
        if let Ok(all) = self.store.all() {
            *self.items.lock().unwrap() = all
                .into_iter()
                .map(|(seq, text, mtime)| HistoryItem {
                    seq,
                    text,
                    mtime,
                    from: self.store.incoming_from(seq),
                })
                .collect();
        }
        self.refresh();
        n
    }

    /// 挪进 `trash/`，重名就加时间戳，避免覆盖之前删的同名文件。
    fn move_to_trash(&self, path: &Path) -> bool {
        let _ = std::fs::create_dir_all(&self.trash);
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut dst = self.trash.join(&name);
        if dst.exists() {
            let stamp = crate::queue::now_secs();
            let (stem, ext) = match name.rsplit_once('.') {
                Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
                _ => (name.clone(), String::new()),
            };
            dst = self.trash.join(format!("{stem}-{stamp}{ext}"));
        }
        std::fs::rename(path, &dst).is_ok()
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
                let zone = if hit(&zone_rects(rc.right), pt) { 1 } else { 0 };
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
                let card = zone_rects(rc.right);
                let (x, y) = (((lp & 0xFFFF) as i16) as i32, (((lp >> 16) & 0xFFFF) as i16) as i32);
                let pt = POINT { x, y };
                // 先看「删除 N」那枚胶囊：它在标题行，和列表区不重叠，但顺序放在最前面免得起冲突
                if let Some(cr) = delete_chip_rect(&*app) {
                    if hit(&cr, pt) {
                        delete_rows(&*app, hwnd);
                        return 0;
                    }
                }
                if hit(&card, pt) {
                    pick_file(&*app);
                } else if let Some(sel) = peer_chip_at(&*app, rc.right, pt) {
                    (*app).set_target(sel);
                } else if hit(&log_chip_rect(rc.right), pt) {
                    let next = if (*app).page() == 0 { 1 } else { 0 };
                    (*app).set_page(next);
                } else {
                    let idx = row_at(&*app, rc.right, pt);
                    let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                    let shift = GetKeyState(VK_SHIFT as i32) < 0;
                    if idx < 0 {
                        // 点空白：取消选中
                        if (*app).clear_selection() {
                            InvalidateRect(hwnd, ptr::null(), 0);
                        }
                    } else if shift && (*app).anchor() >= 0 {
                        // Shift+点：从锚点连选到这一行（和资源管理器一致）
                        (*app).select_range((*app).anchor(), idx);
                        InvalidateRect(hwnd, ptr::null(), 0);
                    } else if ctrl {
                        // Ctrl+点：加进选中 / 移出选中，不触发复制或打开
                        let key = {
                            let rows = (*app).rows.lock().unwrap();
                            rows.get(idx as usize).map(|r| r.key.clone())
                        };
                        if let Some(k) = key {
                            (*app).toggle_selected(&k);
                            (*app).set_anchor(idx);
                            InvalidateRect(hwnd, ptr::null(), 0);
                        }
                    } else {
                        // 普通点：先清掉多选，再做原来的动作（复制 / 打开）
                        (*app).set_anchor(idx);
                        (*app).clear_selection();
                        activate_row(&*app, hwnd, pt);
                    }
                }
            }
            0
        }
        WM_KEYDOWN => {
            if is_hist && !app.is_null() {
                let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                if wp == VK_V as usize && ctrl {
                    // 焦点在哪都行，Ctrl+V 直接发剪贴板里的文本或图片
                    send_clipboard(&*app, hwnd);
                } else if wp == VK_DELETE as usize {
                    delete_rows(&*app, hwnd);
                } else if wp == VK_ESCAPE as usize {
                    if (*app).clear_selection() {
                        InvalidateRect(hwnd, ptr::null(), 0);
                    }
                } else if wp == VK_A as usize && ctrl {
                    (*app).select_all_rows();
                    InvalidateRect(hwnd, ptr::null(), 0);
                }
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

/// 删除选中的条目（Delete 键和「删除 N」胶囊共用）。文件不真删，挪进 trash。
unsafe fn delete_rows(app: &App, hwnd: HWND) {
    if app.selection_len() == 0 {
        app.set_hint("先 Ctrl+点选要删的条目");
        return;
    }
    let n = app.delete_selected();
    let trash = app.trash_dir().display().to_string();
    if n == 0 {
        app.set_hint("没删成（文件可能被别的程序占用）");
    } else if app.page() == 1 {
        app.set_hint(&format!("已删除 {n} 条"));
    } else {
        app.set_hint(&format!("已删除 {n} 条（文件挪到了 {trash}）"));
    }
    InvalidateRect(hwnd, ptr::null(), 0);
}

/// 发送目标那一行的胶囊几何：第一枚是「全部」，后面每台手机一枚。
/// 宽度按字符估（ASCII 7px、中文 12px），够用了——不做精确排版。
fn peer_chips(app: &App, width: i32) -> Vec<(Option<String>, String, RECT)> {
    let mut out = Vec::new();
    let peers = app.peer_slots();
    if peers.is_empty() {
        return out;
    }
    let mut x = 84;
    if peers.len() > 1 {
        let label = "全部";
        let w = 16 + chip_text_w(label);
        out.push((
            None,
            label.to_string(),
            RECT {
                left: x,
                top: PEER_TOP,
                right: x + w,
                bottom: PEER_TOP + PEER_H,
            },
        ));
        x += w + 6;
    }
    for p in peers.iter() {
        let name = if p.name.is_empty() {
            format!("{}…", &p.id[..8.min(p.id.len())])
        } else if p.name.chars().count() > 12 {
            format!("{}…", p.name.chars().take(12).collect::<String>())
        } else {
            p.name.clone()
        };
        // 多台时 Capsule 上带自己的延迟，才分得清徽标里那个数字是谁的
        let label = if p.rtt_ms > 0 {
            format!("{name} · {}", fmt_rtt(p.rtt_ms))
        } else {
            name
        };
        let w = 16 + chip_text_w(&label);
        if x + w > width - 16 {
            break;
        }
        out.push((
            Some(p.id.clone()),
            label,
            RECT {
                left: x,
                top: PEER_TOP,
                right: x + w,
                bottom: PEER_TOP + PEER_H,
            },
        ));
        x += w + 6;
    }
    out
}

fn chip_text_w(s: &str) -> i32 {
    s.chars()
        .map(|c| if c.is_ascii() { 7 } else { 12 })
        .sum::<i32>()
        .max(14)
}

/// 「删除 N」那枚胶囊的位置：贴着「全部条目」标题右边，和列表首行齐高。
fn delete_chip_rect(app: &App) -> Option<RECT> {
    let n = app.selection_len();
    if n == 0 {
        return None;
    }
    let label = format!("删除 {n}");
    let wpx = 22 + chip_text_w(&label);
    Some(RECT {
        left: 96,
        top: LIST_TOP - 32,
        right: 96 + wpx,
        bottom: LIST_TOP - 8,
    })
}

/// 点中的是哪一枚：`Some(None)` = 全部，`Some(Some(id))` = 指定手机，`None` = 没点这里。
fn peer_chip_at(app: &App, width: i32, pt: POINT) -> Option<Option<String>> {
    for (id, _, rc) in peer_chips(app, width) {
        if hit(&rc, pt) {
            return Some(id);
        }
    }
    None
}

/// 顶部一整张卡片：点它挑文件，Ctrl+V 直接粘剪贴板里的文本或图片。
fn zone_rects(width: i32) -> RECT {
    let pad = 16;
    let top = 78;
    let h = 68;
    RECT {
        left: pad,
        top,
        right: width - pad,
        bottom: top + h,
    }
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
            // 一行状态写的是「现在有几台在用」，不是发送目标：选定了某一台时，
            // 这一行如果只写一个名字，看着就像「只连上了一台」，所以两台以上报台数。
            let online = app.peer_slots().len();
            let who = if online >= 2 {
                format!("{online} 台在线")
            } else {
                app.peer_label()
            };
            (format!("已连接 · {who}"), C_DIM)
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

    // 状态徽标：单台报「直连/中继 + 延迟」；多台只报汇总（几台、各自走什么），
    // 具体某台的延迟写在「发给」那一行的胶囊上，不然根本分不清这个数字是谁的。
    let link = *app.link.lock().unwrap();
    let (label, fg, bg) = if !link.connected {
        ("未连接".to_string(), 0x0087_9584u32, 0x001E_2C39u32)
    } else if link.peers >= 2 {
        let n = link.peers;
        if link.direct_n == n {
            (format!("{n} 台 · 直连"), 0x00A6_E37Bu32, 0x002B_4314u32)
        } else if link.direct_n == 0 {
            (format!("{n} 台 · 中继"), 0x00B3_A496u32, 0x0023_1B14u32)
        } else {
            (format!("{n} 台 · 混合"), 0x005B_B9F5u32, 0x0012_324Au32)
        }
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

    // 一整张卡片就是发送入口：点它 = 挑文件，Ctrl+V = 直接粘剪贴板里的文本或图片
    let card = zone_rects(w);
    let hot = app.zone_hover.load(Ordering::Relaxed) != 0;
    fill_round(hdc, card, C_PANEL, if hot { C_AMBER } else { C_BORDER }, 10);
    paint_zone(
        hdc,
        card,
        hot,
        C_AMBER,
        "选择文件",
        "点这里挑文件；Ctrl+V 直接粘贴文本或图片，也能拖文件进来",
    );

    // 发送目标：连着多台手机时给胶囊选择（默认「全部」广播），只连一台就只做说明
    let peers = app.peer_slots();
    let target = app.target_peer();
    text_in(
        hdc,
        "发给",
        RECT {
            left: 18,
            top: PEER_TOP,
            right: 84,
            bottom: PEER_TOP + PEER_H,
        },
        small_font(),
        C_FAINT,
        DT_SINGLELINE | DT_VCENTER,
    );
    if peers.len() <= 1 {
        text_in(
            hdc,
            &format!("{}（{}）", app.peer_label(), if peers.is_empty() { "未连接" } else { "唯一在线设备" }),
            RECT {
                left: 66,
                top: PEER_TOP,
                right: w - 16,
                bottom: PEER_TOP + PEER_H,
            },
            small_font(),
            if peers.is_empty() { C_FAINT } else { C_DIM },
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    } else {
        for (id, label, rc) in peer_chips(app, w) {
            let on = match (&id, &target) {
                (None, None) => true,
                (Some(i), Some(t)) => i == t,
                _ => false,
            };
            fill_round(
                hdc,
                rc,
                if on { 0x002B_4314 } else { C_PANEL },
                if on { C_TEAL } else { C_BORDER },
                PEER_H,
            );
            text_in(
                hdc,
                &label,
                rc,
                small_font(),
                if on { 0x00A6_E37B } else { C_DIM },
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );
        }
        let hint = match &target {
            None => "广播给全部在线设备".to_string(),
            Some(id) => {
                let name = peers
                    .iter()
                    .find(|p| &p.id == id)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                if name.is_empty() {
                    format!("只发给 {}", &id[..8.min(id.len())])
                } else {
                    format!("只发给 {name}")
                }
            }
        };
        text_in(
            hdc,
            &hint,
            RECT {
                // 提示跟胶囊同一行、贴右边：不再单占一行，免得和「全部条目」标题挤在一起
                left: w / 2 + 100,
                top: PEER_TOP,
                right: w - 16,
                bottom: PEER_TOP + PEER_H,
            },
            small_font(),
            C_FAINT,
            DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_END_ELLIPSIS,
        );
    }

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

    // 有选中才有这枚「删除 N」：没选中时一个多余控件都不画
    if let Some(cr) = delete_chip_rect(app) {
        let label = format!("删除 {}", app.selection_len());
        fill_round(hdc, cr, C_PANEL, C_AMBER, 6);
        text_in(
            hdc,
            &label,
            RECT {
                left: cr.left,
                top: cr.top,
                right: cr.right,
                bottom: cr.bottom,
            },
            small_font(),
            C_AMBER,
            DT_SINGLELINE | DT_CENTER | DT_VCENTER,
        );
    }

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
            "点上方卡片挑文件、按 Ctrl+V 粘文本或图片，或手机上复制 / 发文件",
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
            let sel = app.is_selected(&row.key);
            let is_file = matches!(row.kind, RowKind::File { .. });
            let is_image = matches!(row.kind, RowKind::Image { .. });
            // 方向配色和手机端一致：发=琥珀，收=青。静默走暗档、悬浮提亮，边框 1px。
            // 选中行不提亮边框、只换底色 + 保持满色，免得和「悬浮」两套高亮打架。
            let edge = match (row.outgoing, hot || sel) {
                (true, true) => C_SEND,
                (true, false) => C_SEND_DIM,
                (false, true) => C_RECV,
                (false, false) => C_RECV_DIM,
            };
            let face = if sel {
                C_ROW_SEL
            } else if hot {
                C_ROW_HOVER
            } else {
                C_PANEL
            };
            fill_round(hdc, r, face, edge, 8);

            // 类型图标：文本=复制小方块，文件=文档，图片=缩略图
            let mut text_left = r.left + 40;
            if is_image {
                let path = match &row.kind {
                    RowKind::Image { path, .. } => path.clone(),
                    _ => PathBuf::new(),
                };
                if let Some(thumb) = app.thumb(&path, row.mtime) {
                    let box_ = RECT {
                        left: r.left + 11,
                        top: r.top + 6,
                        right: r.left + 45,
                        bottom: r.top + ROW_H - 6,
                    };
                    clipimg::draw_thumb(hdc, &thumb, box_);
                } else {
                    draw_doc(hdc, r.left + 15, r.top + (ROW_H - 15) / 2, C_DIM);
                }
                text_left = r.left + 54;
            } else {
                let icon_x = r.left + 15;
                let icon_y = r.top + (ROW_H - 15) / 2;
                if is_file {
                    draw_doc(hdc, icon_x, icon_y, if hot { C_TEAL } else { C_DIM });
                } else {
                    draw_copy(hdc, icon_x, icon_y, if hot { C_TEAL } else { C_DIM });
                }
            }

            // 文件/图片行右侧留出「打开所在目录」图标的位置
            let folder_x = r.right - 16 - 18;
            let meta_right = if is_file || is_image { folder_x - 12 } else { r.right - 16 };

            text_in(
                hdc,
                &row.title,
                RECT {
                    left: text_left,
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
            if is_file || is_image {
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

/// 顶部那张整卡：左侧图标 + 两行文案；悬浮时整卡泛一点底色。
unsafe fn paint_zone(hdc: HDC, rc: RECT, hot: bool, accent: u32, title: &str, hint: &str) {
    if hot {
        // 悬浮高亮：整卡单独铺一层更亮的底色
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
    draw_folder_big(hdc, ix, iy, accent);
    // 右边挂一枚快捷键提示，把这张宽卡片撑起来，也顺手告诉用户 Ctrl+V 能用
    let label = "Ctrl+V";
    let cw = 20 + chip_text_w(label);
    let chip = RECT {
        left: rc.right - 20 - cw,
        top: rc.top + (rc.bottom - rc.top) / 2 - 12,
        right: rc.right - 20,
        bottom: rc.top + (rc.bottom - rc.top) / 2 + 12,
    };
    fill_round(hdc, chip, C_BG, if hot { accent } else { C_BORDER }, 12);
    text_in(
        hdc,
        label,
        chip,
        small_font(),
        if hot { accent } else { C_DIM },
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
    text_in(
        hdc,
        title,
        RECT {
            left: ix + 32,
            top: rc.top + 14,
            right: chip.left - 16,
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
            right: chip.left - 16,
            bottom: rc.top + 65,
        },
        small_font(),
        if hot { accent } else { C_FAINT },
        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
}


/// 投递记录落盘：每行 `Row::key<TAB>标签`。只是给历史条目留个「当初发给了谁」。
fn save_delivered(path: &Path, map: &std::collections::HashMap<String, String>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut out = String::new();
    for (k, v) in map {
        out.push_str(k);
        out.push('\t');
        out.push_str(&v.replace('\t', " "));
        out.push('\n');
    }
    let _ = std::fs::write(path, out);
}

fn load_delivered(path: &Path) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in text.lines() {
        let mut it = line.splitn(2, '\t');
        if let (Some(k), Some(v)) = (it.next(), it.next()) {
            if !k.is_empty() && !v.is_empty() {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
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
    let peer_in = app.peer_label_in();
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
        let key = format!("t{}", it.seq);
        // 收进来的用「来自 谁」，发出去的写「当初发给了谁」；两个都不跟当前选择漂移
        let outgoing = it.from.is_none();
        let meta_head = match &it.from {
            Some(from) => format!("来自 {from}"),
            None => match app.delivered_of(&key) {
                Some(w) => format!("发给 {w}"),
                None => "已发送".to_string(),
            },
        };
        rows.push(Row {
            key,
            kind: RowKind::Text {
                seq: it.seq,
                text: it.text.clone(),
            },
            mtime: it.mtime,
            outgoing,
            title: head.trim().replace('\t', " "),
            meta: format!("{meta_head} · {}", fmt_time(it.mtime)),
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
                let key = format!("f{}", path.display());
                let who = app
                    .delivered_of(&format!("fname:{name}"))
                    .or_else(|| app.delivered_of(&key));
                rows.push(Row {
                    key,
                    kind: RowKind::File { path: path.clone() },
                    mtime,
                    outgoing,
                    title: name,
                    meta: format!(
                        "{} · {}",
                        human_size(meta.len()),
                        if outgoing {
                            match who {
                                Some(w) => format!("发给 {w}"),
                                None => "已发送".to_string(),
                            }
                        } else {
                            format!("来自 {peer_in}")
                        }
                    ),
                });
            }
        }
    }

    // 剪贴板图片：文件名 `out_<ms>_<w>x<h>.png` / `in_...`，方向和尺寸直接从名字读，
    // 不必为了列一行去解码 PNG。
    if let Ok(entries) = std::fs::read_dir(&app.images) {
        for e in entries.flatten() {
            let path = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if !name.ends_with(".png") {
                continue;
            }
            let outgoing = name.starts_with("out");
            let dims = name
                .trim_end_matches(".png")
                .rsplit('_')
                .next()
                .and_then(|d| d.split_once('x'))
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let (w, h) = dims.unwrap_or((0, 0));
            let key = format!("i{}", path.display());
            let who = app
                .delivered_of(&format!("fname:{name}"))
                .or_else(|| app.delivered_of(&key));
            rows.push(Row {
                key,
                kind: RowKind::Image { path: path.clone() },
                mtime,
                outgoing,
                title: if w > 0 {
                    format!("图片 {w}×{h}")
                } else {
                    "图片".to_string()
                },
                meta: format!(
                    "{} · {} · {}",
                    human_size(meta.len()),
                    if outgoing {
                        match who {
                            Some(w) => format!("发给 {w}"),
                            None => "已发送".to_string(),
                        }
                    } else {
                        format!("来自 {peer_in}")
                    },
                    fmt_time(mtime)
                ),
            });
        }
    }

    rows.sort_by_key(|r| std::cmp::Reverse(r.mtime));
    rows.truncate(200);
    // 选中集合只保留还存在的行：条目被别人删了/自己发完移走了，选中项不能赖着不走
    {
        let alive: std::collections::HashSet<&str> =
            rows.iter().map(|r| r.key.as_str()).collect();
        let mut set = app.selected.lock().unwrap();
        set.retain(|k| alive.contains(k.as_str()));
    }
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
            Some(RowKind::Image { path, .. }) => {
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                let r = row_rect(rc.right, idx as i32, app.scroll.load(Ordering::Relaxed) as i32);
                if pt.x >= r.right - 44 {
                    Some(Action::OpenDir(path.clone()))
                } else {
                    Some(Action::CopyImage(path.clone()))
                }
            }
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
        Some(Action::CopyImage(p)) => match std::fs::read(&p).ok().and_then(|b| clipimg::decode_png(&b)) {
            Some(bmp) => {
                if clipimg::write_clipboard(&bmp) {
                    app.set_hint(&format!("已复制图片 {}×{}", bmp.w, bmp.h));
                } else {
                    app.set_hint("复制失败，剪贴板被占用");
                }
            }
            None => app.set_hint("这张图读不出来"),
        },
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
    CopyImage(PathBuf),
    OpenDir(PathBuf),
    OpenFile(PathBuf),
}

/// 把剪贴板里的东西发出去（不做后台监听，只有用户明确粘贴才发）。
/// 优先级：有文字发文字；没文字但剪贴板是张图，就转成 PNG 发图。
unsafe fn send_clipboard(app: &App, hwnd: HWND) {
    if let Some(text) = read_clipboard_text() {
        if !text.trim().is_empty() {
            let chars = text.chars().count();
            if app.send_text(text) {
                app.set_hint(&format!("已发送 {chars} 字到手机"));
            } else {
                app.set_hint("发送通道未就绪，稍后再试");
            }
            let _ = hwnd;
            return;
        }
    }
    match clipimg::read_clipboard() {
        Some(bmp) => match clipimg::encode_png(&bmp) {
            Some(png) => {
                let (w, h) = (bmp.w, bmp.h);
                // 协议单帧上限 16MB，留点余量：超了就让用户先缩一下，不要发出去才发现写帧失败
                if png.len() > 12 * 1024 * 1024 {
                    app.set_hint("图片太大（超过 12MB），先压缩再发");
                } else if app.send_image(png, w, h) {
                    app.set_hint(&format!("已发送图片 {w}×{h}"));
                } else {
                    app.set_hint("发送通道未就绪，稍后再试");
                }
            }
            None => app.set_hint("这张图转 PNG 失败"),
        },
        None => {
            // 读不出图时把剪贴板里现有的格式打到日志里，下次一看就知道是谁的格式没吃下
            let fmts = clipimg::describe_formats();
            println!("[clip] 没有可用的图，剪贴板格式: {fmts}");
            if fmts == "剪贴板被占用" {
                app.set_hint("剪贴板被别的程序占用，稍后再试");
            } else {
                app.set_hint("剪贴板里的图片格式不认识，换个方式复制再试");
            }
        }
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
