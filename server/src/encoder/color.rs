/// Fast BGRA to YUV420P color conversion.
///
/// BT.601 coefficients (matching OpenH264's internal expectation):
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

            // Y for each pixel
            y_plane[y0 * width + x0] = rgb_to_y(r00, g00, b00);
            y_plane[y0 * width + x1] = rgb_to_y(r10, g10, b10);
            y_plane[y1 * width + x0] = rgb_to_y(r01, g01, b01);
            y_plane[y1 * width + x1] = rgb_to_y(r11, g11, b11);

            // Average RGB for the 2x2 block for chroma
            let r_avg = (r00 + r10 + r01 + r11 + 2) >> 2;
            let g_avg = (g00 + g10 + g01 + g11 + 2) >> 2;
            let b_avg = (b00 + b10 + b01 + b11 + 2) >> 2;

            u_plane[uv_row + col_pair] = ((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8).wrapping_add(128) as u8;
            v_plane[uv_row + col_pair] = ((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8).wrapping_add(128) as u8;
        }
    }
}
