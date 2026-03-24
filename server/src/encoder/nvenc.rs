/// High-level NVENC H.264 encoder backed by `libnvidia-encode.so`.
///
/// Loads the NVIDIA encoder library at runtime, opens an encode session on
/// the supplied CUDA context, and provides a simple frame-in / packet-out
/// interface through the [`Encoder`] trait.

use std::ffi::c_void;
use std::ptr;

use anyhow::{bail, Context, Result};
use libloading::Library;
use tracing;

use crate::cuda::CudaContext;
use super::{EncodedPacket, Encoder};
use super::nvenc_sys::*;

// ---------------------------------------------------------------------------
// Helper: check an NVENCSTATUS and convert to anyhow::Error
// ---------------------------------------------------------------------------

fn check(status: NVENCSTATUS, context: &str) -> Result<()> {
    if status == NV_ENC_SUCCESS {
        Ok(())
    } else {
        bail!(
            "NVENC error in {}: {} ({})",
            context,
            nvenc_status_name(status),
            status,
        )
    }
}

// ---------------------------------------------------------------------------
// NvencEncoder
// ---------------------------------------------------------------------------

/// GPU-accelerated H.264 encoder using the NVIDIA Video Encoder (NVENC) API.
///
/// The encoder is configured for low-latency CBR streaming with baseline
/// profile, making it suitable for real-time remote display.
pub struct NvencEncoder {
    /// Keep the shared library alive for the lifetime of the encoder.
    _lib: Library,

    /// NVENC dispatch table (function pointers).
    fn_list: NvEncFunctionList,

    /// Opaque encoder session handle.
    encoder: *mut c_void,

    /// NVENC-managed input buffer handle.
    input_buffer: *mut c_void,

    /// NVENC-managed output (bitstream) buffer handle.
    output_buffer: *mut c_void,

    /// Configured frame width in pixels.
    width: u32,

    /// Configured frame height in pixels.
    height: u32,

    /// Monotonically increasing presentation timestamp counter.
    pts_counter: u64,
}

// SAFETY: The NVENC API is thread-safe when each encoder session is only used
// from one thread at a time, which Rust's `&mut self` enforces.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    /// Path to the NVIDIA encoder shared library.
    const LIB_PATH: &str = "/usr/lib/aarch64-linux-gnu/libnvidia-encode.so";

    /// Create a new NVENC H.264 encoder.
    ///
    /// # Arguments
    /// * `cuda_ctx` — An initialised CUDA context (device must have NVENC support).
    /// * `width` — Frame width in pixels (must be > 0).
    /// * `height` — Frame height in pixels (must be > 0).
    /// * `fps` — Target frame rate (frames per second).
    /// * `bitrate` — Target average bitrate in bits per second.
    pub fn new(
        cuda_ctx: &CudaContext,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("Encoder dimensions must be non-zero (got {}x{})", width, height);
        }

        unsafe {
            // ------------------------------------------------------------------
            // 1. Load the shared library
            // ------------------------------------------------------------------
            let lib = Library::new(Self::LIB_PATH)
                .with_context(|| format!("Failed to load {}", Self::LIB_PATH))?;

            // ------------------------------------------------------------------
            // 2. Resolve NvEncodeAPICreateInstance and populate function table
            // ------------------------------------------------------------------
            let create_instance: libloading::Symbol<NvEncodeAPICreateInstanceFn> = lib
                .get(b"NvEncodeAPICreateInstance\0")
                .context("Failed to find NvEncodeAPICreateInstance")?;

            let mut fn_list = NvEncFunctionList::default();
            let status = create_instance(&mut fn_list);
            check(status, "NvEncodeAPICreateInstance")?;

            tracing::debug!("NVENC function list populated successfully");

            // ------------------------------------------------------------------
            // 3. Open an encode session on the CUDA context
            // ------------------------------------------------------------------
            let mut session_params = NvEncOpenEncodeSessionExParams::default();
            session_params.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
            session_params.device = cuda_ctx.as_ptr();
            session_params.apiVersion = NVENCAPI_VERSION;

            let mut encoder: *mut c_void = ptr::null_mut();

            let open_fn = fn_list
                .nvEncOpenEncodeSessionEx
                .context("nvEncOpenEncodeSessionEx is null")?;
            let status = open_fn(&mut session_params, &mut encoder);
            check(status, "nvEncOpenEncodeSessionEx")?;

            tracing::debug!("NVENC encode session opened");

            // ------------------------------------------------------------------
            // 4. Query preset config (P4 + low-latency tuning)
            // ------------------------------------------------------------------
            let mut preset_config = NvEncPresetConfig::default();

            let get_preset_fn = fn_list
                .nvEncGetEncodePresetConfigEx
                .context("nvEncGetEncodePresetConfigEx is null")?;
            let status = get_preset_fn(
                encoder,
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P4_GUID,
                NV_ENC_TUNING_INFO_LOW_LATENCY,
                &mut preset_config,
            );
            check(status, "nvEncGetEncodePresetConfigEx")?;

            tracing::debug!("Retrieved preset P4 low-latency config");

            // ------------------------------------------------------------------
            // 5. Customise the encode configuration
            // ------------------------------------------------------------------
            let mut encode_config = preset_config.presetCfg.clone();
            encode_config.version = NV_ENC_CONFIG_VER;

            // Profile: Baseline (widely supported by decoders)
            encode_config.profileGUID = NV_ENC_H264_PROFILE_BASELINE_GUID;

            // GOP: one keyframe per second
            encode_config.gopLength = fps;
            encode_config.frameIntervalP = 1; // no B-frames

            // Rate control: CBR
            encode_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
            encode_config.rcParams.averageBitRate = bitrate;
            encode_config.rcParams.maxBitRate = bitrate;
            encode_config.rcParams.vbvBufferSize = bitrate / fps; // one frame
            encode_config.rcParams.vbvInitialDelay = encode_config.rcParams.vbvBufferSize;
            encode_config.rcParams.multiPass = NV_ENC_MULTI_PASS_DISABLED;

            // H.264-specific: repeat SPS/PPS with IDR so decoders can join mid-stream
            {
                let h264_cfg = &mut encode_config.encodeCodecConfig.h264Config;
                h264_cfg.repeatSPSPPS = 1;
                h264_cfg.disableSPSPPS = 0;
                h264_cfg.enableIntraRefresh = 0;
            }

            // ------------------------------------------------------------------
            // 6. Initialise the encoder
            // ------------------------------------------------------------------
            let mut init_params = NvEncInitializeParams::default();
            init_params.encodeGUID = NV_ENC_CODEC_H264_GUID;
            init_params.presetGUID = NV_ENC_PRESET_P4_GUID;
            init_params.encodeWidth = width;
            init_params.encodeHeight = height;
            init_params.darWidth = width;
            init_params.darHeight = height;
            init_params.frameRateNum = fps;
            init_params.frameRateDen = 1;
            init_params.enableEncodeAsync = 0; // synchronous mode
            init_params.enablePTD = 1; // let NVENC decide picture type
            init_params.maxEncodeWidth = width;
            init_params.maxEncodeHeight = height;
            init_params.encodeConfig = &mut encode_config;
            init_params.tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY;

            let init_fn = fn_list
                .nvEncInitializeEncoder
                .context("nvEncInitializeEncoder is null")?;
            let status = init_fn(encoder, &mut init_params);
            check(status, "nvEncInitializeEncoder")?;

            tracing::info!(
                "NVENC encoder initialised: {}x{} @ {} fps, {} bps CBR, H.264 Baseline",
                width,
                height,
                fps,
                bitrate,
            );

            // ------------------------------------------------------------------
            // 7. Create input buffer (ARGB / BGRA, encoder-managed system memory)
            // ------------------------------------------------------------------
            let mut input_buf_params = NvEncCreateInputBuffer::default();
            input_buf_params.width = width;
            input_buf_params.height = height;
            input_buf_params.bufferFmt = NV_ENC_BUFFER_FORMAT_ARGB;
            input_buf_params.memoryHeap = NV_ENC_MEMORY_HEAP_AUTOSELECT;

            let create_input_fn = fn_list
                .nvEncCreateInputBuffer
                .context("nvEncCreateInputBuffer is null")?;
            let status = create_input_fn(encoder, &mut input_buf_params);
            check(status, "nvEncCreateInputBuffer")?;

            let input_buffer = input_buf_params.inputBuffer;
            tracing::debug!("Input buffer created: {:?}", input_buffer);

            // ------------------------------------------------------------------
            // 8. Create output bitstream buffer
            // ------------------------------------------------------------------
            let mut output_buf_params = NvEncCreateBitstreamBuffer::default();

            let create_output_fn = fn_list
                .nvEncCreateBitstreamBuffer
                .context("nvEncCreateBitstreamBuffer is null")?;
            let status = create_output_fn(encoder, &mut output_buf_params);
            check(status, "nvEncCreateBitstreamBuffer")?;

            let output_buffer = output_buf_params.bitstreamBuffer;
            tracing::debug!("Bitstream buffer created: {:?}", output_buffer);

            Ok(Self {
                _lib: lib,
                fn_list,
                encoder,
                input_buffer,
                output_buffer,
                width,
                height,
                pts_counter: 0,
            })
        }
    }

    /// Copy a BGRA frame into the NVENC input buffer, row by row.
    ///
    /// The caller's `frame_data` may have a different stride (pitch) than the
    /// NVENC buffer, so we copy row-by-row using the minimum of the two.
    ///
    /// # Safety
    /// The caller must ensure that `self.encoder` and `self.input_buffer` are
    /// valid NVENC handles and that the function pointers in `self.fn_list`
    /// are properly initialised.
    unsafe fn upload_frame(
        &self,
        frame_data: &[u8],
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<u32> {
        // Lock the input buffer to get a CPU-writable pointer + pitch.
        let lock_fn = self
            .fn_list
            .nvEncLockInputBuffer
            .context("nvEncLockInputBuffer is null")?;

        let mut lock_params = NvEncLockInputBuffer::default();
        lock_params.inputBuffer = self.input_buffer;

        let status = unsafe { lock_fn(self.encoder, &mut lock_params) };
        check(status, "nvEncLockInputBuffer")?;

        let dst_ptr = lock_params.bufferDataPtr as *mut u8;
        let dst_pitch = lock_params.pitch;
        let src_row_bytes = (width as usize) * 4; // 4 bytes per pixel (BGRA)

        // Sanity check: the source buffer must be large enough.
        let required_src_size = if height > 1 {
            (stride as usize) * ((height - 1) as usize) + src_row_bytes
        } else {
            src_row_bytes
        };
        if frame_data.len() < required_src_size {
            // Unlock before bailing.
            let unlock_fn = self
                .fn_list
                .nvEncUnlockInputBuffer
                .context("nvEncUnlockInputBuffer is null")?;
            let _ = unsafe { unlock_fn(self.encoder, self.input_buffer) };
            bail!(
                "Frame data too small: need at least {} bytes, got {}",
                required_src_size,
                frame_data.len(),
            );
        }

        // Copy row by row.
        for y in 0..height as usize {
            let src_offset = y * stride as usize;
            let dst_offset = y * dst_pitch as usize;
            unsafe {
                ptr::copy_nonoverlapping(
                    frame_data.as_ptr().add(src_offset),
                    dst_ptr.add(dst_offset),
                    src_row_bytes,
                );
            }
        }

        // Unlock the input buffer.
        let unlock_fn = self
            .fn_list
            .nvEncUnlockInputBuffer
            .context("nvEncUnlockInputBuffer is null")?;
        let status = unsafe { unlock_fn(self.encoder, self.input_buffer) };
        check(status, "nvEncUnlockInputBuffer")?;

        Ok(dst_pitch)
    }

    /// Read the encoded bitstream out of the output buffer.
    ///
    /// # Safety
    /// The caller must ensure that `self.encoder` and `self.output_buffer` are
    /// valid NVENC handles and that the function pointers in `self.fn_list`
    /// are properly initialised.
    unsafe fn read_bitstream(&self) -> Result<(Vec<u8>, bool)> {
        let lock_fn = self
            .fn_list
            .nvEncLockBitstream
            .context("nvEncLockBitstream is null")?;

        let mut lock_params = NvEncLockBitstream::default();
        lock_params.outputBitstream = self.output_buffer;

        let status = unsafe { lock_fn(self.encoder, &mut lock_params) };
        check(status, "nvEncLockBitstream")?;

        let size = lock_params.bitstreamSizeInBytes as usize;
        let ptr = lock_params.bitstreamBufferPtr as *const u8;
        let data = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();

        let is_keyframe = lock_params.pictureType == NV_ENC_PIC_TYPE_IDR;

        let unlock_fn = self
            .fn_list
            .nvEncUnlockBitstream
            .context("nvEncUnlockBitstream is null")?;
        let status = unsafe { unlock_fn(self.encoder, self.output_buffer) };
        check(status, "nvEncUnlockBitstream")?;

        Ok((data, is_keyframe))
    }
}

// ---------------------------------------------------------------------------
// Encoder trait implementation
// ---------------------------------------------------------------------------

impl Encoder for NvencEncoder {
    fn encode(
        &mut self,
        frame_data: &[u8],
        width: u32,
        height: u32,
        stride: u32,
        force_keyframe: bool,
    ) -> Result<EncodedPacket> {
        // Validate dimensions match what the encoder was initialised with.
        if width != self.width || height != self.height {
            bail!(
                "Frame dimensions {}x{} do not match encoder dimensions {}x{}",
                width,
                height,
                self.width,
                self.height,
            );
        }

        unsafe {
            // 1. Upload the frame data into the NVENC input buffer.
            let pitch = self.upload_frame(frame_data, width, height, stride)?;

            // 2. Build encode picture params.
            let mut pic_params = NvEncPicParams::default();
            pic_params.inputWidth = width;
            pic_params.inputHeight = height;
            pic_params.inputPitch = pitch;
            pic_params.inputBuffer = self.input_buffer;
            pic_params.outputBitstream = self.output_buffer;
            pic_params.bufferFmt = NV_ENC_BUFFER_FORMAT_ARGB;
            pic_params.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
            pic_params.inputTimeStamp = self.pts_counter;

            if force_keyframe {
                pic_params.encodePicFlags =
                    NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
            }

            // 3. Encode.
            let encode_fn = self
                .fn_list
                .nvEncEncodePicture
                .context("nvEncEncodePicture is null")?;
            let status = encode_fn(self.encoder, &mut pic_params);
            check(status, "nvEncEncodePicture")?;

            // 4. Read the resulting bitstream.
            let (data, keyframe) = self.read_bitstream()?;

            let pts = self.pts_counter;
            self.pts_counter += 1;

            Ok(EncodedPacket {
                data,
                keyframe,
                pts,
            })
        }
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        let mut packets = Vec::new();

        unsafe {
            // Send an EOS notification to flush any buffered frames.
            let mut pic_params = NvEncPicParams::default();
            pic_params.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
            pic_params.version = NV_ENC_PIC_PARAMS_VER;

            let encode_fn = self
                .fn_list
                .nvEncEncodePicture
                .context("nvEncEncodePicture is null")?;

            let status = encode_fn(self.encoder, &mut pic_params);

            // NEED_MORE_INPUT means there were no buffered frames — that is
            // normal for low-latency configs with enablePTD=1 and no B-frames.
            if status == NV_ENC_ERR_NEED_MORE_INPUT {
                return Ok(packets);
            }
            check(status, "nvEncEncodePicture (EOS)")?;

            // There might be a final frame to collect.
            match self.read_bitstream() {
                Ok((data, keyframe)) if !data.is_empty() => {
                    let pts = self.pts_counter;
                    self.pts_counter += 1;
                    packets.push(EncodedPacket {
                        data,
                        keyframe,
                        pts,
                    });
                }
                _ => {}
            }
        }

        Ok(packets)
    }
}

// ---------------------------------------------------------------------------
// Drop — clean up NVENC resources
// ---------------------------------------------------------------------------

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe {
            // Destroy input buffer.
            if let Some(destroy_input) = self.fn_list.nvEncDestroyInputBuffer {
                let status = destroy_input(self.encoder, self.input_buffer);
                if status != NV_ENC_SUCCESS {
                    tracing::warn!(
                        "nvEncDestroyInputBuffer failed: {} ({})",
                        nvenc_status_name(status),
                        status,
                    );
                }
            }

            // Destroy output bitstream buffer.
            if let Some(destroy_output) = self.fn_list.nvEncDestroyBitstreamBuffer {
                let status = destroy_output(self.encoder, self.output_buffer);
                if status != NV_ENC_SUCCESS {
                    tracing::warn!(
                        "nvEncDestroyBitstreamBuffer failed: {} ({})",
                        nvenc_status_name(status),
                        status,
                    );
                }
            }

            // Destroy the encoder session itself.
            if let Some(destroy_encoder) = self.fn_list.nvEncDestroyEncoder {
                let status = destroy_encoder(self.encoder);
                if status != NV_ENC_SUCCESS {
                    tracing::warn!(
                        "nvEncDestroyEncoder failed: {} ({})",
                        nvenc_status_name(status),
                        status,
                    );
                }
            }

            tracing::debug!("NVENC encoder resources released");
        }
    }
}
