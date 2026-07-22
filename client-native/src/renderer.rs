/// GPU-accelerated renderer using wgpu.
///
/// Uploads decoded YUV420 planes to persistent GPU textures, converts
/// YUV→RGB in the fragment shader (BT.601 limited range), and renders a
/// letterboxed quad that preserves the remote desktop's aspect ratio.
/// Compared to CPU conversion + RGBA upload this saves a full-frame CPU
/// pass and 62% of the per-frame upload bandwidth.
use anyhow::{Context, Result};
use std::sync::Arc;

#[cfg(windows)]
use crate::decoder::PlaneStorage;
use crate::decoder::{DecodedFrame, FrameFormat};
use crate::overlay::{DisplayMode, EguiRenderData};

#[cfg(windows)]
struct SharedDx12Textures {
    imported: crate::dx12_interop::ImportedNv12Generation,
    bind_groups: Vec<wgpu::BindGroup>,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    /// I420 pipeline: three R8 planes (software decoders).
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    /// NV12 pipeline: R8 luma + Rg8 interleaved chroma (hardware decode).
    pipeline_nv12: wgpu::RenderPipeline,
    bind_group_layout_nv12: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Plane textures for the current format (3 for I420, 2 for NV12).
    plane_textures: Vec<wgpu::Texture>,
    current_bind_group: Option<wgpu::BindGroup>,
    current_format: FrameFormat,
    texture_size: (u32, u32),
    scale_buffer: wgpu::Buffer,
    scale_bind_group: wgpu::BindGroup,
    window_size: (u32, u32),
    remote_size: (u32, u32),
    /// Sub-rect of the decoded frame to sample, in UV space
    /// [u0, v0, u_width, v_height]. Currently always the full frame, since each
    /// monitor head is its own stream.
    crop_uv: [f32; 4],
    display_mode: DisplayMode,
    egui_renderer: egui_wgpu::Renderer,
    surface_format: wgpu::TextureFormat,
    #[cfg(windows)]
    dx12: Option<crate::dx12_interop::Dx12RenderInterop>,
    #[cfg(windows)]
    dx12_decode_config: Option<crate::dx12_interop::Dx12DecodeConfig>,
    #[cfg(windows)]
    shared_dx12: Option<SharedDx12Textures>,
    #[cfg(windows)]
    current_shared_frame: Option<crate::dx12_interop::SharedNv12Frame>,
}

impl Renderer {
    pub async fn new(window: Arc<winit::window::Window>) -> Result<Self> {
        #[cfg(windows)]
        if std::env::var("DDISPLAY_NO_DX12")
            .map(|v| v != "1")
            .unwrap_or(true)
        {
            match Self::new_with_backends(window.clone(), wgpu::Backends::DX12, true).await {
                Ok(renderer) => return Ok(renderer),
                Err(e) => eprintln!(
                    "[dx12-video] DX12 renderer unavailable ({e:#}); using standard wgpu path"
                ),
            }
        }

        Self::new_with_backends(window, wgpu::Backends::all(), false).await
    }

    async fn new_with_backends(
        window: Arc<winit::window::Window>,
        backends: wgpu::Backends,
        request_dx12_video: bool,
    ) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let surface = instance
            .create_surface(window.clone())
            .context("Failed to create wgpu surface")?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("No suitable GPU adapter found")?;

        eprintln!("[gpu] Adapter: {}", adapter.get_info().name);
        eprintln!("[gpu] Backend: {:?}", adapter.get_info().backend);

        let nv12_format = adapter.get_texture_format_features(wgpu::TextureFormat::NV12);
        let dx12_video_supported = request_dx12_video
            && adapter
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
            && nv12_format
                .allowed_usages
                .contains(wgpu::TextureUsages::TEXTURE_BINDING)
            && nv12_format
                .flags
                .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE);
        if request_dx12_video && !dx12_video_supported {
            eprintln!("[dx12-video] adapter lacks shared NV12 texture support");
        }
        let required_features = if dx12_video_supported {
            wgpu::Features::TEXTURE_FORMAT_NV12
        } else {
            wgpu::Features::empty()
        };

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("ddisplay"),
                required_features,
                ..Default::default()
            })
            .await
            .context("Failed to create wgpu device")?;

        #[cfg(windows)]
        let (dx12, dx12_decode_config) = if dx12_video_supported {
            match crate::dx12_interop::Dx12RenderInterop::new(&device, &queue) {
                Ok((interop, config)) => {
                    eprintln!("[dx12-video] shared NV12 interop candidate initialized");
                    (Some(interop), Some(config))
                }
                Err(e) => {
                    eprintln!("[dx12-video] interop initialization failed: {e:#}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let size = window.inner_size();
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        // Prefer Mailbox (triple-buffered, low latency, GPU-paced by display) over
        // Immediate (uncapped, spins GPU at 100%). Fall back through FifoRelaxed to Fifo.
        let present_mode = if surface_caps
            .present_modes
            .contains(&wgpu::PresentMode::Mailbox)
        {
            eprintln!("[gpu] Present mode: Mailbox (low latency, GPU-paced)");
            wgpu::PresentMode::Mailbox
        } else if surface_caps
            .present_modes
            .contains(&wgpu::PresentMode::FifoRelaxed)
        {
            eprintln!("[gpu] Present mode: FifoRelaxed");
            wgpu::PresentMode::FifoRelaxed
        } else {
            eprintln!("[gpu] Present mode: Fifo (vsync)");
            wgpu::PresentMode::Fifo
        };

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &surface_config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fullscreen_quad"),
            source: wgpu::ShaderSource::Wgsl(FULLSCREEN_QUAD_WGSL.into()),
        });
        let shader_nv12 = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fullscreen_quad_nv12"),
            source: wgpu::ShaderSource::Wgsl(FULLSCREEN_QUAD_NV12_WGSL.into()),
        });

        // Bind group 0: plane textures + sampler (3 planes I420, 2 planes NV12)
        let texture_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let sampler_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture_bind_group_layout"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                sampler_entry(3),
            ],
        });
        let bind_group_layout_nv12 =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("texture_bind_group_layout_nv12"),
                entries: &[texture_entry(0), texture_entry(1), sampler_entry(2)],
            });

        // Bind group 1: scale uniform (aspect ratio + srgb flag, read in both stages)
        let scale_bg_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scale_bind_group_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let make_pipeline = |label: &str,
                             layout: &wgpu::BindGroupLayout,
                             module: &wgpu::ShaderModule| {
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(layout), Some(&scale_bg_layout)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: surface_format,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let pipeline = make_pipeline("render_pipeline_i420", &bind_group_layout, &shader);
        let pipeline_nv12 = make_pipeline(
            "render_pipeline_nv12",
            &bind_group_layout_nv12,
            &shader_nv12,
        );

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("frame_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Params uniform: vec4 scale [scale_x, scale_y, srgb_flag, 0] then
        // vec4 crop [u0, v0, u_width, v_height].
        let scale_data: [f32; 8] = [
            1.0,
            1.0,
            if surface_format.is_srgb() { 1.0 } else { 0.0 },
            0.0,
            0.0,
            0.0,
            1.0,
            1.0,
        ];
        let scale_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params_uniform"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&scale_buffer, 0, bytemuck::cast_slice(&scale_data));

        let scale_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scale_bind_group"),
            layout: &scale_bg_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: scale_buffer.as_entire_binding(),
            }],
        });

        let ws = (size.width.max(1), size.height.max(1));

        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            surface_format,
            egui_wgpu::RendererOptions::default(),
        );

        Ok(Self {
            device,
            queue,
            surface,
            surface_config,
            pipeline,
            bind_group_layout,
            pipeline_nv12,
            bind_group_layout_nv12,
            sampler,
            plane_textures: Vec::new(),
            current_bind_group: None,
            current_format: FrameFormat::I420,
            texture_size: (0, 0),
            scale_buffer,
            scale_bind_group,
            window_size: ws,
            remote_size: (0, 0),
            crop_uv: [0.0, 0.0, 1.0, 1.0],
            display_mode: DisplayMode::default(),
            egui_renderer,
            surface_format,
            #[cfg(windows)]
            dx12,
            #[cfg(windows)]
            dx12_decode_config,
            #[cfg(windows)]
            shared_dx12: None,
            #[cfg(windows)]
            current_shared_frame: None,
        })
    }

    #[cfg(windows)]
    pub fn dx12_decode_config(&self) -> Option<crate::dx12_interop::Dx12DecodeConfig> {
        self.dx12_decode_config
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
        self.window_size = (width, height);
        self.update_scale();
    }

    #[cfg(windows)]
    fn upload_shared_nv12(&mut self, frame: &crate::dx12_interop::SharedNv12Frame) {
        let Some(dx12) = &self.dx12 else {
            frame.mark_failed();
            return;
        };

        let needs_import = self.shared_dx12.as_ref().map_or(true, |generation| {
            generation.imported.id() != frame.generation_id()
        });
        if needs_import {
            // HAL imports return normal handles while wgpu validation reports
            // through error scopes. Capture both channels so a driver-specific
            // import failure falls back instead of invoking the panic handler.
            let internal_scope = self.device.push_error_scope(wgpu::ErrorFilter::Internal);
            let oom_scope = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
            let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let candidate: Result<SharedDx12Textures> = (|| {
                let imported = dx12.import_generation(&self.device, frame)?;
                let mut bind_groups = Vec::with_capacity(imported.textures().len());
                for texture in imported.textures() {
                    let y = texture.create_view(&wgpu::TextureViewDescriptor {
                        label: Some("mf_shared_y"),
                        format: Some(wgpu::TextureFormat::R8Unorm),
                        dimension: Some(wgpu::TextureViewDimension::D2),
                        aspect: wgpu::TextureAspect::Plane0,
                        ..Default::default()
                    });
                    let uv = texture.create_view(&wgpu::TextureViewDescriptor {
                        label: Some("mf_shared_uv"),
                        format: Some(wgpu::TextureFormat::Rg8Unorm),
                        dimension: Some(wgpu::TextureViewDimension::D2),
                        aspect: wgpu::TextureAspect::Plane1,
                        ..Default::default()
                    });
                    bind_groups.push(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("mf_shared_nv12_bind_group"),
                        layout: &self.bind_group_layout_nv12,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&y),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(&uv),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                        ],
                    }));
                }
                Ok(SharedDx12Textures {
                    imported,
                    bind_groups,
                })
            })();
            let validation_error = pollster::block_on(validation_scope.pop());
            let oom_error = pollster::block_on(oom_scope.pop());
            let internal_error = pollster::block_on(internal_scope.pop());
            if let Some(error) = validation_error.or(oom_error).or(internal_error) {
                eprintln!("[dx12-video] wgpu import failed: {error}");
                frame.mark_failed();
                return;
            }
            match candidate {
                Ok(generation) => {
                    eprintln!("[dx12-video] shared NV12 presentation active");
                    self.shared_dx12 = Some(generation);
                }
                Err(e) => {
                    eprintln!("[dx12-video] texture import failed: {e:#}");
                    frame.mark_failed();
                    return;
                }
            };
        }

        let generation = self.shared_dx12.as_ref().unwrap();
        if let Err(e) = dx12.wait_ready(&generation.imported, frame) {
            eprintln!("[dx12-video] ready wait failed: {e:#}");
            frame.mark_failed();
            return;
        }

        let (allocation_w, allocation_h) = frame.allocation_size();
        self.current_bind_group = Some(generation.bind_groups[frame.slot()].clone());
        self.current_shared_frame = Some(frame.clone());
        self.plane_textures.clear();
        self.current_format = FrameFormat::Nv12;
        self.texture_size = (allocation_w, allocation_h);
        // update_scale derives visible aspect from allocation size * crop.
        self.remote_size = (allocation_w, allocation_h);
        self.crop_uv = [
            0.0,
            0.0,
            frame.visible_width as f32 / allocation_w as f32,
            frame.visible_height as f32 / allocation_h as f32,
        ];
        self.update_scale();
    }

    /// Upload a decoded frame to the GPU — three R8 planes for I420
    /// (software decoders) or R8 + Rg8 for NV12 (hardware decode). The
    /// YUV→RGB conversion always runs in the fragment shader.
    pub fn upload_frame(&mut self, frame: &DecodedFrame) {
        #[cfg(windows)]
        if let PlaneStorage::Dx12(shared) = &frame.storage {
            self.upload_shared_nv12(shared);
            return;
        }

        #[cfg(windows)]
        if self.current_shared_frame.take().is_some() {
            self.current_bind_group = None;
            self.shared_dx12 = None;
            self.texture_size = (0, 0);
        }
        self.crop_uv = [0.0, 0.0, 1.0, 1.0];

        let (width, height) = (frame.width, frame.height);
        if width == 0 || height == 0 {
            return;
        }
        let format = frame.format();
        let chroma_w = width.div_ceil(2);
        let chroma_h = height.div_ceil(2);

        // Recreate textures when dimensions or pixel layout changed
        if self.texture_size != (width, height) || self.current_format != format {
            self.texture_size = (width, height);
            self.remote_size = (width, height);
            self.current_format = format;

            let plane = |label, w, h, fmt| {
                self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: fmt,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let textures: Vec<wgpu::Texture> = match format {
                FrameFormat::I420 => vec![
                    plane("frame_y", width, height, wgpu::TextureFormat::R8Unorm),
                    plane("frame_u", chroma_w, chroma_h, wgpu::TextureFormat::R8Unorm),
                    plane("frame_v", chroma_w, chroma_h, wgpu::TextureFormat::R8Unorm),
                ],
                FrameFormat::Nv12 => vec![
                    plane("frame_y", width, height, wgpu::TextureFormat::R8Unorm),
                    plane(
                        "frame_uv",
                        chroma_w,
                        chroma_h,
                        wgpu::TextureFormat::Rg8Unorm,
                    ),
                ],
            };
            let views: Vec<wgpu::TextureView> = textures
                .iter()
                .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()))
                .collect();

            let mut entries: Vec<wgpu::BindGroupEntry> = views
                .iter()
                .enumerate()
                .map(|(i, v)| wgpu::BindGroupEntry {
                    binding: i as u32,
                    resource: wgpu::BindingResource::TextureView(v),
                })
                .collect();
            entries.push(wgpu::BindGroupEntry {
                binding: views.len() as u32,
                resource: wgpu::BindingResource::Sampler(&self.sampler),
            });
            let layout = match format {
                FrameFormat::I420 => &self.bind_group_layout,
                FrameFormat::Nv12 => &self.bind_group_layout_nv12,
            };
            self.current_bind_group =
                Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("frame_bind_group"),
                    layout,
                    entries: &entries,
                }));

            self.plane_textures = textures;
            self.update_scale();
        }

        // Fast plane uploads — straight from the decoder's buffers (for AV1
        // these are dav1d's own refcounted planes, zero CPU repack), using
        // the source row stride as bytes_per_row.
        let textures = &self.plane_textures;
        let write = |tex: &wgpu::Texture, data: &[u8], stride: u32, w: u32, h: u32| {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        };
        match format {
            FrameFormat::I420 => {
                let (y, ys) = frame.y_plane();
                let (u, us) = frame.u_plane();
                let (v, vs) = frame.v_plane();
                write(&textures[0], y, ys as u32, width, height);
                write(&textures[1], u, us as u32, chroma_w, chroma_h);
                write(&textures[2], v, vs as u32, chroma_w, chroma_h);
            }
            FrameFormat::Nv12 => {
                let (y, ys) = frame.y_plane();
                let (uv, uvs) = frame.uv_plane();
                write(&textures[0], y, ys as u32, width, height);
                write(&textures[1], uv, uvs as u32, chroma_w, chroma_h);
            }
        }
    }

    /// Set the display mode (Letterbox or Stretch).
    pub fn set_display_mode(&mut self, mode: DisplayMode) {
        if self.display_mode != mode {
            self.display_mode = mode;
            self.update_scale();
        }
    }

    /// Render the current frame + optional egui overlay to the window.
    ///
    /// Runs even before the first video frame when there is egui output —
    /// egui sends its font atlas in the textures_delta of its first pass
    /// exactly once, and skipping that delta poisons every later egui draw.
    pub fn render(&mut self, egui_output: Option<EguiRenderData>) -> Result<()> {
        if self.current_bind_group.is_none() && egui_output.is_none() {
            return Ok(());
        }

        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(())
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(anyhow::anyhow!("wgpu validation error on surface"));
            }
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("render_encoder"),
            });

        // Video frame pass
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            let pipeline = match self.current_format {
                FrameFormat::I420 => &self.pipeline,
                FrameFormat::Nv12 => &self.pipeline_nv12,
            };
            if let Some(bind_group) = &self.current_bind_group {
                render_pass.set_pipeline(pipeline);
                render_pass.set_bind_group(0, bind_group, &[]);
                render_pass.set_bind_group(1, &self.scale_bind_group, &[]);
                render_pass.draw(0..6, 0..1);
            }
        }

        // egui overlay pass (drawn on top of video)
        if let Some(egui_data) = egui_output {
            let screen_descriptor = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [self.window_size.0, self.window_size.1],
                pixels_per_point: egui_data.pixels_per_point,
            };
            for (id, delta) in &egui_data.textures_delta.set {
                self.egui_renderer
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
            for id in &egui_data.textures_delta.free {
                self.egui_renderer.free_texture(id);
            }
            let extra_cmds = self.egui_renderer.update_buffers(
                &self.device,
                &self.queue,
                &mut encoder,
                &egui_data.clipped,
                &screen_descriptor,
            );
            {
                let mut egui_pass = encoder
                    .begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("egui_pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            depth_slice: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    })
                    .forget_lifetime();
                self.egui_renderer
                    .render(&mut egui_pass, &egui_data.clipped, &screen_descriptor);
            }
            self.queue.submit(
                extra_cmds
                    .into_iter()
                    .chain(std::iter::once(encoder.finish())),
            );
        } else {
            self.queue.submit(std::iter::once(encoder.finish()));
        }

        #[cfg(windows)]
        if let (Some(dx12), Some(generation), Some(frame)) = (
            &mut self.dx12,
            &self.shared_dx12,
            &self.current_shared_frame,
        ) {
            if let Err(e) = dx12.signal_done(&generation.imported, frame) {
                frame.mark_failed();
                return Err(e.context("DX12 shared-video completion"));
            }
        }
        output.present();

        Ok(())
    }

    /// Recompute the scale factors based on current display mode + crop.
    fn update_scale(&mut self) {
        if self.remote_size.0 == 0 || self.remote_size.1 == 0 {
            return;
        }

        let srgb = if self.surface_format.is_srgb() {
            1.0f32
        } else {
            0.0
        };
        let [u0, v0, uw, vh] = self.crop_uv;
        // The visible content is the cropped region, so aspect is taken from
        // its pixel size, not the whole framebuffer.
        let scale = match self.display_mode {
            DisplayMode::Stretch => [1.0f32, 1.0],
            DisplayMode::Letterbox => {
                let (ww, wh) = (self.window_size.0 as f32, self.window_size.1 as f32);
                let rw = self.remote_size.0 as f32 * uw;
                let rh = self.remote_size.1 as f32 * vh;
                let s = (ww / rw).min(wh / rh);
                [(rw * s) / ww, (rh * s) / wh]
            }
        };
        let data: [f32; 8] = [scale[0], scale[1], srgb, 0.0, u0, v0, uw, vh];
        self.queue
            .write_buffer(&self.scale_buffer, 0, bytemuck::cast_slice(&data));
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // Flush pending GPU/presentation work before the swapchain and any
        // imported shared textures are torn down.
        // wgpu 29 panics if a SwapchainAcquireSemaphore is still in-flight when Surface drops.
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }
}

const FULLSCREEN_QUAD_WGSL: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct Params {
    // [scale_x, scale_y, srgb_flag, 0]
    scale: vec4<f32>,
    // [u0, v0, u_width, v_height], the sub-rect of the frame this window shows
    crop: vec4<f32>,
};
@group(1) @binding(0) var<uniform> params: Params;

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    // Fullscreen quad scaled to preserve aspect ratio (letterboxed)
    var positions = array<vec2<f32>, 6>(
        vec2(-1.0, -1.0),
        vec2( 1.0, -1.0),
        vec2(-1.0,  1.0),
        vec2(-1.0,  1.0),
        vec2( 1.0, -1.0),
        vec2( 1.0,  1.0),
    );
    var uvs = array<vec2<f32>, 6>(
        vec2(0.0, 1.0),
        vec2(1.0, 1.0),
        vec2(0.0, 0.0),
        vec2(0.0, 0.0),
        vec2(1.0, 1.0),
        vec2(1.0, 0.0),
    );

    var out: VertexOutput;
    // Scale positions by aspect ratio correction
    var pos = positions[idx];
    pos.x *= params.scale.x;
    pos.y *= params.scale.y;
    out.position = vec4(pos, 0.0, 1.0);
    // Map the quad's [0,1] UVs into this window's crop sub-rect.
    out.uv = params.crop.xy + uvs[idx] * params.crop.zw;
    return out;
}

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_u: texture_2d<f32>;
@group(0) @binding(2) var tex_v: texture_2d<f32>;
@group(0) @binding(3) var frame_sampler: sampler;

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lower = c / 12.92;
    let higher = pow((c + vec3(0.055)) / 1.055, vec3(2.4));
    return select(higher, lower, c <= vec3(0.04045));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // BT.601 limited-range YUV -> RGB (matches the server's encode matrix).
    let y = (textureSample(tex_y, frame_sampler, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let u = textureSample(tex_u, frame_sampler, in.uv).r - 0.5;
    let v = textureSample(tex_v, frame_sampler, in.uv).r - 0.5;

    var rgb = vec3<f32>(
        y + 1.596 * v,
        y - 0.391 * u - 0.813 * v,
        y + 2.018 * u,
    );
    rgb = clamp(rgb, vec3(0.0), vec3(1.0));

    // The decoded values are gamma-encoded; sRGB surfaces expect linear input.
    if (params.scale.z > 0.5) {
        rgb = srgb_to_linear(rgb);
    }
    return vec4(rgb, 1.0);
}
"#;

/// NV12 variant: Y in an R8 texture, interleaved UV in an Rg8 texture
/// (hardware decoder output, uploaded without any CPU repacking).
const FULLSCREEN_QUAD_NV12_WGSL: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct Params {
    scale: vec4<f32>,
    crop: vec4<f32>,
};
@group(1) @binding(0) var<uniform> params: Params;

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 6>(
        vec2(-1.0, -1.0),
        vec2( 1.0, -1.0),
        vec2(-1.0,  1.0),
        vec2(-1.0,  1.0),
        vec2( 1.0, -1.0),
        vec2( 1.0,  1.0),
    );
    var uvs = array<vec2<f32>, 6>(
        vec2(0.0, 1.0),
        vec2(1.0, 1.0),
        vec2(0.0, 0.0),
        vec2(0.0, 0.0),
        vec2(1.0, 1.0),
        vec2(1.0, 0.0),
    );

    var out: VertexOutput;
    var pos = positions[idx];
    pos.x *= params.scale.x;
    pos.y *= params.scale.y;
    out.position = vec4(pos, 0.0, 1.0);
    out.uv = params.crop.xy + uvs[idx] * params.crop.zw;
    return out;
}

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_uv: texture_2d<f32>;
@group(0) @binding(2) var frame_sampler: sampler;

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lower = c / 12.92;
    let higher = pow((c + vec3(0.055)) / 1.055, vec3(2.4));
    return select(higher, lower, c <= vec3(0.04045));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let crop_min = params.crop.xy;
    let crop_max = params.crop.xy + params.crop.zw;
    let y_texel = 1.0 / vec2<f32>(textureDimensions(tex_y));
    let uv_texel = 1.0 / vec2<f32>(textureDimensions(tex_uv));
    let y_uv = clamp(in.uv, crop_min + 0.5 * y_texel, crop_max - 0.5 * y_texel);
    let chroma_uv = clamp(in.uv, crop_min + 0.5 * uv_texel, crop_max - 0.5 * uv_texel);
    // BT.601 limited-range YUV -> RGB (matches the server's encode matrix).
    let y = (textureSample(tex_y, frame_sampler, y_uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let chroma = textureSample(tex_uv, frame_sampler, chroma_uv).rg - vec2(0.5);
    let u = chroma.x;
    let v = chroma.y;

    var rgb = vec3<f32>(
        y + 1.596 * v,
        y - 0.391 * u - 0.813 * v,
        y + 2.018 * u,
    );
    rgb = clamp(rgb, vec3(0.0), vec3(1.0));

    if (params.scale.z > 0.5) {
        rgb = srgb_to_linear(rgb);
    }
    return vec4(rgb, 1.0);
}
"#;
