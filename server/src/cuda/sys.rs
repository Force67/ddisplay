/// Minimal CUDA Driver API type definitions for dynamic loading.
///
/// These are the bare minimum types and function signatures needed
/// to create a CUDA context for NVENC.

use std::ffi::c_void;

/// CUDA result code
pub type CUresult = i32;

/// CUDA device handle
pub type CUdevice = i32;

/// CUDA context handle (opaque pointer)
pub type CUcontext = *mut c_void;

// Function pointer types for dynamically loaded CUDA functions
pub type CuInitFn = unsafe extern "C" fn(flags: u32) -> CUresult;
pub type CuDeviceGetFn = unsafe extern "C" fn(device: *mut CUdevice, ordinal: i32) -> CUresult;
pub type CuCtxCreateFn = unsafe extern "C" fn(ctx: *mut CUcontext, flags: u32, dev: CUdevice) -> CUresult;
pub type CuCtxDestroyFn = unsafe extern "C" fn(ctx: CUcontext) -> CUresult;
