/// H.264 encoder using Cisco's OpenH264 library.
///
/// Accepts BGRA frames from X11 capture, converts to YUV420P using our
/// own fast batch converter, and feeds raw planes to OpenH264.

use anyhow::{Context, Result};
use openh264::encoder::{
    Encoder, EncoderConfig, FrameType,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::YUVSource;
use openh264::Timestamp;

use super::color;
use super::{EncodedPacket, Encoder as EncoderTrait};

/// Zero-copy YUV420P view over a contiguous [Y|U|V] buffer.
struct YuvView<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    y_len: usize,
    u_len: usize,
}

impl YUVSource for YuvView<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.data[..self.y_len]
    }

    fn u(&self) -> &[u8] {
        &self.data[self.y_len..self.y_len + self.u_len]
    }

    fn v(&self) -> &[u8] {
        &self.data[self.y_len + self.u_len..]
    }
}

pub struct OpenH264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    pts_counter: u64,
    /// Single contiguous buffer for YUV data: [Y | U | V]
    yuv_data: Vec<u8>,
    y_len: usize,
    u_len: usize,
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
            .skip_frames(true)
            .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(fps));

        let encoder = Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        ).context("Failed to create OpenH264 encoder")?;

        let w = width as usize;
        let h = height as usize;
        let y_len = w * h;
        let u_len = (w / 2) * (h / 2);
        let yuv_data = vec![0u8; y_len + u_len + u_len];

        tracing::info!(
            "OpenH264 encoder initialized: {}x{} @ {} fps, {} bps",
            width, height, fps, bitrate,
        );

        Ok(Self {
            encoder,
            width,
            height,
            pts_counter: 0,
            yuv_data,
            y_len,
            u_len,
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

        let w = width as usize;
        let h = height as usize;

        // Fast BGRA -> YUV420 conversion into our contiguous buffer.
        // Split the buffer into Y, U, V plane slices.
        let (y_plane, uv_rest) = self.yuv_data.split_at_mut(self.y_len);
        let (u_plane, v_plane) = uv_rest.split_at_mut(self.u_len);

        color::bgra_to_yuv420(
            frame_data,
            w,
            h,
            stride as usize,
            y_plane,
            u_plane,
            v_plane,
        );

        // Zero-copy borrow of our pre-allocated buffer (avoids ~3MB clone per frame).
        let yuv_view = YuvView {
            data: &self.yuv_data,
            width: w,
            height: h,
            y_len: self.y_len,
            u_len: self.u_len,
        };

        let timestamp = Timestamp::from_millis(self.pts_counter * 16);
        let bitstream = self.encoder
            .encode_at(&yuv_view, timestamp)
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
