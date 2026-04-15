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

pub struct DecodedFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
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
}

// ---------------------------------------------------------------------------
// H.264 decoder
// ---------------------------------------------------------------------------

pub struct H264Decoder {
    decoder: Decoder,
    rgba_buf: Vec<u8>,
}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::new().context("Failed to create OpenH264 decoder")?;
        Ok(Self { decoder, rgba_buf: Vec::new() })
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

        self.rgba_buf.resize(w * h * 4, 255);
        yuv_to_rgba(
            yuv.y(), yuv.u(), yuv.v(),
            yuv.strides().0, yuv.strides().1, yuv.strides().2,
            w, h, &mut self.rgba_buf,
        );

        Ok(Some(DecodedFrame { rgba: self.rgba_buf.clone(), width: w as u32, height: h as u32 }))
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
    rgba_buf: Vec<u8>,
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
}

// Safety: dav1d context is not shared; we call it from one thread at a time.
unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
    pub fn new() -> Result<Self> {
        unsafe {
            let mut settings: Dav1dSettings = std::mem::zeroed();
            dav1d_default_settings(&mut settings);
            settings.n_threads = 4;

            eprintln!("[av1] opening dav1d context (4 threads)...");
            let mut ctx: *mut c_void = std::ptr::null_mut();
            let rc = dav1d_open(&mut ctx, &settings);
            if rc != 0 || ctx.is_null() {
                anyhow::bail!("dav1d_open failed: {} ({})", rc, dav1d_strerror(rc));
            }

            eprintln!("[av1] dav1d context opened OK  ptr={:p}", ctx);
            Ok(Self {
                ctx,
                rgba_buf: Vec::new(),
                frames_in: 0,
                frames_out: 0,
                send_eagain: 0,
                get_eagain: 0,
                decode_errors: 0,
            })
        }
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        self.frames_in += 1;
        // Verbose per-frame logging only for the first 5 frames; after that errors only.
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

            let send_rc = dav1d_send_data(self.ctx, &mut dav1d_data);
            dav1d_data_unref(&mut dav1d_data);

            match send_rc {
                0 => {
                    if verbose { eprintln!("[av1]   send_data OK"); }
                }
                DAV1D_EAGAIN => {
                    self.send_eagain += 1;
                    if verbose || self.send_eagain <= 3 {
                        eprintln!("[av1]   send_data EAGAIN #{} (draining)", self.send_eagain);
                    }
                }
                rc => {
                    self.decode_errors += 1;
                    anyhow::bail!("dav1d_send_data error {} ({}) on frame #{}",
                        rc, dav1d_strerror(rc), self.frames_in);
                }
            }

            let mut pic: Dav1dPicture = std::mem::zeroed();
            let get_rc = dav1d_get_picture(self.ctx, &mut pic);

            match get_rc {
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
                        return Ok(None);
                    }

                    let y_stride = pic.stride[0] as usize;
                    let uv_stride = pic.stride[1] as usize;

                    let y_ptr = pic.data[0].map(|p| p.as_ptr() as *const u8);
                    let u_ptr = pic.data[1].map(|p| p.as_ptr() as *const u8);
                    let v_ptr = pic.data[2].map(|p| p.as_ptr() as *const u8);

                    if y_ptr.is_none() || u_ptr.is_none() || v_ptr.is_none() {
                        eprintln!("[av1]   ERROR: null plane pointers Y={} U={} V={}",
                            y_ptr.is_some(), u_ptr.is_some(), v_ptr.is_some());
                        dav1d_picture_unref(&mut pic);
                        return Ok(None);
                    }

                    let y_data = std::slice::from_raw_parts(y_ptr.unwrap(), y_stride * h);
                    let u_data = std::slice::from_raw_parts(u_ptr.unwrap(), uv_stride * ((h + 1) / 2));
                    let v_data = std::slice::from_raw_parts(v_ptr.unwrap(), uv_stride * ((h + 1) / 2));

                    self.rgba_buf.resize(w * h * 4, 255);
                    yuv_to_rgba(y_data, u_data, v_data, y_stride, uv_stride, uv_stride, w, h, &mut self.rgba_buf);

                    dav1d_picture_unref(&mut pic);
                    self.frames_out += 1;

                    // Stats every 300 frames (~5 s at 60 fps).
                    if self.frames_out % 300 == 0 {
                        eprintln!("[av1] stats: in={} out={} send_eagain={} get_eagain={} errors={}",
                            self.frames_in, self.frames_out,
                            self.send_eagain, self.get_eagain, self.decode_errors);
                    }

                    Ok(Some(DecodedFrame { rgba: self.rgba_buf.clone(), width: w as u32, height: h as u32 }))
                }
                DAV1D_EAGAIN => {
                    self.get_eagain += 1;
                    if verbose || self.get_eagain <= 3 {
                        eprintln!("[av1]   get_picture EAGAIN #{} (need more input)", self.get_eagain);
                    }
                    Ok(None)
                }
                rc => {
                    self.decode_errors += 1;
                    eprintln!("[av1]   get_picture ERROR {} ({}) on frame #{}",
                        rc, dav1d_strerror(rc), self.frames_in);
                    Ok(None)
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

// ---------------------------------------------------------------------------
// Integer BT.601 YUV→RGBA
// ---------------------------------------------------------------------------

fn yuv_to_rgba(
    y_data: &[u8], u_data: &[u8], v_data: &[u8],
    ys: usize, us: usize, vs: usize,
    w: usize, h: usize,
    rgba: &mut [u8],
) {
    for row in 0..h {
        let y_row = row * ys;
        let uv_u = (row / 2) * us;
        let uv_v = (row / 2) * vs;
        let dst_row = row * w * 4;

        for col in 0..w {
            let c = 298 * (y_data[y_row + col] as i32 - 16);
            let d = u_data[uv_u + col / 2] as i32 - 128;
            let e = v_data[uv_v + col / 2] as i32 - 128;

            let idx = dst_row + col * 4;
            rgba[idx]     = ((c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
            rgba[idx + 1] = ((c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
            rgba[idx + 2] = ((c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
        }
    }
}
