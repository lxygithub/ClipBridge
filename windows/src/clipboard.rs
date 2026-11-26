//! Windows 剪贴板监听。
//!
//! 用 `GetClipboardSequenceNumber` 轮询而不是消息监听：序号变化才真正打开剪贴板读取，
//! 200ms 一次的开销可忽略，却省掉自建消息窗口 + 类注册的一堆 unsafe 样板。
//! 需要彻底避免轮询时再换 `AddClipboardFormatListener`。

use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW,
};
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

/// 超过这个长度不采集，避免把大段文本塞进队列。
const MAX_TEXT: usize = 64 * 1024;

/// 自己写入剪贴板的文本哈希：轮询发现「内容没变过」就不用回推。
static SELF_WRITE_HASH: AtomicU64 = AtomicU64::new(0);

/// 记录一次自写（历史窗口复制/粘贴回剪贴板时调用）。
pub fn note_self_write(text: &str) {
    SELF_WRITE_HASH.store(fnv1a(text), Ordering::Relaxed);
}

pub struct ClipboardWatcher {
    last_seq: AtomicU64,
}

impl ClipboardWatcher {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            last_seq: AtomicU64::new(u64::from(unsafe { GetClipboardSequenceNumber() })),
        })
    }

    /// 后台轮询：防抖 400ms，内容去重后回调。
    pub fn spawn_poll(
        self: &Arc<Self>,
        mut on_text: impl FnMut(String) + Send + 'static,
    ) -> Result<()> {
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name("clipboard-poll".into())
            .spawn(move || {
                let mut pending: Option<Instant> = None;
                let mut last_hash: u64 = 0;
                loop {
                    std::thread::sleep(Duration::from_millis(200));
                    let seq = u64::from(unsafe { GetClipboardSequenceNumber() });
                    let prev = me.last_seq.swap(seq, Ordering::Relaxed);
                    if seq != prev {
                        pending = Some(Instant::now());
                        continue;
                    }
                    let Some(since) = pending else { continue };
                    if since.elapsed() < Duration::from_millis(400) {
                        continue;
                    }
                    pending = None;

                    let Some(text) = read_clipboard_text() else { continue };
                    let hash = fnv1a(&text);
                    if hash == last_hash {
                        continue;
                    }
                    last_hash = hash;
                    // 自己刚写进剪贴板的内容（历史窗口「复制」）不要再回推给手机
                    if hash == SELF_WRITE_HASH.load(Ordering::Relaxed) {
                        continue;
                    }
                    on_text(text);
                }
            })?;
        Ok(())
    }
}

/// 读取纯文本；含敏感标记或超长则返回 None。
fn read_clipboard_text() -> Option<String> {
    unsafe {
        if is_sensitive() {
            return None;
        }
        if IsClipboardFormatAvailable(CF_UNICODETEXT as u32) == 0 {
            return None;
        }
        if OpenClipboard(ptr::null_mut()) == 0 {
            return None;
        }
        let handle = GetClipboardData(CF_UNICODETEXT as u32);
        let text = if handle.is_null() {
            None
        } else {
            let p = GlobalLock(handle) as *const u16;
            if p.is_null() {
                None
            } else {
                let mut len = 0usize;
                while *p.add(len) != 0 && len < MAX_TEXT {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
                GlobalUnlock(handle);
                Some(s)
            }
        };
        CloseClipboard();
        text.filter(|t| !t.trim().is_empty())
    }
}

/// 密码管理器会打这个标记表示「不要监控」，命中就跳过。
///
/// 只认 `ExcludeClipboardContentFromMonitorProcessing`：
/// `CanIncludeInClipboardHistory` 等格式只要出现就跳过会过度过滤，误伤正常复制。
fn is_sensitive() -> bool {
    static FMT: OnceLock<u32> = OnceLock::new();
    let fmt = *FMT.get_or_init(|| unsafe {
        let name: Vec<u16> = "ExcludeClipboardContentFromMonitorProcessing"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        RegisterClipboardFormatW(name.as_ptr())
    });
    fmt != 0 && unsafe { IsClipboardFormatAvailable(fmt) } != 0
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}
