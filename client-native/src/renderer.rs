/// GPU-accelerated renderer using wgpu.
///
/// Uploads decoded RGBA frames to a persistent GPU texture and renders a
/// letterboxed quad that preserves the remote desktop's aspect ratio.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::overlay::DisplayMode;

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    frame_texture: Option<wgpu::Texture>,
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

        // Use Immediate (no vsync) for lowest latency. Fall back to Mailbox, then Fifo.
        let present_mode = if surface_caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            eprintln!("[gpu] Present mode: Immediate (no vsync, lowest latency)");
            wgpu::PresentMode::Immediate
        } else if surface_caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            eprintln!("[gpu] Present mode: Mailbox (low latency)");
            wgpu::PresentMode::Mailbox
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

        // Bind group 0: frame texture + sampler
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        // Bind group 1: scale uniform (aspect ratio correction)
        let scale_bg_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scale_bind_group_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
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

        // Scale uniform: [scale_x, scale_y, 0, 0]
        let scale_data = [1.0f32, 1.0, 0.0, 0.0];
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
            frame_texture: None,
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

    /// Upload a decoded RGBA frame to the GPU.
    pub fn upload_frame(&mut self, rgba: &[u8], width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }

        // Recreate texture only if dimensions changed
        if self.texture_size != (width, height) {
            self.texture_size = (width, height);
            self.remote_size = (width, height);

            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("frame_texture"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

            self.current_bind_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("frame_bind_group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            }));

            self.frame_texture = Some(texture);
            self.update_scale();
        }

        // Fast texture upload (no alloc, just copies into existing GPU texture)
        if let Some(texture) = &self.frame_texture {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
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
    pub fn render(&mut self, egui_output: Option<egui::FullOutput>, pixels_per_point: f32) -> Result<()> {
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
        if let Some(full_output) = egui_output {
            // Tessellate into paint jobs
            let clipped = {
                let ctx = egui::Context::default();
                ctx.tessellate(full_output.shapes, full_output.pixels_per_point)
            };
            let screen_descriptor = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [self.window_size.0, self.window_size.1],
                pixels_per_point,
            };
            for (id, delta) in &full_output.textures_delta.set {
                self.egui_renderer.update_texture(&self.device, &self.queue, *id, delta);
            }
            for id in &full_output.textures_delta.free {
                self.egui_renderer.free_texture(id);
            }
            let extra_cmds = self.egui_renderer.update_buffers(
                &self.device,
                &self.queue,
                &mut encoder,
                &clipped,
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
                self.egui_renderer.render(&mut egui_pass, &clipped, &screen_descriptor);
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

        let data = match self.display_mode {
            DisplayMode::Stretch => [1.0f32, 1.0, 0.0, 0.0],
            DisplayMode::Letterbox => {
                let (ww, wh) = (self.window_size.0 as f32, self.window_size.1 as f32);
                let (rw, rh) = (self.remote_size.0 as f32, self.remote_size.1 as f32);

                let scale = (ww / rw).min(wh / rh);
                let sx = (rw * scale) / ww;
                let sy = (rh * scale) / wh;
                [sx, sy, 0.0f32, 0.0]
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

@group(0) @binding(0) var frame_texture: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(frame_texture, frame_sampler, in.uv);
}
"#;
