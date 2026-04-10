/// H.264 decoder using OpenH264.
///
/// Uses integer-only BT.601 YUV→RGB for fast conversion.

use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

pub struct DecodedFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct H264Decoder {
    decoder: Decoder,
    /// Reusable RGBA buffer (avoids allocation per frame)
    rgba_buf: Vec<u8>,
}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::new()
            .context("Failed to create OpenH264 decoder")?;
        Ok(Self { decoder, rgba_buf: Vec::new() })
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        let maybe_yuv = self.decoder.decode(data)
            .map_err(|e| anyhow::anyhow!("OpenH264 decode error: {:?}", e))?;

        let yuv = match maybe_yuv {
            Some(yuv) => yuv,
            None => return Ok(None),
        };

        let (w, h) = yuv.dimensions();
        if w == 0 || h == 0 {
            return Ok(None);
        }

        let pixel_count = w * h;
        let rgba_size = pixel_count * 4;

        // Reuse buffer
        self.rgba_buf.resize(rgba_size, 255);

        let ys = yuv.strides().0;
        let us = yuv.strides().1;
        let vs = yuv.strides().2;
        let y_data = yuv.y();
        let u_data = yuv.u();
        let v_data = yuv.v();
        let rgba = &mut self.rgba_buf;

        // Integer BT.601 YUV→RGB (no floats, ~3x faster than f32 version)
        //   R = clamp((298*(Y-16) + 409*(V-128) + 128) >> 8)
        //   G = clamp((298*(Y-16) - 100*(U-128) - 208*(V-128) + 128) >> 8)
        //   B = clamp((298*(Y-16) + 516*(U-128) + 128) >> 8)
        for row in 0..h {
            let y_row = row * ys;
            let uv_row = (row / 2) * us;
            let uv_row_v = (row / 2) * vs;
            let dst_row = row * w * 4;

            for col in 0..w {
                let y_val = y_data[y_row + col] as i32;
                let u_val = u_data[uv_row + col / 2] as i32;
                let v_val = v_data[uv_row_v + col / 2] as i32;

                let c = 298 * (y_val - 16);
                let d = u_val - 128;
                let e = v_val - 128;

                let r = (c + 409 * e + 128) >> 8;
                let g = (c - 100 * d - 208 * e + 128) >> 8;
                let b = (c + 516 * d + 128) >> 8;

                let idx = dst_row + col * 4;
                rgba[idx]     = r.clamp(0, 255) as u8;
                rgba[idx + 1] = g.clamp(0, 255) as u8;
                rgba[idx + 2] = b.clamp(0, 255) as u8;
                // rgba[idx + 3] already 255 from resize
            }
        }

        Ok(Some(DecodedFrame {
            rgba: self.rgba_buf.clone(),
            width: w as u32,
            height: h as u32,
        }))
    }
}
