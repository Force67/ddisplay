pub mod nvenc;
pub mod nvenc_sys;

/// Encoded video packet.
#[derive(Debug)]
pub struct EncodedPacket {
    /// H.264 Annex B encoded data (with start codes)
    pub data: Vec<u8>,
    /// Whether this is a keyframe (IDR)
    pub keyframe: bool,
    /// Presentation timestamp
    pub pts: u64,
}

/// Trait for video encoders.
pub trait Encoder: Send {
    /// Encode a raw frame (BGRA format).
    fn encode(&mut self, frame_data: &[u8], width: u32, height: u32, stride: u32, force_keyframe: bool) -> anyhow::Result<EncodedPacket>;

    /// Flush the encoder and get any remaining packets.
    fn flush(&mut self) -> anyhow::Result<Vec<EncodedPacket>>;
}
