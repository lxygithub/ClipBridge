//! ClipBridge core：传输层、协议帧、设备身份。
//!
//! 该 crate 不依赖任何 UI/平台 API，Windows 端与 Android 端（经 uniffi/JNI）共用。

pub mod net;
pub mod protocol;

/// 本协议的 ALPN 标识。
pub const ALPN: &[u8] = b"clipbridge/1";

/// 默认数据目录。
pub const DEFAULT_DIR: &str = r"D:\ClipBridge";

/// 默认落盘目录（接收到的文件）。
pub const DEFAULT_INBOX: &str = r"D:\ClipBridge\inbox";
