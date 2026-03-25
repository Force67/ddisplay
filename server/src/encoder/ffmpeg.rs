/// Software H.264 encoder using ffmpeg as a subprocess.
///
/// Pipes raw BGRA frames to ffmpeg's stdin and reads H.264 Annex B
/// encoded output from stdout. Uses libx264 with ultrafast/zerolatency
/// settings for minimal encoding latency.
///
/// Synchronization: a reader thread continuously reads from ffmpeg stdout.
/// After the main thread writes a frame and flushes stdin, it sends a
/// "frame written" signal. The reader thread then waits briefly for ffmpeg
/// to produce output, collects it, and sends it back.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};

use super::{EncodedPacket, Encoder};

/// H.264 encoder backed by an ffmpeg subprocess.
pub struct FfmpegEncoder {
    child: Child,
    stdin: std::process::ChildStdin,
    /// Signal to the reader that a frame was written.
    frame_written_tx: mpsc::SyncSender<()>,
    /// Receive encoded packets from the reader thread.
    encoded_rx: mpsc::Receiver<EncodedPacket>,
    /// Signal to stop the reader thread.
    shutdown: Arc<AtomicBool>,
    width: u32,
    height: u32,
    pts_counter: u64,
}

unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    fn ffmpeg_path() -> &'static str {
        if std::path::Path::new("/home/captainspark/.local/bin/ffmpeg").exists() {
            "/home/captainspark/.local/bin/ffmpeg"
        } else {
            "ffmpeg"
        }
    }

    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let size = format!("{}x{}", width, height);
        let br = format!("{}", bitrate);
        let fr = format!("{}", fps);
        let gop = format!("{}", fps);

        let mut child = Command::new(Self::ffmpeg_path())
            .args([
                "-hide_banner",
                "-loglevel", "error",
                "-f", "rawvideo",
                "-pixel_format", "bgra",
                "-video_size", &size,
                "-framerate", &fr,
                "-i", "pipe:0",
                "-pix_fmt", "yuv420p",
                "-c:v", "libx264",
                "-preset", "ultrafast",
                "-tune", "zerolatency",
                "-profile:v", "baseline",
                "-level", "4.0",
                "-b:v", &br,
                "-maxrate", &br,
                "-bufsize", &format!("{}", bitrate / fps),
                "-g", &gop,
                "-keyint_min", &gop,
                "-bf", "0",
                "-flags", "+cgop",
                "-sc_threshold", "0",
                "-x264-params", "repeat-headers=1",
                "-f", "h264",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn ffmpeg")?;

        let stdin = child.stdin.take().context("No stdin")?;
        let mut stdout = child.stdout.take().context("No stdout")?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let (frame_written_tx, frame_written_rx) = mpsc::sync_channel::<()>(1);
        let (encoded_tx, encoded_rx) = mpsc::sync_channel::<EncodedPacket>(4);

        // Reader thread: waits for "frame written" signals, then reads output
        thread::Builder::new()
            .name("ffmpeg-reader".into())
            .spawn(move || {
                let mut accum = Vec::with_capacity(256 * 1024);
                let mut buf = vec![0u8; 128 * 1024];
                let mut frame_idx: u64 = 0;

                // Set stdout to non-blocking using platform-specific calls
                #[cfg(unix)]
                {
                    use std::os::unix::io::AsRawFd;
                    let fd = stdout.as_raw_fd();
                    unsafe {
                        let flags = libc::fcntl(fd, libc::F_GETFL);
                        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                }

                while !shutdown_clone.load(Ordering::Relaxed) {
                    // Wait for the main thread to signal that a frame was written
                    match frame_written_rx.recv_timeout(std::time::Duration::from_secs(5)) {
                        Ok(()) => {}
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }

                    // Give ffmpeg a moment to encode and flush output.
                    // With zerolatency, this should be very fast.
                    thread::sleep(std::time::Duration::from_millis(5));

                    // Read all available output (non-blocking)
                    loop {
                        match stdout.read(&mut buf) {
                            Ok(0) => break, // EOF
                            Ok(n) => {
                                accum.extend_from_slice(&buf[..n]);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(_) => break,
                        }
                    }

                    if accum.is_empty() {
                        // No data yet, try reading with a blocking delay
                        // Set back to blocking mode temporarily
                        #[cfg(unix)]
                        {
                            use std::os::unix::io::AsRawFd;
                            let fd = stdout.as_raw_fd();
                            unsafe {
                                let flags = libc::fcntl(fd, libc::F_GETFL);
                                libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
                            }
                        }

                        // Read with a timeout by using a blocking read
                        match stdout.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => accum.extend_from_slice(&buf[..n]),
                            Err(_) => break,
                        }

                        // Back to non-blocking
                        #[cfg(unix)]
                        {
                            use std::os::unix::io::AsRawFd;
                            let fd = stdout.as_raw_fd();
                            unsafe {
                                let flags = libc::fcntl(fd, libc::F_GETFL);
                                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                            }
                        }

                        // Try to read more non-blocking
                        loop {
                            match stdout.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => accum.extend_from_slice(&buf[..n]),
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(_) => break,
                            }
                        }
                    }

                    if !accum.is_empty() {
                        let data = std::mem::take(&mut accum);
                        let keyframe = is_keyframe(&data);
                        frame_idx += 1;

                        if encoded_tx.send(EncodedPacket {
                            data,
                            keyframe,
                            pts: frame_idx,
                        }).is_err() {
                            break;
                        }
                    }
                }
            })
            .context("Failed to spawn reader thread")?;

        tracing::info!(
            "ffmpeg H.264 encoder started: {}x{} @ {} fps, {} bps (libx264 ultrafast/zerolatency)",
            width, height, fps, bitrate,
        );

        Ok(Self {
            child,
            stdin,
            frame_written_tx,
            encoded_rx,
            shutdown,
            width,
            height,
            pts_counter: 0,
        })
    }
}

impl Encoder for FfmpegEncoder {
    fn encode(
        &mut self,
        frame_data: &[u8],
        width: u32,
        height: u32,
        stride: u32,
        _force_keyframe: bool,
    ) -> Result<EncodedPacket> {
        if width != self.width || height != self.height {
            bail!("Dimension mismatch: {}x{} vs {}x{}", width, height, self.width, self.height);
        }

        // Write the raw BGRA frame to ffmpeg's stdin
        let row_bytes = (width as usize) * 4;
        if stride as usize == row_bytes {
            let frame_size = row_bytes * height as usize;
            self.stdin.write_all(&frame_data[..frame_size])
                .context("write to ffmpeg failed")?;
        } else {
            for y in 0..height as usize {
                let offset = y * stride as usize;
                self.stdin.write_all(&frame_data[offset..offset + row_bytes])
                    .context("write row to ffmpeg failed")?;
            }
        }
        self.stdin.flush().context("flush ffmpeg stdin")?;

        // Signal the reader that a frame was written
        let _ = self.frame_written_tx.send(());

        // Wait for the encoded frame
        let mut packet = self.encoded_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .context("Timeout waiting for encoded frame")?;

        packet.pts = self.pts_counter;
        self.pts_counter += 1;

        Ok(packet)
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        Ok(Vec::new())
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Check if H.264 Annex B data contains an IDR NAL unit (keyframe).
fn is_keyframe(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        // Look for start code: 0x00 0x00 0x01 or 0x00 0x00 0x00 0x01
        if data[i] == 0 && data[i + 1] == 0 {
            let (nal_start, _sc_len) = if data[i + 2] == 1 {
                (i + 3, 3)
            } else if i + 4 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                (i + 4, 4)
            } else {
                i += 1;
                continue;
            };
            if nal_start < data.len() {
                let nal_type = data[nal_start] & 0x1F;
                if nal_type == 5 {
                    return true; // IDR
                }
            }
            i = nal_start;
        } else {
            i += 1;
        }
    }
    false
}
