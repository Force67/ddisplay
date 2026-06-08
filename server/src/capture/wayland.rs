//! Wayland screen capture via Mutter/portal ScreenCast + PipeWire.
//!
//! The remote session (Mutter-native or XDG portal, see
//! `crate::wayland_session` / `crate::portal_session`) hands us a PipeWire
//! node id — and, for the portal flavor, a PipeWire remote fd from
//! OpenPipeWireRemote that we must connect through. A dedicated thread runs
//! the PipeWire main loop with an input video stream connected to that node,
//! negotiating BGRx/BGRA memfd/SHM buffers (no DMA-BUF in v1). Each buffer is
//! copied into a shared double buffer that the capture loop reads through the
//! `ScreenCapturer` trait.
//!
//! The cursor is composited into the frames by the compositor (EMBEDDED
//! cursor mode), so `embeds_cursor()` is true and no separate cursor updates
//! are sent.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::{Condvar, Mutex};
use pipewire as pw;

use super::{CapturedFrameRef, CursorInfo, ScreenCapturer};
use crate::wayland_session::RemoteSessionApi;

/// Frame data shared between the PipeWire thread and the capture loop.
#[derive(Default)]
struct FrameBuf {
    data: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
}

#[derive(Default)]
struct Shared {
    frame: Mutex<FrameBuf>,
    new_frame: AtomicBool,
    /// Last negotiated video size (param_changed).
    format: Mutex<Option<(u32, u32)>>,
    format_cv: Condvar,
}

/// Per-stream state owned by the PipeWire callbacks.
struct PwUserData {
    shared: Arc<Shared>,
    video_info: pw::spa::param::video::VideoInfoRaw,
    have_format: bool,
}

pub struct WaylandCapturer {
    /// Keeps the remote D-Bus session alive (shared with the input injector).
    #[allow(dead_code)]
    session: Arc<dyn RemoteSessionApi>,
    shared: Arc<Shared>,
    quit_tx: Option<pw::channel::Sender<()>>,
    pw_thread: Option<std::thread::JoinHandle<()>>,
    /// Front buffer the encoder reads from (swapped with the shared buffer).
    front: FrameBuf,
    width: u32,
    height: u32,
}

impl WaylandCapturer {
    pub fn new(session: Arc<dyn RemoteSessionApi>) -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();

        let node_id = session.node_id();
        // Portal sessions hand us a dedicated PipeWire remote fd; Mutter
        // sessions use the default per-user socket (None).
        let remote_fd = session.take_pipewire_fd();
        let thread_shared = Arc::clone(&shared);
        let pw_thread = std::thread::Builder::new()
            .name("pw-capture".into())
            .spawn(move || {
                if let Err(e) = run_pipewire_loop(node_id, remote_fd, thread_shared, quit_rx) {
                    tracing::error!("[wayland] PipeWire loop failed: {:#}", e);
                }
            })
            .context("spawn PipeWire thread")?;

        // Block until the video format is negotiated so we know the size
        // before the encoder is built.
        let (width, height) = {
            let mut fmt = shared.format.lock();
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let Some(size) = *fmt {
                    break size;
                }
                let timeout = deadline.saturating_duration_since(Instant::now());
                if timeout.is_zero()
                    || shared.format_cv.wait_for(&mut fmt, timeout).timed_out()
                {
                    if let Some(size) = *fmt {
                        break size;
                    }
                    anyhow::bail!(
                        "timed out waiting for PipeWire video format negotiation \
                         (node {})",
                        node_id
                    );
                }
            }
        };

        tracing::info!("[wayland] capturer initialized: {}x{}", width, height);

        Ok(Self {
            session,
            shared,
            quit_tx: Some(quit_tx),
            pw_thread: Some(pw_thread),
            front: FrameBuf::default(),
            width,
            height,
        })
    }
}

impl ScreenCapturer for WaylandCapturer {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn has_new_frame(&mut self) -> bool {
        self.shared.new_frame.load(Ordering::Acquire)
    }

    fn frame_ref(&mut self) -> Result<CapturedFrameRef<'_>> {
        // Swap in the newest frame if there is one. If we have never received
        // a frame, wait briefly for the first buffer (stream just started).
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if self.shared.new_frame.swap(false, Ordering::AcqRel) {
                let mut shared = self.shared.frame.lock();
                std::mem::swap(&mut self.front, &mut *shared);
                break;
            }
            if !self.front.data.is_empty() {
                break; // no new frame; re-serve the previous one (keyframes)
            }
            if Instant::now() >= deadline {
                anyhow::bail!("no frame received from PipeWire stream yet");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        Ok(CapturedFrameRef {
            data: &self.front.data,
            width: self.front.width,
            height: self.front.height,
            stride: self.front.stride,
        })
    }

    fn cursor_info(&mut self) -> Result<CursorInfo> {
        // Cursor is embedded in the frames; never broadcast separately.
        Ok(CursorInfo { x: 0, y: 0, visible: false })
    }

    fn size_changed(&mut self) -> Option<(u32, u32)> {
        let fmt = *self.shared.format.lock();
        match fmt {
            Some((w, h)) if (w, h) != (self.width, self.height) => Some((w, h)),
            _ => None,
        }
    }

    fn reinit(&mut self) -> Result<()> {
        // The PipeWire stream renegotiated in place — just adopt the new size;
        // frames already arrive at the new resolution.
        if let Some((w, h)) = *self.shared.format.lock() {
            self.width = w;
            self.height = h;
        }
        // Drop the stale front buffer so we never serve an old-size frame.
        self.front = FrameBuf::default();
        Ok(())
    }

    fn embeds_cursor(&self) -> bool {
        true
    }
}

impl Drop for WaylandCapturer {
    fn drop(&mut self) {
        if let Some(tx) = self.quit_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.pw_thread.take() {
            let _ = handle.join();
        }
        tracing::debug!("[wayland] capturer shut down");
    }
}

/// Dedicated PipeWire main-loop thread: connects an input video stream to
/// `node_id` and copies every buffer into the shared double buffer.
fn run_pipewire_loop(
    node_id: u32,
    remote_fd: Option<OwnedFd>,
    shared: Arc<Shared>,
    quit_rx: pw::channel::Receiver<()>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoop::new(None).context("create PipeWire main loop")?;
    let context = pw::context::Context::new(&mainloop).context("create PipeWire context")?;
    let core = match remote_fd {
        Some(fd) => context
            .connect_fd(fd, None)
            .context("connect to PipeWire via the portal's OpenPipeWireRemote fd")?,
        None => context
            .connect(None)
            .context("connect to PipeWire daemon (is pipewire running for this user?)")?,
    };

    // Let the capture loop stop us via the channel.
    let loop_clone = mainloop.clone();
    let _quit_attach = quit_rx.attach(mainloop.loop_(), move |_| {
        loop_clone.quit();
    });

    let stream = pw::stream::Stream::new(
        &core,
        "ddisplay-capture",
        pw::properties::properties! {
            *pw::keys::NODE_NAME => "ddisplay-capture",
            *pw::keys::APP_NAME => "ddisplay-server",
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            // Video streams need the plain client-node factory; without this
            // some client configs route through module-adapter, which then
            // fails to load its (audio) SPA follower for a video stream.
            *pw::keys::FACTORY_NAME => "client-node",
        },
    )
    .context("create PipeWire stream")?;

    let user_data = PwUserData {
        shared,
        video_info: pw::spa::param::video::VideoInfoRaw::new(),
        have_format: false,
    };

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(|_, _, old, new| {
            tracing::debug!("[wayland] pw stream state: {:?} -> {:?}", old, new);
        })
        .param_changed(|_, udata, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let (media_type, media_subtype) =
                match pw::spa::param::format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(_) => return,
                };
            if media_type != pw::spa::param::format::MediaType::Video
                || media_subtype != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if udata.video_info.parse(param).is_err() {
                tracing::warn!("[wayland] failed to parse negotiated video format");
                return;
            }
            udata.have_format = true;
            let size = udata.video_info.size();
            tracing::info!(
                "[wayland] negotiated format: {:?} {}x{}",
                udata.video_info.format(),
                size.width,
                size.height,
            );
            let mut fmt = udata.shared.format.lock();
            *fmt = Some((size.width, size.height));
            udata.shared.format_cv.notify_all();
        })
        .process(|stream, udata| {
            // Drain queued buffers, keeping only the newest frame.
            while let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    continue;
                }
                let data = &mut datas[0];
                let chunk_size = data.chunk().size() as usize;
                let chunk_offset = data.chunk().offset() as usize;
                let chunk_stride = data.chunk().stride();
                if chunk_size == 0 {
                    continue;
                }
                let Some(slice) = data.data() else { continue };
                if chunk_offset + chunk_size > slice.len() {
                    tracing::warn!(
                        "[wayland] bogus chunk: offset {} + size {} > map {}",
                        chunk_offset, chunk_size, slice.len()
                    );
                    continue;
                }

                let size = udata.video_info.size();
                let (width, height) = (size.width, size.height);
                if !udata.have_format || width == 0 || height == 0 {
                    continue;
                }
                let stride = if chunk_stride > 0 {
                    chunk_stride as u32
                } else {
                    width * 4
                };

                let src = &slice[chunk_offset..chunk_offset + chunk_size];
                let mut frame = udata.shared.frame.lock();
                frame.data.clear();
                frame.data.extend_from_slice(src);
                frame.width = width;
                frame.height = height;
                frame.stride = stride;
                drop(frame);
                udata.shared.new_frame.store(true, Ordering::Release);
            }
        })
        .register()
        .context("register PipeWire stream listener")?;

    // Advertise raw BGRx/BGRA video at any size, memfd/SHM only (v1: no
    // DMA-BUF — we don't add the SPA_PARAM_BUFFERS dataType for it).
    let format_obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRA
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle { width: 1920, height: 1080 },
            pw::spa::utils::Rectangle { width: 1, height: 1 },
            pw::spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 60, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 1000, denom: 1 }
        ),
    );
    let format_bytes = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(format_obj),
    )
    .context("serialize format pod")?
    .0
    .into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&format_bytes)
        .context("format pod from_bytes")?];

    stream
        .connect(
            pw::spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .with_context(|| format!("connect PipeWire stream to node {}", node_id))?;

    tracing::info!("[wayland] PipeWire stream connected to node {}", node_id);
    mainloop.run();
    tracing::info!("[wayland] PipeWire loop exited");
    Ok(())
}
