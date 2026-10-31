//! BGRA → NV12/I420 色彩转换（BT.709 limited range）。
//!
//! MF 编码器接受 NV12 输入，openh264 需要 I420（三平面）。
//!
//! 主路径由 `yuv` 按 CPU 特性选择 SIMD 实现，借用调用方缓冲直接写入，
//! 无每帧分配。与标量参考计算的输出相差不超过 1 LSB。
//! 420 抽样要求偶数宽高；`yuv` 不接受的尺寸使用标量转换。

use yuv::{
    BufferStoreMut, YuvBiPlanarImageMut, YuvConversionMode, YuvPlanarImageMut, YuvRange,
    YuvStandardMatrix,
};

/// BT.709 limited range：与协商下发给客户端的流参数一致。
const RANGE: YuvRange = YuvRange::Limited;
const MATRIX: YuvStandardMatrix = YuvStandardMatrix::Bt709;
/// 精度/速度均衡模式（`Fast` 有可感知画质损失，本场景不值得）。
const MODE: YuvConversionMode = YuvConversionMode::Balanced;

/// 转换一帧 BGRA（stride = w*4）到 NV12。
/// `nv12` 至少 w*h*3/2 字节（Y 平面 + UV 交织平面，2x2 下采样）。
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize, nv12: &mut [u8]) {
    assert_eq!(bgra.len(), width * height * 4, "BGRA 尺寸不符");
    assert!(nv12.len() >= width * height * 3 / 2, "NV12 缓冲不足");
    if nv12_via_crate(bgra, width, height, nv12) {
        return;
    }
    nv12_scalar(bgra, width, height, nv12);
}

/// 转换一帧 BGRA（stride = w*4）到 I420（Y、U、V 三平面）。
/// `i420` 至少 w*h*3/2 字节。相比 NV12 + 重排，少一次整帧色度拷贝。
pub fn bgra_to_i420(bgra: &[u8], width: usize, height: usize, i420: &mut [u8]) {
    assert_eq!(bgra.len(), width * height * 4, "BGRA 尺寸不符");
    assert!(i420.len() >= width * height * 3 / 2, "I420 缓冲不足");
    if i420_via_crate(bgra, width, height, i420) {
        return;
    }
    i420_scalar(bgra, width, height, i420);
}

// ---------------------------------------------------------------- yuv crate

/// 借调用方缓冲零分配转换；尺寸不合法返回 false（由调用方回退标量）。
fn nv12_via_crate(bgra: &[u8], width: usize, height: usize, nv12: &mut [u8]) -> bool {
    let y_size = width * height;
    let (y, uv) = nv12[..y_size + y_size / 2].split_at_mut(y_size);
    let mut image = YuvBiPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(y),
        y_stride: width as u32,
        uv_plane: BufferStoreMut::Borrowed(uv),
        // 420 抽样：每行 width/2 对 × 2 字节 = width
        uv_stride: width as u32,
        width: width as u32,
        height: height as u32,
    };
    yuv::bgra_to_yuv_nv12(&mut image, bgra, (width * 4) as u32, RANGE, MATRIX, MODE).is_ok()
}

/// 借调用方缓冲零分配转换；尺寸不合法返回 false（由调用方回退标量）。
fn i420_via_crate(bgra: &[u8], width: usize, height: usize, i420: &mut [u8]) -> bool {
    let y_size = width * height;
    let uv_size = y_size / 4;
    let (y, uv) = i420[..y_size + uv_size * 2].split_at_mut(y_size);
    let (u, v) = uv.split_at_mut(uv_size);
    let mut image = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(y),
        y_stride: width as u32,
        u_plane: BufferStoreMut::Borrowed(u),
        u_stride: (width / 2) as u32,
        v_plane: BufferStoreMut::Borrowed(v),
        v_stride: (width / 2) as u32,
        width: width as u32,
        height: height as u32,
    };
    yuv::bgra_to_yuv420(&mut image, bgra, (width * 4) as u32, RANGE, MATRIX, MODE).is_ok()
}

// ---------------------------------------------------------------- 标量回退

/// clamped 到合法范围
#[inline]
fn clamp_u8(v: i32, lo: i32, hi: i32) -> u8 {
    v.clamp(lo, hi) as u8
}

/// 色度（1/256 缩放）：
/// Cb = 128 + (112B - 26R -  87G + 128) >> 8
/// Cr = 128 + (112R - 102G -  10B + 128) >> 8
#[inline]
fn chroma(b: i32, g: i32, r: i32) -> (u8, u8) {
    let cb = 128 + ((112 * b - 26 * r - 87 * g + 128) >> 8);
    let cr = 128 + ((112 * r - 102 * g - 10 * b + 128) >> 8);
    (clamp_u8(cb, 16, 240), clamp_u8(cr, 16, 240))
}

/// Y 平面整行（BGRA 行紧密排列，stride = width*4）。
fn y_row_scalar(src_row: &[u8], dst_row: &mut [u8]) {
    for (d, px) in dst_row.iter_mut().zip(src_row.as_chunks::<4>().0.iter()) {
        let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
        let v = 16 + ((47 * r + 157 * g + 16 * b + 128) >> 8);
        *d = clamp_u8(v, 16, 235);
    }
}

/// 2x2 块同通道四者和（整除 4 由调用方 >>2）。
#[inline]
fn quad_sum(r1: &[u8], r2: &[u8], o: usize, ch: usize) -> i32 {
    r1[o + ch] as i32 + r1[o + 4 + ch] as i32 + r2[o + ch] as i32 + r2[o + 4 + ch] as i32
}

/// NV12 色度行对（2x2 下采样，U/V 交织）。
fn uv_pair_scalar_interleaved(r1: &[u8], r2: &[u8], dst: &mut [u8]) {
    for (pair, d) in dst.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let o = pair * 8;
        let (cb, cr) = chroma(
            quad_sum(r1, r2, o, 0) >> 2,
            quad_sum(r1, r2, o, 1) >> 2,
            quad_sum(r1, r2, o, 2) >> 2,
        );
        d[0] = cb;
        d[1] = cr;
    }
}

/// I420 色度行对（U/V 各自独立平面）。
fn uv_pair_scalar_planar(r1: &[u8], r2: &[u8], u: &mut [u8], v: &mut [u8]) {
    for (pair, (du, dv)) in u.iter_mut().zip(v.iter_mut()).enumerate() {
        let o = pair * 8;
        let (cb, cr) = chroma(
            quad_sum(r1, r2, o, 0) >> 2,
            quad_sum(r1, r2, o, 1) >> 2,
            quad_sum(r1, r2, o, 2) >> 2,
        );
        *du = cb;
        *dv = cr;
    }
}

/// 标量 NV12：奇数尺寸回退路径，同时作为测试参照。
fn nv12_scalar(bgra: &[u8], width: usize, height: usize, nv12: &mut [u8]) {
    let pairs = width / 2;
    let row_bytes = width * 4;
    let (y, uv) = nv12.split_at_mut(width * height);
    let mut uv_rows = uv.chunks_exact_mut(pairs * 2);
    let mut row = 0;
    while row + 1 < height {
        let r1 = &bgra[row * row_bytes..(row + 1) * row_bytes];
        let r2 = &bgra[(row + 1) * row_bytes..(row + 2) * row_bytes];
        let (top, bottom) = y[row * width..(row + 2) * width].split_at_mut(width);
        y_row_scalar(r1, top);
        y_row_scalar(r2, bottom);
        if let Some(d) = uv_rows.next() {
            uv_pair_scalar_interleaved(r1, r2, d);
        }
        row += 2;
    }
    if row < height {
        // 奇数高度：末行只有 Y，无色度
        let r = &bgra[row * row_bytes..(row + 1) * row_bytes];
        y_row_scalar(r, &mut y[row * width..(row + 1) * width]);
    }
}

/// 标量 I420：奇数尺寸回退路径，同时作为测试参照。
fn i420_scalar(bgra: &[u8], width: usize, height: usize, i420: &mut [u8]) {
    let pairs = width / 2;
    let row_bytes = width * 4;
    let (y, uv) = i420.split_at_mut(width * height);
    // 平面尺寸约定 (h/2)*(w/2)：与 openh264 的 YUVSlices 切片一致
    let (u, v) = uv.split_at_mut((height / 2) * (width / 2));
    let mut u_rows = u.chunks_exact_mut(pairs);
    let mut v_rows = v.chunks_exact_mut(pairs);
    let mut row = 0;
    while row + 1 < height {
        let r1 = &bgra[row * row_bytes..(row + 1) * row_bytes];
        let r2 = &bgra[(row + 1) * row_bytes..(row + 2) * row_bytes];
        let (top, bottom) = y[row * width..(row + 2) * width].split_at_mut(width);
        y_row_scalar(r1, top);
        y_row_scalar(r2, bottom);
        if let (Some(du), Some(dv)) = (u_rows.next(), v_rows.next()) {
            uv_pair_scalar_planar(r1, r2, du, dv);
        }
        row += 2;
    }
    if row < height {
        let r = &bgra[row * row_bytes..(row + 1) * row_bytes];
        y_row_scalar(r, &mut y[row * width..(row + 1) * width]);
    }
}

/// NV12 → I420（UV 交织平面拆成 U、V 独立平面）。
pub fn nv12_to_i420(nv12: &[u8], width: usize, height: usize, i420: &mut [u8]) {
    let y_size = width * height;
    assert!(nv12.len() >= y_size * 3 / 2 && i420.len() >= y_size * 3 / 2);
    i420[..y_size].copy_from_slice(&nv12[..y_size]);
    let uv_size = y_size / 4;
    let (u_plane, v_plane) = i420[y_size..].split_at_mut(uv_size);
    let uv_src = &nv12[y_size..y_size + uv_size * 2];
    for i in 0..uv_size {
        u_plane[i] = uv_src[i * 2];
        v_plane[i] = uv_src[i * 2 + 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 优化前的参照实现（著名的 4 次小循环版色度）。重构必须与它一致：
    /// 标量回退路径逐字节相同，`yuv` 路径相差不超过 1 LSB（其定点系数
    /// 精度更高）——任何真正的系数错误都会产生 10 以上的偏差，足以被
    /// 这个容差捕获，避免固定点系数缩放错误导致褪色。
    fn reference_bgra_to_nv12(bgra: &[u8], width: usize, height: usize, nv12: &mut [u8]) {
        let y_size = width * height;
        let uv_stride = width / 2;
        let uv_rows = height / 2;
        for row in 0..height {
            let src = row * width * 4;
            let dst = row * width;
            for col in 0..width {
                let p = src + col * 4;
                let (b, g, r) = (bgra[p] as i32, bgra[p + 1] as i32, bgra[p + 2] as i32);
                let y = 16 + ((47 * r + 157 * g + 16 * b + 128) >> 8);
                nv12[dst + col] = y.clamp(16, 235) as u8;
            }
        }
        for urow in 0..uv_rows {
            let r1 = urow * 2 * width * 4;
            let r2 = (urow * 2 + 1) * width * 4;
            let dst = y_size + urow * uv_stride * 2;
            for ucol in 0..uv_stride {
                let c1 = r1 + ucol * 2 * 4;
                let c2 = c1 + 4;
                let c3 = r2 + ucol * 2 * 4;
                let c4 = c3 + 4;
                let mut b = 0i32;
                let mut g = 0i32;
                let mut r = 0i32;
                for p in [c1, c2, c3, c4] {
                    b += bgra[p] as i32;
                    g += bgra[p + 1] as i32;
                    r += bgra[p + 2] as i32;
                }
                b /= 4;
                g /= 4;
                r /= 4;
                let cb = 128 + ((112 * b - 26 * r - 87 * g + 128) >> 8);
                let cr = 128 + ((112 * r - 102 * g - 10 * b + 128) >> 8);
                nv12[dst + ucol * 2] = cb.clamp(16, 240) as u8;
                nv12[dst + ucol * 2 + 1] = cr.clamp(16, 240) as u8;
            }
        }
    }

    /// 伪随机 BGRA（覆盖极端值与中间值）
    fn pseudo_bgra(w: usize, h: usize, seed: u32) -> Vec<u8> {
        let mut s = seed;
        (0..w * h * 4)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn scalar_matches_reference_byte_exact() {
        // 标量回退路径必须与参考计算逐字节一致
        for &(w, h) in &[(64usize, 64usize), (65, 33), (32, 48), (128, 96)] {
            let bgra = pseudo_bgra(w, h, 0x5EED_0000 + w as u32);
            let mut got = vec![0u8; w * h * 3 / 2];
            let mut want = vec![0u8; w * h * 3 / 2];
            nv12_scalar(&bgra, w, h, &mut got);
            reference_bgra_to_nv12(&bgra, w, h, &mut want);
            assert_eq!(got, want, "标量 NV12 与参照实现在 {w}x{h} 不一致");
        }
    }

    #[test]
    fn yuv_crate_within_one_lsb() {
        // yuv crate 定点系数精度更高，与旧实现相差不超过 1 LSB
        for &(w, h) in &[(64usize, 64usize), (128, 96), (32, 48)] {
            let bgra = pseudo_bgra(w, h, 0x5EED_0000 + w as u32);
            let mut got = vec![0u8; w * h * 3 / 2];
            let mut want = vec![0u8; w * h * 3 / 2];
            assert!(
                nv12_via_crate(&bgra, w, h, &mut got),
                "{w}x{h} 应走 yuv 路径"
            );
            reference_bgra_to_nv12(&bgra, w, h, &mut want);
            for (i, (a, b)) in got.iter().zip(want.iter()).enumerate() {
                assert!(
                    (*a as i32 - *b as i32).abs() <= 1,
                    "yuv 与参照偏差过大：偏移 {i} 处 {a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn odd_dimensions_fall_back_to_scalar() {
        // 奇数宽：yuv 的 420 抽样不接受，必须回退标量且结果正确
        let (w, h) = (65usize, 33usize);
        let bgra = pseudo_bgra(w, h, 0x0DD);
        let mut got = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut got);
        let mut want = vec![0u8; w * h * 3 / 2];
        nv12_scalar(&bgra, w, h, &mut want);
        assert_eq!(got, want, "奇数宽回退结果与标量不一致");

        let mut i420 = vec![0u8; w * h * 3 / 2];
        bgra_to_i420(&bgra, w, h, &mut i420);
        let mut want_i420 = vec![0u8; w * h * 3 / 2];
        i420_scalar(&bgra, w, h, &mut want_i420);
        assert_eq!(i420, want_i420, "奇数宽 I420 回退结果与标量不一致");
    }

    #[test]
    fn i420_matches_nv12_reorder() {
        // I420 直转必须等于「NV12 + 重排」（两种布局一致性）
        for &(w, h) in &[(64usize, 64usize), (32, 48)] {
            let bgra = pseudo_bgra(w, h, 0x5EED_0000 + w as u32);
            let mut i420 = vec![0u8; w * h * 3 / 2];
            bgra_to_i420(&bgra, w, h, &mut i420);
            let mut nv12 = vec![0u8; w * h * 3 / 2];
            bgra_to_nv12(&bgra, w, h, &mut nv12);
            let mut via_nv12 = vec![0u8; w * h * 3 / 2];
            nv12_to_i420(&nv12, w, h, &mut via_nv12);
            assert_eq!(i420, via_nv12, "I420 与 NV12 重排在 {w}x{h} 不一致");
        }
    }

    #[test]
    fn gray_input_is_neutral() {
        let (w, h) = (64usize, 64usize);
        let bgra = vec![128u8; w * h * 4];
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv12);
        // 灰色：Y ≈ 126，UV 必须严格中性（128）
        assert!((nv12[0] as i32 - 126).abs() <= 1, "Y={}", nv12[0]);
        assert!((nv12[w * h] as i32 - 128).abs() <= 1, "Cb={}", nv12[w * h]);
        assert!(
            (nv12[w * h + 1] as i32 - 128).abs() <= 1,
            "Cr={}",
            nv12[w * h + 1]
        );
    }

    #[test]
    fn saturated_colors_have_strong_chroma() {
        // 饱和红色应得到低 Cb / 高 Cr，避免固定点缩放削弱色差。
        let (w, h) = (16usize, 16usize);
        let mut bgra = vec![0u8; w * h * 4];
        for px in bgra.as_chunks_mut::<4>().0 {
            px[0] = 0; // B
            px[1] = 0; // G
            px[2] = 255; // R
            px[3] = 255;
        }
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv12);
        let cb = nv12[w * h] as i32;
        let cr = nv12[w * h + 1] as i32;
        assert!(cb < 110, "纯红 Cb 应显著偏低，实际 {cb}");
        assert!(cr > 230, "纯红 Cr 应显著偏高，实际 {cr}");
        // 纯蓝相反
        for px in bgra.as_chunks_mut::<4>().0 {
            px[0] = 255;
            px[2] = 0;
        }
        bgra_to_nv12(&bgra, w, h, &mut nv12);
        assert!(nv12[w * h] as i32 > 145, "纯蓝 Cb 应显著偏高");
        assert!((nv12[w * h + 1] as i32) < 125, "纯蓝 Cr 应低于中性");
    }
}
