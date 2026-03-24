#![allow(non_snake_case, non_camel_case_types, dead_code)]

//! Raw FFI bindings for the NVIDIA Video Encoder (NVENC) API.
//!
//! These definitions are hand-written from the NVENC SDK documentation since no
//! `nvEncodeAPI.h` header is available on this system. All structs use `#[repr(C)]`
//! to match the C ABI layout expected by `libnvidia-encode.so`.
//!
//! Target: aarch64-linux, NVENC API version 12.2.

use std::ffi::c_void;

// ---------------------------------------------------------------------------
// NVENC API versioning
// ---------------------------------------------------------------------------

/// NVENC API major version.
pub const NVENCAPI_MAJOR_VERSION: u32 = 12;

/// NVENC API minor version.
pub const NVENCAPI_MINOR_VERSION: u32 = 2;

/// Packed NVENC API version: major in low bits, minor shifted left 24 bits.
pub const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);

/// Compute a versioned struct tag. The NVENC convention is:
///   (struct_version) | (NVENCAPI_VERSION << 16) | (0x7 << 28)
pub const fn nvenc_struct_version(ver: u32) -> u32 {
    ver | (NVENCAPI_VERSION << 16) | (0x7 << 28)
}

// ---------------------------------------------------------------------------
// NVENCSTATUS — return codes
// ---------------------------------------------------------------------------

pub type NVENCSTATUS = i32;

pub const NV_ENC_SUCCESS: NVENCSTATUS = 0;
pub const NV_ENC_ERR_NO_ENCODE_DEVICE: NVENCSTATUS = 1;
pub const NV_ENC_ERR_UNSUPPORTED_DEVICE: NVENCSTATUS = 2;
pub const NV_ENC_ERR_INVALID_ENCODERDEVICE: NVENCSTATUS = 3;
pub const NV_ENC_ERR_INVALID_DEVICE: NVENCSTATUS = 4;
pub const NV_ENC_ERR_DEVICE_NOT_EXIST: NVENCSTATUS = 5;
pub const NV_ENC_ERR_INVALID_PTR: NVENCSTATUS = 6;
pub const NV_ENC_ERR_INVALID_EVENT: NVENCSTATUS = 7;
pub const NV_ENC_ERR_INVALID_PARAM: NVENCSTATUS = 8;
pub const NV_ENC_ERR_INVALID_CALL: NVENCSTATUS = 9;
pub const NV_ENC_ERR_OUT_OF_MEMORY: NVENCSTATUS = 10;
pub const NV_ENC_ERR_ENCODER_NOT_INITIALIZED: NVENCSTATUS = 11;
pub const NV_ENC_ERR_UNSUPPORTED_PARAM: NVENCSTATUS = 12;
pub const NV_ENC_ERR_LOCK_BUSY: NVENCSTATUS = 13;
pub const NV_ENC_ERR_NOT_ENOUGH_BUFFER: NVENCSTATUS = 14;
pub const NV_ENC_ERR_INVALID_VERSION: NVENCSTATUS = 15;
pub const NV_ENC_ERR_MAP_FAILED: NVENCSTATUS = 16;
pub const NV_ENC_ERR_NEED_MORE_INPUT: NVENCSTATUS = 17;
pub const NV_ENC_ERR_ENCODER_BUSY: NVENCSTATUS = 18;
pub const NV_ENC_ERR_EVENT_NOT_REGISTERD: NVENCSTATUS = 19;
pub const NV_ENC_ERR_GENERIC: NVENCSTATUS = 20;
pub const NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY: NVENCSTATUS = 21;
pub const NV_ENC_ERR_UNIMPLEMENTED: NVENCSTATUS = 22;
pub const NV_ENC_ERR_RESOURCE_REGISTER_FAILED: NVENCSTATUS = 23;
pub const NV_ENC_ERR_RESOURCE_NOT_REGISTERED: NVENCSTATUS = 24;
pub const NV_ENC_ERR_RESOURCE_NOT_MAPPED: NVENCSTATUS = 25;

/// Return a human-readable name for an NVENCSTATUS code.
pub fn nvenc_status_name(status: NVENCSTATUS) -> &'static str {
    match status {
        NV_ENC_SUCCESS => "NV_ENC_SUCCESS",
        NV_ENC_ERR_NO_ENCODE_DEVICE => "NV_ENC_ERR_NO_ENCODE_DEVICE",
        NV_ENC_ERR_UNSUPPORTED_DEVICE => "NV_ENC_ERR_UNSUPPORTED_DEVICE",
        NV_ENC_ERR_INVALID_ENCODERDEVICE => "NV_ENC_ERR_INVALID_ENCODERDEVICE",
        NV_ENC_ERR_INVALID_DEVICE => "NV_ENC_ERR_INVALID_DEVICE",
        NV_ENC_ERR_DEVICE_NOT_EXIST => "NV_ENC_ERR_DEVICE_NOT_EXIST",
        NV_ENC_ERR_INVALID_PTR => "NV_ENC_ERR_INVALID_PTR",
        NV_ENC_ERR_INVALID_EVENT => "NV_ENC_ERR_INVALID_EVENT",
        NV_ENC_ERR_INVALID_PARAM => "NV_ENC_ERR_INVALID_PARAM",
        NV_ENC_ERR_INVALID_CALL => "NV_ENC_ERR_INVALID_CALL",
        NV_ENC_ERR_OUT_OF_MEMORY => "NV_ENC_ERR_OUT_OF_MEMORY",
        NV_ENC_ERR_ENCODER_NOT_INITIALIZED => "NV_ENC_ERR_ENCODER_NOT_INITIALIZED",
        NV_ENC_ERR_UNSUPPORTED_PARAM => "NV_ENC_ERR_UNSUPPORTED_PARAM",
        NV_ENC_ERR_LOCK_BUSY => "NV_ENC_ERR_LOCK_BUSY",
        NV_ENC_ERR_NOT_ENOUGH_BUFFER => "NV_ENC_ERR_NOT_ENOUGH_BUFFER",
        NV_ENC_ERR_INVALID_VERSION => "NV_ENC_ERR_INVALID_VERSION",
        NV_ENC_ERR_MAP_FAILED => "NV_ENC_ERR_MAP_FAILED",
        NV_ENC_ERR_NEED_MORE_INPUT => "NV_ENC_ERR_NEED_MORE_INPUT",
        NV_ENC_ERR_ENCODER_BUSY => "NV_ENC_ERR_ENCODER_BUSY",
        NV_ENC_ERR_EVENT_NOT_REGISTERD => "NV_ENC_ERR_EVENT_NOT_REGISTERD",
        NV_ENC_ERR_GENERIC => "NV_ENC_ERR_GENERIC",
        NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY => "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY",
        NV_ENC_ERR_UNIMPLEMENTED => "NV_ENC_ERR_UNIMPLEMENTED",
        NV_ENC_ERR_RESOURCE_REGISTER_FAILED => "NV_ENC_ERR_RESOURCE_REGISTER_FAILED",
        NV_ENC_ERR_RESOURCE_NOT_REGISTERED => "NV_ENC_ERR_RESOURCE_NOT_REGISTERED",
        NV_ENC_ERR_RESOURCE_NOT_MAPPED => "NV_ENC_ERR_RESOURCE_NOT_MAPPED",
        _ => "NV_ENC_ERR_UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// GUID type
// ---------------------------------------------------------------------------

/// NVENC GUID — identical layout to Windows GUID / UUID.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvEncGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl NvEncGuid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self { data1, data2, data3, data4 }
    }
}

// ---------------------------------------------------------------------------
// Well-known GUIDs
// ---------------------------------------------------------------------------

/// H.264 codec GUID.
pub const NV_ENC_CODEC_H264_GUID: NvEncGuid = NvEncGuid::new(
    0x6BC82762, 0x4E63, 0x4CA4,
    [0xAA, 0x85, 0x1E, 0xAD, 0x0D, 0x3E, 0x58, 0x96],
);

/// HEVC (H.265) codec GUID.
pub const NV_ENC_CODEC_HEVC_GUID: NvEncGuid = NvEncGuid::new(
    0x790CDC88, 0x4522, 0x4D7B,
    [0x94, 0x25, 0xBD, 0xA9, 0x97, 0x5F, 0x76, 0x03],
);

/// Preset P4 — balanced quality / speed.
pub const NV_ENC_PRESET_P4_GUID: NvEncGuid = NvEncGuid::new(
    0xB514C39A, 0x635D, 0x11E9,
    [0x86, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
);

/// Preset P1 — fastest.
pub const NV_ENC_PRESET_P1_GUID: NvEncGuid = NvEncGuid::new(
    0xFC0A8D3E, 0x45F8, 0x4CF8,
    [0x80, 0xC7, 0x29, 0x87, 0x91, 0x16, 0x02, 0x01],
);

/// H.264 Baseline profile GUID.
pub const NV_ENC_H264_PROFILE_BASELINE_GUID: NvEncGuid = NvEncGuid::new(
    0x0727BCAA, 0x78C4, 0x4C83,
    [0x8C, 0x2F, 0xEF, 0x3D, 0xFF, 0x26, 0x7C, 0x6A],
);

/// H.264 Main profile GUID.
pub const NV_ENC_H264_PROFILE_MAIN_GUID: NvEncGuid = NvEncGuid::new(
    0x4D307288, 0x7F50, 0x4B6B,
    [0x9A, 0x28, 0x26, 0x3D, 0x9A, 0xFD, 0x56, 0x10],
);

/// H.264 High profile GUID.
pub const NV_ENC_H264_PROFILE_HIGH_GUID: NvEncGuid = NvEncGuid::new(
    0xE7CBC309, 0x4F7A, 0x4B89,
    [0xAF, 0x2A, 0xD5, 0x37, 0xC9, 0x2B, 0xE3, 0x10],
);

// ---------------------------------------------------------------------------
// Device and buffer format constants
// ---------------------------------------------------------------------------

pub const NV_ENC_DEVICE_TYPE_CUDA: u32 = 1;
pub const NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR: u32 = 0x04;

/// ARGB 8-bit per channel (the format X11 SHM gives us as BGRA, which NVENC
/// calls "ARGB" due to the opposite byte-order naming convention).
pub const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x00000020;
pub const NV_ENC_BUFFER_FORMAT_ABGR: u32 = 0x00000010;
pub const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x00000003;
pub const NV_ENC_BUFFER_FORMAT_YV12: u32 = 0x00000004;
pub const NV_ENC_BUFFER_FORMAT_IYUV: u32 = 0x00000005;
pub const NV_ENC_BUFFER_FORMAT_YUV444: u32 = 0x00000006;
pub const NV_ENC_BUFFER_FORMAT_UNDEFINED: u32 = 0x00000000;

// ---------------------------------------------------------------------------
// Picture struct / type / flags
// ---------------------------------------------------------------------------

pub const NV_ENC_PIC_STRUCT_FRAME: u32 = 0x01;
pub const NV_ENC_PIC_STRUCT_FIELD_TOP_BOTTOM: u32 = 0x02;
pub const NV_ENC_PIC_STRUCT_FIELD_BOTTOM_TOP: u32 = 0x03;

pub const NV_ENC_PIC_TYPE_P: u32 = 0;
pub const NV_ENC_PIC_TYPE_B: u32 = 1;
pub const NV_ENC_PIC_TYPE_I: u32 = 2;
pub const NV_ENC_PIC_TYPE_IDR: u32 = 4;
pub const NV_ENC_PIC_TYPE_UNKNOWN: u32 = 0xFF;

pub const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 4;
pub const NV_ENC_PIC_FLAG_FORCEINTRA: u32 = 1;
pub const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x10;
pub const NV_ENC_PIC_FLAG_EOS: u32 = 0x20;

// ---------------------------------------------------------------------------
// Tuning, rate control, multi-pass, level
// ---------------------------------------------------------------------------

pub const NV_ENC_TUNING_INFO_UNDEFINED: u32 = 0;
pub const NV_ENC_TUNING_INFO_HIGH_QUALITY: u32 = 1;
pub const NV_ENC_TUNING_INFO_LOW_LATENCY: u32 = 2;
pub const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
pub const NV_ENC_TUNING_INFO_LOSSLESS: u32 = 4;

pub const NV_ENC_PARAMS_RC_CONSTQP: u32 = 0;
pub const NV_ENC_PARAMS_RC_VBR: u32 = 1;
pub const NV_ENC_PARAMS_RC_CBR: u32 = 2;

pub const NV_ENC_MULTI_PASS_DISABLED: u32 = 0;
pub const NV_ENC_MULTI_PASS_QUARTER_RESOLUTION: u32 = 1;
pub const NV_ENC_MULTI_PASS_FULL_RESOLUTION: u32 = 2;

pub const NV_ENC_LEVEL_AUTOSELECT: u32 = 0;

/// Memory heap — default (let driver decide).
pub const NV_ENC_MEMORY_HEAP_AUTOSELECT: u32 = 0;

// ---------------------------------------------------------------------------
// Struct version constants (used in the `version` field of each params struct)
// ---------------------------------------------------------------------------

pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = nvenc_struct_version(1);
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = nvenc_struct_version(6);
pub const NV_ENC_CONFIG_VER: u32 = nvenc_struct_version(8);
pub const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = nvenc_struct_version(1);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = nvenc_struct_version(1);
pub const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = nvenc_struct_version(1);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = nvenc_struct_version(1);
pub const NV_ENC_PIC_PARAMS_VER: u32 = nvenc_struct_version(6);
pub const NV_ENC_PRESET_CONFIG_VER: u32 = nvenc_struct_version(4);
pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = nvenc_struct_version(2);

// ---------------------------------------------------------------------------
// NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct NvEncOpenEncodeSessionExParams {
    pub version: u32,
    pub deviceType: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub apiVersion: u32,
    pub reserved1: u32,
    pub reserved2: [*mut c_void; 64],
}

impl Default for NvEncOpenEncodeSessionExParams {
    fn default() -> Self {
        Self {
            version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: 0,
            device: std::ptr::null_mut(),
            reserved: std::ptr::null_mut(),
            apiVersion: NVENCAPI_VERSION,
            reserved1: 0,
            reserved2: [std::ptr::null_mut(); 64],
        }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_RC_PARAMS — rate-control sub-structure inside NV_ENC_CONFIG
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone)]
pub struct NvEncRcParams {
    pub version: u32,
    pub rateControlMode: u32,
    pub constQP_interP: u32,
    pub constQP_interB: u32,
    pub constQP_intra: u32,
    pub maxBitRate: u32,
    pub averageBitRate: u32,
    pub vbvBufferSize: u32,
    pub vbvInitialDelay: u32,
    pub enableMinQP: u32,
    pub enableMaxQP: u32,
    pub minQP_interP: u32,
    pub minQP_interB: u32,
    pub minQP_intra: u32,
    pub maxQP_interP: u32,
    pub maxQP_interB: u32,
    pub maxQP_intra: u32,
    pub targetQuality: u32,
    pub targetQualityLSB: u32,
    pub lookaheadDepth: u16,
    pub lowDelayKeyFrameScale: u16,
    pub reserved1: u16,
    pub multiPass: u32,
    pub alphaLayerBitrateRatio: u32,
    pub reserved: [u32; 4],
}

impl Default for NvEncRcParams {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_CONFIG_H264 — H.264 codec-specific configuration
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvEncConfigH264 {
    pub enableTemporalSVC: u32,
    pub enableStereoMVC: u32,
    pub hierarchicalPFrames: u32,
    pub hierarchicalBFrames: u32,
    pub outputBufferingPeriodSEI: u32,
    pub outputPictureTimingSEI: u32,
    pub outputAUD: u32,
    pub disableSPSPPS: u32,
    pub outputFramePackingSEI: u32,
    pub outputRecoveryPointSEI: u32,
    pub enableIntraRefresh: u32,
    pub enableConstrainedEncoding: u32,
    pub repeatSPSPPS: u32,
    pub enableVFR: u32,
    pub enableLTR: u32,
    pub qpPrimeYZeroTransformBypassFlag: u32,
    pub useConstrainedIntraPred: u32,
    pub enableFillerDataInsertion: u32,
    pub disableSVCPrefixNalu: u32,
    pub enableScalabilityInfoSEI: u32,
    pub reserved1: [u32; 218],
    pub reserved2: [*mut c_void; 64],
}

impl Default for NvEncConfigH264 {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_CONFIG_HEVC — HEVC codec-specific configuration (placeholder size)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvEncConfigHevc {
    pub reserved: [u32; 256],
    pub reserved2: [*mut c_void; 64],
}

impl Default for NvEncConfigHevc {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_CODEC_CONFIG — union of codec-specific configs
// ---------------------------------------------------------------------------

/// This union holds codec-specific configuration. We model it as a union
/// with known codec configs. The struct is large enough to cover all variants.
#[repr(C)]
#[derive(Clone, Copy)]
pub union NvEncCodecConfig {
    pub h264Config: NvEncConfigH264,
    pub hevcConfig: NvEncConfigHevc,
    pub reserved: [u32; 320],
}

impl Default for NvEncCodecConfig {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_CONFIG — master encode configuration
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone)]
pub struct NvEncConfig {
    pub version: u32,
    pub profileGUID: NvEncGuid,
    pub gopLength: u32,
    pub frameIntervalP: i32,
    pub monoChromeEncoding: u32,
    pub frameFieldMode: u32,
    pub mvPrecision: u32,
    pub rcParams: NvEncRcParams,
    pub encodeCodecConfig: NvEncCodecConfig,
    pub reserved: [u32; 278],
    pub reserved2: [*mut c_void; 64],
}

impl Default for NvEncConfig {
    fn default() -> Self {
        unsafe {
            let mut cfg: Self = std::mem::zeroed();
            cfg.version = NV_ENC_CONFIG_VER;
            cfg
        }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_INITIALIZE_PARAMS — encoder initialization parameters
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct NvEncInitializeParams {
    pub version: u32,
    pub encodeGUID: NvEncGuid,
    pub presetGUID: NvEncGuid,
    pub encodeWidth: u32,
    pub encodeHeight: u32,
    pub darWidth: u32,
    pub darHeight: u32,
    pub frameRateNum: u32,
    pub frameRateDen: u32,
    pub enableEncodeAsync: u32,
    pub enablePTD: u32,
    pub reportSliceOffsets: u32,
    pub enableSubFrameWrite: u32,
    pub encodeConfig: *mut NvEncConfig,
    pub maxEncodeWidth: u32,
    pub maxEncodeHeight: u32,
    pub maxMEHintCountsPerBlock: [u32; 2],
    pub tuningInfo: u32,
    pub reserved: [u32; 289],
    pub reserved2: [*mut c_void; 64],
}

impl Default for NvEncInitializeParams {
    fn default() -> Self {
        unsafe {
            let mut p: Self = std::mem::zeroed();
            p.version = NV_ENC_INITIALIZE_PARAMS_VER;
            p
        }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_CREATE_INPUT_BUFFER
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct NvEncCreateInputBuffer {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub memoryHeap: u32,
    pub bufferFmt: u32,
    pub inputBuffer: *mut c_void,
    pub pSysMemBuffer: *mut c_void,
    pub reserved: u32,
    pub reserved2: [*mut c_void; 57],
}

impl Default for NvEncCreateInputBuffer {
    fn default() -> Self {
        unsafe {
            let mut b: Self = std::mem::zeroed();
            b.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
            b
        }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_CREATE_BITSTREAM_BUFFER
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct NvEncCreateBitstreamBuffer {
    pub version: u32,
    pub bitstreamBuffer: *mut c_void,
    pub size: u32,
    pub memoryHeap: u32,
    pub reserved: u32,
    pub bitstreamBufferPtr: *mut c_void,
    pub reserved2: [*mut c_void; 64],
}

impl Default for NvEncCreateBitstreamBuffer {
    fn default() -> Self {
        unsafe {
            let mut b: Self = std::mem::zeroed();
            b.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            b
        }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_LOCK_INPUT_BUFFER
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct NvEncLockInputBuffer {
    pub version: u32,
    pub doNotWait: u32,
    pub inputBuffer: *mut c_void,
    pub bufferDataPtr: *mut c_void,
    pub pitch: u32,
    pub reserved1: u32,
    pub reserved2: [*mut c_void; 64],
}

impl Default for NvEncLockInputBuffer {
    fn default() -> Self {
        unsafe {
            let mut b: Self = std::mem::zeroed();
            b.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
            b
        }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_LOCK_BITSTREAM
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct NvEncLockBitstream {
    pub version: u32,
    pub doNotWait: u32,
    pub outputBitstream: *mut c_void,
    pub sliceOffsets: *mut u32,
    pub bitstreamSizeInBytes: u32,
    pub outputTimeStamp: u64,
    pub outputDuration: u64,
    pub pictureType: u32,
    pub pictureStruct: u32,
    pub frameAvgQP: u32,
    pub frameSatd: u32,
    pub ltrFrameIdx: u32,
    pub ltrFrameBitmap: u32,
    pub reserved: [u32; 13],
    pub intraMBCount: u32,
    pub interMBCount: u32,
    pub averageMVX: i32,
    pub averageMVY: i32,
    pub reserved1: u32,
    pub reserved2: u32,
    pub bitstreamBufferPtr: *mut c_void,
    pub reserved3: [*mut c_void; 64],
}

impl Default for NvEncLockBitstream {
    fn default() -> Self {
        unsafe {
            let mut b: Self = std::mem::zeroed();
            b.version = NV_ENC_LOCK_BITSTREAM_VER;
            b
        }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_CODEC_PIC_PARAMS — codec-specific per-picture params (union)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub union NvEncCodecPicParams {
    pub h264PicParams: NvEncH264PicParams,
    pub reserved: [u32; 256],
}

impl Default for NvEncCodecPicParams {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// H.264-specific per-picture parameters.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvEncH264PicParams {
    pub displayPOCSyntax: u32,
    pub reserved3: u32,
    pub refPicFlag: u32,
    pub colourPlaneId: u32,
    pub forceIntraRefreshWithFrameCnt: u32,
    pub constrainedFrame: u32,
    pub sliceModeDataUpdate: u32,
    pub ltrMarkFrame: u32,
    pub ltrUseFrames: u32,
    pub ltrUsageMode: u32,
    pub forceIntraSliceCount: u32,
    pub forceIntraSliceIdx: *mut u32,
    pub reserved: [u32; 244],
    pub reserved2: [*mut c_void; 60],
}

impl Default for NvEncH264PicParams {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_MEONLY_PARAMS placeholder (unused but occupies space in the struct)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct NvEncMeHintCountsPerBlock {
    pub numCandsPerBlk16x16: u32,
    pub numCandsPerBlk16x8: u32,
    pub numCandsPerBlk8x16: u32,
    pub numCandsPerBlk8x8: u32,
}

// ---------------------------------------------------------------------------
// NV_ENC_PIC_PARAMS — per-frame encode parameters
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct NvEncPicParams {
    pub version: u32,
    pub inputWidth: u32,
    pub inputHeight: u32,
    pub inputPitch: u32,
    pub encodePicFlags: u32,
    pub frameIdx: u32,
    pub inputTimeStamp: u64,
    pub inputDuration: u64,
    pub inputBuffer: *mut c_void,
    pub outputBitstream: *mut c_void,
    pub completionEvent: *mut c_void,
    pub bufferFmt: u32,
    pub pictureStruct: u32,
    pub pictureType: u32,
    pub codecPicParams: NvEncCodecPicParams,
    pub meHintCountsPerBlock: [NvEncMeHintCountsPerBlock; 2],
    pub meHints: [*mut c_void; 2],
    pub reserved: [u32; 286],
    pub reserved2: [*mut c_void; 60],
}

impl Default for NvEncPicParams {
    fn default() -> Self {
        unsafe {
            let mut p: Self = std::mem::zeroed();
            p.version = NV_ENC_PIC_PARAMS_VER;
            p
        }
    }
}

// ---------------------------------------------------------------------------
// NV_ENC_PRESET_CONFIG — returned by nvEncGetEncodePresetConfigEx
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct NvEncPresetConfig {
    pub version: u32,
    pub presetCfg: NvEncConfig,
    pub reserved: [u32; 255],
    pub reserved2: [*mut c_void; 64],
}

impl Default for NvEncPresetConfig {
    fn default() -> Self {
        unsafe {
            let mut p: Self = std::mem::zeroed();
            p.version = NV_ENC_PRESET_CONFIG_VER;
            p.presetCfg.version = NV_ENC_CONFIG_VER;
            p
        }
    }
}

// ---------------------------------------------------------------------------
// NV_ENCODE_API_FUNCTION_LIST — the dispatch table populated by
// NvEncodeAPICreateInstance.
// ---------------------------------------------------------------------------

// Individual function pointer type aliases for readability.

/// nvEncOpenEncodeSessionEx(params, encoder) -> NVENCSTATUS
pub type FnNvEncOpenEncodeSessionEx = unsafe extern "C" fn(
    params: *mut NvEncOpenEncodeSessionExParams,
    encoder: *mut *mut c_void,
) -> NVENCSTATUS;

/// nvEncGetEncodeGUIDCount(encoder, count) -> NVENCSTATUS
pub type FnNvEncGetEncodeGUIDCount = unsafe extern "C" fn(
    encoder: *mut c_void,
    encodeGUIDCount: *mut u32,
) -> NVENCSTATUS;

/// nvEncGetEncodeGUIDs(encoder, guids, guidArraySize, count) -> NVENCSTATUS
pub type FnNvEncGetEncodeGUIDs = unsafe extern "C" fn(
    encoder: *mut c_void,
    GUIDs: *mut NvEncGuid,
    guidArraySize: u32,
    GUIDCount: *mut u32,
) -> NVENCSTATUS;

/// nvEncGetEncodePresetConfigEx(encoder, encodeGUID, presetGUID, tuningInfo, presetConfig)
pub type FnNvEncGetEncodePresetConfigEx = unsafe extern "C" fn(
    encoder: *mut c_void,
    encodeGUID: NvEncGuid,
    presetGUID: NvEncGuid,
    tuningInfo: u32,
    presetConfig: *mut NvEncPresetConfig,
) -> NVENCSTATUS;

/// nvEncInitializeEncoder(encoder, params) -> NVENCSTATUS
pub type FnNvEncInitializeEncoder = unsafe extern "C" fn(
    encoder: *mut c_void,
    createEncodeParams: *mut NvEncInitializeParams,
) -> NVENCSTATUS;

/// nvEncCreateInputBuffer(encoder, params) -> NVENCSTATUS
pub type FnNvEncCreateInputBuffer = unsafe extern "C" fn(
    encoder: *mut c_void,
    createInputBufferParams: *mut NvEncCreateInputBuffer,
) -> NVENCSTATUS;

/// nvEncDestroyInputBuffer(encoder, inputBuffer) -> NVENCSTATUS
pub type FnNvEncDestroyInputBuffer = unsafe extern "C" fn(
    encoder: *mut c_void,
    inputBuffer: *mut c_void,
) -> NVENCSTATUS;

/// nvEncCreateBitstreamBuffer(encoder, params) -> NVENCSTATUS
pub type FnNvEncCreateBitstreamBuffer = unsafe extern "C" fn(
    encoder: *mut c_void,
    createBitstreamBufferParams: *mut NvEncCreateBitstreamBuffer,
) -> NVENCSTATUS;

/// nvEncDestroyBitstreamBuffer(encoder, bitstreamBuffer) -> NVENCSTATUS
pub type FnNvEncDestroyBitstreamBuffer = unsafe extern "C" fn(
    encoder: *mut c_void,
    bitstreamBuffer: *mut c_void,
) -> NVENCSTATUS;

/// nvEncLockInputBuffer(encoder, lockInputBufferParams) -> NVENCSTATUS
pub type FnNvEncLockInputBuffer = unsafe extern "C" fn(
    encoder: *mut c_void,
    lockInputBufferParams: *mut NvEncLockInputBuffer,
) -> NVENCSTATUS;

/// nvEncUnlockInputBuffer(encoder, inputBuffer) -> NVENCSTATUS
pub type FnNvEncUnlockInputBuffer = unsafe extern "C" fn(
    encoder: *mut c_void,
    inputBuffer: *mut c_void,
) -> NVENCSTATUS;

/// nvEncLockBitstream(encoder, lockBitstreamBufferParams) -> NVENCSTATUS
pub type FnNvEncLockBitstream = unsafe extern "C" fn(
    encoder: *mut c_void,
    lockBitstreamBufferParams: *mut NvEncLockBitstream,
) -> NVENCSTATUS;

/// nvEncUnlockBitstream(encoder, bitstreamBuffer) -> NVENCSTATUS
pub type FnNvEncUnlockBitstream = unsafe extern "C" fn(
    encoder: *mut c_void,
    bitstreamBuffer: *mut c_void,
) -> NVENCSTATUS;

/// nvEncEncodePicture(encoder, encodePicParams) -> NVENCSTATUS
pub type FnNvEncEncodePicture = unsafe extern "C" fn(
    encoder: *mut c_void,
    encodePicParams: *mut NvEncPicParams,
) -> NVENCSTATUS;

/// nvEncDestroyEncoder(encoder) -> NVENCSTATUS
pub type FnNvEncDestroyEncoder = unsafe extern "C" fn(
    encoder: *mut c_void,
) -> NVENCSTATUS;

/// The full NVENC function dispatch table. Populated by `NvEncodeAPICreateInstance`.
///
/// Fields use `Option<unsafe extern "C" fn(...)>` so they can be null-initialized
/// before the driver fills them in.
#[repr(C)]
pub struct NvEncFunctionList {
    pub version: u32,
    pub reserved: u32,
    pub nvEncOpenEncodeSession: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetEncodeGUIDCount: Option<FnNvEncGetEncodeGUIDCount>,
    pub nvEncGetEncodeProfileGUIDCount: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetEncodeProfileGUIDs: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetEncodeGUIDs: Option<FnNvEncGetEncodeGUIDs>,
    pub nvEncGetInputFormatCount: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetInputFormats: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetEncodeCaps: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetEncodePresetCount: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetEncodePresetGUIDs: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetEncodePresetConfig: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncInitializeEncoder: Option<FnNvEncInitializeEncoder>,
    pub nvEncCreateInputBuffer: Option<FnNvEncCreateInputBuffer>,
    pub nvEncDestroyInputBuffer: Option<FnNvEncDestroyInputBuffer>,
    pub nvEncCreateBitstreamBuffer: Option<FnNvEncCreateBitstreamBuffer>,
    pub nvEncDestroyBitstreamBuffer: Option<FnNvEncDestroyBitstreamBuffer>,
    pub nvEncEncodePicture: Option<FnNvEncEncodePicture>,
    pub nvEncLockBitstream: Option<FnNvEncLockBitstream>,
    pub nvEncUnlockBitstream: Option<FnNvEncUnlockBitstream>,
    pub nvEncLockInputBuffer: Option<FnNvEncLockInputBuffer>,
    pub nvEncUnlockInputBuffer: Option<FnNvEncUnlockInputBuffer>,
    pub nvEncGetEncodeStats: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetSequenceParams: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncRegisterAsyncEvent: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncUnregisterAsyncEvent: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncMapInputResource: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncUnmapInputResource: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncDestroyEncoder: Option<FnNvEncDestroyEncoder>,
    pub nvEncInvalidateRefFrames: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncOpenEncodeSessionEx: Option<FnNvEncOpenEncodeSessionEx>,
    pub nvEncRegisterResource: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncUnregisterResource: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncReconfigureEncoder: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub reserved1: *mut c_void,
    pub nvEncCreateMVBuffer: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncDestroyMVBuffer: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncRunMotionEstimationOnly: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetLastErrorString: Option<unsafe extern "C" fn(encoder: *mut c_void) -> *const i8>,
    pub nvEncSetIOCudaStreams: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncGetEncodePresetConfigEx: Option<FnNvEncGetEncodePresetConfigEx>,
    pub nvEncGetSequenceParamEx: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub nvEncLookaheadPicture: Option<unsafe extern "C" fn() -> NVENCSTATUS>,
    pub reserved2: [*mut c_void; 277],
}

impl Default for NvEncFunctionList {
    fn default() -> Self {
        unsafe {
            let mut fl: Self = std::mem::zeroed();
            fl.version = NV_ENCODE_API_FUNCTION_LIST_VER;
            fl
        }
    }
}

// ---------------------------------------------------------------------------
// NvEncodeAPICreateInstance — the single entry point we load from the .so
// ---------------------------------------------------------------------------

/// Signature for the `NvEncodeAPICreateInstance` function exported by
/// `libnvidia-encode.so`.
pub type NvEncodeAPICreateInstanceFn =
    unsafe extern "C" fn(functionList: *mut NvEncFunctionList) -> NVENCSTATUS;
