//! 端点：设备身份、relay 配置、peer 地址解析。

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey, TransportAddr};

use crate::ALPN;

/// 读取现有密钥；不存在则生成并落盘，保证 `EndpointId` 跨重启稳定。
///
/// 存储格式为 64 位小写 hex（`SecretKey::from_str` 同时接受 hex 与 base32）。
pub fn load_or_create_secret(path: &Path) -> Result<SecretKey> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let t = text.trim();
        if !t.is_empty() {
            return SecretKey::from_str(t).context("invalid key file");
        }
    }
    let key = SecretKey::generate();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create key dir")?;
    }
    std::fs::write(path, hex_encode(&key.to_bytes())).context("write key file")?;
    Ok(key)
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 建端点：N0 preset（含 DNS/pkarr 发现服务）+ 官方 relay，等待地址发布最多 8 秒。
///
/// 用 `N0` 而不是 `Minimal` 是为了「只凭 EndpointId 就能连」：
/// relay 区域会随网络变化（实测拿到过 euc1 与 usw1），把 relay URL 写死在配置里迟早对不上，
/// 交给发现服务解析对端当前的家庭 relay 更可靠。
pub async fn build_endpoint(secret: SecretKey) -> Result<Endpoint> {
    let ep = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .relay_mode(RelayMode::Default)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("bind endpoint")?;

    let online = tokio::time::timeout(Duration::from_secs(8), ep.online()).await;
    if online.is_err() {
        eprintln!("[endpoint] warning: not online within 8s, relay may be unreachable");
    }
    Ok(ep)
}

/// 解析命令行给的 `peer_id` + `relay_url`。relay 可为空（只依赖直连地址）。
pub fn parse_peer(peer_id: &str, relay_url: &str) -> Result<EndpointAddr> {
    let id = EndpointId::from_str(peer_id).context("invalid peer id")?;
    let mut addrs: Vec<TransportAddr> = Vec::new();
    if !relay_url.is_empty() {
        let url = RelayUrl::from_str(relay_url).context("invalid relay url")?;
        addrs.push(TransportAddr::Relay(url));
    }
    Ok(EndpointAddr::from_parts(id, addrs))
}

/// 打印本机端点信息，便于手工复制给另一台设备。
pub fn print_endpoint_info(ep: &Endpoint) {
    let addr = ep.addr();
    println!("ID:     {}", ep.id());
    println!(
        "RELAY:  {}",
        addr.relay_urls()
            .next()
            .map(|u| u.to_string())
            .unwrap_or_else(|| "(none)".into())
    );
    println!(
        "DIRECT: {}",
        addr.ip_addrs()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
}
