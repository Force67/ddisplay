/// H.264 encoder using Cisco's OpenH264 library.
///
/// Accepts BGRA frames from X11 capture, converts to YUV420P internally,
/// and produces H.264 Annex B encoded output.

use anyhow::{Context, Result};
use openh264::encoder::{
    Encoder, EncoderConfig, FrameType,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::Timestamp;

use super::{EncodedPacket, Encoder as EncoderTrait};

pub struct OpenH264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    pts_counter: u64,
    /// Pre-allocated YUV buffer for color conversion.
    yuv_buf: YUVBuffer,
}

unsafe impl Send for OpenH264Encoder {}

impl OpenH264Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let config = EncoderConfig::new()
            .bitrate(openh264::encoder::BitRate::from_bps(bitrate))
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(UsageType::ScreenContentRealTime)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .skip_frames(false)
            .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(fps)); // IDR every second

        let encoder = Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        ).context("Failed to create OpenH264 encoder")?;

        let yuv_buf = YUVBuffer::new(width as usize, height as usize);

        tracing::info!(
            "OpenH264 encoder initialized: {}x{} @ {} fps, {} bps",
            width, height, fps, bitrate,
        );

        Ok(Self {
            encoder,
            width,
            height,
            pts_counter: 0,
            yuv_buf,
        })
    }
}

impl EncoderTrait for OpenH264Encoder {
    fn encode(
        &mut self,
        frame_data: &[u8],
        width: u32,
        height: u32,
        stride: u32,
        force_keyframe: bool,
    ) -> Result<EncodedPacket> {
        if width != self.width || height != self.height {
            anyhow::bail!(
                "Frame dimensions {}x{} != encoder {}x{}",
                width, height, self.width, self.height,
            );
        }

        if force_keyframe {
            self.encoder.force_intra_frame();
        }

        // Convert BGRA to YUV420P.
        // If stride matches width*4, we can use BgraSliceU8 directly.
        // Otherwise, we need to compact the data first.
        let row_bytes = width as usize * 4;
        let expected_size = row_bytes * height as usize;

        if stride as usize == row_bytes {
            let bgra = BgraSliceU8::new(
                &frame_data[..expected_size],
                (width as usize, height as usize),
            );
            self.yuv_buf.read_rgb(bgra);
        } else {
            // Compact rows into a contiguous buffer
            let mut compact = vec![0u8; expected_size];
            for y in 0..height as usize {
                let src_offset = y * stride as usize;
                let dst_offset = y * row_bytes;
                compact[dst_offset..dst_offset + row_bytes]
                    .copy_from_slice(&frame_data[src_offset..src_offset + row_bytes]);
            }
            let bgra = BgraSliceU8::new(
                &compact,
                (width as usize, height as usize),
            );
            self.yuv_buf.read_rgb(bgra);
        }

        // Encode the YUV frame.
        let timestamp = Timestamp::from_millis(self.pts_counter * 33); // ~30fps timing
        let bitstream = self.encoder
            .encode_at(&self.yuv_buf, timestamp)
            .context("OpenH264 encode failed")?;

        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();

        let pts = self.pts_counter;
        self.pts_counter += 1;

        Ok(EncodedPacket {
            data,
            keyframe,
            pts,
        })
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        Ok(Vec::new())
    }
}
