//! 极简持久化队列：每条文本一个文件，seq 单调递增。
//!
//! 文件形式（而不是 SQLite）是刻意的：M1 只要求"断连期间不丢内容"，
//! 目录扫描足够；M2 再接 SQLite 做 ACK 水位线与历史管理。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 当前时间戳（秒），历史列表展示用。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("items")).context("create items dir")?;
        Ok(Self { root })
    }

    fn seq_path(&self) -> PathBuf {
        self.root.join("seq")
    }

    pub fn last_seq(&self) -> u64 {
        fs::read_to_string(self.seq_path())
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// 入队一条文本，返回它的 seq。
    pub fn push(&self, text: &str) -> Result<u64> {
        let seq = self.last_seq() + 1;
        fs::write(self.root.join("items").join(format!("{seq:012}.txt")), text)
            .context("write item")?;
        fs::write(self.seq_path(), seq.to_string()).context("write seq")?;
        Ok(seq)
    }

    /// 取 seq > after 的全部条目，按 seq 升序。
    pub fn since(&self, after: u64) -> Result<Vec<(u64, String)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("items")).context("read items dir")? {
            let path = entry?.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(seq) = stem.parse::<u64>() else {
                continue;
            };
            if seq <= after {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap_or_default();
            out.push((seq, text));
        }
        out.sort_by_key(|(seq, _)| *seq);
        Ok(out)
    }

    /// 全量条目（含 mtime），供历史窗口展示，按 seq 降序（新的在前）。
    pub fn all(&self) -> Result<Vec<(u64, String, u64)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("items")).context("read items dir")? {
            let path = entry?.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(seq) = stem.parse::<u64>() else {
                continue;
            };
            let text = fs::read_to_string(&path).unwrap_or_default();
            let mtime = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push((seq, text, mtime));
        }
        out.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(out)
    }

    /// 删除单条（历史管理页用）。
    pub fn remove(&self, seq: u64) -> Result<()> {
        fs::remove_file(self.root.join("items").join(format!("{seq:012}.txt"))
        )
        .context("remove item")?;
        Ok(())
    }

    /// 某条文本在磁盘上的文件路径。删条目时要把它整个挪进回收站，而不是抹掉。
    pub fn item_path(&self, seq: u64) -> std::path::PathBuf {
        self.root.join("items").join(format!("{seq:012}.txt"))
    }

    /// 清空全部条目（保留 seq 水位线，避免 seq 回退）。
    pub fn clear(&self) -> Result<()> {
        let items = self.root.join("items");
        for entry in fs::read_dir(&items).context("read items dir")? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("txt") {
                let _ = fs::remove_file(path);
            }
        }
        Ok(())
    }
}
