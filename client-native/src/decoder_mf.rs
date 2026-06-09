//! Hardware video decoding on Windows via Media Foundation + D3D11 (DXVA).
//!
//! The decode itself runs on the GPU's fixed-function video block (NVDEC /
//! QuickSync / VCN) — the same path Moonlight uses. Flow:
//!
//!   Annex-B / OBU packet → IMFSample → decoder MFT (D3D11-bound, low
//!   latency) → NV12 ID3D11Texture2D → staging copy → CPU NV12 planes →
//!   wgpu NV12 textures (YUV→RGB stays on the GPU in the fragment shader).
//!
//! `probe()` checks the actual D3D11 video decoder profiles so we only
//! report hardware support when the GPU truly has the codec block —
//! otherwise the MF software fallback would silently eat CPU while looking
//! like a hardware path.

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D, ID3D11VideoDevice, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaType, IMFSample,
    IMFTransform, MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFStartup, MFTEnumEx, MFSTARTUP_FULL,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_NV12,
    MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

use crate::decoder::{DecodedFrame, PlaneStorage};

/// D3D11 video decoder profile GUIDs (dxva.h) — presence means the GPU has
/// a hardware decode block for the codec.
const PROFILE_H264_VLD_NOFGT: windows::core::GUID =
    windows::core::GUID::from_u128(0x1b81be68_a0c7_11d3_b984_00c04f2e73c5);
const PROFILE_AV1_VLD_PROFILE0: windows::core::GUID =
    windows::core::GUID::from_u128(0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a);

const MF_VERSION: u32 = 0x0002_0070; // MF_SDK_VERSION << 16 | MF_API_VERSION

fn ensure_mf_initialized() -> Result<()> {
    unsafe {
        // Per-thread COM init; RPC_E_CHANGED_MODE just means the thread
        // already has an apartment, which is fine for our usage.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup failed")?;
    }
    Ok(())
}

fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice failed")?;
        let device = device.context("no D3D11 device")?;
        let context = context.context("no D3D11 context")?;
        // MF decoder worker threads touch the device concurrently.
        let mt: ID3D11Multithread = context.cast().context("ID3D11Multithread")?;
        let _ = mt.SetMultithreadProtected(true);
        Ok((device, context))
    }
}

/// True when the GPU exposes a D3D11 hardware decode profile for the codec.
fn gpu_has_profile(device: &ID3D11Device, profile: &windows::core::GUID) -> bool {
    unsafe {
        let Ok(video) = device.cast::<ID3D11VideoDevice>() else {
            return false;
        };
        let count = video.GetVideoDecoderProfileCount();
        for i in 0..count {
            if video.GetVideoDecoderProfile(i).map(|g| g == *profile).unwrap_or(false) {
                return true;
            }
        }
        false
    }
}

/// Find a synchronous decoder MFT for `subtype` (NV12 out).
fn find_decoder_mft(subtype: &windows::core::GUID) -> Result<IMFTransform> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: *subtype,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
        .context("MFTEnumEx failed")?;
        if count == 0 || activates.is_null() {
            bail!("no decoder MFT registered for this codec");
        }
        // Use the first (best-ranked) MFT; release the rest + the array.
        let list = std::slice::from_raw_parts_mut(activates, count as usize);
        let mft = list[0]
            .as_ref()
            .context("null IMFActivate")
            .and_then(|act| {
                act.ActivateObject::<IMFTransform>()
                    .context("IMFActivate::ActivateObject")
            });
        for act in list.iter_mut() {
            *act = None;
        }
        CoTaskMemFree(Some(activates as *const _));
        mft
    }
}

pub struct MfHwDecoder {
    mft: IMFTransform,
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    _dxgi_mgr: IMFDXGIDeviceManager,
    /// MFT allocates its own output samples (the DXVA path).
    provides_samples: bool,
    /// Staging texture sized to the decoder's (aligned) output texture.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Display dimensions from the negotiated output type.
    width: u32,
    height: u32,
    pts: i64,
    pub needs_keyframe: bool,
    frames_out: u64,
}

// SAFETY: the decoder is only ever used from the single decode thread; COM
// objects here are MTA-safe.
unsafe impl Send for MfHwDecoder {}

impl MfHwDecoder {
    pub fn new(codec: &str) -> Result<Self> {
        ensure_mf_initialized()?;

        let (subtype, profile) = match codec {
            "av1" => (MFVideoFormat_AV1, PROFILE_AV1_VLD_PROFILE0),
            _ => (MFVideoFormat_H264, PROFILE_H264_VLD_NOFGT),
        };

        let (device, context) = create_d3d11_device()?;
        if !gpu_has_profile(&device, &profile) {
            bail!("GPU has no hardware decode profile for {}", codec);
        }

        let mft = find_decoder_mft(&subtype)?;

        unsafe {
            // Bind the MFT to our D3D11 device → decode runs on the GPU and
            // outputs D3D11 NV12 textures. If this fails we bail; the MF
            // software path would be slower than our existing decoders.
            let mut reset_token: u32 = 0;
            let mut mgr: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut mgr)
                .context("MFCreateDXGIDeviceManager")?;
            let mgr = mgr.context("no DXGI device manager")?;
            mgr.ResetDevice(&device, reset_token).context("ResetDevice")?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)
                .context("MFT rejected D3D11 device manager (no DXVA)")?;

            // Lowest-latency output: don't buffer frames for reordering.
            if let Ok(attrs) = mft.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }

            // Input type: elementary stream, size parsed from the bitstream.
            let in_type: IMFMediaType = MFCreateMediaType().context("MFCreateMediaType")?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            mft.SetInputType(0, &in_type, 0).context("SetInputType")?;

            let mut dec = Self {
                mft,
                _device: device,
                context,
                _dxgi_mgr: mgr,
                provides_samples: false,
                staging: None,
                width: 0,
                height: 0,
                pts: 0,
                needs_keyframe: false,
                frames_out: 0,
            };
            dec.negotiate_output_type()?;
            if !dec.provides_samples {
                // Without MFT-allocated samples the decoder is not running
                // the DXVA/D3D11 path — software MF would be slower than our
                // own decoders, so report failure and let the caller fall back.
                bail!("decoder MFT did not engage the D3D11 (DXVA) sample path");
            }

            dec.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("NOTIFY_BEGIN_STREAMING")?;
            dec.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("NOTIFY_START_OF_STREAM")?;

            eprintln!(
                "[mf-decode] hardware {} decoder ready (D3D11/DXVA, low-latency)",
                codec
            );
            Ok(dec)
        }
    }

    /// Pick the NV12 output type and refresh stream info / dimensions.
    /// Called at init and again on MF_E_TRANSFORM_STREAM_CHANGE (resolution
    /// changes when the server rebuilds its encoder).
    fn negotiate_output_type(&mut self) -> Result<()> {
        unsafe {
            let mut i = 0;
            loop {
                let t = self
                    .mft
                    .GetOutputAvailableType(0, i)
                    .context("no NV12 output type available")?;
                let sub = t.GetGUID(&MF_MT_SUBTYPE)?;
                if sub == MFVideoFormat_NV12 {
                    self.mft.SetOutputType(0, &t, 0).context("SetOutputType")?;
                    if let Ok(sz) = t.GetUINT64(&MF_MT_FRAME_SIZE) {
                        self.width = (sz >> 32) as u32;
                        self.height = (sz & 0xFFFF_FFFF) as u32;
                    }
                    break;
                }
                i += 1;
            }
            let info = self.mft.GetOutputStreamInfo(0).context("GetOutputStreamInfo")?;
            self.provides_samples =
                info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            // Old staging texture may be the wrong size now.
            self.staging = None;
            Ok(())
        }
    }

    /// Feed one encoded packet; returns the newest decoded frame, if any.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32).context("create buffer")?;
            {
                let mut ptr: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut ptr, None, None).context("buffer lock")?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
                buffer.Unlock().context("buffer unlock")?;
                buffer.SetCurrentLength(data.len() as u32)?;
            }
            let sample = MFCreateSample().context("create sample")?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.pts)?;
            sample.SetSampleDuration(166_667)?; // 60fps in 100ns units
            self.pts += 166_667;

            let mut latest: Option<DecodedFrame> = None;
            // The sync MFT alternates: drain outputs whenever input is
            // refused, then retry the input once.
            match self.mft.ProcessInput(0, &sample, 0) {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    self.drain_outputs(&mut latest)?;
                    self.mft
                        .ProcessInput(0, &sample, 0)
                        .context("ProcessInput retry")?;
                }
                Err(e) => return Err(e).context("ProcessInput"),
            }
            self.drain_outputs(&mut latest)?;
            Ok(latest)
        }
    }

    /// Pull every ready output frame, keeping only the newest.
    unsafe fn drain_outputs(&mut self, latest: &mut Option<DecodedFrame>) -> Result<()> {
        loop {
            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status: u32 = 0;
            let hr = unsafe {
                self.mft
                    .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status)
            };
            // Take ownership so the COM refs are released on every path.
            let sample = unsafe { std::mem::ManuallyDrop::take(&mut out.pSample) };
            let _events = unsafe { std::mem::ManuallyDrop::take(&mut out.pEvents) };

            match hr {
                Ok(()) => {
                    if let Some(sample) = sample {
                        match unsafe { self.read_nv12_sample(&sample) } {
                            Ok(frame) => {
                                self.frames_out += 1;
                                if self.frames_out <= 3 || self.frames_out % 600 == 0 {
                                    eprintln!(
                                        "[mf-decode] frame #{} {}x{}",
                                        self.frames_out, frame.width, frame.height
                                    );
                                }
                                *latest = Some(frame);
                            }
                            Err(e) => eprintln!("[mf-decode] readback failed: {e:#}"),
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.negotiate_output_type()
                        .context("renegotiate after stream change")?;
                    eprintln!(
                        "[mf-decode] stream change → {}x{}",
                        self.width, self.height
                    );
                }
                Err(e) => return Err(e).context("ProcessOutput"),
            }
        }
    }

    /// Copy the decoder's NV12 D3D11 texture through a staging texture into
    /// CPU-visible planes. (The decode itself already happened on the GPU's
    /// video block; this is the one unavoidable readback before wgpu upload.)
    unsafe fn read_nv12_sample(&mut self, sample: &IMFSample) -> Result<DecodedFrame> {
        let buffer = unsafe { sample.GetBufferByIndex(0) }.context("GetBufferByIndex")?;
        let dxgi: IMFDXGIBuffer = buffer.cast().context("output is not a D3D11 texture")?;

        let mut tex_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut tex_ptr) }
            .context("IMFDXGIBuffer::GetResource")?;
        let texture = unsafe { ID3D11Texture2D::from_raw(tex_ptr) };
        let subresource = unsafe { dxgi.GetSubresourceIndex() }.context("subresource index")?;

        // The decoder texture is alignment-padded (e.g. 1920x1088); stage at
        // that size and crop when building the frame.
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut src_desc) };
        let (aligned_w, aligned_h) = (src_desc.Width, src_desc.Height);

        if self
            .staging
            .as_ref()
            .map(|(_, w, h)| (*w, *h) != (aligned_w, aligned_h))
            .unwrap_or(true)
        {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: aligned_w,
                Height: aligned_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            unsafe { self._device.CreateTexture2D(&desc, None, Some(&mut staging)) }
                .context("create staging texture")?;
            self.staging = Some((staging.context("no staging texture")?, aligned_w, aligned_h));
        }
        let (staging, _, _) = self.staging.as_ref().unwrap();

        unsafe {
            self.context.CopySubresourceRegion(
                staging,
                0,
                0,
                0,
                0,
                &texture,
                subresource,
                None,
            );
        }

        let w = if self.width > 0 { self.width.min(aligned_w) } else { aligned_w };
        let h = if self.height > 0 { self.height.min(aligned_h) } else { aligned_h };

        let mut mapped = windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .context("map staging texture")?;
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;

        // NV12 staging layout: aligned_h rows of Y, then aligned_h/2 rows of
        // interleaved UV, all at RowPitch. Copy the visible rows, keep the
        // pitch as stride (wgpu upload reads bytes_per_row = stride).
        let y_rows = h as usize;
        let uv_rows = (h as usize).div_ceil(2);
        let mut y = vec![0u8; pitch * y_rows];
        let mut uv = vec![0u8; pitch * uv_rows];
        unsafe {
            std::ptr::copy_nonoverlapping(base, y.as_mut_ptr(), pitch * y_rows);
            std::ptr::copy_nonoverlapping(
                base.add(pitch * aligned_h as usize),
                uv.as_mut_ptr(),
                pitch * uv_rows,
            );
            self.context.Unmap(staging, 0);
        }

        Ok(DecodedFrame {
            storage: PlaneStorage::Nv12 { y, uv, stride: pitch },
            width: w,
            height: h,
        })
    }

    /// Reset the decoder after a corrupt-stream condition; the caller should
    /// request an IDR from the server.
    pub fn flush(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
    }
}

/// Probe hardware decode support: (h264, av1). Cheap — one D3D11 device,
/// profile enumeration, and an MFT registry lookup per codec.
pub fn probe() -> (bool, bool) {
    if std::env::var("DDISPLAY_NO_HWDEC").map(|v| v == "1").unwrap_or(false) {
        eprintln!("[mf-decode] hardware decode disabled by DDISPLAY_NO_HWDEC");
        return (false, false);
    }
    if ensure_mf_initialized().is_err() {
        return (false, false);
    }
    let Ok((device, _context)) = create_d3d11_device() else {
        eprintln!("[mf-decode] no D3D11 hardware device — software decode");
        return (false, false);
    };
    let h264 = gpu_has_profile(&device, &PROFILE_H264_VLD_NOFGT)
        && find_decoder_mft(&MFVideoFormat_H264).is_ok();
    let av1 = gpu_has_profile(&device, &PROFILE_AV1_VLD_PROFILE0)
        && find_decoder_mft(&MFVideoFormat_AV1).is_ok();
    eprintln!("[mf-decode] hardware decode probe: h264={h264} av1={av1}");
    (h264, av1)
}
