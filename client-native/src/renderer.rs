/// GPU-accelerated renderer using wgpu.
///
/// Uploads decoded YUV420 planes to persistent GPU textures, converts
/// YUV→RGB in the fragment shader (BT.601 limited range), and renders a
/// letterboxed quad that preserves the remote desktop's aspect ratio.
/// Compared to CPU conversion + RGBA upload this saves a full-frame CPU
/// pass and 62% of the per-frame upload bandwidth.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::decoder::DecodedFrame;
use crate::overlay::{DisplayMode, EguiRenderData};

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Y, U, V planes as R8 textures.
    plane_textures: Option<[wgpu::Texture; 3]>,
    current_bind_group: Option<wgpu::BindGroup>,
    texture_size: (u32, u32),
    scale_buffer: wgpu::Buffer,
    scale_bind_group: wgpu::BindGroup,
    window_size: (u32, u32),
    remote_size: (u32, u32),
    display_mode: DisplayMode,
    egui_renderer: egui_wgpu::Renderer,
    surface_format: wgpu::TextureFormat,
}

impl Renderer {
    pub async fn new(window: Arc<winit::window::Window>) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let surface = instance.create_surface(window.clone())
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

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("ddisplay"),
                ..Default::default()
            })
            .await
            .context("Failed to create wgpu device")?;

        let size = window.inner_size();
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps.formats.iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        // Prefer Mailbox (triple-buffered, low latency, GPU-paced by display) over
        // Immediate (uncapped, spins GPU at 100%). Fall back through FifoRelaxed to Fifo.
        let present_mode = if surface_caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            eprintln!("[gpu] Present mode: Mailbox (low latency, GPU-paced)");
            wgpu::PresentMode::Mailbox
        } else if surface_caps.present_modes.contains(&wgpu::PresentMode::FifoRelaxed) {
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

        // Bind group 0: Y/U/V plane textures + sampler
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
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture_bind_group_layout"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
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

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout), Some(&scale_bg_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("render_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
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
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("frame_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Scale uniform: [scale_x, scale_y, srgb_flag, 0]
        let scale_data = [1.0f32, 1.0, if surface_format.is_srgb() { 1.0 } else { 0.0 }, 0.0];
        let scale_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scale_uniform"),
            size: 16,
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
            sampler,
            plane_textures: None,
            current_bind_group: None,
            texture_size: (0, 0),
            scale_buffer,
            scale_bind_group,
            window_size: ws,
            remote_size: (0, 0),
            display_mode: DisplayMode::default(),
            egui_renderer,
            surface_format,
        })
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

    /// Upload a decoded YUV420 frame to the GPU (three R8 plane textures).
    pub fn upload_frame(&mut self, frame: &DecodedFrame) {
        let (width, height) = (frame.width, frame.height);
        if width == 0 || height == 0 {
            return;
        }
        let chroma_w = width.div_ceil(2);
        let chroma_h = height.div_ceil(2);

        // Recreate textures only if dimensions changed
        if self.texture_size != (width, height) {
            self.texture_size = (width, height);
            self.remote_size = (width, height);

            let plane = |label, w, h| {
                self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let textures = [
                plane("frame_y", width, height),
                plane("frame_u", chroma_w, chroma_h),
                plane("frame_v", chroma_w, chroma_h),
            ];
            let views: Vec<wgpu::TextureView> = textures
                .iter()
                .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()))
                .collect();

            self.current_bind_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("frame_bind_group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&views[0]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&views[1]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&views[2]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            }));

            self.plane_textures = Some(textures);
            self.update_scale();
        }

        // Fast plane uploads — straight from the decoder's buffers (for AV1
        // these are dav1d's own refcounted planes, zero CPU repack), using
        // the source row stride as bytes_per_row.
        if let Some(textures) = &self.plane_textures {
            let mut write = |tex: &wgpu::Texture, data: &[u8], stride: u32, w: u32, h: u32| {
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
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                );
            };
            let (y, ys) = frame.y_plane();
            let (u, us) = frame.u_plane();
            let (v, vs) = frame.v_plane();
            write(&textures[0], y, ys as u32, width, height);
            write(&textures[1], u, us as u32, chroma_w, chroma_h);
            write(&textures[2], v, vs as u32, chroma_w, chroma_h);
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
    pub fn render(&mut self, egui_output: Option<EguiRenderData>) -> Result<()> {
        let bind_group = match &self.current_bind_group {
            Some(bg) => bg,
            None => return Ok(()),
        };

        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(anyhow::anyhow!("wgpu validation error on surface"));
            }
        };
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
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

            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_bind_group(0, bind_group, &[]);
            render_pass.set_bind_group(1, &self.scale_bind_group, &[]);
            render_pass.draw(0..6, 0..1);
        }

        // egui overlay pass (drawn on top of video)
        if let Some(egui_data) = egui_output {
            let screen_descriptor = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [self.window_size.0, self.window_size.1],
                pixels_per_point: egui_data.pixels_per_point,
            };
            for (id, delta) in &egui_data.textures_delta.set {
                self.egui_renderer.update_texture(&self.device, &self.queue, *id, delta);
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
                let mut egui_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
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
                }).forget_lifetime();
                self.egui_renderer.render(&mut egui_pass, &egui_data.clipped, &screen_descriptor);
            }
            self.queue.submit(extra_cmds.into_iter().chain(std::iter::once(encoder.finish())));
        } else {
            self.queue.submit(std::iter::once(encoder.finish()));
        }
        output.present();

        Ok(())
    }

    /// Recompute the scale factors based on current display mode.
    fn update_scale(&mut self) {
        if self.remote_size.0 == 0 || self.remote_size.1 == 0 {
            return;
        }

        let srgb = if self.surface_format.is_srgb() { 1.0f32 } else { 0.0 };
        let data = match self.display_mode {
            DisplayMode::Stretch => [1.0f32, 1.0, srgb, 0.0],
            DisplayMode::Letterbox => {
                let (ww, wh) = (self.window_size.0 as f32, self.window_size.1 as f32);
                let (rw, rh) = (self.remote_size.0 as f32, self.remote_size.1 as f32);

                let scale = (ww / rw).min(wh / rh);
                let sx = (rw * scale) / ww;
                let sy = (rh * scale) / wh;
                [sx, sy, srgb, 0.0]
            }
        };
        self.queue.write_buffer(&self.scale_buffer, 0, bytemuck::cast_slice(&data));
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // Flush all pending GPU/presentation work before the Vulkan swapchain is torn down.
        // wgpu 29 panics if a SwapchainAcquireSemaphore is still in-flight when Surface drops.
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }
}

const FULLSCREEN_QUAD_WGSL: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(1) @binding(0) var<uniform> scale: vec4<f32>;

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
    pos.x *= scale.x;
    pos.y *= scale.y;
    out.position = vec4(pos, 0.0, 1.0);
    out.uv = uvs[idx];
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
    if (scale.z > 0.5) {
        rgb = srgb_to_linear(rgb);
    }
    return vec4(rgb, 1.0);
}
"#;
