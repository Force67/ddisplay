//! GPU-resident Media Foundation/D3D11 decode to wgpu/DX12 rendering.
//!
//! MF owns its decode surfaces, so each output is copied once on the GPU into
//! a small ring of shared NV12 textures. Shared fences order D3D11 writes and
//! DX12 sampling without a CPU wait or pixel readback.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, LUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence,
    ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_FEATURE_D3D11_OPTIONS4,
    D3D11_FEATURE_DATA_D3D11_OPTIONS4, D3D11_FENCE_FLAG_SHARED, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Direct3D12::{
    ID3D12CommandQueue, ID3D12Device, ID3D12Fence, ID3D12Resource,
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};

// Displayed frame, mailbox frame, and a drain pass holding one decoded frame
// while copying the next, so a newer frame never finds the ring full.
const SLOT_COUNT: usize = 4;
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Adapter identity copied from the renderer's actual DX12 device.
#[derive(Clone, Copy, Debug)]
pub struct Dx12DecodeConfig {
    luid_low: u32,
    luid_high: i32,
}

impl Dx12DecodeConfig {
    pub fn luid(self) -> LUID {
        LUID {
            LowPart: self.luid_low,
            HighPart: self.luid_high,
        }
    }
}

struct SharedNtHandle(HANDLE);

impl SharedNtHandle {
    fn new(handle: HANDLE) -> Result<Self> {
        if handle.is_invalid() {
            bail!("invalid shared handle");
        }
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

// HANDLE is an opaque kernel object handle. Access is synchronized by the
// D3D fences represented by these handles.
unsafe impl Send for SharedNtHandle {}
unsafe impl Sync for SharedNtHandle {}

impl Drop for SharedNtHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct SharedNv12Generation {
    id: u64,
    width: u32,
    height: u32,
    texture_handles: [SharedNtHandle; SLOT_COUNT],
    ready_fence_handle: SharedNtHandle,
    done_fence_handle: SharedNtHandle,
    leases: [AtomicU32; SLOT_COUNT],
    done_values: [AtomicU64; SLOT_COUNT],
    failed: AtomicBool,
}

/// A lease on one shared ring slot. The decoder cannot overwrite the slot
/// while a queued or currently displayed frame holds a lease.
pub struct SharedNv12Frame {
    generation: Arc<SharedNv12Generation>,
    slot: usize,
    ready_value: u64,
    pub visible_width: u32,
    pub visible_height: u32,
}

impl SharedNv12Frame {
    pub fn generation_id(&self) -> u64 {
        self.generation.id
    }

    pub fn slot(&self) -> usize {
        self.slot
    }

    pub fn allocation_size(&self) -> (u32, u32) {
        (self.generation.width, self.generation.height)
    }

    pub fn mark_failed(&self) {
        self.generation.failed.store(true, Ordering::Release);
    }
}

impl Clone for SharedNv12Frame {
    fn clone(&self) -> Self {
        self.generation.leases[self.slot].fetch_add(1, Ordering::AcqRel);
        Self {
            generation: self.generation.clone(),
            slot: self.slot,
            ready_value: self.ready_value,
            visible_width: self.visible_width,
            visible_height: self.visible_height,
        }
    }
}

impl Drop for SharedNv12Frame {
    fn drop(&mut self) {
        self.generation.leases[self.slot].fetch_sub(1, Ordering::AcqRel);
    }
}

/// D3D11 side of the shared texture ring. Used only by the decode thread.
pub struct SharedNv12Producer {
    generation: Arc<SharedNv12Generation>,
    textures: [ID3D11Texture2D; SLOT_COUNT],
    context: ID3D11DeviceContext4,
    ready_fence: ID3D11Fence,
    done_fence: ID3D11Fence,
    next_slot: usize,
    next_ready_value: u64,
}

impl SharedNv12Producer {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            bail!("shared NV12 dimensions must be non-zero and even");
        }

        unsafe {
            let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS4::default();
            device
                .CheckFeatureSupport(
                    D3D11_FEATURE_D3D11_OPTIONS4,
                    &mut options as *mut _ as *mut _,
                    std::mem::size_of_val(&options) as u32,
                )
                .context("query shared NV12 support")?;
            if !options.ExtendedNV12SharedTextureSupported.as_bool() {
                bail!("D3D11 device does not support shared NV12 textures");
            }

            let device5: ID3D11Device5 = device.cast().context("ID3D11Device5")?;
            let context4: ID3D11DeviceContext4 = context.cast().context("ID3D11DeviceContext4")?;

            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                    as u32,
            };

            let mut textures = Vec::with_capacity(SLOT_COUNT);
            let mut texture_handles = Vec::with_capacity(SLOT_COUNT);
            let access = DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0;
            for _ in 0..SLOT_COUNT {
                let mut texture = None;
                device
                    .CreateTexture2D(&desc, None, Some(&mut texture))
                    .context("create shared NV12 texture")?;
                let texture = texture.context("D3D11 returned no shared texture")?;
                let resource: IDXGIResource1 = texture.cast().context("IDXGIResource1")?;
                let handle = resource
                    .CreateSharedHandle(None, access, PCWSTR::null())
                    .context("create NV12 shared handle")?;
                textures.push(texture);
                texture_handles.push(SharedNtHandle::new(handle)?);
            }

            let mut ready_fence = None;
            device5
                .CreateFence::<ID3D11Fence>(0, D3D11_FENCE_FLAG_SHARED, &mut ready_fence)
                .context("create D3D11 ready fence")?;
            let ready_fence = ready_fence.context("D3D11 returned no ready fence")?;
            let ready_fence_handle = SharedNtHandle::new(
                ready_fence
                    .CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())
                    .context("share ready fence")?,
            )?;

            let mut done_fence = None;
            device5
                .CreateFence::<ID3D11Fence>(0, D3D11_FENCE_FLAG_SHARED, &mut done_fence)
                .context("create D3D11 done fence")?;
            let done_fence = done_fence.context("D3D11 returned no done fence")?;
            let done_fence_handle = SharedNtHandle::new(
                done_fence
                    .CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())
                    .context("share done fence")?,
            )?;

            let generation = Arc::new(SharedNv12Generation {
                id: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
                width,
                height,
                texture_handles: texture_handles
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("shared texture ring has wrong size"))?,
                ready_fence_handle,
                done_fence_handle,
                leases: std::array::from_fn(|_| AtomicU32::new(0)),
                done_values: std::array::from_fn(|_| AtomicU64::new(0)),
                failed: AtomicBool::new(false),
            });

            eprintln!(
                "[dx12-video] shared NV12 ring ready: {}x{}, {} slots",
                width, height, SLOT_COUNT
            );

            Ok(Self {
                generation,
                textures: textures.try_into().unwrap(),
                context: context4,
                ready_fence,
                done_fence,
                next_slot: 0,
                next_ready_value: 0,
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.generation.width, self.generation.height)
    }

    /// Queue one GPU-local copy. Returns `None` rather than blocking when all
    /// ring slots are still displayed or in flight.
    pub fn copy_frame(
        &mut self,
        source: &ID3D11Texture2D,
        source_subresource: u32,
        visible_width: u32,
        visible_height: u32,
    ) -> Result<Option<SharedNv12Frame>> {
        if self.generation.failed.load(Ordering::Acquire) {
            bail!("DX12 consumer rejected the shared NV12 generation");
        }

        let completed = unsafe { self.done_fence.GetCompletedValue() };
        let mut selected = None;
        for offset in 0..SLOT_COUNT {
            let slot = (self.next_slot + offset) % SLOT_COUNT;
            let leased = self.generation.leases[slot].load(Ordering::Acquire);
            let required = self.generation.done_values[slot].load(Ordering::Acquire);
            if leased == 0 && completed >= required {
                if self.generation.leases[slot]
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    selected = Some(slot);
                    break;
                }
            }
        }
        let Some(slot) = selected else {
            return Ok(None);
        };

        unsafe {
            self.context.CopySubresourceRegion(
                &self.textures[slot],
                0,
                0,
                0,
                0,
                source,
                source_subresource,
                None,
            );
            self.next_ready_value += 1;
            if let Err(e) = self
                .context
                .Signal(&self.ready_fence, self.next_ready_value)
            {
                self.generation.leases[slot].store(0, Ordering::Release);
                return Err(e).context("signal shared NV12 ready fence");
            }
            // Submit now: the DX12 queue waits on this fence value, and D3D11
            // would otherwise hold the signal until its next implicit flush.
            self.context.Flush();
        }
        self.next_slot = (slot + 1) % SLOT_COUNT;

        Ok(Some(SharedNv12Frame {
            generation: self.generation.clone(),
            slot,
            ready_value: self.next_ready_value,
            visible_width,
            visible_height,
        }))
    }
}

/// DX12 resources opened from one D3D11 shared ring generation.
pub struct ImportedNv12Generation {
    generation: Arc<SharedNv12Generation>,
    textures: [wgpu::Texture; SLOT_COUNT],
    ready_fence: ID3D12Fence,
    done_fence: ID3D12Fence,
}

impl ImportedNv12Generation {
    pub fn id(&self) -> u64 {
        self.generation.id
    }

    pub fn textures(&self) -> &[wgpu::Texture; SLOT_COUNT] {
        &self.textures
    }
}

/// DX12 side of the bridge. It uses wgpu's own device and queue, so video and
/// egui remain in one submission stream and one swapchain.
pub struct Dx12RenderInterop {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    next_done_value: u64,
}

impl Dx12RenderInterop {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<(Self, Dx12DecodeConfig)> {
        unsafe {
            let hal_device = device
                .as_hal::<wgpu::hal::api::Dx12>()
                .context("wgpu device is not DX12")?;
            let raw_device = hal_device.raw_device().clone();
            drop(hal_device);

            let hal_queue = queue
                .as_hal::<wgpu::hal::api::Dx12>()
                .context("wgpu queue is not DX12")?;
            let raw_queue = hal_queue.as_raw().clone();
            drop(hal_queue);

            let luid = raw_device.GetAdapterLuid();
            let config = Dx12DecodeConfig {
                luid_low: luid.LowPart,
                luid_high: luid.HighPart,
            };
            Ok((
                Self {
                    device: raw_device,
                    queue: raw_queue,
                    next_done_value: 0,
                },
                config,
            ))
        }
    }

    pub fn import_generation(
        &self,
        device: &wgpu::Device,
        frame: &SharedNv12Frame,
    ) -> Result<ImportedNv12Generation> {
        unsafe {
            let mut textures = Vec::with_capacity(SLOT_COUNT);
            for handle in &frame.generation.texture_handles {
                let mut resource: Option<ID3D12Resource> = None;
                self.device
                    .OpenSharedHandle(handle.raw(), &mut resource)
                    .context("open shared NV12 texture on DX12")?;
                let resource = resource.context("DX12 returned no shared NV12 resource")?;
                let desc = resource.GetDesc();
                if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
                    || desc.Format != DXGI_FORMAT_NV12
                    || desc.Width != frame.generation.width as u64
                    || desc.Height != frame.generation.height
                    || desc.MipLevels != 1
                    || desc.DepthOrArraySize != 1
                    || !desc
                        .Flags
                        .contains(D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS)
                {
                    bail!("shared NV12 resource has an incompatible DX12 descriptor: {desc:?}");
                }

                let size = wgpu::Extent3d {
                    width: frame.generation.width,
                    height: frame.generation.height,
                    depth_or_array_layers: 1,
                };
                let hal_texture = wgpu::hal::dx12::Device::texture_from_raw(
                    resource,
                    wgpu::TextureFormat::NV12,
                    wgpu::TextureDimension::D2,
                    size,
                    1,
                    1,
                );
                let texture = device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                    hal_texture,
                    &wgpu::TextureDescriptor {
                        label: Some("mf_shared_nv12"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::NV12,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING,
                        // Plane view formats are selected with TextureAspect,
                        // not declared as whole-texture reinterpretations.
                        view_formats: &[],
                    },
                );
                textures.push(texture);
            }

            let mut ready_fence = None;
            self.device
                .OpenSharedHandle(frame.generation.ready_fence_handle.raw(), &mut ready_fence)
                .context("open DX12 ready fence")?;
            let mut done_fence = None;
            self.device
                .OpenSharedHandle(frame.generation.done_fence_handle.raw(), &mut done_fence)
                .context("open DX12 done fence")?;

            Ok(ImportedNv12Generation {
                generation: frame.generation.clone(),
                textures: textures.try_into().unwrap(),
                ready_fence: ready_fence.context("DX12 returned no ready fence")?,
                done_fence: done_fence.context("DX12 returned no done fence")?,
            })
        }
    }

    pub fn wait_ready(
        &self,
        imported: &ImportedNv12Generation,
        frame: &SharedNv12Frame,
    ) -> Result<()> {
        if imported.id() != frame.generation_id() {
            bail!("shared NV12 generation mismatch");
        }
        unsafe {
            self.queue
                .Wait(&imported.ready_fence, frame.ready_value)
                .context("queue DX12 wait for decoded frame")?;
        }
        Ok(())
    }

    /// Signal after wgpu submitted every command that may sample this slot.
    pub fn signal_done(
        &mut self,
        imported: &ImportedNv12Generation,
        frame: &SharedNv12Frame,
    ) -> Result<()> {
        if imported.id() != frame.generation_id() {
            bail!("shared NV12 generation mismatch");
        }
        self.next_done_value += 1;
        unsafe {
            self.queue
                .Signal(&imported.done_fence, self.next_done_value)
                .context("signal DX12 frame completion")?;
        }
        frame.generation.done_values[frame.slot].store(self.next_done_value, Ordering::Release);
        Ok(())
    }
}
