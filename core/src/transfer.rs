//! 文件传输：报价（控制面 JSON）+ 数据面裸流 + `.part` 落盘 + SHA256 校验。
//!
//! 设计取舍（见 `docs/architecture.md` 5.3）：
//! - 不做自研分块：QUIC 流自带流控与重传，发送方直接顺序写；
//! - 每条文件一条新的双向流，流首一帧 `FILE_DATA` 带 transfer id，其后是裸字节；
//! - 落盘先写 `<name>.part`，校验通过再 `rename`；
//! - 暂停 = 停写并把 offset 记下来；恢复 = 新开流从 offset 续写（QT 连接断掉也一样）。

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// 单文件上限：2GB（软上限，只在 OFFER 阶段校验）。
pub const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// 读写块大小。取 256KB：够大以摊薄 syscall，够小以便暂停/取消及时响应。
pub const CHUNK: usize = 256 * 1024;

/// 传输状态：0=运行 1=暂停 2=取消。
pub const ST_RUN: u8 = 0;
pub const ST_PAUSE: u8 = 1;
pub const ST_CANCEL: u8 = 2;

/// 控制面 `FILE_OFFER` 的 payload。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offer {
    /// 16 字节 transfer id 的 hex
    pub id: String,
    /// 原始文件名（不含路径）
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub mime: String,
}

/// 控制面 `FILE_ACCEPT` / `FILE_PAUSE` / `FILE_RESUME` 的 payload。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Accept {
    pub id: String,
    #[serde(default)]
    pub offset: u64,
}

/// 控制面 `FILE_COMPLETE` 的 payload。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Complete {
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
}

/// 共享的传输开关（UI / 通知按钮改状态，发送循环轮询）。
#[derive(Debug, Default)]
pub struct TransferFlags {
    state: AtomicU8,
    /// 已写入对端的字节数（发送方 checkpoint / 接收方进度）
    offset: AtomicU64,
}

impl TransferFlags {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    pub fn pause(&self) {
        self.state.store(ST_PAUSE, Ordering::Relaxed);
    }

    pub fn cancel(&self) {
        self.state.store(ST_CANCEL, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.state.store(ST_RUN, Ordering::Relaxed);
    }

    pub fn offset(&self) -> u64 {
        self.offset.load(Ordering::Relaxed)
    }

    pub fn add_offset(&self, delta: u64) {
        self.offset.fetch_add(delta, Ordering::Relaxed);
    }

    pub fn set_offset(&self, value: u64) {
        self.offset.store(value, Ordering::Relaxed);
    }
}

/// 发送结束的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Done(u64),
    Paused(u64),
    Canceled(u64),
}

/// 生成 transfer id：时间戳 + pid + 目标路径做 SHA256，取前 16 字节。
///
/// 不引入 rand 依赖：这里的 id 只用于本次会话内配对，不需要密码学随机。
/// 传输 id：由「源路径 + 大小」稳定生成。
///
/// 故意不含时间戳/进程 id —— 同一个文件重试时 id 不变，接收端才能认出旧 `.part`
/// 并从断点续传（`Accept.offset` 才有意义）。
pub fn new_id(seed: &str, size: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update(size.to_le_bytes());
    let digest = hasher.finalize();
    hex16(&digest[..16])
}

fn hex16(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 计算整文件 SHA256（流式，内存占用与文件大小无关）。
pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).context("open for hash")?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf).context("read for hash")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex16(&hasher.finalize()))
}

/// 保证落盘文件名安全：去掉路径分隔符、控制字符，空名给个兜底。
pub fn safe_name(raw: &str) -> String {
    let name = raw
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    let trimmed = name.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        format!("file-{}", new_id("unnamed", 0))
    } else {
        trimmed
    }
}

/// 同名文件已存在时追加 ` (1)`、` (2)`…，不覆盖用户已有文件。
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let base = Path::new(name);
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let ext = base
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let mut candidate = dir.join(format!("{stem}{ext}"));
    let mut n = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
        if n > 999 {
            break;
        }
    }
    candidate
}

/// 发送：从 `offset` 开始把文件写到这条流上，直到写完 / 被暂停 / 被取消。
///
/// `send` 已在此之前写过 `FILE_DATA` 帧（带 transfer id）。
pub async fn send_stream(
    send: &mut SendStream,
    path: &Path,
    offset: u64,
    flags: &TransferFlags,
) -> Result<SendOutcome> {
    let mut file = tokio::fs::File::open(path).await.context("open source")?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).await.context("seek")?;
    }
    let mut written = offset;
    let mut buf = vec![0u8; CHUNK];

    loop {
        match flags.state() {
            ST_CANCEL => return Ok(SendOutcome::Canceled(written)),
            ST_PAUSE => return Ok(SendOutcome::Paused(written)),
            _ => {}
        }
        let n = file.read(&mut buf).await.context("read source")?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await.context("write stream")?;
        written += n as u64;
        flags.set_offset(written);
    }

    send.finish().ok();
    Ok(SendOutcome::Done(written))
}

/// 接收：把流里的裸字节写进 `.part`（从 `offset` 追加），返回最终字节数。
pub async fn recv_stream(
    recv: &mut RecvStream,
    part: &Path,
    offset: u64,
    flags: &TransferFlags,
    total: u64,
) -> Result<u64> {
    if let Some(parent) = part.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let mut file = if offset > 0 {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(part)
            .await
            .context("open part for resume")?
    } else {
        tokio::fs::File::create(part).await.context("create part")?
    };
    if offset > 0 {
        // 关键：对齐到 offset，而不是简单 append 到文件末尾。
        // 上一条流可能写超了（对端算的 offset 偏小 / 同一 id 被两次投递），
        // 末尾对齐会导致重复字节、落盘尺寸比源文件大。
        let cur = file.metadata().await.map(|m| m.len()).unwrap_or(0);
        if cur > offset {
            file.set_len(offset).await.context("truncate part")?;
        }
        file.seek(SeekFrom::Start(offset)).await.context("seek part")?;
    }

    let mut written = offset;
    let mut buf = vec![0u8; CHUNK];
    loop {
        match flags.state() {
            ST_CANCEL => {
                drop(file);
                tokio::fs::remove_file(part).await.ok();
                return Ok(written);
            }
            ST_PAUSE => break,
            _ => {}
        }
        let n = match recv.read(&mut buf).await {
            Ok(Some(n)) => n,
            Ok(None) => break,
            Err(err) => {
                eprintln!("[transfer] read error at {written}: {err:#}");
                break;
            }
        };
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).await.context("write part")?;
        written += n as u64;
        flags.set_offset(written);
        if total > 0 && written >= total {
            break;
        }
    }
    file.flush().await.ok();
    drop(file);
    Ok(written)
}

