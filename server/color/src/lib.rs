//! Fast BGRA to YUV/NV12 color conversion (scalar + NEON + AVX2).
//!
//! BT.601 coefficients (matching OpenH264 and NVENC expectations):
//!   Y  =  (( 66*R + 129*G +  25*B + 128) >> 8) + 16
//!   Cb =  ((-38*R -  74*G + 112*B + 128) >> 8) + 128
//!   Cr =  ((112*R -  94*G -  18*B + 128) >> 8) + 128
//!
//! Chroma is subsampled by averaging each 2x2 block: (a+b+c+d+2)>>2.
//! All SIMD paths are bit-identical to the scalar reference (verified by
//! tests in this crate, runnable natively on aarch64 and via qemu for
//! x86_64).

#[cfg(target_arch = "aarch64")]
use std::arch::is_aarch64_feature_detected;

#[inline(always)]
fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
    ((66 * r + 129 * g + 25 * b + 128) >> 8).wrapping_add(16) as u8
}

/// Max worker threads for frame-sized conversions. Spawn cost is ~tens of µs
/// per thread, negligible against the multi-ms conversion of a 1080p+ frame.
fn conversion_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8)
}

/// Convert BGRA frame to YUV420P planes, parallelized across rows.
pub fn bgra_to_yuv420(
    bgra: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
) {
    let threads = conversion_threads();
    let row_pairs = height / 2;
    if threads < 2 || row_pairs < 64 {
        bgra_to_yuv420_chunk(bgra, width, height, stride, y_plane, u_plane, v_plane);
        return;
    }

    let half_w = width / 2;
    let chunk = row_pairs.div_ceil(threads);
    std::thread::scope(|s| {
        let mut y_rest = &mut y_plane[..row_pairs * 2 * width];
        let mut u_rest = &mut u_plane[..row_pairs * half_w];
        let mut v_rest = &mut v_plane[..row_pairs * half_w];
        let mut rp = 0;
        while rp < row_pairs {
            let take = chunk.min(row_pairs - rp);
            let (y_chunk, y_next) = y_rest.split_at_mut(take * 2 * width);
            let (u_chunk, u_next) = u_rest.split_at_mut(take * half_w);
            let (v_chunk, v_next) = v_rest.split_at_mut(take * half_w);
            y_rest = y_next;
            u_rest = u_next;
            v_rest = v_next;
            let src = &bgra[rp * 2 * stride..];
            s.spawn(move || {
                bgra_to_yuv420_chunk(src, width, take * 2, stride, y_chunk, u_chunk, v_chunk);
            });
            rp += take;
        }
    });
}

/// Single-threaded YUV420 conversion of a contiguous run of rows
/// (NEON on aarch64, AVX2 on x86_64, scalar elsewhere).
fn bgra_to_yuv420_chunk(
    bgra: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
) {
    #[cfg(target_arch = "aarch64")]
    {
        if is_aarch64_feature_detected!("neon") && width >= 16 {
            // SAFETY: NEON detected at runtime, width >= 16 for full vector processing
            unsafe {
                neon::bgra_to_yuv420_neon(bgra, width, height, stride, y_plane, u_plane, v_plane);
            }
            return;
        }
    }

    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") && width >= 16 {
            // SAFETY: AVX2 detected at runtime, width >= 16 for full vector processing
            unsafe {
                x86::bgra_to_yuv420_avx2(bgra, width, height, stride, y_plane, u_plane, v_plane);
            }
            return;
        }
    }

    bgra_to_yuv420_scalar(bgra, width, height, stride, y_plane, u_plane, v_plane);
}

/// Scalar fallback: works on all architectures.
fn bgra_to_yuv420_scalar(
    bgra: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
) {
    let half_w = width / 2;
    for row_pair in 0..(height / 2) {
        let y0 = row_pair * 2;
        let y1 = y0 + 1;
        let uv_row = row_pair * half_w;
        bgra_to_yuv420_scalar_cols(bgra, width, stride, y_plane, u_plane, v_plane, y0, y1, uv_row, 0);
    }
}

/// Scalar conversion of one row pair starting at `start_col` (must be even).
/// Used as the full fallback and for SIMD trailing columns.
#[allow(clippy::too_many_arguments)]
fn bgra_to_yuv420_scalar_cols(
    bgra: &[u8],
    width: usize,
    src_stride: usize,
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
    y0: usize,
    y1: usize,
    uv_row: usize,
    start_col: usize,
) {
    let src_row0 = y0 * src_stride;
    let src_row1 = y1 * src_stride;
    let half_w = width / 2;

    for col_pair in (start_col / 2)..half_w {
        let x0 = col_pair * 2;
        let x1 = x0 + 1;

        let i00 = src_row0 + x0 * 4;
        let i10 = src_row0 + x1 * 4;
        let i01 = src_row1 + x0 * 4;
        let i11 = src_row1 + x1 * 4;

        let (b00, g00, r00) = (bgra[i00] as i32, bgra[i00 + 1] as i32, bgra[i00 + 2] as i32);
        let (b10, g10, r10) = (bgra[i10] as i32, bgra[i10 + 1] as i32, bgra[i10 + 2] as i32);
        let (b01, g01, r01) = (bgra[i01] as i32, bgra[i01 + 1] as i32, bgra[i01 + 2] as i32);
        let (b11, g11, r11) = (bgra[i11] as i32, bgra[i11 + 1] as i32, bgra[i11 + 2] as i32);

        y_plane[y0 * width + x0] = rgb_to_y(r00, g00, b00);
        y_plane[y0 * width + x1] = rgb_to_y(r10, g10, b10);
        y_plane[y1 * width + x0] = rgb_to_y(r01, g01, b01);
        y_plane[y1 * width + x1] = rgb_to_y(r11, g11, b11);

        let r_avg = (r00 + r10 + r01 + r11 + 2) >> 2;
        let g_avg = (g00 + g10 + g01 + g11 + 2) >> 2;
        let b_avg = (b00 + b10 + b01 + b11 + 2) >> 2;

        u_plane[uv_row + col_pair] =
            ((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8).wrapping_add(128) as u8;
        v_plane[uv_row + col_pair] =
            ((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8).wrapping_add(128) as u8;
    }
}

/// Convert BGRA frame to NV12 (Y plane + interleaved UV plane).
///
/// NV12 layout:
///   - Y plane: width * height bytes
///   - UV plane: width * (height/2) bytes, interleaved [U0,V0,U1,V1,...]
pub fn bgra_to_nv12(
    bgra: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
) {
    bgra_to_nv12_pitched(bgra, width, height, stride, width, y_plane, uv_plane);
}

/// Convert BGRA to NV12 with explicit output pitch (stride), parallelized
/// across rows.
///
/// `dst_pitch` is the row stride of the Y and UV output planes (may be > width
/// when writing into NVENC's locked buffer which has its own alignment).
pub fn bgra_to_nv12_pitched(
    bgra: &[u8],
    width: usize,
    height: usize,
    src_stride: usize,
    dst_pitch: usize,
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
) {
    let threads = conversion_threads();
    let row_pairs = height / 2;
    if threads < 2 || row_pairs < 64 {
        bgra_to_nv12_chunk(bgra, width, height, src_stride, dst_pitch, y_plane, uv_plane);
        return;
    }

    let chunk = row_pairs.div_ceil(threads);
    std::thread::scope(|s| {
        let mut y_rest = &mut y_plane[..row_pairs * 2 * dst_pitch];
        let mut uv_rest = &mut uv_plane[..row_pairs * dst_pitch];
        let mut rp = 0;
        while rp < row_pairs {
            let take = chunk.min(row_pairs - rp);
            let (y_chunk, y_next) = y_rest.split_at_mut(take * 2 * dst_pitch);
            let (uv_chunk, uv_next) = uv_rest.split_at_mut(take * dst_pitch);
            y_rest = y_next;
            uv_rest = uv_next;
            let src = &bgra[rp * 2 * src_stride..];
            s.spawn(move || {
                bgra_to_nv12_chunk(src, width, take * 2, src_stride, dst_pitch, y_chunk, uv_chunk);
            });
            rp += take;
        }
    });
}

/// Single-threaded NV12 conversion of a contiguous run of rows
/// (NEON on aarch64, AVX2 on x86_64, scalar elsewhere).
fn bgra_to_nv12_chunk(
    bgra: &[u8],
    width: usize,
    height: usize,
    src_stride: usize,
    dst_pitch: usize,
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
) {
    #[cfg(target_arch = "aarch64")]
    {
        if is_aarch64_feature_detected!("neon") && width >= 16 {
            // SAFETY: NEON detected at runtime, width >= 16 for full vector processing
            unsafe {
                neon::bgra_to_nv12_neon(bgra, width, height, src_stride, dst_pitch, y_plane, uv_plane);
            }
            return;
        }
    }

    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") && width >= 16 {
            // SAFETY: AVX2 detected at runtime, width >= 16 for full vector processing
            unsafe {
                x86::bgra_to_nv12_avx2(bgra, width, height, src_stride, dst_pitch, y_plane, uv_plane);
            }
            return;
        }
    }

    bgra_to_nv12_scalar(bgra, width, height, src_stride, dst_pitch, y_plane, uv_plane);
}

/// Scalar fallback: works on all architectures.
fn bgra_to_nv12_scalar(
    bgra: &[u8],
    width: usize,
    height: usize,
    src_stride: usize,
    dst_pitch: usize,
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
) {
    for row_pair in 0..(height / 2) {
        let y0 = row_pair * 2;
        let y1 = y0 + 1;
        let yd0 = y0 * dst_pitch;
        let yd1 = y1 * dst_pitch;
        let uvd = row_pair * dst_pitch;
        bgra_to_nv12_scalar_cols(bgra, width, src_stride, y_plane, uv_plane, y0, y1, yd0, yd1, uvd, 0);
    }
}

/// Scalar NV12 conversion of one row pair starting at `start_col` (must be
/// even). Used as the full fallback and for SIMD trailing columns.
#[allow(clippy::too_many_arguments)]
fn bgra_to_nv12_scalar_cols(
    bgra: &[u8],
    width: usize,
    src_stride: usize,
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    y0: usize,
    y1: usize,
    yd0: usize,
    yd1: usize,
    uvd: usize,
    start_col: usize,
) {
    let src_row0 = y0 * src_stride;
    let src_row1 = y1 * src_stride;
    let half_w = width / 2;

    for col_pair in (start_col / 2)..half_w {
        let x0 = col_pair * 2;
        let x1 = x0 + 1;

        let i00 = src_row0 + x0 * 4;
        let i10 = src_row0 + x1 * 4;
        let i01 = src_row1 + x0 * 4;
        let i11 = src_row1 + x1 * 4;

        let (b00, g00, r00) = (bgra[i00] as i32, bgra[i00 + 1] as i32, bgra[i00 + 2] as i32);
        let (b10, g10, r10) = (bgra[i10] as i32, bgra[i10 + 1] as i32, bgra[i10 + 2] as i32);
        let (b01, g01, r01) = (bgra[i01] as i32, bgra[i01 + 1] as i32, bgra[i01 + 2] as i32);
        let (b11, g11, r11) = (bgra[i11] as i32, bgra[i11 + 1] as i32, bgra[i11 + 2] as i32);

        y_plane[yd0 + x0] = rgb_to_y(r00, g00, b00);
        y_plane[yd0 + x1] = rgb_to_y(r10, g10, b10);
        y_plane[yd1 + x0] = rgb_to_y(r01, g01, b01);
        y_plane[yd1 + x1] = rgb_to_y(r11, g11, b11);

        let r_avg = (r00 + r10 + r01 + r11 + 2) >> 2;
        let g_avg = (g00 + g10 + g01 + g11 + 2) >> 2;
        let b_avg = (b00 + b10 + b01 + b11 + 2) >> 2;

        let uv_idx = uvd + col_pair * 2;
        uv_plane[uv_idx] =
            ((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8).wrapping_add(128) as u8;
        uv_plane[uv_idx + 1] =
            ((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8).wrapping_add(128) as u8;
    }
}

// ---------------------------------------------------------------------------
// aarch64 NEON implementation
// ---------------------------------------------------------------------------

#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::*;

    /// NEON-accelerated BGRA→NV12 conversion.
    ///
    /// Processes 16 pixels at a time (two rows of 16) using 128-bit NEON
    /// vectors. Falls through to scalar for trailing pixels when width is not
    /// a multiple of 16.
    ///
    /// # Safety
    /// Caller must ensure NEON is available and the slices cover the
    /// described geometry (height even, src rows of `src_stride` bytes with at
    /// least `width*4` valid bytes each, dst planes with `dst_pitch` row
    /// stride and `width` valid columns).
    #[target_feature(enable = "neon")]
    pub(crate) unsafe fn bgra_to_nv12_neon(
        bgra: &[u8],
        width: usize,
        height: usize,
        src_stride: usize,
        dst_pitch: usize,
        y_plane: &mut [u8],
        uv_plane: &mut [u8],
    ) {
        unsafe {
            let vec_16 = vdupq_n_u16(16);
            let vec_128_16 = vdupq_n_u16(128);

            let blocks = width / 16;

            for row_pair in 0..(height / 2) {
                let y0 = row_pair * 2;
                let y1 = y0 + 1;
                let src0 = y0 * src_stride;
                let src1 = y1 * src_stride;
                let yd0 = y0 * dst_pitch;
                let yd1 = y1 * dst_pitch;
                let uvd = row_pair * dst_pitch;

                let mut col = 0usize;
                for _ in 0..blocks {
                    let bgra_ptr0 = bgra.as_ptr().add(src0 + col * 4);
                    let bgra_ptr1 = bgra.as_ptr().add(src1 + col * 4);

                    // Load 16 BGRA pixels = 64 bytes per row, deinterleaved
                    // into B,G,R,A channels
                    let px0 = vld4q_u8(bgra_ptr0);
                    let px1 = vld4q_u8(bgra_ptr1);

                    let (b0, g0, r0) = (px0.0, px0.1, px0.2);
                    let (b1, g1, r1) = (px1.0, px1.1, px1.2);

                    // Y for all 16 pixels in each row
                    let y0_val = compute_y_neon(r0, g0, b0, vec_16, vec_128_16);
                    let y1_val = compute_y_neon(r1, g1, b1, vec_16, vec_128_16);

                    vst1q_u8(y_plane.as_mut_ptr().add(yd0 + col), y0_val);
                    vst1q_u8(y_plane.as_mut_ptr().add(yd1 + col), y1_val);

                    let (u_u8, v_u8) = compute_uv_neon(b0, g0, r0, b1, g1, r1);

                    // Interleave U,V for NV12: [U0,V0,U1,V1,...] = 16 bytes
                    let uv_lo = vzip1_u8(u_u8, v_u8);
                    let uv_hi = vzip2_u8(u_u8, v_u8);
                    vst1q_u8(uv_plane.as_mut_ptr().add(uvd + col), vcombine_u8(uv_lo, uv_hi));

                    col += 16;
                }

                // Handle remaining columns with scalar
                if col < width {
                    super::bgra_to_nv12_scalar_cols(
                        bgra, width, src_stride, y_plane, uv_plane, y0, y1, yd0, yd1, uvd, col,
                    );
                }
            }
        }
    }

    /// NEON-accelerated BGRA→YUV420P conversion (separate U and V planes).
    ///
    /// Same math as [`bgra_to_nv12_neon`], but U and V are stored to separate
    /// planar outputs (Y stride = width, U/V stride = width/2).
    ///
    /// # Safety
    /// Caller must ensure NEON is available and the slices cover the
    /// described geometry (height even, src rows of `src_stride` bytes with at
    /// least `width*4` valid bytes each).
    #[target_feature(enable = "neon")]
    pub(crate) unsafe fn bgra_to_yuv420_neon(
        bgra: &[u8],
        width: usize,
        height: usize,
        src_stride: usize,
        y_plane: &mut [u8],
        u_plane: &mut [u8],
        v_plane: &mut [u8],
    ) {
        unsafe {
            let vec_16 = vdupq_n_u16(16);
            let vec_128_16 = vdupq_n_u16(128);

            let half_w = width / 2;
            let blocks = width / 16;

            for row_pair in 0..(height / 2) {
                let y0 = row_pair * 2;
                let y1 = y0 + 1;
                let src0 = y0 * src_stride;
                let src1 = y1 * src_stride;
                let yd0 = y0 * width;
                let yd1 = y1 * width;
                let uv_row = row_pair * half_w;

                let mut col = 0usize;
                for _ in 0..blocks {
                    let bgra_ptr0 = bgra.as_ptr().add(src0 + col * 4);
                    let bgra_ptr1 = bgra.as_ptr().add(src1 + col * 4);

                    let px0 = vld4q_u8(bgra_ptr0);
                    let px1 = vld4q_u8(bgra_ptr1);

                    let (b0, g0, r0) = (px0.0, px0.1, px0.2);
                    let (b1, g1, r1) = (px1.0, px1.1, px1.2);

                    let y0_val = compute_y_neon(r0, g0, b0, vec_16, vec_128_16);
                    let y1_val = compute_y_neon(r1, g1, b1, vec_16, vec_128_16);

                    vst1q_u8(y_plane.as_mut_ptr().add(yd0 + col), y0_val);
                    vst1q_u8(y_plane.as_mut_ptr().add(yd1 + col), y1_val);

                    let (u_u8, v_u8) = compute_uv_neon(b0, g0, r0, b1, g1, r1);

                    // Planar store: 8 chroma samples each
                    vst1_u8(u_plane.as_mut_ptr().add(uv_row + col / 2), u_u8);
                    vst1_u8(v_plane.as_mut_ptr().add(uv_row + col / 2), v_u8);

                    col += 16;
                }

                if col < width {
                    super::bgra_to_yuv420_scalar_cols(
                        bgra, width, src_stride, y_plane, u_plane, v_plane, y0, y1, uv_row, col,
                    );
                }
            }
        }
    }

    /// Compute Y = (66*R + 129*G + 25*B + 128) >> 8 + 16 for 16 pixels.
    #[target_feature(enable = "neon")]
    unsafe fn compute_y_neon(
        r: uint8x16_t,
        g: uint8x16_t,
        b: uint8x16_t,
        vec_16: uint16x8_t,
        vec_128: uint16x8_t,
    ) -> uint8x16_t {
        // Process low 8 and high 8 separately (u8→u16 widening)
        let r_lo = vmovl_u8(vget_low_u8(r));
        let r_hi = vmovl_u8(vget_high_u8(r));
        let g_lo = vmovl_u8(vget_low_u8(g));
        let g_hi = vmovl_u8(vget_high_u8(g));
        let b_lo = vmovl_u8(vget_low_u8(b));
        let b_hi = vmovl_u8(vget_high_u8(b));

        let mut y_lo = vmulq_n_u16(r_lo, 66);
        y_lo = vmlaq_n_u16(y_lo, g_lo, 129);
        y_lo = vmlaq_n_u16(y_lo, b_lo, 25);
        y_lo = vaddq_u16(y_lo, vec_128);
        y_lo = vshrq_n_u16(y_lo, 8);
        y_lo = vaddq_u16(y_lo, vec_16);

        let mut y_hi = vmulq_n_u16(r_hi, 66);
        y_hi = vmlaq_n_u16(y_hi, g_hi, 129);
        y_hi = vmlaq_n_u16(y_hi, b_hi, 25);
        y_hi = vaddq_u16(y_hi, vec_128);
        y_hi = vshrq_n_u16(y_hi, 8);
        y_hi = vaddq_u16(y_hi, vec_16);

        vcombine_u8(vmovn_u16(y_lo), vmovn_u16(y_hi))
    }

    /// Compute 8 U and 8 V chroma samples from a 16x2 pixel block
    /// (2x2 average, then BT.601 chroma equations).
    #[target_feature(enable = "neon")]
    unsafe fn compute_uv_neon(
        b0: uint8x16_t,
        g0: uint8x16_t,
        r0: uint8x16_t,
        b1: uint8x16_t,
        g1: uint8x16_t,
        r1: uint8x16_t,
    ) -> (uint8x8_t, uint8x8_t) {
        // Chroma: average the 2x2 blocks, exactly like scalar:
        // (a+b+c+d+2) >> 2.
        // vpaddlq_u8 sums horizontal pairs of row0 into u16 lanes,
        // vpadalq_u8 accumulates the horizontal pairs of row1 on top
        // (max 1020), then add the rounding bias and shift.
        // NOTE: do NOT use vhaddq_u8 here -- its truncating halving
        // (((a+c)>>1 + (b+d)>>1) >> 1) is off by one vs scalar for some
        // inputs.
        let two = vdupq_n_u16(2);
        let r_h = vshrq_n_u16(vaddq_u16(vpadalq_u8(vpaddlq_u8(r0), r1), two), 2);
        let g_h = vshrq_n_u16(vaddq_u16(vpadalq_u8(vpaddlq_u8(g0), g1), two), 2);
        let b_h = vshrq_n_u16(vaddq_u16(vpadalq_u8(vpaddlq_u8(b0), b1), two), 2);

        let r_s = vreinterpretq_s16_u16(r_h);
        let g_s = vreinterpretq_s16_u16(g_h);
        let b_s = vreinterpretq_s16_u16(b_h);

        // U = (-38*R - 74*G + 112*B + 128) >> 8 + 128
        // Max intermediate: 112*255 = 28560, fits s16
        let mut u_val = vmulq_n_s16(r_s, -38);
        u_val = vmlaq_n_s16(u_val, g_s, -74);
        u_val = vmlaq_n_s16(u_val, b_s, 112);
        u_val = vaddq_s16(u_val, vdupq_n_s16(128));
        u_val = vshrq_n_s16(u_val, 8);
        u_val = vaddq_s16(u_val, vdupq_n_s16(128));

        // V = (112*R - 94*G - 18*B + 128) >> 8 + 128
        let mut v_val = vmulq_n_s16(r_s, 112);
        v_val = vmlaq_n_s16(v_val, g_s, -94);
        v_val = vmlaq_n_s16(v_val, b_s, -18);
        v_val = vaddq_s16(v_val, vdupq_n_s16(128));
        v_val = vshrq_n_s16(v_val, 8);
        v_val = vaddq_s16(v_val, vdupq_n_s16(128));

        // Saturating narrow to u8 (values are in [16,240], so exact)
        (vqmovun_s16(u_val), vqmovun_s16(v_val))
    }
}

// ---------------------------------------------------------------------------
// x86_64 AVX2 implementation
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    /// Lane fix-up for `_mm256_packus_epi32(lo, hi)` / `_mm256_hadd_epi32(a, b)`
    /// which operate per 128-bit lane: restores natural element order.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn lane_fix() -> __m256i {
        _mm256_setr_epi32(0, 1, 4, 5, 2, 3, 6, 7)
    }

    /// Extract B, G, R channels of 8 BGRA pixels as zero-extended u32 lanes.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn channels(p: __m256i) -> (__m256i, __m256i, __m256i) {
        let mask = _mm256_set1_epi32(0xFF);
        let b = _mm256_and_si256(p, mask);
        let g = _mm256_and_si256(_mm256_srli_epi32(p, 8), mask);
        let r = _mm256_and_si256(_mm256_srli_epi32(p, 16), mask);
        (b, g, r)
    }

    /// Y = ((66R + 129G + 25B + 128) >> 8) + 16, per u32 lane (8 pixels).
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn y_from(b: __m256i, g: __m256i, r: __m256i) -> __m256i {
        let mut y = _mm256_mullo_epi32(r, _mm256_set1_epi32(66));
        y = _mm256_add_epi32(y, _mm256_mullo_epi32(g, _mm256_set1_epi32(129)));
        y = _mm256_add_epi32(y, _mm256_mullo_epi32(b, _mm256_set1_epi32(25)));
        y = _mm256_add_epi32(y, _mm256_set1_epi32(128));
        y = _mm256_srli_epi32(y, 8);
        _mm256_add_epi32(y, _mm256_set1_epi32(16))
    }

    /// Chroma = ((cr*R + cg*G + cb*B + 128) >> 8) + 128, per i32 lane.
    /// Arithmetic shift matches the scalar `i32 >> 8` exactly.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn chroma_from(r: __m256i, g: __m256i, b: __m256i, cr: i32, cg: i32, cb: i32) -> __m256i {
        let mut c = _mm256_mullo_epi32(r, _mm256_set1_epi32(cr));
        c = _mm256_add_epi32(c, _mm256_mullo_epi32(g, _mm256_set1_epi32(cg)));
        c = _mm256_add_epi32(c, _mm256_mullo_epi32(b, _mm256_set1_epi32(cb)));
        c = _mm256_add_epi32(c, _mm256_set1_epi32(128));
        c = _mm256_srai_epi32(c, 8);
        _mm256_add_epi32(c, _mm256_set1_epi32(128))
    }

    /// 2x2 block average over one channel: `lo`/`hi` are 8-lane u32 channel
    /// values of pixels 0..8 / 8..16 of each row. Returns 8 averages
    /// ((a+b+c+d+2)>>2) in natural order.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn avg2x2(row0_lo: __m256i, row0_hi: __m256i, row1_lo: __m256i, row1_hi: __m256i) -> __m256i {
        unsafe {
            // Vertical sums (max 510), then horizontal pairwise add (max 1020).
            let h = _mm256_hadd_epi32(
                _mm256_add_epi32(row0_lo, row1_lo),
                _mm256_add_epi32(row0_hi, row1_hi),
            );
            // hadd works per 128-bit lane: restore natural pair order.
            let h = _mm256_permutevar8x32_epi32(h, lane_fix());
            _mm256_srli_epi32(_mm256_add_epi32(h, _mm256_set1_epi32(2)), 2)
        }
    }

    /// Pack 16 u32 values (lo = elements 0..8, hi = 8..16, each in 0..=255)
    /// into 16 bytes in natural order.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn pack_u32x16_to_u8(lo: __m256i, hi: __m256i) -> __m128i {
        unsafe {
            let w = _mm256_packus_epi32(lo, hi); // per-lane: [l0..3,h0..3 | l4..7,h4..7]
            let w = _mm256_permutevar8x32_epi32(w, lane_fix()); // [l0..7 | h0..7] u16
            let b = _mm256_packus_epi16(w, w); // lane0: [l0..7,l0..7], lane1: [h0..7,h0..7]
            // pick qword0 of lane0 and qword0 of lane1
            _mm256_castsi256_si128(_mm256_permute4x64_epi64(b, 0b00_00_10_00))
        }
    }

    /// Load 2x16 pixels, write 2x16 Y bytes, return ([u0..7, v0..7]) packed
    /// in an __m128i for the caller to store as NV12 or planar.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn block16x2(
        src0: *const u8,
        src1: *const u8,
        y_dst0: *mut u8,
        y_dst1: *mut u8,
    ) -> __m128i {
        unsafe {
            let p0_lo = _mm256_loadu_si256(src0 as *const __m256i);
            let p0_hi = _mm256_loadu_si256(src0.add(32) as *const __m256i);
            let p1_lo = _mm256_loadu_si256(src1 as *const __m256i);
            let p1_hi = _mm256_loadu_si256(src1.add(32) as *const __m256i);

            let (b0l, g0l, r0l) = channels(p0_lo);
            let (b0h, g0h, r0h) = channels(p0_hi);
            let (b1l, g1l, r1l) = channels(p1_lo);
            let (b1h, g1h, r1h) = channels(p1_hi);

            let y0 = pack_u32x16_to_u8(y_from(b0l, g0l, r0l), y_from(b0h, g0h, r0h));
            let y1 = pack_u32x16_to_u8(y_from(b1l, g1l, r1l), y_from(b1h, g1h, r1h));
            _mm_storeu_si128(y_dst0 as *mut __m128i, y0);
            _mm_storeu_si128(y_dst1 as *mut __m128i, y1);

            let ra = avg2x2(r0l, r0h, r1l, r1h);
            let ga = avg2x2(g0l, g0h, g1l, g1h);
            let ba = avg2x2(b0l, b0h, b1l, b1h);

            let u = chroma_from(ra, ga, ba, -38, -74, 112);
            let v = chroma_from(ra, ga, ba, 112, -94, -18);
            // [U0..U7, V0..V7] as bytes (values in [16,240], pack is exact)
            pack_u32x16_to_u8(u, v)
        }
    }

    /// AVX2-accelerated BGRA→NV12 conversion. Processes 16 pixels at a time
    /// (two rows of 16); scalar fallback for trailing columns.
    ///
    /// # Safety
    /// Caller must ensure AVX2 is available and the slices cover the
    /// described geometry (height even, src rows of `src_stride` bytes with at
    /// least `width*4` valid bytes each, dst planes with `dst_pitch` row
    /// stride and `width` valid columns).
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn bgra_to_nv12_avx2(
        bgra: &[u8],
        width: usize,
        height: usize,
        src_stride: usize,
        dst_pitch: usize,
        y_plane: &mut [u8],
        uv_plane: &mut [u8],
    ) {
        unsafe {
            let blocks = width / 16;

            for row_pair in 0..(height / 2) {
                let y0 = row_pair * 2;
                let y1 = y0 + 1;
                let src0 = y0 * src_stride;
                let src1 = y1 * src_stride;
                let yd0 = y0 * dst_pitch;
                let yd1 = y1 * dst_pitch;
                let uvd = row_pair * dst_pitch;

                let mut col = 0usize;
                for _ in 0..blocks {
                    let uv = block16x2(
                        bgra.as_ptr().add(src0 + col * 4),
                        bgra.as_ptr().add(src1 + col * 4),
                        y_plane.as_mut_ptr().add(yd0 + col),
                        y_plane.as_mut_ptr().add(yd1 + col),
                    );
                    // Interleave [U0..7 | V0..7] -> [U0,V0,U1,V1,...]
                    let v_half = _mm_unpackhi_epi64(uv, uv);
                    let interleaved = _mm_unpacklo_epi8(uv, v_half);
                    _mm_storeu_si128(
                        uv_plane.as_mut_ptr().add(uvd + col) as *mut __m128i,
                        interleaved,
                    );
                    col += 16;
                }

                if col < width {
                    super::bgra_to_nv12_scalar_cols(
                        bgra, width, src_stride, y_plane, uv_plane, y0, y1, yd0, yd1, uvd, col,
                    );
                }
            }
        }
    }

    /// AVX2-accelerated BGRA→YUV420P conversion (separate U and V planes,
    /// Y stride = width, U/V stride = width/2). Processes 16 pixels at a
    /// time (two rows of 16); scalar fallback for trailing columns.
    ///
    /// # Safety
    /// Caller must ensure AVX2 is available and the slices cover the
    /// described geometry (height even, src rows of `src_stride` bytes with at
    /// least `width*4` valid bytes each).
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn bgra_to_yuv420_avx2(
        bgra: &[u8],
        width: usize,
        height: usize,
        src_stride: usize,
        y_plane: &mut [u8],
        u_plane: &mut [u8],
        v_plane: &mut [u8],
    ) {
        unsafe {
            let half_w = width / 2;
            let blocks = width / 16;

            for row_pair in 0..(height / 2) {
                let y0 = row_pair * 2;
                let y1 = y0 + 1;
                let src0 = y0 * src_stride;
                let src1 = y1 * src_stride;
                let yd0 = y0 * width;
                let yd1 = y1 * width;
                let uv_row = row_pair * half_w;

                let mut col = 0usize;
                for _ in 0..blocks {
                    let uv = block16x2(
                        bgra.as_ptr().add(src0 + col * 4),
                        bgra.as_ptr().add(src1 + col * 4),
                        y_plane.as_mut_ptr().add(yd0 + col),
                        y_plane.as_mut_ptr().add(yd1 + col),
                    );
                    // Planar store: low 8 bytes = U, high 8 bytes = V
                    _mm_storel_epi64(
                        u_plane.as_mut_ptr().add(uv_row + col / 2) as *mut __m128i,
                        uv,
                    );
                    _mm_storel_epi64(
                        v_plane.as_mut_ptr().add(uv_row + col / 2) as *mut __m128i,
                        _mm_unpackhi_epi64(uv, uv),
                    );
                    col += 16;
                }

                if col < width {
                    super::bgra_to_yuv420_scalar_cols(
                        bgra, width, src_stride, y_plane, u_plane, v_plane, y0, y1, uv_row, col,
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: SIMD vs scalar exact-match across awkward geometries
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random fill (LCG, no external deps).
    fn lcg_fill(buf: &mut [u8], mut seed: u64) {
        for b in buf.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (seed >> 33) as u8;
        }
    }

    const WIDTHS: &[usize] = &[2, 16, 18, 254, 1920];
    const HEIGHTS: &[usize] = &[2, 18, 110];
    const STRIDE_EXTRAS: &[usize] = &[0, 64];
    const PITCH_EXTRAS: &[usize] = &[0, 32];

    // `#[target_feature]` fns can't coerce to fn pointers, so the SIMD
    // variant under test is passed as a closure wrapping the unsafe call.
    fn compare_nv12(
        simd: impl Fn(&[u8], usize, usize, usize, usize, &mut [u8], &mut [u8]),
        name: &str,
    ) {
        let mut seed = 1u64;
        for &width in WIDTHS {
            for &height in HEIGHTS {
                for &se in STRIDE_EXTRAS {
                    for &pe in PITCH_EXTRAS {
                        let src_stride = width * 4 + se;
                        let dst_pitch = width + pe;
                        let mut bgra = vec![0u8; height * src_stride];
                        lcg_fill(&mut bgra, seed);
                        seed = seed.wrapping_add(0x9E3779B97F4A7C15);

                        let mut y_ref = vec![0xAAu8; height * dst_pitch];
                        let mut uv_ref = vec![0xAAu8; (height / 2) * dst_pitch];
                        let mut y_simd = y_ref.clone();
                        let mut uv_simd = uv_ref.clone();

                        bgra_to_nv12_scalar(
                            &bgra, width, height, src_stride, dst_pitch, &mut y_ref, &mut uv_ref,
                        );
                        simd(&bgra, width, height, src_stride, dst_pitch, &mut y_simd, &mut uv_simd);

                        assert_eq!(
                            y_ref, y_simd,
                            "{name} NV12 Y mismatch w={width} h={height} stride={src_stride} pitch={dst_pitch}"
                        );
                        assert_eq!(
                            uv_ref, uv_simd,
                            "{name} NV12 UV mismatch w={width} h={height} stride={src_stride} pitch={dst_pitch}"
                        );
                    }
                }
            }
        }
    }

    fn compare_yuv420(
        simd: impl Fn(&[u8], usize, usize, usize, &mut [u8], &mut [u8], &mut [u8]),
        name: &str,
    ) {
        let mut seed = 7u64;
        for &width in WIDTHS {
            for &height in HEIGHTS {
                for &se in STRIDE_EXTRAS {
                    let src_stride = width * 4 + se;
                    let mut bgra = vec![0u8; height * src_stride];
                    lcg_fill(&mut bgra, seed);
                    seed = seed.wrapping_add(0x9E3779B97F4A7C15);

                    let half = (width / 2) * (height / 2);
                    let mut y_ref = vec![0xAAu8; width * height];
                    let mut u_ref = vec![0xAAu8; half];
                    let mut v_ref = vec![0xAAu8; half];
                    let mut y_simd = y_ref.clone();
                    let mut u_simd = u_ref.clone();
                    let mut v_simd = v_ref.clone();

                    bgra_to_yuv420_scalar(
                        &bgra, width, height, src_stride, &mut y_ref, &mut u_ref, &mut v_ref,
                    );
                    simd(&bgra, width, height, src_stride, &mut y_simd, &mut u_simd, &mut v_simd);

                    assert_eq!(
                        y_ref, y_simd,
                        "{name} YUV420 Y mismatch w={width} h={height} stride={src_stride}"
                    );
                    assert_eq!(
                        u_ref, u_simd,
                        "{name} YUV420 U mismatch w={width} h={height} stride={src_stride}"
                    );
                    assert_eq!(
                        v_ref, v_simd,
                        "{name} YUV420 V mismatch w={width} h={height} stride={src_stride}"
                    );
                }
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    mod aarch64_tests {
        use super::*;

        #[test]
        fn neon_detected() {
            assert!(
                is_aarch64_feature_detected!("neon"),
                "NEON not detected; SIMD path would never run"
            );
        }

        #[test]
        fn neon_nv12_matches_scalar() {
            // SAFETY: NEON availability asserted by `neon_detected`; geometry
            // invariants upheld by the comparison harness.
            compare_nv12(
                |b, w, h, ss, dp, y, uv| unsafe { neon::bgra_to_nv12_neon(b, w, h, ss, dp, y, uv) },
                "neon",
            );
        }

        #[test]
        fn neon_yuv420_matches_scalar() {
            // SAFETY: as above.
            compare_yuv420(
                |b, w, h, ss, y, u, v| unsafe { neon::bgra_to_yuv420_neon(b, w, h, ss, y, u, v) },
                "neon",
            );
        }
    }

    #[cfg(target_arch = "x86_64")]
    mod x86_64_tests {
        use super::*;

        /// Canary: tells us whether the runtime-dispatch path would actually
        /// pick AVX2 in this environment (under qemu `-cpu max` it should).
        /// The comparison tests below call the AVX2 fns directly either way.
        #[test]
        fn avx2_detected() {
            assert!(
                std::arch::is_x86_feature_detected!("avx2"),
                "AVX2 not detected; runtime dispatch would fall back to scalar \
                 (the direct-call AVX2 tests still verify correctness)"
            );
        }

        #[test]
        fn avx2_nv12_matches_scalar() {
            // SAFETY: called directly (not via runtime dispatch) so this also
            // verifies correctness under qemu even if cpuid hides AVX2;
            // qemu `-cpu max` emulates the instructions either way.
            compare_nv12(
                |b, w, h, ss, dp, y, uv| unsafe { x86::bgra_to_nv12_avx2(b, w, h, ss, dp, y, uv) },
                "avx2",
            );
        }

        #[test]
        fn avx2_yuv420_matches_scalar() {
            // SAFETY: as above.
            compare_yuv420(
                |b, w, h, ss, y, u, v| unsafe { x86::bgra_to_yuv420_avx2(b, w, h, ss, y, u, v) },
                "avx2",
            );
        }
    }

    /// The public multithreaded entry points must also match scalar exactly
    /// (covers the row-pair splitting logic on whatever arch this runs on).
    #[test]
    fn public_entry_points_match_scalar() {
        let (width, height) = (1280, 720);
        let src_stride = width * 4 + 64;
        let dst_pitch = width + 32;
        let mut bgra = vec![0u8; height * src_stride];
        lcg_fill(&mut bgra, 42);

        // NV12 pitched
        let mut y_ref = vec![0u8; height * dst_pitch];
        let mut uv_ref = vec![0u8; (height / 2) * dst_pitch];
        let mut y_out = y_ref.clone();
        let mut uv_out = uv_ref.clone();
        bgra_to_nv12_scalar(&bgra, width, height, src_stride, dst_pitch, &mut y_ref, &mut uv_ref);
        bgra_to_nv12_pitched(&bgra, width, height, src_stride, dst_pitch, &mut y_out, &mut uv_out);
        assert_eq!(y_ref, y_out, "bgra_to_nv12_pitched Y mismatch");
        assert_eq!(uv_ref, uv_out, "bgra_to_nv12_pitched UV mismatch");

        // YUV420
        let half = (width / 2) * (height / 2);
        let mut y_ref = vec![0u8; width * height];
        let mut u_ref = vec![0u8; half];
        let mut v_ref = vec![0u8; half];
        let mut y_out = y_ref.clone();
        let mut u_out = u_ref.clone();
        let mut v_out = v_ref.clone();
        bgra_to_yuv420_scalar(&bgra, width, height, src_stride, &mut y_ref, &mut u_ref, &mut v_ref);
        bgra_to_yuv420(&bgra, width, height, src_stride, &mut y_out, &mut u_out, &mut v_out);
        assert_eq!(y_ref, y_out, "bgra_to_yuv420 Y mismatch");
        assert_eq!(u_ref, u_out, "bgra_to_yuv420 U mismatch");
        assert_eq!(v_ref, v_out, "bgra_to_yuv420 V mismatch");
    }

    /// Rough single-threaded throughput numbers at 1920x1080.
    /// Run with: cargo test -p ddisplay-color --release -- --ignored bench --nocapture
    #[test]
    #[ignore]
    fn bench_1080p() {
        let (width, height) = (1920usize, 1080usize);
        let src_stride = width * 4;
        let mut bgra = vec![0u8; height * src_stride];
        lcg_fill(&mut bgra, 1234);

        let mut y = vec![0u8; width * height];
        let mut uv = vec![0u8; width * height / 2];
        let mut u = vec![0u8; width * height / 4];
        let mut v = vec![0u8; width * height / 4];

        let iters = 30;
        let time = |name: &str, f: &mut dyn FnMut()| {
            f(); // warm up
            let t0 = std::time::Instant::now();
            for _ in 0..iters {
                f();
            }
            let ns = t0.elapsed().as_nanos() / iters as u128;
            println!("{name}: {ns} ns/frame ({:.2} ms)", ns as f64 / 1e6);
        };

        time("nv12 scalar      ", &mut || {
            bgra_to_nv12_scalar(&bgra, width, height, src_stride, width, &mut y, &mut uv)
        });
        time("nv12 simd (chunk)", &mut || {
            bgra_to_nv12_chunk(&bgra, width, height, src_stride, width, &mut y, &mut uv)
        });
        time("yuv420 scalar      ", &mut || {
            bgra_to_yuv420_scalar(&bgra, width, height, src_stride, &mut y, &mut u, &mut v)
        });
        time("yuv420 simd (chunk)", &mut || {
            bgra_to_yuv420_chunk(&bgra, width, height, src_stride, &mut y, &mut u, &mut v)
        });
    }
}
