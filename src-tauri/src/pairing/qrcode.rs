//! 二维码生成（SVG / PNG）。
//!
//! 从 `pairing/mod.rs` 拆出：QR 编码与渲染是独立于网络栈的一块，
//! 单独成文件后 `pairing/mod.rs` 只关心"门禁 + 转发"。

use super::{QR_MARGIN, QR_SCALE};

/// 解析 QR 矩阵：返回（宽度, 行优先的深色标记）。
#[allow(deprecated)] // qrcode 0.14 的 to_colors 依赖私有 Color，to_vec 等效且无隐私问题
pub(crate) fn qr_modules(url: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(url).ok()?;
    Some((code.width(), code.to_vec()))
}

/// 生成 QR 的 SVG。
pub(crate) fn qr_svg(url: &str) -> Option<String> {
    let (w, bits) = qr_modules(url)?;
    let mut path = String::new();
    for y in 0..w {
        for x in 0..w {
            if bits[y * w + x] {
                path.push_str(&format!("M{x} {y}h1v1h-1z"));
            }
        }
    }
    Some(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {w}" shape-rendering="crispEdges"><rect width="{w}" height="{w}" fill="#ffffff"/><path fill="#1f2430" d="{path}"/></svg>"##
    ))
}

/// 从 QR 矩阵生成 RGBA 位图（供写 PNG / 复制到剪贴板）。
///
/// 用 `qr_modules` 的位矩阵自己画像素，而不是 `QrCode::render::<Luma<u8>>()`：
/// 后者依赖 `image` 的类型体系，而本项目只需要一份裸 RGBA buffer。
pub(crate) fn qr_rgba(url: &str) -> Option<(u32, u32, Vec<u8>)> {
    let (w, bits) = qr_modules(url)?;
    let size = (w as u32 + QR_MARGIN * 2) * QR_SCALE;
    let mut buf = vec![0u8; (size * size * 4) as usize];
    for y in 0..w {
        for x in 0..w {
            if !bits[y * w + x] {
                continue;
            }
            // 每个模块放大成 QR_SCALE × QR_SCALE 的深色方块
            for dy in 0..QR_SCALE {
                for dx in 0..QR_SCALE {
                    let px = ((x as u32 + QR_MARGIN) * QR_SCALE + dx) as usize;
                    let py = ((y as u32 + QR_MARGIN) * QR_SCALE + dy) as usize;
                    let off = (py * size as usize + px) * 4;
                    buf[off] = 0x1f;
                    buf[off + 1] = 0x24;
                    buf[off + 2] = 0x30;
                    buf[off + 3] = 0xff;
                }
            }
        }
    }
    // 白色背景：先填白再画深色块更简单，这里直接把未写入的像素补成白色
    for chunk in buf.chunks_exact_mut(4) {
        if chunk[3] == 0 {
            chunk[0] = 0xff;
            chunk[1] = 0xff;
            chunk[2] = 0xff;
            chunk[3] = 0xff;
        }
    }
    Some((size, size, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SVG 是合法的、含 viewBox 与深色路径的文档。
    #[test]
    fn qr_svg_is_wellformed() {
        let svg = qr_svg("http://192.168.1.5:18080/?pair=123456").expect("应能生成 SVG");
        assert!(svg.starts_with("<svg"), "应以 <svg 开头: {}", &svg[..20]);
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains("xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.contains("viewBox=\"0 0 "));
        assert!(svg.contains("<path fill=\"#1f2430\" d=\""));
        // 路径非空（否则是空白二维码）
        assert!(svg.contains("h1v1h-1z"));
    }

    /// 相同输入产出相同 SVG（确定性），不同输入产出不同 SVG。
    #[test]
    fn qr_is_deterministic_and_input_sensitive() {
        let a = qr_svg("http://a.example/?pair=111111").unwrap();
        let b = qr_svg("http://a.example/?pair=111111").unwrap();
        assert_eq!(a, b, "相同输入必须产出相同 SVG");
        let c = qr_svg("http://a.example/?pair=222222").unwrap();
        assert_ne!(a, c, "不同输入应产出不同 SVG");
    }

    /// 空字符串等极端输入不 panic（返回 None 或可用结果，绝不 panic）。
    #[test]
    fn empty_input_does_not_panic() {
        let _ = qr_svg("");
        let _ = qr_modules("");
    }

    /// RGBA 位图尺寸正确、像素数匹配、不透明。
    #[test]
    fn qr_rgba_has_consistent_dimensions() {
        let (w, h, buf) = qr_rgba("http://192.168.1.5:18080/?pair=123456").expect("应能生成位图");
        assert_eq!(w, h, "二维码位图应为正方形");
        assert_eq!(
            buf.len(),
            (w * h * 4) as usize,
            "buffer 长度应为 w*h*4 (RGBA)"
        );
        // 全部像素不透明
        assert!(
            buf.chunks_exact(4).all(|c| c[3] == 0xff),
            "所有像素应为不透明"
        );
        // 应同时存在深色与浅色像素（真有图案，不是纯色块）
        let dark = buf.chunks_exact(4).filter(|c| c[0] == 0x1f).count();
        let light = buf.chunks_exact(4).filter(|c| c[0] == 0xff).count();
        assert!(dark > 0 && light > 0, "位图应含深浅两种像素");
    }
}
