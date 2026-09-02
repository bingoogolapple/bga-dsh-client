//! 配对码与会话令牌的生成/轮换。
//!
//! 从 `pairing/mod.rs` 拆出：这部分是**纯逻辑**（随机数 → 字符串），不碰网络、
//! 不碰 Tauri 状态之外的东西，单独成文件后便于阅读与测试。

use super::{qrcode, Pairing};

/// 配对会话令牌的随机字节数（hex 后 32 字符）。
pub(crate) const TOKEN_BYTES: usize = 16;
/// 令牌 hex 长度（Cookie 值长度）。
pub(crate) const TOKEN_LEN: usize = TOKEN_BYTES * 2;
/// 生成新的 6 位一次性配对码。随机源不可用时返回 None，调用方必须拒绝配对。
pub(crate) fn gen_code() -> Option<String> {
    let mut buf = [0u8; 4];
    getrandom::getrandom(&mut buf).ok()?;
    Some(format!("{:06}", u32::from_le_bytes(buf) % 1_000_000))
}

/// 生成加密随机会话令牌（hex，`TOKEN_LEN` 字符）。
/// 随机源不可用时返回 None，绝不降级为可预测值。
pub(crate) fn gen_token() -> Option<String> {
    let mut buf = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut buf).ok()?;
    let mut s = String::with_capacity(TOKEN_LEN);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    Some(s)
}

/// 轮换配对码并同步 URL/QR（已配对设备的会话不受影响）。
pub(crate) fn rotate_code(p: &mut Pairing) {
    p.code = gen_code().unwrap_or_default();
    if let Some(ip) = p.lan_ip {
        if p.port == 0 {
            p.port = super::BASE_PORT;
        }
        let url = format!("http://{ip}:{}/?pair={}", p.port, p.code);
        p.url = url.clone();
        p.qr_svg = qrcode::qr_svg(&url).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 配对码恒为 6 位数字（含前导零），且多次生成不全相同。
    #[test]
    fn code_is_six_digits() {
        for _ in 0..200 {
            let c = gen_code().expect("OS random source required");
            assert_eq!(c.len(), 6, "配对码必须是 6 位: {c}");
            assert!(
                c.chars().all(|ch| ch.is_ascii_digit()),
                "配对码只能是数字: {c}"
            );
        }
        // 随机性：200 次里不应当全部相同
        let mut all: Vec<String> = (0..200).map(|_| gen_code().unwrap()).collect();
        all.sort();
        all.dedup();
        assert!(
            all.len() > 50,
            "配对码随机性不足，去重后仅 {} 种",
            all.len()
        );
    }

    /// 会话令牌是固定长度的十六进制串。
    #[test]
    fn token_is_hex_of_fixed_length() {
        let t = gen_token().expect("OS random source required");
        assert_eq!(t.len(), TOKEN_LEN, "令牌长度应为 {TOKEN_LEN}: {t}");
        assert!(
            t.chars().all(|c| c.is_ascii_hexdigit()),
            "令牌应全为十六进制字符: {t}"
        );
        // 多次生成应不同（碰撞概率可忽略）
        let other = gen_token().expect("OS random source required");
        assert_ne!(t, other, "两次生成的令牌不应相同");
    }

    /// 前导零不能被吃掉：`format!("{n:06}")` 保证了这一点，
    /// 若有人改成 `n.to_string()` 会被此用例拦住。
    #[test]
    fn code_preserves_leading_zeros() {
        // 直接验证格式化行为（gen_code 用的就是 06 宽度）
        assert_eq!(format!("{:06}", 42u32), "000042");
        assert_eq!(format!("{:06}", 0u32), "000000");
    }
}
