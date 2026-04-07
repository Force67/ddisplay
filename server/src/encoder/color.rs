#[cfg(target_arch = "aarch64")]
use std::arch::is_aarch64_feature_detected;

/// Fast BGRA to YUV/NV12 color conversion.
///
/// BT.601 coefficients (matching OpenH264 and NVENC expectations):
///   Y  =  (( 66*R + 129*G +  25*B + 128) >> 8) + 16
///   Cb =  ((-38*R -  74*G + 112*B + 128) >> 8) + 128
///   Cr =  ((112*R -  94*G -  18*B + 128) >> 8) + 128

#[inline(always)]
fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
    ((66 * r + 129 * g + 25 * b + 128) >> 8).wrapping_add(16) as u8
}

/// Convert BGRA frame to YUV420P planes in-place.
pub fn bgra_to_yuv420(
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
        let src_row0 = y0 * stride;
        let src_row1 = y1 * stride;
        let uv_row = row_pair * half_w;

        for col_pair in 0..half_w {
            let x0 = col_pair * 2;
            let x1 = x0 + 1;

            let i00 = src_row0 + x0 * 4;
            let i10 = src_row0 + x1 * 4;
            let i01 = src_row1 + x0 * 4;
            let i11 = src_row1 + x1 * 4;

            let (b00, g00, r00) = (bgra[i00] as i32, bgra[i00+1] as i32, bgra[i00+2] as i32);
            let (b10, g10, r10) = (bgra[i10] as i32, bgra[i10+1] as i32, bgra[i10+2] as i32);
            let (b01, g01, r01) = (bgra[i01] as i32, bgra[i01+1] as i32, bgra[i01+2] as i32);
            let (b11, g11, r11) = (bgra[i11] as i32, bgra[i11+1] as i32, bgra[i11+2] as i32);

            y_plane[y0 * width + x0] = rgb_to_y(r00, g00, b00);
            y_plane[y0 * width + x1] = rgb_to_y(r10, g10, b10);
            y_plane[y1 * width + x0] = rgb_to_y(r01, g01, b01);
            y_plane[y1 * width + x1] = rgb_to_y(r11, g11, b11);

            let r_avg = (r00 + r10 + r01 + r11 + 2) >> 2;
            let g_avg = (g00 + g10 + g01 + g11 + 2) >> 2;
            let b_avg = (b00 + b10 + b01 + b11 + 2) >> 2;

            u_plane[uv_row + col_pair] = ((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8).wrapping_add(128) as u8;
            v_plane[uv_row + col_pair] = ((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8).wrapping_add(128) as u8;
        }
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

/// Convert BGRA to NV12 with explicit output pitch (stride).
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
    #[cfg(target_arch = "aarch64")]
    {
        if is_aarch64_feature_detected!("neon") && width >= 16 {
            // SAFETY: NEON detected at runtime, width >= 16 for full vector processing
            unsafe {
                bgra_to_nv12_neon(bgra, width, height, src_stride, dst_pitch, y_plane, uv_plane);
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
    let half_w = width / 2;

    for row_pair in 0..(height / 2) {
        let y0 = row_pair * 2;
        let y1 = y0 + 1;
        let src_row0 = y0 * src_stride;
        let src_row1 = y1 * src_stride;
        let y_dst0 = y0 * dst_pitch;
        let y_dst1 = y1 * dst_pitch;
        let uv_dst = row_pair * dst_pitch;

        for col_pair in 0..half_w {
            let x0 = col_pair * 2;
            let x1 = x0 + 1;

            let i00 = src_row0 + x0 * 4;
            let i10 = src_row0 + x1 * 4;
            let i01 = src_row1 + x0 * 4;
            let i11 = src_row1 + x1 * 4;

            let (b00, g00, r00) = (bgra[i00] as i32, bgra[i00+1] as i32, bgra[i00+2] as i32);
            let (b10, g10, r10) = (bgra[i10] as i32, bgra[i10+1] as i32, bgra[i10+2] as i32);
            let (b01, g01, r01) = (bgra[i01] as i32, bgra[i01+1] as i32, bgra[i01+2] as i32);
            let (b11, g11, r11) = (bgra[i11] as i32, bgra[i11+1] as i32, bgra[i11+2] as i32);

            y_plane[y_dst0 + x0] = rgb_to_y(r00, g00, b00);
            y_plane[y_dst0 + x1] = rgb_to_y(r10, g10, b10);
            y_plane[y_dst1 + x0] = rgb_to_y(r01, g01, b01);
            y_plane[y_dst1 + x1] = rgb_to_y(r11, g11, b11);

            let r_avg = (r00 + r10 + r01 + r11 + 2) >> 2;
            let g_avg = (g00 + g10 + g01 + g11 + 2) >> 2;
            let b_avg = (b00 + b10 + b01 + b11 + 2) >> 2;

            let uv_idx = uv_dst + col_pair * 2;
            uv_plane[uv_idx] = ((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8).wrapping_add(128) as u8;
            uv_plane[uv_idx + 1] = ((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8).wrapping_add(128) as u8;
        }
    }
}

// ---------------------------------------------------------------------------
// aarch64 NEON implementation
// ---------------------------------------------------------------------------

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

/// NEON-accelerated BGRA→NV12 conversion.
///
/// Processes 16 pixels at a time (two rows of 8) using 128-bit NEON vectors.
/// Falls through to scalar for trailing pixels when width is not a multiple of 16.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn bgra_to_nv12_neon(
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

    let chunks = width / 8;

    for row_pair in 0..(height / 2) {
        let y0 = row_pair * 2;
        let y1 = y0 + 1;
        let src0 = y0 * src_stride;
        let src1 = y1 * src_stride;
        let yd0 = y0 * dst_pitch;
        let yd1 = y1 * dst_pitch;
        let uvd = row_pair * dst_pitch;

        let mut col = 0usize;
        for _chunk in 0..(chunks / 2) {
            let bgra_ptr0 = bgra.as_ptr().add(src0 + col * 4);
            let bgra_ptr1 = bgra.as_ptr().add(src1 + col * 4);

            // Load 16 BGRA pixels = 64 bytes, deinterleaved into B,G,R,A channels
            let px0 = vld4q_u8(bgra_ptr0);
            let px1 = vld4q_u8(bgra_ptr1);

            let (b0, g0, r0) = (px0.0, px0.1, px0.2);
            let (b1, g1, r1) = (px1.0, px1.1, px1.2);

            // Compute Y for all 16 pixels in each row
            let y0_val = compute_y_neon(r0, g0, b0, vec_16, vec_128_16);
            let y1_val = compute_y_neon(r1, g1, b1, vec_16, vec_128_16);

            vst1q_u8(y_plane.as_mut_ptr().add(yd0 + col), y0_val);
            vst1q_u8(y_plane.as_mut_ptr().add(yd1 + col), y1_val);

            // Chroma: average the 2x2 blocks
            // Vertical halving add, then horizontal pairwise add + shift to get true average
            let r_avg = vhaddq_u8(r0, r1);
            let g_avg = vhaddq_u8(g0, g1);
            let b_avg = vhaddq_u8(b0, b1);

            // vpaddlq_u8 sums adjacent pairs → u16 (max 510).
            // Shift right by 1 to get the true 2x2 average (max 255),
            // keeping values within s16 range for coefficient multiply.
            let r_h = vshrq_n_u16(vpaddlq_u8(r_avg), 1);
            let g_h = vshrq_n_u16(vpaddlq_u8(g_avg), 1);
            let b_h = vshrq_n_u16(vpaddlq_u8(b_avg), 1);

            // Now r_h/g_h/b_h are true averages (0-255), same as scalar path
            let r_s = vreinterpretq_s16_u16(r_h);
            let g_s = vreinterpretq_s16_u16(g_h);
            let b_s = vreinterpretq_s16_u16(b_h);

            // U = (-38*R - 74*G + 112*B + 128) >> 8 + 128
            // Max intermediate: 112*255 = 28560, fits s16 ✓
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

            // Saturating narrow to u8
            let u_u8 = vqmovun_s16(u_val);
            let v_u8 = vqmovun_s16(v_val);

            // Interleave U,V for NV12: [U0,V0,U1,V1,...] = 16 bytes
            let uv_lo = vzip1_u8(u_u8, v_u8);
            let uv_hi = vzip2_u8(u_u8, v_u8);
            vst1q_u8(uv_plane.as_mut_ptr().add(uvd + col), vcombine_u8(uv_lo, uv_hi));

            col += 16;
        }

        // Handle remaining columns with scalar
        if col < width {
            bgra_to_nv12_scalar_row(
                bgra, width, src_stride, dst_pitch,
                y_plane, uv_plane, y0, y1, yd0, yd1, uvd, col,
            );
        }
    }
    } // end unsafe
}

/// Compute Y = (66*R + 129*G + 25*B + 128) >> 8 + 16 for 16 pixels.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn compute_y_neon(
    r: uint8x16_t,
    g: uint8x16_t,
    b: uint8x16_t,
    vec_16: uint16x8_t,
    vec_128: uint16x8_t,
) -> uint8x16_t {
    unsafe {
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
}

/// Scalar fallback for remaining columns in a row pair.
#[cfg(target_arch = "aarch64")]
fn bgra_to_nv12_scalar_row(
    bgra: &[u8],
    width: usize,
    src_stride: usize,
    dst_pitch: usize,
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
    let half_start = start_col / 2;
    let half_w = width / 2;

    for col_pair in half_start..half_w {
        let x0 = col_pair * 2;
        let x1 = x0 + 1;

        let i00 = src_row0 + x0 * 4;
        let i10 = src_row0 + x1 * 4;
        let i01 = src_row1 + x0 * 4;
        let i11 = src_row1 + x1 * 4;

        let (b00, g00, r00) = (bgra[i00] as i32, bgra[i00+1] as i32, bgra[i00+2] as i32);
        let (b10, g10, r10) = (bgra[i10] as i32, bgra[i10+1] as i32, bgra[i10+2] as i32);
        let (b01, g01, r01) = (bgra[i01] as i32, bgra[i01+1] as i32, bgra[i01+2] as i32);
        let (b11, g11, r11) = (bgra[i11] as i32, bgra[i11+1] as i32, bgra[i11+2] as i32);

        y_plane[yd0 + x0] = rgb_to_y(r00, g00, b00);
        y_plane[yd0 + x1] = rgb_to_y(r10, g10, b10);
        y_plane[yd1 + x0] = rgb_to_y(r01, g01, b01);
        y_plane[yd1 + x1] = rgb_to_y(r11, g11, b11);

        let r_avg = (r00 + r10 + r01 + r11 + 2) >> 2;
        let g_avg = (g00 + g10 + g01 + g11 + 2) >> 2;
        let b_avg = (b00 + b10 + b01 + b11 + 2) >> 2;

        let uv_idx = uvd + col_pair * 2;
        uv_plane[uv_idx] = ((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8).wrapping_add(128) as u8;
        uv_plane[uv_idx + 1] = ((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8).wrapping_add(128) as u8;
    }
}
