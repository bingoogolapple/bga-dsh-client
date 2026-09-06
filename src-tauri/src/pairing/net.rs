//! 局域网 IP 探测。
//!
//! 从 `pairing/mod.rs` 拆出：这是唯一的 `unsafe` 集中地（`getifaddrs`），
//! 单独隔离后，其余模块可以放心地保持 100% safe Rust。

use std::net::{IpAddr, Ipv4Addr};

/// 探测局域网 IPv4：优先默认路由出口 IP（Wi-Fi/有线即手机同网段），
/// 取不到时枚举所有接口里第一个私网 IPv4。
pub(crate) fn lan_ipv4() -> Option<Ipv4Addr> {
    let udp = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    if udp.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).is_ok() {
        if let Ok(addr) = udp.local_addr() {
            if let IpAddr::V4(v) = addr.ip() {
                if !v.is_loopback() {
                    // 容器/虚拟网卡常把默认出口解析成 172.16/12（例如
                    // 172.18.0.1），手机无法通过 Wi‑Fi 访问该地址。枚举接口后
                    // 优先选择 192.168/16、10/8 等真实局域网地址。
                    if let Some(candidate) = lan_ipv4_ifaddrs() {
                        if address_rank(candidate) > address_rank(v) {
                            return Some(candidate);
                        }
                    }
                    return Some(v);
                }
            }
        }
    }
    lan_ipv4_ifaddrs()
}

fn address_rank(ip: Ipv4Addr) -> u8 {
    let [a, b, ..] = ip.octets();
    if a == 192 && b == 168 {
        3
    } else if a == 10 {
        2
    } else if a == 172 && (16..=31).contains(&b) {
        1
    } else {
        0
    }
}

#[cfg(unix)]
fn lan_ipv4_ifaddrs() -> Option<Ipv4Addr> {
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut best: Option<Ipv4Addr> = None;
        let mut p = ifap;
        while !p.is_null() {
            let ifa = &*p;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_loopback()
                    && !ip.is_link_local()
                    && !ip.is_unspecified()
                    && ip.is_private()
                    && best
                        .map(|b| address_rank(ip) > address_rank(b))
                        .unwrap_or(true)
                {
                    best = Some(ip);
                }
            }
            p = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        best
    }
}

#[cfg(not(unix))]
fn lan_ipv4_ifaddrs() -> Option<Ipv4Addr> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探测不应 panic；即使无网络也返回 None 而非崩溃。
    #[test]
    fn lan_ipv4_never_panics() {
        let _ = lan_ipv4();
    }

    /// 若探测成功，结果必须是**非 loopback、非 link-local、非 unspecified** 的 IPv4
    /// ——这是 QR 里广播给手机的地址，若是 127.0.0.1 或 0.0.0.0 则二维码无法使用。
    #[test]
    fn detected_ip_is_broadcastable_when_present() {
        if let Some(ip) = lan_ipv4() {
            assert!(!ip.is_loopback(), "不应返回 loopback 地址: {ip}");
            assert!(!ip.is_unspecified(), "不应返回 0.0.0.0");
            assert!(!ip.is_link_local(), "不应返回 169.254.x.x: {ip}");
            // 私网地址（10/8, 172.16/12, 192.168/16）——路由出口探测偶尔会拿到
            // 公网地址（如直连公网的环境），故此处不断言 is_private，
            // 只保证"可用作广播地址"的必要条件。
        }
    }
}
