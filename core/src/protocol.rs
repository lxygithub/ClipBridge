//! 协议帧：`[type u8][len u32 LE][payload]`，与 docs/architecture.md 第 5 节一致。

use anyhow::{Context, Result};
use iroh::endpoint::{RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const MSG_HELLO: u8 = 0x01;
pub const MSG_CLIP_TEXT: u8 = 0x02;
pub const MSG_CLIP_IMAGE_META: u8 = 0x03;
pub const MSG_CLIP_FILES: u8 = 0x04;
/// 历史同步：电脑一次把最近若干条文本甩给手机（补发只发 `>last_seq`，手机本地历史空了就永远补不回来）。
pub const MSG_HISTORY: u8 = 0x06;
pub const MSG_FILE_OFFER: u8 = 0x10;
pub const MSG_FILE_ACCEPT: u8 = 0x11;
pub const MSG_FILE_REJECT: u8 = 0x12;
pub const MSG_FILE_COMPLETE: u8 = 0x13;
pub const MSG_FILE_PAUSE: u8 = 0x14;
pub const MSG_FILE_RESUME: u8 = 0x15;
pub const MSG_FILE_CANCEL: u8 = 0x16;
/// 数据面流首帧：`payload` = 16 字节 transfer id 的 hex，其后是裸文件字节。
/// 架构文档里这张表停在 0x16，这里补 0x17，不改动已有编号。
pub const MSG_FILE_DATA: u8 = 0x17;
pub const MSG_ACK: u8 = 0x20;
pub const MSG_PING: u8 = 0x21;

/// 单帧上限，防止恶意/异常 payload 撑爆内存（比 2GB 文件上限小得多，因为大文件走独立流）。
const MAX_FRAME: usize = 16 * 1024 * 1024;

pub async fn write_frame(send: &mut SendStream, ty: u8, payload: &[u8]) -> Result<()> {
    send.write_u8(ty).await.context("write frame type")?;
    send.write_u32_le(payload.len() as u32)
        .await
        .context("write frame len")?;
    send.write_all(payload)
        .await
        .context("write frame payload")?;
    send.flush().await.context("flush frame")?;
    Ok(())
}

/// 读一帧；对端正常关闭时返回 `Ok(None)`。
pub async fn read_frame(recv: &mut RecvStream) -> Result<Option<(u8, Vec<u8>)>> {
    let ty = match recv.read_u8().await {
        Ok(ty) => ty,
        Err(_) => return Ok(None),
    };
    let len = recv.read_u32_le().await.context("read frame len")? as usize;
    if len > MAX_FRAME {
        anyhow::bail!("frame too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    if len > 0 {
        recv.read_exact(&mut buf)
            .await
            .context("read frame payload")?;
    }
    Ok(Some((ty, buf)))
}
