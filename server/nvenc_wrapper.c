/**
 * Thin C wrapper around the NVENC API for use from Rust via FFI.
 *
 * Supports both H.264 and AV1 codecs. Compiled via build.rs using the cc crate.
 * Links dynamically against libcuda.so.1 and libnvidia-encode.so.1 at runtime.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <dlfcn.h>
#include "nvEncodeAPI.h"

/* ---- CUDA driver API types (loaded via dlopen) ---- */
typedef int CUresult;
typedef int CUdevice;
typedef void* CUcontext;

typedef CUresult (*PFN_cuInit)(unsigned);
typedef CUresult (*PFN_cuDeviceGet)(CUdevice*, int);
typedef CUresult (*PFN_cuCtxCreate)(CUcontext*, unsigned, CUdevice);
typedef CUresult (*PFN_cuCtxDestroy)(CUcontext);

typedef NVENCSTATUS (NVENCAPI *PFN_NvEncodeAPIGetMaxSupportedVersion)(uint32_t*);
typedef NVENCSTATUS (NVENCAPI *PFN_NvEncodeAPICreateInstance)(NV_ENCODE_API_FUNCTION_LIST*);

/* ---- Codec identifiers (passed from Rust) ---- */
#define DDISPLAY_CODEC_H264 0
#define DDISPLAY_CODEC_AV1  1

/* ---- Encoder context ---- */
typedef struct {
    void *cuda_lib;
    void *nvenc_lib;
    PFN_cuCtxDestroy cuCtxDestroy;
    CUcontext cuda_ctx;
    NV_ENCODE_API_FUNCTION_LIST funcs;
    void *encoder;
    NV_ENC_INPUT_PTR  input_buf;
    NV_ENC_OUTPUT_PTR output_buf;
    uint32_t width;
    uint32_t height;
    uint32_t codec;
    uint64_t pts;
} nvenc_ctx_t;

/* ---- Encode result passed back to Rust ---- */
typedef struct {
    const uint8_t *data;
    uint32_t size;
    int is_keyframe;
    uint64_t pts;
} nvenc_frame_t;

/* ---- Public API ---- */

nvenc_ctx_t* nvenc_create(uint32_t width, uint32_t height, uint32_t fps, uint32_t bitrate, uint32_t codec) {
    nvenc_ctx_t *ctx = calloc(1, sizeof(nvenc_ctx_t));
    if (!ctx) return NULL;
    ctx->width = width;
    ctx->height = height;
    ctx->codec = codec;

    /* Load libraries */
    ctx->cuda_lib = dlopen("libcuda.so.1", RTLD_LAZY);
    ctx->nvenc_lib = dlopen("libnvidia-encode.so.1", RTLD_LAZY);
    if (!ctx->cuda_lib || !ctx->nvenc_lib) goto fail;

    PFN_cuInit cuInit = (PFN_cuInit)dlsym(ctx->cuda_lib, "cuInit");
    PFN_cuDeviceGet cuDeviceGet = (PFN_cuDeviceGet)dlsym(ctx->cuda_lib, "cuDeviceGet");
    PFN_cuCtxCreate cuCtxCreate = (PFN_cuCtxCreate)dlsym(ctx->cuda_lib, "cuCtxCreate_v2");
    ctx->cuCtxDestroy = (PFN_cuCtxDestroy)dlsym(ctx->cuda_lib, "cuCtxDestroy_v2");
    PFN_NvEncodeAPIGetMaxSupportedVersion getMaxVer =
        (PFN_NvEncodeAPIGetMaxSupportedVersion)dlsym(ctx->nvenc_lib, "NvEncodeAPIGetMaxSupportedVersion");
    PFN_NvEncodeAPICreateInstance createInstance =
        (PFN_NvEncodeAPICreateInstance)dlsym(ctx->nvenc_lib, "NvEncodeAPICreateInstance");

    if (!cuInit || !cuDeviceGet || !cuCtxCreate || !ctx->cuCtxDestroy || !getMaxVer || !createInstance) goto fail;

    /* Init CUDA */
    if (cuInit(0) != 0) goto fail;
    CUdevice dev;
    if (cuDeviceGet(&dev, 0) != 0) goto fail;
    if (cuCtxCreate(&ctx->cuda_ctx, 0, dev) != 0) goto fail;

    /* Query NVENC version */
    uint32_t ver = 0;
    if (getMaxVer(&ver) != NV_ENC_SUCCESS) goto fail_cuda;

    /* Function table */
    memset(&ctx->funcs, 0, sizeof(ctx->funcs));
    ctx->funcs.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (createInstance(&ctx->funcs) != NV_ENC_SUCCESS) goto fail_cuda;

    /* Open encode session */
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS openParams = {0};
    openParams.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    openParams.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
    openParams.device = ctx->cuda_ctx;
    openParams.apiVersion = NVENCAPI_VERSION;
    if (ctx->funcs.nvEncOpenEncodeSessionEx(&openParams, &ctx->encoder) != NV_ENC_SUCCESS) goto fail_cuda;

    /* Select codec GUID */
    GUID codec_guid = (codec == DDISPLAY_CODEC_AV1) ? NV_ENC_CODEC_AV1_GUID : NV_ENC_CODEC_H264_GUID;

    /* Get preset config */
    NV_ENC_PRESET_CONFIG presetConfig = {0};
    presetConfig.version = NV_ENC_PRESET_CONFIG_VER;
    presetConfig.presetCfg.version = NV_ENC_CONFIG_VER;
    if (ctx->funcs.nvEncGetEncodePresetConfigEx(ctx->encoder,
            codec_guid, NV_ENC_PRESET_P1_GUID,
            NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &presetConfig) != NV_ENC_SUCCESS)
        goto fail_session;

    /* Customize config */
    NV_ENC_CONFIG *cfg = &presetConfig.presetCfg;
    cfg->gopLength = NVENC_INFINITE_GOPLENGTH;
    cfg->frameIntervalP = 1;
    cfg->rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    cfg->rcParams.averageBitRate = bitrate;
    cfg->rcParams.maxBitRate = bitrate;
    cfg->rcParams.zeroReorderDelay = 1;

    if (codec == DDISPLAY_CODEC_AV1) {
        cfg->encodeCodecConfig.av1Config.idrPeriod = NVENC_INFINITE_GOPLENGTH;
        cfg->encodeCodecConfig.av1Config.repeatSeqHdr = 1;
        cfg->encodeCodecConfig.av1Config.chromaFormatIDC = 1; /* YUV420 */
    } else {
        cfg->encodeCodecConfig.h264Config.idrPeriod = NVENC_INFINITE_GOPLENGTH;
        cfg->encodeCodecConfig.h264Config.repeatSPSPPS = 1;
        cfg->encodeCodecConfig.h264Config.sliceMode = 0;
        cfg->encodeCodecConfig.h264Config.sliceModeData = 0;
    }

    /* Initialize encoder */
    NV_ENC_INITIALIZE_PARAMS initParams = {0};
    initParams.version = NV_ENC_INITIALIZE_PARAMS_VER;
    initParams.encodeGUID = codec_guid;
    initParams.presetGUID = NV_ENC_PRESET_P1_GUID;
    initParams.encodeWidth = width;
    initParams.encodeHeight = height;
    initParams.darWidth = width;
    initParams.darHeight = height;
    initParams.frameRateNum = fps;
    initParams.frameRateDen = 1;
    initParams.enablePTD = 1;
    initParams.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    initParams.encodeConfig = cfg;
    if (ctx->funcs.nvEncInitializeEncoder(ctx->encoder, &initParams) != NV_ENC_SUCCESS)
        goto fail_session;

    /* Create input buffer (NV12) */
    NV_ENC_CREATE_INPUT_BUFFER createIn = {0};
    createIn.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
    createIn.width = width;
    createIn.height = height;
    createIn.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
    if (ctx->funcs.nvEncCreateInputBuffer(ctx->encoder, &createIn) != NV_ENC_SUCCESS)
        goto fail_session;
    ctx->input_buf = createIn.inputBuffer;

    /* Create output bitstream buffer */
    NV_ENC_CREATE_BITSTREAM_BUFFER createOut = {0};
    createOut.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    if (ctx->funcs.nvEncCreateBitstreamBuffer(ctx->encoder, &createOut) != NV_ENC_SUCCESS)
        goto fail_input;
    ctx->output_buf = createOut.bitstreamBuffer;

    return ctx;

fail_input:
    ctx->funcs.nvEncDestroyInputBuffer(ctx->encoder, ctx->input_buf);
fail_session:
    ctx->funcs.nvEncDestroyEncoder(ctx->encoder);
fail_cuda:
    ctx->cuCtxDestroy(ctx->cuda_ctx);
fail:
    if (ctx->cuda_lib) dlclose(ctx->cuda_lib);
    if (ctx->nvenc_lib) dlclose(ctx->nvenc_lib);
    free(ctx);
    return NULL;
}

int nvenc_encode(nvenc_ctx_t *ctx, const uint8_t *nv12_data, int force_keyframe,
                 nvenc_frame_t *out) {
    if (!ctx || !nv12_data || !out) return -1;

    /* Lock input buffer and copy NV12 data */
    NV_ENC_LOCK_INPUT_BUFFER lockIn = {0};
    lockIn.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
    lockIn.inputBuffer = ctx->input_buf;
    if (ctx->funcs.nvEncLockInputBuffer(ctx->encoder, &lockIn) != NV_ENC_SUCCESS)
        return -2;

    uint32_t pitch = lockIn.pitch;
    uint8_t *dst = (uint8_t*)lockIn.bufferDataPtr;
    uint32_t w = ctx->width;
    uint32_t h = ctx->height;

    /* Copy Y plane */
    if (pitch == w) {
        memcpy(dst, nv12_data, w * h);
    } else {
        for (uint32_t row = 0; row < h; row++)
            memcpy(dst + row * pitch, nv12_data + row * w, w);
    }
    /* Copy UV plane */
    const uint8_t *uv_src = nv12_data + w * h;
    uint8_t *uv_dst = dst + pitch * h;
    uint32_t uv_h = h / 2;
    if (pitch == w) {
        memcpy(uv_dst, uv_src, w * uv_h);
    } else {
        for (uint32_t row = 0; row < uv_h; row++)
            memcpy(uv_dst + row * pitch, uv_src + row * w, w);
    }

    ctx->funcs.nvEncUnlockInputBuffer(ctx->encoder, ctx->input_buf);

    /* Encode */
    NV_ENC_PIC_PARAMS picParams = {0};
    picParams.version = NV_ENC_PIC_PARAMS_VER;
    picParams.inputWidth = w;
    picParams.inputHeight = h;
    picParams.inputPitch = pitch;
    picParams.inputBuffer = ctx->input_buf;
    picParams.outputBitstream = ctx->output_buf;
    picParams.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
    picParams.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    picParams.inputTimeStamp = ctx->pts;
    if (force_keyframe)
        picParams.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;

    if (ctx->funcs.nvEncEncodePicture(ctx->encoder, &picParams) != NV_ENC_SUCCESS)
        return -3;

    /* Lock bitstream */
    NV_ENC_LOCK_BITSTREAM lockBs = {0};
    lockBs.version = NV_ENC_LOCK_BITSTREAM_VER;
    lockBs.outputBitstream = ctx->output_buf;
    if (ctx->funcs.nvEncLockBitstream(ctx->encoder, &lockBs) != NV_ENC_SUCCESS)
        return -4;

    out->data = (const uint8_t*)lockBs.bitstreamBufferPtr;
    out->size = lockBs.bitstreamSizeInBytes;
    out->is_keyframe = (lockBs.pictureType == NV_ENC_PIC_TYPE_IDR ||
                        lockBs.pictureType == NV_ENC_PIC_TYPE_I);
    out->pts = ctx->pts;
    ctx->pts++;

    /* NOTE: bitstream stays locked — caller must call nvenc_unlock_bitstream after copying */
    return 0;
}

void nvenc_unlock_bitstream(nvenc_ctx_t *ctx) {
    if (ctx) ctx->funcs.nvEncUnlockBitstream(ctx->encoder, ctx->output_buf);
}

void nvenc_destroy(nvenc_ctx_t *ctx) {
    if (!ctx) return;
    if (ctx->encoder) {
        if (ctx->input_buf) ctx->funcs.nvEncDestroyInputBuffer(ctx->encoder, ctx->input_buf);
        if (ctx->output_buf) ctx->funcs.nvEncDestroyBitstreamBuffer(ctx->encoder, ctx->output_buf);
        ctx->funcs.nvEncDestroyEncoder(ctx->encoder);
    }
    if (ctx->cuda_ctx) ctx->cuCtxDestroy(ctx->cuda_ctx);
    if (ctx->cuda_lib) dlclose(ctx->cuda_lib);
    if (ctx->nvenc_lib) dlclose(ctx->nvenc_lib);
    free(ctx);
}

/* Probe: returns bitmask of supported codecs. bit 0 = H.264, bit 1 = AV1.
 * For each codec, creates an encoder AND test-encodes a blank frame to
 * verify the hardware can actually produce valid output (some drivers
 * accept AV1 session creation on GPUs without a hardware AV1 encoder). */
int nvenc_probe_codecs(void) {
    int result = 0;

    /* Try H.264 — create + test encode */
    nvenc_ctx_t *ctx = nvenc_create(256, 256, 30, 2000000, DDISPLAY_CODEC_H264);
    if (ctx) {
        uint32_t nv12_sz = 256 * 256 + 256 * 128; /* Y + UV */
        uint8_t *nv12 = (uint8_t*)calloc(1, nv12_sz);
        if (nv12) {
            nvenc_frame_t out = {0};
            if (nvenc_encode(ctx, nv12, 1, &out) == 0 && out.size > 0) {
                nvenc_unlock_bitstream(ctx);
                result |= 1;
            }
            free(nv12);
        }
        nvenc_destroy(ctx);
    }

    /* Try AV1 — create + test encode */
    ctx = nvenc_create(256, 256, 30, 2000000, DDISPLAY_CODEC_AV1);
    if (ctx) {
        uint32_t nv12_sz = 256 * 256 + 256 * 128;
        uint8_t *nv12 = (uint8_t*)calloc(1, nv12_sz);
        if (nv12) {
            nvenc_frame_t out = {0};
            if (nvenc_encode(ctx, nv12, 1, &out) == 0 && out.size > 0) {
                nvenc_unlock_bitstream(ctx);
                result |= 2;
            }
            free(nv12);
        }
        nvenc_destroy(ctx);
    }

    return result;
}

/* Legacy probe for backward compat */
int nvenc_probe(void) {
    return nvenc_probe_codecs() != 0;
}
