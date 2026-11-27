//! 端点：设备身份、relay 配置、peer 地址解析。

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::endpoint::presets;
use iroh::endpoint::{IdleTimeout, QuicTransportConfig};
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
/// 默认 relay：空串 = 官方默认地图（use1/usw1/euc1/aps1 全在里面），由 iroh 自己挑最近且 QAD 可用的节点。
///
/// 实测（北京，2026-11-26，`info --diag`）：
/// - `euc1` QAD ✅ 153ms，`usw1` QAD ✅ 211ms
/// - `aps1` QAD ❌ 超时 1527ms —— 钉它等于让 QAD 永远失败，拿不到公网映射，打洞直接归零
/// 所以别钉单个亚太节点，交给默认地图；要多节点容错就别写死。
pub const DEFAULT_RELAY: &str = "";

/// 电脑端默认固定 UDP 端口。
///
/// 固定端口才谈得上端口映射：实测 4242 上路由器 UPnP 自动映射成功（公网映射
/// `117.133.83.22:4242`，内外端口一致），本机 NAT 被判成锥形；随机端口时公网端口跟着变，
/// 对端只能猜，打洞成功率就低。端口被占时换一个（`--port 4243`）。
pub const DEFAULT_PORT: u16 = 4242;

/// 端点建参数。默认值与 [build_endpoint] 一致，只是把「固定端口 / 只绑一块网卡」这两个
/// 决定打洞成败的旋钮露出来。
#[derive(Clone, Debug, Default)]
pub struct NetOptions {
    /// relay；空串 = 官方默认地图自动选
    pub relay: String,
    /// 本地 UDP 端口；0 = 随机。固定端口才能在路由器上做端口映射（打洞最有效的一招）
    pub port: u16,
    /// 只绑这一块网卡的地址；None = 0.0.0.0 绑全部接口
    pub bind_ip: Option<std::net::IpAddr>,
}

impl NetOptions {
    pub fn with_relay(relay: impl Into<String>) -> Self {
        Self {
            relay: relay.into(),
            ..Default::default()
        }
    }
}

pub async fn build_endpoint(secret: SecretKey) -> Result<Endpoint> {
    build_endpoint_with_relay(secret, DEFAULT_RELAY).await
}

/// 同 [build_endpoint]，但可以钉死一个 relay（空串 = 官方默认地图，自动选最近）。
///
/// 为什么要钉：默认地图里节点会自己挑，北京实测挑到了 euc1（法兰克福），
/// 传大文件只有 70KB/s 还老被 idle 超时掐断。钉到 aps1（新加坡）能少一半 RTT。
pub async fn build_endpoint_with_relay(secret: SecretKey, relay_url: &str) -> Result<Endpoint> {
    build_endpoint_opts(secret, &NetOptions::with_relay(relay_url)).await
}

/// 完整版：relay + 端口 + 网卡全在这里定。
///
/// 绑 `0.0.0.0` 会把 WireGuard / Tailscale / ZeroTier / WSL 等所有接口的地址都当成本机候选
/// 发布出去，对端要逐个试，真正能用的往往只有一个，探测窗口就这么被摊薄了。
/// 绑到单块网卡（`bind_ip`）只发那一个地址；配 `port` 固定端口，再在路由器上做一条 UDP
/// 映射，PC 这一侧就变成「端口可预测」，打洞成功率会明显不一样。
pub async fn build_endpoint_opts(secret: SecretKey, opts: &NetOptions) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        // QUIC 层：链路烂的时候别急着判死
        // - keep_alive 3s：一条 PING 只要 3s 没发就会补，relay 路径不会因为静默被判 30s idle
        // - 连接级 idle 60s：默认 30s，跨网抖动一下就断
        // - 路径级 idle 15s 是 iroh 上限（再大也会被 clamp），保持默认
        .transport_config(transport_config())
        // UPnP/PCAP/NAT-PMP 默认就开；显式写上是为了别让人以为它被关了
        .portmapper_config(iroh::endpoint::PortmapperConfig::default());

    if opts.port != 0 || opts.bind_ip.is_some() {
        let ip = opts
            .bind_ip
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        builder = builder
            .bind_addr(std::net::SocketAddr::new(ip, opts.port))
            .context("invalid bind address")?;
    }

    if opts.relay.is_empty() {
        builder = builder.relay_mode(RelayMode::Default);
    } else {
        let url = RelayUrl::from_str(&opts.relay).context("invalid relay url")?;
        builder = builder.relay_mode(RelayMode::custom([url]));
    }

    let ep = builder.bind().await.context("bind endpoint")?;

    let online = tokio::time::timeout(Duration::from_secs(8), ep.online()).await;
    if online.is_err() {
        eprintln!("[endpoint] warning: not online within 8s, relay may be unreachable");
    }
    Ok(ep)
}

/// 猜「主网卡」的地址：拿一个临时 UDP socket 连一下公网，OS 选出来的源地址就是默认出口那块网卡。
///
/// 只用来给 `--bind-ip auto` 兜底，连不通（比如全被墙）就返回 None，调用方退回绑全部接口。
pub fn detect_primary_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:53").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// 传输层的 QUIC 参数：都是「链路差的时候别自己判死」的取向。
fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_secs(3))
        .default_path_keep_alive_interval(Duration::from_secs(3))
        .max_idle_timeout(Some(
            IdleTimeout::try_from(Duration::from_secs(60)).unwrap_or_default(),
        ))
        .build()
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

/// 打洞诊断：NAT 类型、公网映射、各 relay 延迟、本机实际发布了哪些候选地址。
///
/// `net_report` 是异步探测出来的，这里轮询最多 `wait_secs` 秒；期间不发任何业务数据。
pub async fn print_diagnostics(ep: &Endpoint, wait_secs: u64) {
    println!("--- 打洞诊断 ---");
    let mut watcher = ep.net_report();
    let mut report = None;
    for _ in 0..(wait_secs * 2) {
        match iroh::Watcher::get(&mut watcher) {
            Some(r) if r.global_v4.is_some() || r.global_v6.is_some() => {
                report = Some(r);
                break;
            }
            Some(r) => {
                report = Some(r);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            None => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }

    let addr = ep.addr();
    let published: Vec<String> = addr.ip_addrs().map(|a| a.to_string()).collect();
    println!("本地绑定:  {}", if local_port(ep) == 0 { "(未知)".into() } else { format!("UDP {}", local_port(ep)) });
    println!("发布候选: {}", if published.is_empty() { "(无)".into() } else { published.join(", ") });

    let Some(r) = report else {
        println!("NAT 类型:  探测未完成（relay 不通或还没测完）");
        return;
    };

    let mapping = match (r.mapping_varies_by_dest_ipv4, r.mapping_varies_by_dest_ipv6) {
        (Some(true), _) | (_, Some(true)) => "对称 NAT（换目的地端口就变）→ 打洞很难，需要端口预测/端口映射",
        (Some(false), _) | (_, Some(false)) => "锥形 NAT（端口稳定复用）→ 打洞条件好",
        _ => "未知（探测样本不够）",
    };
    println!("NAT 类型:  {mapping}");
    println!(
        "公网映射:  v4={}  v6={}",
        r.global_v4.map(|a| a.to_string()).unwrap_or_else(|| "(无)".into()),
        r.global_v6.map(|a| a.to_string()).unwrap_or_else(|| "(无)".into())
    );
    println!(
        "UDP 可用:  v4={} v6={}",
        r.udp_v4, r.udp_v6
    );
    println!(
        "首选 relay: {}",
        r.preferred_relay
            .as_ref()
            .map(|u| u.to_string())
            .unwrap_or_else(|| "(无)".into())
    );

    let mut lat: Vec<(String, Duration)> = Vec::new();
    for (_probe, url, ms) in r.relay_latency.iter() {
        lat.push((url.to_string(), ms));
    }
    lat.sort_by_key(|(_, ms)| *ms);
    for (url, ms) in lat.iter().take(4) {
        println!("  relay {} → {}ms", url, ms.as_millis());
    }

    println!("--- 结论 ---");
    if r.global_v4.is_none() && r.global_v6.is_none() {
        println!("· 没有测到公网映射：QAD 失败，只能靠 relay 中转。先确认 relay 的 UDP 端口没被拦。");
    } else if matches!(r.mapping_varies_by_dest_ipv4, Some(true)) {
        println!("· 对称 NAT：换 relay 节点救不了，必须固定端口 + 路由器映射，或同网段直连。");
    } else {
        println!("· 端口稳定：若直连仍不成立，多半是防火墙拦 UDP 入站 / 路由器不支持回环，做一条固定端口映射再试。");
    }
}

/// 端点实际用的 UDP 端口（从已发布地址里反推；取不到就当 0）。
fn local_port(ep: &Endpoint) -> u16 {
    ep.addr()
        .ip_addrs()
        .find(|a| !a.ip().is_unspecified())
        .map(|a| a.port())
        .unwrap_or(0)
}
