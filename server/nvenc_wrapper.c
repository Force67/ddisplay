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
typedef void* CUmodule;
typedef void* CUfunction;
typedef unsigned long long CUdeviceptr;

typedef CUresult (*PFN_cuInit)(unsigned);
typedef CUresult (*PFN_cuDeviceGet)(CUdevice*, int);
typedef CUresult (*PFN_cuCtxCreate)(CUcontext*, unsigned, CUdevice);
typedef CUresult (*PFN_cuCtxDestroy)(CUcontext);
typedef CUresult (*PFN_cuCtxSetCurrent)(CUcontext);
typedef CUresult (*PFN_cuCtxSynchronize)(void);
typedef CUresult (*PFN_cuMemAlloc)(CUdeviceptr*, size_t);
typedef CUresult (*PFN_cuMemAllocPitch)(CUdeviceptr*, size_t*, size_t, size_t, unsigned);
typedef CUresult (*PFN_cuMemFree)(CUdeviceptr);
typedef CUresult (*PFN_cuMemcpyHtoD)(CUdeviceptr, const void*, size_t);
typedef CUresult (*PFN_cuModuleLoadData)(CUmodule*, const void*);
typedef CUresult (*PFN_cuModuleUnload)(CUmodule);
typedef CUresult (*PFN_cuModuleGetFunction)(CUfunction*, CUmodule, const char*);
typedef CUresult (*PFN_cuMemHostRegister)(void*, size_t, unsigned);
typedef CUresult (*PFN_cuMemHostUnregister)(void*);
typedef CUresult (*PFN_cuMemHostGetDevicePointer)(CUdeviceptr*, void*, unsigned);
#define DD_CU_MEMHOSTREGISTER_DEVICEMAP 0x02
typedef CUresult (*PFN_cuLaunchKernel)(CUfunction,
    unsigned, unsigned, unsigned, unsigned, unsigned, unsigned,
    unsigned, void*, void**, void**);

/* ---- NVRTC (runtime kernel compilation, loaded via dlopen) ---- */
typedef int nvrtcResult;
typedef void* nvrtcProgram;
typedef nvrtcResult (*PFN_nvrtcCreateProgram)(nvrtcProgram*, const char*, const char*, int, const char**, const char**);
typedef nvrtcResult (*PFN_nvrtcCompileProgram)(nvrtcProgram, int, const char**);
typedef nvrtcResult (*PFN_nvrtcGetPTXSize)(nvrtcProgram, size_t*);
typedef nvrtcResult (*PFN_nvrtcGetPTX)(nvrtcProgram, char*);
typedef nvrtcResult (*PFN_nvrtcGetProgramLogSize)(nvrtcProgram, size_t*);
typedef nvrtcResult (*PFN_nvrtcGetProgramLog)(nvrtcProgram, char*);
typedef nvrtcResult (*PFN_nvrtcDestroyProgram)(nvrtcProgram*);

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
    uint32_t fps;
    uint64_t pts;
    /* Persistent copies of the init parameters so the encoder can be
     * reconfigured (bitrate changes) without a full teardown. */
    NV_ENC_INITIALIZE_PARAMS init_params;
    NV_ENC_CONFIG config;

    /* ---- GPU BGRA->NV12 conversion (optional fast path) ---- */
    int gpu_convert;            /* 1 when the CUDA conversion path is usable */
    void *nvrtc_lib;
    PFN_cuCtxSetCurrent cuCtxSetCurrent;
    PFN_cuCtxSynchronize cuCtxSynchronize;
    PFN_cuMemFree cuMemFree;
    PFN_cuMemcpyHtoD cuMemcpyHtoD;
    PFN_cuModuleUnload cuModuleUnload;
    PFN_cuLaunchKernel cuLaunchKernel;
    CUmodule conv_module;
    CUfunction conv_kernel;
    CUdeviceptr d_bgra;         /* device staging buffer for the raw frame */
    size_t d_bgra_size;
    CUdeviceptr d_nv12;         /* kernel output, registered with NVENC */
    size_t d_nv12_pitch;
    NV_ENC_REGISTERED_PTR nv12_registered;
    /* Zero-copy source: the caller's frame buffer (X11 SHM or a capture
     * double-buffer) registered with CUDA so the kernel reads it directly —
     * skips the HtoD copy entirely. Effective on coherent unified-memory
     * systems (Grace/GB10). Small cache because capture backends may
     * alternate between a few stable buffers (Wayland double-buffers);
     * registration is expensive so it must not happen per frame. */
    PFN_cuMemHostRegister cuMemHostRegister;
    PFN_cuMemHostUnregister cuMemHostUnregister;
    PFN_cuMemHostGetDevicePointer cuMemHostGetDevicePointer;
#define DD_HOST_REG_SLOTS 4
    struct {
        const void *host_ptr;   /* registered host buffer (or NULL) */
        size_t size;
        CUdeviceptr dev_ptr;    /* device alias */
    } host_reg[DD_HOST_REG_SLOTS];
    unsigned host_reg_count;    /* registrations performed (also evict cursor) */
    int host_reg_failed;        /* registration failed once — stop retrying */
} nvenc_ctx_t;

/* ---- Encode result passed back to Rust ---- */
typedef struct {
    const uint8_t *data;
    uint32_t size;
    int is_keyframe;
    uint64_t pts;
} nvenc_frame_t;

/* ---- GPU BGRA->NV12 conversion ----
 *
 * Compiled at runtime with NVRTC (dlopen'd, so there is no build-time CUDA
 * dependency). Same BT.601 integer math as the CPU converter in color.rs.
 * Each thread handles one 2x2 pixel block (4 luma + 1 chroma pair).
 */
static const char *kConvKernelSrc =
"extern \"C\" __global__ void bgra_to_nv12(\n"
"    const unsigned char* __restrict__ src, int src_pitch,\n"
"    unsigned char* __restrict__ dst, int dst_pitch,\n"
"    int width, int height)\n"
"{\n"
"    int cx = blockIdx.x * blockDim.x + threadIdx.x;\n"
"    int cy = blockIdx.y * blockDim.y + threadIdx.y;\n"
"    int x0 = cx * 2, y0 = cy * 2;\n"
"    if (x0 >= width || y0 >= height) return;\n"
"    int x1 = x0 + 1 < width  ? x0 + 1 : x0;\n"
"    int y1 = y0 + 1 < height ? y0 + 1 : y0;\n"
"    const unsigned char *p00 = src + y0 * src_pitch + x0 * 4;\n"
"    const unsigned char *p10 = src + y0 * src_pitch + x1 * 4;\n"
"    const unsigned char *p01 = src + y1 * src_pitch + x0 * 4;\n"
"    const unsigned char *p11 = src + y1 * src_pitch + x1 * 4;\n"
"    int b00 = p00[0], g00 = p00[1], r00 = p00[2];\n"
"    int b10 = p10[0], g10 = p10[1], r10 = p10[2];\n"
"    int b01 = p01[0], g01 = p01[1], r01 = p01[2];\n"
"    int b11 = p11[0], g11 = p11[1], r11 = p11[2];\n"
"    unsigned char *yrow0 = dst + y0 * dst_pitch;\n"
"    unsigned char *yrow1 = dst + y1 * dst_pitch;\n"
"    yrow0[x0] = (unsigned char)((( 66*r00 + 129*g00 +  25*b00 + 128) >> 8) + 16);\n"
"    yrow0[x1] = (unsigned char)((( 66*r10 + 129*g10 +  25*b10 + 128) >> 8) + 16);\n"
"    yrow1[x0] = (unsigned char)((( 66*r01 + 129*g01 +  25*b01 + 128) >> 8) + 16);\n"
"    yrow1[x1] = (unsigned char)((( 66*r11 + 129*g11 +  25*b11 + 128) >> 8) + 16);\n"
"    int r = (r00 + r10 + r01 + r11 + 2) >> 2;\n"
"    int g = (g00 + g10 + g01 + g11 + 2) >> 2;\n"
"    int b = (b00 + b10 + b01 + b11 + 2) >> 2;\n"
"    unsigned char *uv = dst + (size_t)dst_pitch * height + cy * dst_pitch + cx * 2;\n"
"    uv[0] = (unsigned char)(((-38*r -  74*g + 112*b + 128) >> 8) + 128);\n"
"    uv[1] = (unsigned char)(((112*r -  94*g -  18*b + 128) >> 8) + 128);\n"
"}\n";

/* Try to bring up the CUDA conversion path. Any failure leaves the context
 * fully functional on the CPU path (gpu_convert stays 0). */
static void setup_gpu_convert(nvenc_ctx_t *ctx) {
    if (getenv("DDISPLAY_NO_GPU_CONVERT")) {
        fprintf(stderr, "[nvenc] GPU conversion disabled by env\n");
        return;
    }

    PFN_cuMemAllocPitch cuMemAllocPitch =
        (PFN_cuMemAllocPitch)dlsym(ctx->cuda_lib, "cuMemAllocPitch_v2");
    PFN_cuMemAlloc cuMemAlloc = (PFN_cuMemAlloc)dlsym(ctx->cuda_lib, "cuMemAlloc_v2");
    PFN_cuModuleLoadData cuModuleLoadData =
        (PFN_cuModuleLoadData)dlsym(ctx->cuda_lib, "cuModuleLoadData");
    PFN_cuModuleGetFunction cuModuleGetFunction =
        (PFN_cuModuleGetFunction)dlsym(ctx->cuda_lib, "cuModuleGetFunction");
    ctx->cuCtxSetCurrent = (PFN_cuCtxSetCurrent)dlsym(ctx->cuda_lib, "cuCtxSetCurrent");
    ctx->cuCtxSynchronize = (PFN_cuCtxSynchronize)dlsym(ctx->cuda_lib, "cuCtxSynchronize");
    ctx->cuMemFree = (PFN_cuMemFree)dlsym(ctx->cuda_lib, "cuMemFree_v2");
    ctx->cuMemcpyHtoD = (PFN_cuMemcpyHtoD)dlsym(ctx->cuda_lib, "cuMemcpyHtoD_v2");
    ctx->cuModuleUnload = (PFN_cuModuleUnload)dlsym(ctx->cuda_lib, "cuModuleUnload");
    ctx->cuLaunchKernel = (PFN_cuLaunchKernel)dlsym(ctx->cuda_lib, "cuLaunchKernel");
    /* Optional zero-copy support (absence only disables the host-register
     * path). DDISPLAY_NO_ZEROCOPY opts out — required when capture buffers
     * are not address-stable for the encoder's lifetime (heap-backed Wayland
     * double buffers can reallocate on resize while still registered). */
    if (getenv("DDISPLAY_NO_ZEROCOPY")) {
        ctx->host_reg_failed = 1;
        fprintf(stderr, "[nvenc] zero-copy source disabled by env\n");
    }
    ctx->cuMemHostRegister =
        (PFN_cuMemHostRegister)dlsym(ctx->cuda_lib, "cuMemHostRegister_v2");
    if (!ctx->cuMemHostRegister)
        ctx->cuMemHostRegister = (PFN_cuMemHostRegister)dlsym(ctx->cuda_lib, "cuMemHostRegister");
    ctx->cuMemHostUnregister =
        (PFN_cuMemHostUnregister)dlsym(ctx->cuda_lib, "cuMemHostUnregister");
    ctx->cuMemHostGetDevicePointer =
        (PFN_cuMemHostGetDevicePointer)dlsym(ctx->cuda_lib, "cuMemHostGetDevicePointer_v2");
    if (!cuMemAllocPitch || !cuMemAlloc || !cuModuleLoadData || !cuModuleGetFunction ||
        !ctx->cuCtxSetCurrent || !ctx->cuCtxSynchronize || !ctx->cuMemFree ||
        !ctx->cuMemcpyHtoD || !ctx->cuModuleUnload || !ctx->cuLaunchKernel) {
        fprintf(stderr, "[nvenc] GPU conversion unavailable: missing CUDA symbols\n");
        return;
    }

    /* NVRTC: try common sonames */
    const char *nvrtc_names[] = {
        "libnvrtc.so", "libnvrtc.so.13", "libnvrtc.so.12", "libnvrtc.so.11.2", NULL,
    };
    for (int i = 0; nvrtc_names[i] && !ctx->nvrtc_lib; i++)
        ctx->nvrtc_lib = dlopen(nvrtc_names[i], RTLD_LAZY);
    if (!ctx->nvrtc_lib) {
        fprintf(stderr, "[nvenc] GPU conversion unavailable: libnvrtc not found\n");
        return;
    }

    PFN_nvrtcCreateProgram nvrtcCreateProgram =
        (PFN_nvrtcCreateProgram)dlsym(ctx->nvrtc_lib, "nvrtcCreateProgram");
    PFN_nvrtcCompileProgram nvrtcCompileProgram =
        (PFN_nvrtcCompileProgram)dlsym(ctx->nvrtc_lib, "nvrtcCompileProgram");
    PFN_nvrtcGetPTXSize nvrtcGetPTXSize =
        (PFN_nvrtcGetPTXSize)dlsym(ctx->nvrtc_lib, "nvrtcGetPTXSize");
    PFN_nvrtcGetPTX nvrtcGetPTX = (PFN_nvrtcGetPTX)dlsym(ctx->nvrtc_lib, "nvrtcGetPTX");
    PFN_nvrtcGetProgramLogSize nvrtcGetProgramLogSize =
        (PFN_nvrtcGetProgramLogSize)dlsym(ctx->nvrtc_lib, "nvrtcGetProgramLogSize");
    PFN_nvrtcGetProgramLog nvrtcGetProgramLog =
        (PFN_nvrtcGetProgramLog)dlsym(ctx->nvrtc_lib, "nvrtcGetProgramLog");
    PFN_nvrtcDestroyProgram nvrtcDestroyProgram =
        (PFN_nvrtcDestroyProgram)dlsym(ctx->nvrtc_lib, "nvrtcDestroyProgram");
    if (!nvrtcCreateProgram || !nvrtcCompileProgram || !nvrtcGetPTXSize ||
        !nvrtcGetPTX || !nvrtcDestroyProgram) {
        fprintf(stderr, "[nvenc] GPU conversion unavailable: missing NVRTC symbols\n");
        return;
    }

    /* Compile the kernel to PTX (driver JITs it for the actual GPU) */
    nvrtcProgram prog = NULL;
    if (nvrtcCreateProgram(&prog, kConvKernelSrc, "ddisplay_conv.cu", 0, NULL, NULL) != 0)
        return;
    char *ptx = NULL;
    if (nvrtcCompileProgram(prog, 0, NULL) != 0) {
        if (nvrtcGetProgramLogSize && nvrtcGetProgramLog) {
            size_t log_size = 0;
            nvrtcGetProgramLogSize(prog, &log_size);
            char *log = malloc(log_size + 1);
            if (log) {
                nvrtcGetProgramLog(prog, log);
                log[log_size] = 0;
                fprintf(stderr, "[nvenc] kernel compile failed:\n%s\n", log);
                free(log);
            }
        }
        nvrtcDestroyProgram(&prog);
        return;
    }
    size_t ptx_size = 0;
    nvrtcGetPTXSize(prog, &ptx_size);
    ptx = malloc(ptx_size + 1);
    if (!ptx || nvrtcGetPTX(prog, ptx) != 0) {
        free(ptx);
        nvrtcDestroyProgram(&prog);
        return;
    }
    nvrtcDestroyProgram(&prog);

    ctx->cuCtxSetCurrent(ctx->cuda_ctx);
    if (cuModuleLoadData(&ctx->conv_module, ptx) != 0) {
        fprintf(stderr, "[nvenc] cuModuleLoadData failed\n");
        free(ptx);
        return;
    }
    free(ptx);
    if (cuModuleGetFunction(&ctx->conv_kernel, ctx->conv_module, "bgra_to_nv12") != 0) {
        ctx->cuModuleUnload(ctx->conv_module);
        ctx->conv_module = NULL;
        return;
    }

    /* Device buffers: raw BGRA staging + pitched NV12 output */
    ctx->d_bgra_size = (size_t)ctx->width * 4 * ctx->height;
    if (cuMemAlloc(&ctx->d_bgra, ctx->d_bgra_size) != 0)
        goto fail_module;
    size_t pitch = 0;
    size_t nv12_rows = (size_t)ctx->height + ctx->height / 2;
    if (cuMemAllocPitch(&ctx->d_nv12, &pitch, ctx->width, nv12_rows, 16) != 0)
        goto fail_bgra;
    ctx->d_nv12_pitch = pitch;

    /* Register the NV12 device buffer as an NVENC input resource */
    NV_ENC_REGISTER_RESOURCE reg = {0};
    reg.version = NV_ENC_REGISTER_RESOURCE_VER;
    reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
    reg.width = ctx->width;
    reg.height = ctx->height;
    reg.pitch = (uint32_t)pitch;
    reg.resourceToRegister = (void*)(uintptr_t)ctx->d_nv12;
    reg.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
    reg.bufferUsage = NV_ENC_INPUT_IMAGE;
    if (ctx->funcs.nvEncRegisterResource(ctx->encoder, &reg) != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] nvEncRegisterResource(CUDA) failed\n");
        goto fail_nv12;
    }
    ctx->nv12_registered = reg.registeredResource;

    ctx->gpu_convert = 1;
    fprintf(stderr, "[nvenc] GPU BGRA->NV12 conversion enabled (pitch=%zu)\n", pitch);
    return;

fail_nv12:
    ctx->cuMemFree(ctx->d_nv12);
    ctx->d_nv12 = 0;
fail_bgra:
    ctx->cuMemFree(ctx->d_bgra);
    ctx->d_bgra = 0;
fail_module:
    ctx->cuModuleUnload(ctx->conv_module);
    ctx->conv_module = NULL;
}

/* ---- Public API ---- */

nvenc_ctx_t* nvenc_create(uint32_t width, uint32_t height, uint32_t fps, uint32_t bitrate, uint32_t codec) {
    nvenc_ctx_t *ctx = calloc(1, sizeof(nvenc_ctx_t));
    if (!ctx) return NULL;
    ctx->width = width;
    ctx->height = height;
    ctx->codec = codec;
    ctx->fps = fps;

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
    /* Single-frame VBV: the rate controller may never buffer more than one
     * frame's worth of bits, which caps the worst-case send burst and keeps
     * per-frame latency flat (the classic low-latency CBR configuration). */
    cfg->rcParams.vbvBufferSize = bitrate / (fps ? fps : 60);
    cfg->rcParams.vbvInitialDelay = cfg->rcParams.vbvBufferSize;
    /* Spatial AQ shifts bits towards visually complex regions (text, UI
     * edges) which noticeably improves desktop content at a fixed bitrate. */
    cfg->rcParams.enableAQ = 1;

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

    /* Initialize encoder. Keep persistent copies of the parameters in the
     * context so nvenc_set_bitrate() can reconfigure without re-init. */
    ctx->config = *cfg;
    memset(&ctx->init_params, 0, sizeof(ctx->init_params));
    ctx->init_params.version = NV_ENC_INITIALIZE_PARAMS_VER;
    ctx->init_params.encodeGUID = codec_guid;
    ctx->init_params.presetGUID = NV_ENC_PRESET_P1_GUID;
    ctx->init_params.encodeWidth = width;
    ctx->init_params.encodeHeight = height;
    ctx->init_params.darWidth = width;
    ctx->init_params.darHeight = height;
    ctx->init_params.frameRateNum = fps;
    ctx->init_params.frameRateDen = 1;
    ctx->init_params.enablePTD = 1;
    ctx->init_params.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    ctx->init_params.encodeConfig = &ctx->config;
    if (ctx->funcs.nvEncInitializeEncoder(ctx->encoder, &ctx->init_params) != NV_ENC_SUCCESS)
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

    /* Optional: GPU color conversion (falls back to the CPU path silently) */
    setup_gpu_convert(ctx);

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

/* Submit one picture and lock the resulting bitstream into `out`.
 * The bitstream stays locked — caller must call nvenc_unlock_bitstream. */
static int submit_and_lock(nvenc_ctx_t *ctx, NV_ENC_INPUT_PTR input, uint32_t pitch,
                           int force_keyframe, nvenc_frame_t *out) {
    NV_ENC_PIC_PARAMS picParams = {0};
    picParams.version = NV_ENC_PIC_PARAMS_VER;
    picParams.inputWidth = ctx->width;
    picParams.inputHeight = ctx->height;
    picParams.inputPitch = pitch;
    picParams.inputBuffer = input;
    picParams.outputBitstream = ctx->output_buf;
    picParams.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
    picParams.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    picParams.inputTimeStamp = ctx->pts;
    if (force_keyframe)
        picParams.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;

    if (ctx->funcs.nvEncEncodePicture(ctx->encoder, &picParams) != NV_ENC_SUCCESS)
        return -3;

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
    return 0;
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

    return submit_and_lock(ctx, ctx->input_buf, pitch, force_keyframe, out);
}

/* 1 if the CUDA BGRA->NV12 fast path is active for this context. */
int nvenc_has_gpu_convert(nvenc_ctx_t *ctx) {
    return ctx && ctx->gpu_convert;
}

/* Encode straight from a raw BGRA frame: upload to the GPU, convert with the
 * CUDA kernel, and feed NVENC the device buffer. No CPU pixel work at all. */
int nvenc_encode_bgra(nvenc_ctx_t *ctx, const uint8_t *bgra, uint32_t src_stride,
                      int force_keyframe, nvenc_frame_t *out) {
    if (!ctx || !bgra || !out) return -1;
    if (!ctx->gpu_convert) return -100;
    if ((size_t)src_stride * ctx->height > ctx->d_bgra_size) return -101;

    ctx->cuCtxSetCurrent(ctx->cuda_ctx);

    size_t src_size = (size_t)src_stride * ctx->height;

    /* Zero-copy fast path: register the caller's buffer with CUDA once (per
     * distinct buffer — capture backends may double-buffer) and let the
     * kernel read it directly. Falls back to HtoD copy. */
    CUdeviceptr src_dev = 0;
    if (ctx->cuMemHostRegister && ctx->cuMemHostUnregister &&
        ctx->cuMemHostGetDevicePointer && !ctx->host_reg_failed) {
        int slot = -1;
        for (int i = 0; i < DD_HOST_REG_SLOTS; i++) {
            if (ctx->host_reg[i].host_ptr == (const void*)bgra &&
                ctx->host_reg[i].size >= src_size) {
                slot = i;
                break;
            }
        }
        if (slot < 0) {
            /* Not registered yet — evict round-robin if all slots are used. */
            slot = (int)(ctx->host_reg_count % DD_HOST_REG_SLOTS);
            if (ctx->host_reg[slot].host_ptr)
                ctx->cuMemHostUnregister((void*)ctx->host_reg[slot].host_ptr);
            ctx->host_reg[slot].host_ptr = NULL;
            if (ctx->cuMemHostRegister((void*)bgra, src_size,
                                       DD_CU_MEMHOSTREGISTER_DEVICEMAP) == 0 &&
                ctx->cuMemHostGetDevicePointer(&ctx->host_reg[slot].dev_ptr,
                                               (void*)bgra, 0) == 0) {
                ctx->host_reg[slot].host_ptr = bgra;
                ctx->host_reg[slot].size = src_size;
                ctx->host_reg_count++;
                fprintf(stderr, "[nvenc] zero-copy source enabled (host buffer %d mapped)\n", slot);
            } else {
                slot = -1;
                ctx->host_reg_failed = 1;
                fprintf(stderr, "[nvenc] host buffer registration failed; using HtoD copy\n");
            }
        }
        if (slot >= 0)
            src_dev = ctx->host_reg[slot].dev_ptr;
    }

    if (!src_dev) {
        if (ctx->cuMemcpyHtoD(ctx->d_bgra, bgra, src_size) != 0)
            return -102;
        src_dev = ctx->d_bgra;
    }

    int w = (int)ctx->width, h = (int)ctx->height;
    int sp = (int)src_stride, dp = (int)ctx->d_nv12_pitch;
    void *args[] = { &src_dev, &sp, &ctx->d_nv12, &dp, &w, &h };
    unsigned grid_x = ((ctx->width  / 2) + 15) / 16;
    unsigned grid_y = ((ctx->height / 2) + 15) / 16;
    if (ctx->cuLaunchKernel(ctx->conv_kernel, grid_x, grid_y, 1, 16, 16, 1,
                            0, NULL, args, NULL) != 0)
        return -103;
    if (ctx->cuCtxSynchronize() != 0)
        return -104;

    /* Map the registered device buffer as the NVENC input for this frame */
    NV_ENC_MAP_INPUT_RESOURCE map = {0};
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = ctx->nv12_registered;
    if (ctx->funcs.nvEncMapInputResource(ctx->encoder, &map) != NV_ENC_SUCCESS)
        return -105;

    int rc = submit_and_lock(ctx, map.mappedResource, (uint32_t)ctx->d_nv12_pitch,
                             force_keyframe, out);
    ctx->funcs.nvEncUnmapInputResource(ctx->encoder, map.mappedResource);
    return rc;
}

void nvenc_unlock_bitstream(nvenc_ctx_t *ctx) {
    if (ctx) ctx->funcs.nvEncUnlockBitstream(ctx->encoder, ctx->output_buf);
}

/* Live bitrate change via NvEncReconfigureEncoder — no teardown, no IDR,
 * no visible glitch. Returns 0 on success. */
int nvenc_set_bitrate(nvenc_ctx_t *ctx, uint32_t bitrate) {
    if (!ctx || !ctx->encoder) return -1;

    ctx->config.rcParams.averageBitRate = bitrate;
    ctx->config.rcParams.maxBitRate = bitrate;
    ctx->config.rcParams.vbvBufferSize = bitrate / (ctx->fps ? ctx->fps : 60);
    ctx->config.rcParams.vbvInitialDelay = ctx->config.rcParams.vbvBufferSize;

    NV_ENC_RECONFIGURE_PARAMS rp = {0};
    rp.version = NV_ENC_RECONFIGURE_PARAMS_VER;
    rp.reInitEncodeParams = ctx->init_params;
    rp.reInitEncodeParams.encodeConfig = &ctx->config;
    rp.resetEncoder = 0;
    rp.forceIDR = 0;

    if (ctx->funcs.nvEncReconfigureEncoder(ctx->encoder, &rp) != NV_ENC_SUCCESS)
        return -2;
    return 0;
}

void nvenc_destroy(nvenc_ctx_t *ctx) {
    if (!ctx) return;
    if (ctx->encoder) {
        if (ctx->nv12_registered)
            ctx->funcs.nvEncUnregisterResource(ctx->encoder, ctx->nv12_registered);
        if (ctx->input_buf) ctx->funcs.nvEncDestroyInputBuffer(ctx->encoder, ctx->input_buf);
        if (ctx->output_buf) ctx->funcs.nvEncDestroyBitstreamBuffer(ctx->encoder, ctx->output_buf);
        ctx->funcs.nvEncDestroyEncoder(ctx->encoder);
    }
    if (ctx->gpu_convert || ctx->conv_module) {
        if (ctx->cuCtxSetCurrent) ctx->cuCtxSetCurrent(ctx->cuda_ctx);
        for (int i = 0; i < DD_HOST_REG_SLOTS; i++)
            if (ctx->host_reg[i].host_ptr)
                ctx->cuMemHostUnregister((void*)ctx->host_reg[i].host_ptr);
        if (ctx->d_bgra) ctx->cuMemFree(ctx->d_bgra);
        if (ctx->d_nv12) ctx->cuMemFree(ctx->d_nv12);
        if (ctx->conv_module) ctx->cuModuleUnload(ctx->conv_module);
    }
    if (ctx->cuda_ctx) ctx->cuCtxDestroy(ctx->cuda_ctx);
    if (ctx->nvrtc_lib) dlclose(ctx->nvrtc_lib);
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
