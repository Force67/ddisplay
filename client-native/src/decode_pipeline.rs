//! Background video decode for one stream. A decode thread turns encoded
//! frames into decoded frames in a slot the render loop reads. One pipeline per
//! monitor head, so each head decodes its own independent stream.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use crate::decoder::{self, DecodedFrame, DecoderOptions};

struct DecodeJob {
    data: Vec<u8>,
    keyframe: bool,
}

/// What the caller should do after handing a frame to [`DecodePipeline::submit`].
#[derive(Default)]
pub struct SubmitOutcome {
    pub dropped: bool,
    pub request_keyframe: bool,
}

pub struct DecodePipeline {
    decode_tx: Option<SyncSender<DecodeJob>>,
    frame_slot: Arc<Mutex<Option<DecodedFrame>>>,
    frame_ready: Arc<AtomicBool>,
    needs_keyframe: Arc<AtomicBool>,
    decode_us: Arc<AtomicU32>,
    /// After a dropped frame the bitstream is broken until the next keyframe.
    skip_until_keyframe: bool,
    options: DecoderOptions,
    pub codec: String,
}

impl DecodePipeline {
    pub fn new(codec: &str, options: DecoderOptions) -> Self {
        let mut pipeline = Self {
            decode_tx: None,
            frame_slot: Arc::new(Mutex::new(None)),
            frame_ready: Arc::new(AtomicBool::new(false)),
            needs_keyframe: Arc::new(AtomicBool::new(false)),
            decode_us: Arc::new(AtomicU32::new(0)),
            skip_until_keyframe: false,
            options,
            codec: codec.to_string(),
        };
        pipeline.start(codec);
        pipeline
    }

    /// Switch codec, restarting the decode thread only if it actually changed.
    pub fn set_codec(&mut self, codec: &str) {
        if codec != self.codec {
            self.start(codec);
            self.codec = codec.to_string();
        }
    }

    fn start(&mut self, codec: &str) {
        // Dropping the old sender closes the channel so the old thread exits.
        self.decode_tx = None;
        self.skip_until_keyframe = false;

        // A shallow queue: every buffered frame is display latency, and
        // inter-frames can't be skipped without breaking the reference chain.
        // 4 frames is about 66ms at 60fps before we drop and resync on an IDR.
        let (tx, rx) = std::sync::mpsc::sync_channel::<DecodeJob>(4);
        self.decode_tx = Some(tx);

        let slot = self.frame_slot.clone();
        let frame_ready = self.frame_ready.clone();
        let needs_keyframe = self.needs_keyframe.clone();
        let decode_us = self.decode_us.clone();
        let codec = codec.to_string();
        let options = self.options.clone();

        std::thread::Builder::new()
            .name(format!("decode-{codec}"))
            .spawn(move || {
                let mut dec = match decoder::VideoDecoder::for_codec(&codec, &options) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("[decode] FATAL: {codec} decoder init failed: {e}");
                        return;
                    }
                };
                // A fresh decoder (or one after a self-reset) needs a keyframe
                // before it can parse inter-frames.
                let mut awaiting_keyframe = true;
                let mut decode_ema_us: f32 = 0.0;

                while let Ok(job) = rx.recv() {
                    if awaiting_keyframe && !job.keyframe {
                        continue;
                    }
                    awaiting_keyframe = false;

                    let t0 = std::time::Instant::now();
                    match dec.decode(&job.data) {
                        Ok(Some(frame)) => {
                            *slot.lock().unwrap() = Some(frame);
                            frame_ready.store(true, Ordering::Relaxed);
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("[decode] error: {e}"),
                    }
                    let us = t0.elapsed().as_micros() as f32;
                    decode_ema_us = if decode_ema_us == 0.0 {
                        us
                    } else {
                        decode_ema_us * 0.9 + us * 0.1
                    };
                    decode_us.store(decode_ema_us as u32, Ordering::Relaxed);

                    if dec.take_needs_keyframe() {
                        awaiting_keyframe = true;
                        needs_keyframe.store(true, Ordering::Relaxed);
                    }
                }
            })
            .expect("failed to spawn decode thread");
    }

    /// Hand an encoded frame to the decode thread.
    pub fn submit(&mut self, data: &[u8], keyframe: bool) -> SubmitOutcome {
        let mut outcome = SubmitOutcome::default();
        if self.skip_until_keyframe && !keyframe {
            outcome.dropped = true;
            return outcome;
        }
        let Some(tx) = &self.decode_tx else {
            return outcome;
        };
        match tx.try_send(DecodeJob {
            data: data.to_vec(),
            keyframe,
        }) {
            Ok(()) => self.skip_until_keyframe = false,
            Err(_) => {
                // Decode thread is behind: drop and resync on the next IDR.
                outcome.dropped = true;
                if !self.skip_until_keyframe {
                    self.skip_until_keyframe = true;
                    outcome.request_keyframe = true;
                }
            }
        }
        outcome
    }

    /// Take the latest decoded frame, if the render loop hasn't already.
    pub fn take_frame(&self) -> Option<DecodedFrame> {
        self.frame_slot.lock().unwrap().take()
    }

    /// Whether a new frame arrived since the last call (clears the flag).
    pub fn take_frame_ready(&self) -> bool {
        self.frame_ready.swap(false, Ordering::Relaxed)
    }

    /// Whether the decoder self-reset and needs a fresh keyframe (clears it).
    pub fn take_needs_keyframe(&self) -> bool {
        self.needs_keyframe.swap(false, Ordering::Relaxed)
    }

    pub fn decode_ms(&self) -> f32 {
        self.decode_us.load(Ordering::Relaxed) as f32 / 1000.0
    }
}
