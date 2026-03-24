pub mod sys;

use anyhow::{Context, Result};
use libloading::Library;
use std::ptr;

/// CUDA context wrapper that manages initialization and cleanup.
pub struct CudaContext {
    _lib: Library,
    context: sys::CUcontext,
    device: sys::CUdevice,
    // Function pointers we need to keep for cleanup
    cu_ctx_destroy: sys::CuCtxDestroyFn,
}

unsafe impl Send for CudaContext {}

impl CudaContext {
    /// Initialize CUDA and create a context on device 0.
    pub fn new() -> Result<Self> {
        unsafe {
            let lib = Library::new("libcuda.so.1")
                .or_else(|_| Library::new("libcuda.so"))
                .context("Failed to load libcuda.so")?;

            // Load function pointers
            let cu_init: sys::CuInitFn = *lib.get(b"cuInit\0")
                .context("Failed to find cuInit")?;
            let cu_device_get: sys::CuDeviceGetFn = *lib.get(b"cuDeviceGet\0")
                .context("Failed to find cuDeviceGet")?;
            let cu_ctx_create: sys::CuCtxCreateFn = *lib.get(b"cuCtxCreate_v2\0")
                .context("Failed to find cuCtxCreate_v2")?;
            let cu_ctx_destroy: sys::CuCtxDestroyFn = *lib.get(b"cuCtxDestroy_v2\0")
                .context("Failed to find cuCtxDestroy_v2")?;

            // Initialize CUDA
            let result = cu_init(0);
            if result != 0 {
                anyhow::bail!("cuInit failed with error code {}", result);
            }

            // Get device 0
            let mut device: sys::CUdevice = 0;
            let result = cu_device_get(&mut device, 0);
            if result != 0 {
                anyhow::bail!("cuDeviceGet failed with error code {}", result);
            }

            // Create context
            let mut context: sys::CUcontext = ptr::null_mut();
            let result = cu_ctx_create(&mut context, 0, device);
            if result != 0 {
                anyhow::bail!("cuCtxCreate failed with error code {}", result);
            }

            tracing::info!("CUDA context created successfully on device {}", device);

            Ok(CudaContext {
                _lib: lib,
                context,
                device,
                cu_ctx_destroy,
            })
        }
    }

    /// Get the raw CUDA context pointer (for NVENC).
    pub fn as_ptr(&self) -> sys::CUcontext {
        self.context
    }

    /// Get the CUDA device ordinal.
    pub fn device(&self) -> sys::CUdevice {
        self.device
    }
}

impl Drop for CudaContext {
    fn drop(&mut self) {
        unsafe {
            (self.cu_ctx_destroy)(self.context);
        }
    }
}
