//! Vulkan Video hardware H.264 decode (Linux).
//!
//! The renderer's wgpu device is created through gpu-video (which enables the
//! VK_KHR_video_decode_* device extensions), so decode and presentation share
//! one VkDevice. Decoded frames come back as NV12 wgpu textures at the SPS
//! visible size and are sampled directly by the renderer — the video never
//! leaves GPU memory and there is no per-frame CPU readback or upload.

use std::sync::Arc;

use anyhow::{Context, Result};
use gpu_video::parameters::{DecoderParameters, VulkanDeviceDescriptor};
use gpu_video::{EncodedInputChunk, VulkanDevice, VulkanInstance, WgpuTexturesDecoder};

use crate::decoder::{DecodedFrame, PlaneStorage};

/// Vulkan instance + device with video-decode queues, shared between one
/// window's renderer and its decode thread. Built by the renderer (adapter
/// selection needs the window surface); decode threads clone the Arc and
/// create their own [`VkHwDecoder`] from it.
pub struct VkVideoContext {
    /// Never read, but load-bearing: VulkanDevice only borrows the instance
    /// at creation, and dropping it would unload the Vulkan library out from
    /// under the device.
    _instance: Arc<VulkanInstance>,
    pub device: Arc<VulkanDevice>,
}

impl VkVideoContext {
    /// Whether DDISPLAY_NO_VKVIDEO=1 disables the Vulkan Video path.
    pub fn disabled() -> bool {
        std::env::var("DDISPLAY_NO_VKVIDEO")
            .map(|v| v == "1")
            .unwrap_or(false)
    }

    /// True when the device advertises any H.264 decode profile.
    pub fn supports_h264(&self) -> bool {
        h264_profile_supported(&self.device.decode_capabilities())
    }
}

fn h264_profile_supported(caps: &gpu_video::capabilities::DecodeCapabilities) -> bool {
    caps.h264.as_ref().map_or(false, |h| {
        h.baseline_profile.is_some() || h.main_profile.is_some() || h.high_profile.is_some()
    })
}

/// Entry point for renderer init: Vulkan loader + instance, or a clean error
/// when Vulkan is unavailable on this system.
pub fn create_instance() -> Result<Arc<VulkanInstance>> {
    VulkanInstance::new().map_err(|e| anyhow::anyhow!("vulkan init failed: {e}"))
}

/// Pick a video-decode-capable adapter that can present to `surface` and
/// build the shared device. gpu-video's own create_adapter takes the first
/// match, so rank by device type to prefer a discrete GPU (matching the
/// HighPerformance preference of the standard wgpu path).
pub fn create_context(
    surface: &wgpu::Surface<'_>,
    instance: Arc<VulkanInstance>,
) -> Result<Arc<VkVideoContext>> {
    use gpu_video::capabilities::VulkanDeviceType;
    let rank = |t: VulkanDeviceType| match t {
        VulkanDeviceType::DISCRETE_GPU => 0,
        VulkanDeviceType::INTEGRATED_GPU => 1,
        VulkanDeviceType::VIRTUAL_GPU => 2,
        _ => 3,
    };

    let adapter = instance
        .iter_adapters()
        .map_err(|e| anyhow::anyhow!("adapter enumeration failed: {e}"))?
        .filter(|a| {
            h264_profile_supported(&a.info().decode_capabilities) && a.supports_surface(surface)
        })
        .min_by_key(|a| rank(a.info().device_type))
        .context("no H.264-decode-capable adapter for this surface")?;
    eprintln!("[vk-video] Adapter: {}", adapter.info().name);

    let device = adapter
        .create_device(&VulkanDeviceDescriptor::default())
        .map_err(|e| anyhow::anyhow!("device creation failed: {e}"))?;

    Ok(Arc::new(VkVideoContext {
        _instance: instance,
        device,
    }))
}

/// Wraps gpu-video's texture decoder with the same contract as the other
/// decoders: on error the bitstream is broken until the next IDR, so a
/// keyframe is requested via take_needs_keyframe() and the decoder recovers
/// when it arrives. Errors that persist across IDR resyncs (e.g. a profile
/// the driver can't decode) mark the decoder dead so VideoDecoder can drop
/// to software.
pub struct VkHwDecoder {
    decoder: WgpuTexturesDecoder,
    pub needs_keyframe: bool,
    consecutive_errors: u32,
    frames_out: u64,
}

impl VkHwDecoder {
    pub fn new(ctx: Arc<VkVideoContext>) -> Result<Self> {
        if !ctx.supports_h264() {
            anyhow::bail!("adapter has no H.264 decode capability");
        }
        let decoder = ctx
            .device
            .create_wgpu_textures_decoder_h264(DecoderParameters::default())
            .map_err(|e| anyhow::anyhow!("decoder creation failed: {e}"))?;
        eprintln!("[vk-video] hardware H.264 decoder ready");
        Ok(Self {
            decoder,
            needs_keyframe: false,
            consecutive_errors: 0,
            frames_out: 0,
        })
    }

    /// A decoder still failing after several IDR resyncs will not recover;
    /// the caller should fall back to software.
    pub fn is_dead(&self) -> bool {
        self.consecutive_errors >= 3
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        if data.is_empty() {
            return Ok(None);
        }

        // Each server message is one complete access unit, but the parser
        // holds an AU until the next one starts and the frame sorter buffers
        // for reorder — flush() drains both, so every frame comes out the
        // same call it went in. Display latency: zero frames.
        let mut frames = match self.decoder.decode(EncodedInputChunk { data, pts: None }) {
            Ok(frames) => frames,
            Err(e) => return Err(self.on_error(e)),
        };
        match self.decoder.flush() {
            Ok(mut flushed) => frames.append(&mut flushed),
            Err(e) => {
                if frames.is_empty() {
                    return Err(self.on_error(e));
                }
                // Keep what decode() already produced; still resync on an IDR.
                self.consecutive_errors += 1;
                self.needs_keyframe = true;
                eprintln!("[vk-video] flush error (keeping decoded frames): {e}");
            }
        }

        // Keep only the newest frame, like the other decoders. Each output is
        // an independent GPU texture; skipped ones simply drop here.
        let mut newest = None;
        for frame in frames {
            let texture = frame.data;
            let (w, h) = (texture.width(), texture.height());
            self.frames_out += 1;
            if self.frames_out <= 3 {
                eprintln!(
                    "[vk-video] frame #{}: {}x{} NV12 texture",
                    self.frames_out, w, h
                );
            }
            newest = Some(DecodedFrame {
                storage: PlaneStorage::VkTexture(texture),
                width: w,
                height: h,
            });
        }
        if newest.is_some() {
            self.consecutive_errors = 0;
        }
        Ok(newest)
    }

    /// Decode errors leave the DPB out of sync until an IDR arrives (Strict
    /// missed-frame handling); ask for one and count strikes toward is_dead.
    fn on_error(&mut self, e: gpu_video::DecoderError) -> anyhow::Error {
        self.consecutive_errors += 1;
        self.needs_keyframe = true;
        anyhow::anyhow!("vulkan decode error: {e}")
    }
}
