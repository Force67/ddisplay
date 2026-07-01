//! One monitor head's encoder. Each head is encoded as its own stream so a
//! second monitor is independent (its own resolution, bitrate and keyframes)
//! rather than a crop of one wide stream.

use anyhow::Result;

use crate::encoder::{EncodedPacket, Encoder};
use crate::protocol::MonitorRect;

pub struct HeadStream {
    pub rect: MonitorRect,
    pub encoder: Box<dyn Encoder + Send>,
    scratch: Vec<u8>,
}

impl HeadStream {
    pub fn new(rect: MonitorRect, encoder: Box<dyn Encoder + Send>) -> Self {
        Self { rect, encoder, scratch: Vec::new() }
    }

    /// Encode this head's region of the captured BGRA frame. A head that covers
    /// the whole frame is encoded in place (the zero-copy fast path); a smaller
    /// head is copied into a tight buffer first, since NVENC uploads by stride
    /// and would overread an offset sub-rect of the full frame.
    pub fn encode(
        &mut self,
        data: &[u8],
        frame_w: u32,
        frame_h: u32,
        frame_stride: u32,
        force_kf: bool,
    ) -> Result<EncodedPacket> {
        let r = &self.rect;
        if r.x == 0 && r.y == 0 && r.width == frame_w && r.height == frame_h {
            return self.encoder.encode(data, frame_w, frame_h, frame_stride, force_kf);
        }

        let bpp = 4usize;
        let tight = r.width as usize * bpp;
        self.scratch.resize(tight * r.height as usize, 0);
        let src_stride = frame_stride as usize;
        let x_off = r.x as usize * bpp;
        for row in 0..r.height as usize {
            let so = (r.y as usize + row) * src_stride + x_off;
            self.scratch[row * tight..row * tight + tight].copy_from_slice(&data[so..so + tight]);
        }
        self.encoder.encode(&self.scratch, r.width, r.height, tight as u32, force_kf)
    }
}
