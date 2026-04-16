//! NVENC hardware H.264/AV1 encoder using a C wrapper for correct struct layouts.

use anyhow::{bail, Result};
use std::ffi::c_void;
use std::ptr;

use super::color;
use super::{EncodedPacket, Encoder as EncoderTrait};

pub const CODEC_H264: u32 = 0;
pub const CODEC_AV1: u32 = 1;

#[repr(C)]
struct NvencFrame {
    data: *const u8,
    size: u32,
    is_keyframe: i32,
    pts: u64,
}

unsafe extern "C" {
    fn nvenc_probe_codecs() -> i32;
    fn nvenc_create(width: u32, height: u32, fps: u32, bitrate: u32, codec: u32) -> *mut c_void;
    fn nvenc_encode(
        ctx: *mut c_void,
        nv12_data: *const u8,
        force_keyframe: i32,
        out: *mut NvencFrame,
    ) -> i32;
    fn nvenc_unlock_bitstream(ctx: *mut c_void);
    fn nvenc_destroy(ctx: *mut c_void);
}

/// Probe which codecs NVENC supports. Returns (h264, av1).
pub fn probe_codecs() -> (bool, bool) {
    let bits = unsafe { nvenc_probe_codecs() };
    let h264 = bits & 1 != 0;
    let av1 = bits & 2 != 0;
    if h264 || av1 {
        tracing::info!(
            "NVENC probe: H.264={}, AV1={}",
            if h264 { "yes" } else { "no" },
            if av1 { "yes" } else { "no" },
        );
    } else {
        tracing::debug!("NVENC probe: not available");
    }
    (h264, av1)
}

pub fn is_nvenc_available() -> bool {
    let (h264, av1) = probe_codecs();
    h264 || av1
}

pub struct NvencEncoder {
    ctx: *mut c_void,
    width: u32,
    height: u32,
    codec: u32,
    nv12_buf: Vec<u8>,
    y_len: usize,
}

unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, codec: u32) -> Result<Self> {
        let codec_name = if codec == CODEC_AV1 { "AV1" } else { "H.264" };

        let ctx = unsafe { nvenc_create(width, height, fps, bitrate, codec) };
        if ctx.is_null() {
            bail!("NVENC {} encoder creation failed", codec_name);
        }

        let y_len = (width as usize) * (height as usize);
        let uv_len = (width as usize) * (height as usize / 2);
        let nv12_buf = vec![0u8; y_len + uv_len];

        tracing::info!(
            "NVENC encoder initialized: {}x{} @ {} fps, {} bps ({} P1 ultra-low-latency)",
            width, height, fps, bitrate, codec_name,
        );

        Ok(Self { ctx, width, height, codec, nv12_buf, y_len })
    }

    pub fn codec_name(&self) -> &'static str {
        if self.codec == CODEC_AV1 { "av1" } else { "h264" }
    }
}

impl EncoderTrait for NvencEncoder {
    fn encode(
        &mut self,
        frame_data: &[u8],
        width: u32,
        height: u32,
        stride: u32,
        force_keyframe: bool,
    ) -> Result<EncodedPacket> {
        if width != self.width || height != self.height {
            bail!(
                "Frame dimensions {}x{} != encoder {}x{}",
                width, height, self.width, self.height,
            );
        }

        let w = width as usize;
        let h = height as usize;

        let (y_plane, uv_plane) = self.nv12_buf.split_at_mut(self.y_len);
        color::bgra_to_nv12_pitched(frame_data, w, h, stride as usize, w, y_plane, uv_plane);

        let mut frame = NvencFrame {
            data: ptr::null(),
            size: 0,
            is_keyframe: 0,
            pts: 0,
        };

        let rc = unsafe {
            nvenc_encode(self.ctx, self.nv12_buf.as_ptr(), force_keyframe as i32, &mut frame)
        };

        if rc != 0 {
            bail!("NVENC encode failed with code {}", rc);
        }

        let data = unsafe { std::slice::from_raw_parts(frame.data, frame.size as usize) }.to_vec();
        let keyframe = frame.is_keyframe != 0;
        let pts = frame.pts;

        unsafe { nvenc_unlock_bitstream(self.ctx) };

        Ok(EncodedPacket { data, keyframe, pts })
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        Ok(Vec::new())
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe { nvenc_destroy(self.ctx) };
        tracing::debug!("NVENC encoder destroyed");
    }
}
