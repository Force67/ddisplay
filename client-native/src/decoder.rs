/// Video decoder supporting H.264 (OpenH264) and AV1 (rav1d).

use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

// dav1d uses POSIX errno values even on Windows
const DAV1D_EAGAIN: i32 = -11;

fn dav1d_strerror(code: i32) -> &'static str {
    match code {
        -11 => "EAGAIN",
        -12 => "ENOMEM",
        -22 => "EINVAL",
        -5  => "EIO",
        -28 => "ENOSPC",
        -16 => "EBUSY",
        -2  => "ENOENT",
        -32 => "EPIPE",
        _   => "?",
    }
}

/// Parse the first OBU header byte to a type name (AV1 spec Table 1).
fn obu_type_name(first_byte: u8) -> &'static str {
    match (first_byte >> 3) & 0x0F {
        1 => "SEQUENCE_HEADER",
        2 => "TEMPORAL_DELIMITER",
        3 => "FRAME_HEADER",
        4 => "TILE_GROUP",
        5 => "METADATA",
        6 => "FRAME",
        7 => "REDUNDANT_FRAME_HEADER",
        8 => "TILE_LIST",
        15 => "PADDING",
        n  => { let _ = n; "RESERVED" },
    }
}

/// Decoded YUV420 frame.
///
/// YUV→RGB conversion happens on the GPU (see renderer.rs) — uploading planar
/// YUV is 1.5 bytes/pixel vs 4 for RGBA and skips a full-frame CPU pass.
///
/// Planes are either owned packed buffers (H.264 path — OpenH264's output
/// only lives until the next decode call) or a zero-copy reference into a
/// refcounted dav1d picture (AV1 path — no repack, the GPU upload reads the
/// decoder's buffer directly using its stride).
pub struct DecodedFrame {
    storage: PlaneStorage,
    pub width: u32,
    pub height: u32,
}

enum PlaneStorage {
    Packed {
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    },
    Dav1d(Dav1dPlaneGuard),
}

impl DecodedFrame {
    /// (plane bytes, row stride) — stride may exceed the visible width.
    pub fn y_plane(&self) -> (&[u8], usize) {
        match &self.storage {
            PlaneStorage::Packed { y, .. } => (y, self.width as usize),
            PlaneStorage::Dav1d(g) => (g.y(), g.y_stride),
        }
    }

    pub fn u_plane(&self) -> (&[u8], usize) {
        match &self.storage {
            PlaneStorage::Packed { u, .. } => (u, (self.width as usize).div_ceil(2)),
            PlaneStorage::Dav1d(g) => (g.u(), g.uv_stride),
        }
    }

    pub fn v_plane(&self) -> (&[u8], usize) {
        match &self.storage {
            PlaneStorage::Packed { v, .. } => (v, (self.width as usize).div_ceil(2)),
            PlaneStorage::Dav1d(g) => (g.v(), g.uv_stride),
        }
    }
}

/// Keeps a dav1d picture alive (refcounted) so its planes can be read
/// zero-copy from the render thread; unrefs on drop.
struct Dav1dPlaneGuard {
    pic: Dav1dPicture,
    y_stride: usize,
    uv_stride: usize,
    height: usize,
}

// SAFETY: dav1d pictures are refcounted with thread-safe release; the planes
// are immutable once output. We only read from them.
unsafe impl Send for Dav1dPlaneGuard {}

impl Dav1dPlaneGuard {
    fn y(&self) -> &[u8] {
        let p = self.pic.data[0].unwrap().as_ptr() as *const u8;
        unsafe { std::slice::from_raw_parts(p, self.y_stride * self.height) }
    }
    fn u(&self) -> &[u8] {
        let p = self.pic.data[1].unwrap().as_ptr() as *const u8;
        unsafe { std::slice::from_raw_parts(p, self.uv_stride * self.height.div_ceil(2)) }
    }
    fn v(&self) -> &[u8] {
        let p = self.pic.data[2].unwrap().as_ptr() as *const u8;
        unsafe { std::slice::from_raw_parts(p, self.uv_stride * self.height.div_ceil(2)) }
    }
}

impl Drop for Dav1dPlaneGuard {
    fn drop(&mut self) {
        unsafe { dav1d_picture_unref(&mut self.pic) };
    }
}

/// Copy a possibly-strided plane into a tightly packed buffer.
fn pack_plane(src: &[u8], src_stride: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    if src_stride == w {
        out.copy_from_slice(&src[..w * h]);
    } else {
        for row in 0..h {
            out[row * w..(row + 1) * w].copy_from_slice(&src[row * src_stride..row * src_stride + w]);
        }
    }
    out
}

pub enum VideoDecoder {
    H264(H264Decoder),
    Av1(Av1Decoder),
}

impl VideoDecoder {
    pub fn for_codec(codec: &str) -> Result<Self> {
        match codec {
            "av1" => Ok(Self::Av1(Av1Decoder::new()?)),
            _ => Ok(Self::H264(H264Decoder::new()?)),
        }
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        match self {
            Self::H264(d) => d.decode(data),
            Self::Av1(d) => d.decode(data),
        }
    }

    /// True if the AV1 decoder just reset itself and needs a keyframe from the server.
    pub fn take_needs_keyframe(&mut self) -> bool {
        match self {
            Self::Av1(d) => {
                let v = d.needs_keyframe;
                d.needs_keyframe = false;
                v
            }
            Self::H264(_) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// H.264 decoder
// ---------------------------------------------------------------------------

pub struct H264Decoder {
    decoder: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::new().context("Failed to create OpenH264 decoder")?;
        Ok(Self { decoder })
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        let maybe_yuv = self.decoder.decode(data)
            .map_err(|e| anyhow::anyhow!("OpenH264 decode error: {:?}", e))?;

        let yuv = match maybe_yuv {
            Some(yuv) => yuv,
            None => return Ok(None),
        };

        let (w, h) = yuv.dimensions();
        if w == 0 || h == 0 { return Ok(None); }

        let (ys, us, vs) = yuv.strides();
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);

        // OpenH264's planes only live until the next decode call — pack them.
        Ok(Some(DecodedFrame {
            storage: PlaneStorage::Packed {
                y: pack_plane(yuv.y(), ys, w, h),
                u: pack_plane(yuv.u(), us, cw, ch),
                v: pack_plane(yuv.v(), vs, cw, ch),
            },
            width: w as u32,
            height: h as u32,
        }))
    }
}

// ---------------------------------------------------------------------------
// AV1 decoder — calls rav1d's exported dav1d_* C functions directly.
//
// IMPORTANT: Dav1dContext in rav1d is PhantomData (zero-sized), so we must
// NOT use Option<Dav1dContext> as a function parameter — it would be 1 byte,
// not the 8-byte pointer C expects.  We use *mut c_void for the opaque handle.
// ---------------------------------------------------------------------------

use std::ffi::c_void;
use std::ptr::NonNull;
use rav1d::include::dav1d::dav1d::Dav1dSettings;
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::picture::Dav1dPicture;

/// Low-latency decoder settings.
///
/// Threads scale with the machine (4K software AV1 needs them). The frame
/// delay is capped at 2: lower decode latency than the unbounded default,
/// while avoiding rav1d 1.1.0's single-frame-context mode (max_frame_delay=1
/// → n_fc=1), which aborts in its error path (decode.rs on_error →
/// in_cdf.try_write().unwrap() panic in a nounwind function).
/// Override for experiments via DDISPLAY_AV1_THREADS / DDISPLAY_AV1_DELAY.
fn apply_low_latency_settings(settings: &mut Dav1dSettings) {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads = std::env::var("DDISPLAY_AV1_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| cores.min(16) as i32);
    let delay = std::env::var("DDISPLAY_AV1_DELAY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    settings.n_threads = threads;
    settings.max_frame_delay = delay;
}

extern "C" {
    fn dav1d_default_settings(s: *mut Dav1dSettings);
    fn dav1d_open(c_out: *mut *mut c_void, s: *const Dav1dSettings) -> i32;
    fn dav1d_send_data(c: *mut c_void, data: *mut Dav1dData) -> i32;
    fn dav1d_get_picture(c: *mut c_void, pic: *mut Dav1dPicture) -> i32;
    fn dav1d_close(c_out: *mut *mut c_void);
    fn dav1d_data_create(data: *mut Dav1dData, sz: usize) -> *mut u8;
    fn dav1d_data_unref(data: *mut Dav1dData);
    fn dav1d_picture_unref(pic: *mut Dav1dPicture);
}

pub struct Av1Decoder {
    /// Opaque dav1d context pointer (never null after successful init).
    ctx: *mut c_void,
    /// Latest decoded frame from drain_pictures (returned by decode()).
    last_frame: Option<DecodedFrame>,
    /// Total packets handed to dav1d_send_data.
    frames_in: u64,
    /// Total pictures pulled from dav1d_get_picture.
    frames_out: u64,
    /// Times dav1d_send_data returned EAGAIN (decoder was full).
    send_eagain: u64,
    /// Times dav1d_get_picture returned EAGAIN (not enough data yet).
    get_eagain: u64,
    /// Hard decode errors (not EAGAIN).
    decode_errors: u64,
    /// Consecutive get_picture errors — used to detect a stuck decoder.
    consecutive_errors: u32,
    /// Set to true after a self-reset so the caller can request a keyframe.
    pub needs_keyframe: bool,
}

// Safety: dav1d context is not shared; we call it from one thread at a time.
unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
    pub fn new() -> Result<Self> {
        unsafe {
            let mut settings: Dav1dSettings = std::mem::zeroed();
            dav1d_default_settings(&mut settings);
            apply_low_latency_settings(&mut settings);

            eprintln!(
                "[av1] opening dav1d context (n_threads={}, max_frame_delay={})...",
                settings.n_threads, settings.max_frame_delay,
            );
            let mut ctx: *mut c_void = std::ptr::null_mut();
            let rc = dav1d_open(&mut ctx, &settings);
            if rc != 0 || ctx.is_null() {
                anyhow::bail!("dav1d_open failed: {} ({})", rc, dav1d_strerror(rc));
            }

            eprintln!("[av1] dav1d context opened OK  ptr={:p}", ctx);
            Ok(Self {
                ctx,
                last_frame: None,
                frames_in: 0,
                frames_out: 0,
                send_eagain: 0,
                get_eagain: 0,
                decode_errors: 0,
                consecutive_errors: 0,
                needs_keyframe: false,
            })
        }
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        self.frames_in += 1;
        let verbose = self.frames_in <= 5;

        if data.is_empty() {
            eprintln!("[av1] WARNING frame #{}: empty packet!", self.frames_in);
            return Ok(None);
        }

        if verbose {
            let n = data.len().min(16);
            let hex: Vec<String> = data[..n].iter().map(|b| format!("{:02x}", b)).collect();
            eprintln!("[av1] >> frame #{}: {} bytes  first_obu={} hdr=[{}]",
                self.frames_in, data.len(), obu_type_name(data[0]), hex.join(" "));
        }

        unsafe {
            let mut dav1d_data: Dav1dData = std::mem::zeroed();
            let ptr = dav1d_data_create(&mut dav1d_data, data.len());
            if ptr.is_null() {
                anyhow::bail!("dav1d_data_create failed (OOM?)");
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());

            // Feed data to dav1d. On EAGAIN the data was NOT consumed — we must
            // drain pictures and retry until the data is fully consumed (sz == 0).
            loop {
                let send_rc = dav1d_send_data(self.ctx, &mut dav1d_data);
                match send_rc {
                    0 => {
                        if verbose { eprintln!("[av1]   send_data OK"); }
                        break;
                    }
                    DAV1D_EAGAIN => {
                        self.send_eagain += 1;
                        if verbose || self.send_eagain <= 3 {
                            eprintln!("[av1]   send_data EAGAIN #{} (draining before retry)", self.send_eagain);
                        }
                        // Drain all available pictures so dav1d frees internal buffers.
                        self.drain_pictures(verbose);
                        // dav1d consumed part of the data; if sz == 0 we're done.
                        if dav1d_data.sz == 0 { break; }
                    }
                    rc => {
                        dav1d_data_unref(&mut dav1d_data);
                        self.decode_errors += 1;
                        anyhow::bail!("dav1d_send_data error {} ({}) on frame #{}",
                            rc, dav1d_strerror(rc), self.frames_in);
                    }
                }
            }

            // Drain all available pictures after the send completes.
            self.drain_pictures(verbose);

            // Return the latest decoded frame (we always keep the newest one).
            let result = self.last_frame.take();
            Ok(result)
        }
    }

    /// Pull all available pictures from dav1d, keeping only the latest one.
    unsafe fn drain_pictures(&mut self, verbose: bool) {
        loop {
            let mut pic: Dav1dPicture = std::mem::zeroed();
            let rc = dav1d_get_picture(self.ctx, &mut pic);
            match rc {
                0 => {
                    let w = pic.p.w as usize;
                    let h = pic.p.h as usize;
                    if verbose {
                        eprintln!("[av1]   get_picture OK → {}x{}  strides Y={} UV={}",
                            w, h, pic.stride[0], pic.stride[1]);
                    }

                    if w == 0 || h == 0 {
                        eprintln!("[av1]   WARNING: zero-size picture {}x{}", w, h);
                        dav1d_picture_unref(&mut pic);
                        continue;
                    }

                    let y_stride = pic.stride[0] as usize;
                    let uv_stride = pic.stride[1] as usize;

                    let y_ptr = pic.data[0].map(|p| p.as_ptr() as *const u8);
                    let u_ptr = pic.data[1].map(|p| p.as_ptr() as *const u8);
                    let v_ptr = pic.data[2].map(|p| p.as_ptr() as *const u8);

                    if y_ptr.is_none() || u_ptr.is_none() || v_ptr.is_none() {
                        eprintln!("[av1]   ERROR: null plane pointers");
                        dav1d_picture_unref(&mut pic);
                        continue;
                    }

                    // Zero-copy: move the refcounted picture into the frame;
                    // the renderer uploads straight from dav1d's buffers and
                    // the guard unrefs when the frame is replaced.
                    let frame = DecodedFrame {
                        storage: PlaneStorage::Dav1d(Dav1dPlaneGuard {
                            pic,
                            y_stride,
                            uv_stride,
                            height: h,
                        }),
                        width: w as u32,
                        height: h as u32,
                    };

                    self.frames_out += 1;
                    self.consecutive_errors = 0;

                    if self.frames_out % 300 == 0 {
                        eprintln!("[av1] stats: in={} out={} send_eagain={} get_eagain={} errors={}",
                            self.frames_in, self.frames_out,
                            self.send_eagain, self.get_eagain, self.decode_errors);
                    }

                    self.last_frame = Some(frame);
                }
                DAV1D_EAGAIN => {
                    self.get_eagain += 1;
                    self.consecutive_errors = 0;
                    break; // no more pictures available
                }
                _ => {
                    self.decode_errors += 1;
                    self.consecutive_errors += 1;
                    eprintln!("[av1]   get_picture ERROR {} ({}) on frame #{}",
                        rc, dav1d_strerror(rc), self.frames_in);

                    if self.consecutive_errors >= 3 {
                        eprintln!("[av1] decoder stuck after {} consecutive errors — reinitialising",
                            self.consecutive_errors);
                        dav1d_close(&mut self.ctx);

                        let mut settings: Dav1dSettings = std::mem::zeroed();
                        dav1d_default_settings(&mut settings);
                        apply_low_latency_settings(&mut settings);
                        let mut new_ctx: *mut c_void = std::ptr::null_mut();
                        let rc2 = dav1d_open(&mut new_ctx, &settings);
                        if rc2 == 0 && !new_ctx.is_null() {
                            self.ctx = new_ctx;
                            self.consecutive_errors = 0;
                            self.needs_keyframe = true;
                            eprintln!("[av1] decoder reinitialised — requesting keyframe from server");
                        } else {
                            eprintln!("[av1] FATAL: failed to reopen dav1d after reset: {}", rc2);
                        }
                    }
                    break; // stop draining on error
                }
            }
        }
    }
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        unsafe { dav1d_close(&mut self.ctx) };
        eprintln!("[av1] decoder closed (in={} out={} errors={})",
            self.frames_in, self.frames_out, self.decode_errors);
    }
}

