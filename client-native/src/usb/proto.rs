/// usbip URB protocol, device side. Big-endian PDUs with a fixed 48-byte
/// header, as specified by the kernel's Documentation/usb/usbip_protocol.rst.
/// Only the URB phase exists here: the sysfs attach on the server replaces
/// the OP_REQ/REP handshake of a full usbip daemon.

pub const CMD_SUBMIT: u32 = 1;
pub const CMD_UNLINK: u32 = 2;
pub const RET_SUBMIT: u32 = 3;
pub const RET_UNLINK: u32 = 4;

pub const DIR_OUT: u32 = 0;
pub const DIR_IN: u32 = 1;

pub const HEADER_LEN: usize = 48;

/// Upper bound on a single URB's transfer buffer. vhci never sends anything
/// close to this; bigger means the stream is corrupt.
const MAX_TRANSFER: u32 = 32 * 1024 * 1024;
/// Iso descriptors are 16 bytes each; same corruption guard.
const MAX_ISO_PACKETS: u32 = 1024;

/// URB cancelled by CMD_UNLINK (the status the kernel expects then).
pub const ECONNRESET: i32 = -104;
/// Unsupported request (isochronous endpoints).
pub const EINVAL: i32 = -22;
/// Device disappeared.
pub const ENODEV: i32 = -19;
/// Endpoint stalled.
pub const EPIPE: i32 = -32;
pub const ETIMEDOUT: i32 = -110;
/// Transfer-level I/O error (what the kernel reports for CRC/bit-stuff errors).
pub const EPROTO: i32 = -71;
pub const EIO: i32 = -5;

#[derive(Debug, PartialEq)]
pub struct SubmitCmd {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    pub transfer_buffer_length: u32,
    /// 0 (or the doc's 0xffffffff) for non-iso URBs.
    pub number_of_packets: u32,
    /// The 8 setup bytes; meaningful for control transfers only.
    pub setup: [u8; 8],
    /// OUT transfer payload (empty for IN).
    pub data: Vec<u8>,
}

impl SubmitCmd {
    pub fn is_iso(&self) -> bool {
        self.number_of_packets != 0 && self.number_of_packets != u32::MAX
    }
}

#[derive(Debug, PartialEq)]
pub enum Pdu {
    Submit(SubmitCmd),
    /// Cancel the URB with seqnum `unlink_seqnum`; reply with RET_UNLINK
    /// carrying this command's own `seqnum`.
    Unlink { seqnum: u32, unlink_seqnum: u32 },
}

/// Reassembles PDUs from arbitrarily chunked stream bytes.
pub struct PduReader {
    buf: Vec<u8>,
}

impl PduReader {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Next complete PDU, `Ok(None)` while more bytes are needed, `Err` when
    /// the stream can only be corrupt (caller should drop the device).
    pub fn next(&mut self) -> Result<Option<Pdu>, String> {
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let command = be32(&self.buf[0..4]);
        let seqnum = be32(&self.buf[4..8]);
        match command {
            CMD_SUBMIT => {
                let devid = be32(&self.buf[8..12]);
                let direction = be32(&self.buf[12..16]);
                let ep = be32(&self.buf[16..20]);
                let transfer_buffer_length = be32(&self.buf[24..28]);
                let number_of_packets = be32(&self.buf[32..36]);
                if transfer_buffer_length > MAX_TRANSFER {
                    return Err(format!("transfer_buffer_length {transfer_buffer_length}"));
                }
                let iso_packets = match number_of_packets {
                    0 | u32::MAX => 0,
                    n if n > MAX_ISO_PACKETS => {
                        return Err(format!("number_of_packets {n}"));
                    }
                    n => n,
                };
                // OUT URBs carry the transfer buffer; iso URBs additionally
                // carry 16 bytes per packet descriptor (both directions).
                let data_len = if direction == DIR_OUT {
                    transfer_buffer_length as usize
                } else {
                    0
                };
                let total = HEADER_LEN + data_len + iso_packets as usize * 16;
                if self.buf.len() < total {
                    return Ok(None);
                }
                let mut setup = [0u8; 8];
                setup.copy_from_slice(&self.buf[40..48]);
                let data = self.buf[HEADER_LEN..HEADER_LEN + data_len].to_vec();
                self.buf.drain(..total);
                Ok(Some(Pdu::Submit(SubmitCmd {
                    seqnum,
                    devid,
                    direction,
                    ep,
                    transfer_buffer_length,
                    number_of_packets,
                    setup,
                    data,
                })))
            }
            CMD_UNLINK => {
                let unlink_seqnum = be32(&self.buf[20..24]);
                self.buf.drain(..HEADER_LEN);
                Ok(Some(Pdu::Unlink { seqnum, unlink_seqnum }))
            }
            other => Err(format!("unexpected command {other}")),
        }
    }
}

/// RET_SUBMIT for a completed URB. `data` is the received payload for IN
/// transfers and must be empty for OUT (whose byte count goes in `actual`).
pub fn ret_submit(cmd: &SubmitCmd, status: i32, actual: u32, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + data.len());
    buf.extend_from_slice(&RET_SUBMIT.to_be_bytes());
    buf.extend_from_slice(&cmd.seqnum.to_be_bytes());
    buf.extend_from_slice(&cmd.devid.to_be_bytes());
    buf.extend_from_slice(&cmd.direction.to_be_bytes());
    buf.extend_from_slice(&cmd.ep.to_be_bytes());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&(actual as i32).to_be_bytes());
    buf.extend_from_slice(&0i32.to_be_bytes()); // start_frame
    buf.extend_from_slice(&0i32.to_be_bytes()); // number_of_packets (non-iso)
    buf.extend_from_slice(&0i32.to_be_bytes()); // error_count
    buf.extend_from_slice(&[0u8; 8]); // padding
    buf.extend_from_slice(data);
    buf
}

/// RET_UNLINK answering the CMD_UNLINK with seqnum `unlink_cmd_seqnum`.
/// Status is -ECONNRESET when the URB was cancelled, 0 when it had already
/// completed (its RET_SUBMIT went out normally).
pub fn ret_unlink(unlink_cmd_seqnum: u32, status: i32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN);
    buf.extend_from_slice(&RET_UNLINK.to_be_bytes());
    buf.extend_from_slice(&unlink_cmd_seqnum.to_be_bytes());
    buf.extend_from_slice(&[0u8; 12]); // devid, direction, ep
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&[0u8; 24]); // padding
    buf
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submit_bytes(seqnum: u32, direction: u32, ep: u32, tbl: u32, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&CMD_SUBMIT.to_be_bytes());
        b.extend_from_slice(&seqnum.to_be_bytes());
        b.extend_from_slice(&0x0001_0002u32.to_be_bytes()); // devid
        b.extend_from_slice(&direction.to_be_bytes());
        b.extend_from_slice(&ep.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // transfer_flags
        b.extend_from_slice(&tbl.to_be_bytes());
        b.extend_from_slice(&0i32.to_be_bytes()); // start_frame
        b.extend_from_slice(&0u32.to_be_bytes()); // number_of_packets
        b.extend_from_slice(&0i32.to_be_bytes()); // interval
        b.extend_from_slice(&[0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]); // setup
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn parses_out_submit_with_payload() {
        let mut r = PduReader::new();
        r.feed(&submit_bytes(7, DIR_OUT, 2, 4, b"abcd"));
        let Some(Pdu::Submit(cmd)) = r.next().unwrap() else {
            panic!("expected submit");
        };
        assert_eq!(cmd.seqnum, 7);
        assert_eq!(cmd.ep, 2);
        assert_eq!(cmd.data, b"abcd");
        assert!(r.next().unwrap().is_none());
    }

    #[test]
    fn in_submit_carries_no_payload() {
        let mut r = PduReader::new();
        r.feed(&submit_bytes(9, DIR_IN, 1, 512, &[]));
        let Some(Pdu::Submit(cmd)) = r.next().unwrap() else {
            panic!("expected submit");
        };
        assert_eq!(cmd.transfer_buffer_length, 512);
        assert!(cmd.data.is_empty());
        assert_eq!(cmd.setup[1], 0x06);
    }

    #[test]
    fn reassembles_across_chunks() {
        let bytes = submit_bytes(3, DIR_OUT, 4, 8, b"12345678");
        let mut r = PduReader::new();
        r.feed(&bytes[..20]);
        assert!(r.next().unwrap().is_none());
        r.feed(&bytes[20..50]);
        assert!(r.next().unwrap().is_none());
        r.feed(&bytes[50..]);
        assert!(matches!(r.next().unwrap(), Some(Pdu::Submit(_))));
    }

    #[test]
    fn parses_unlink() {
        let mut b = Vec::new();
        b.extend_from_slice(&CMD_UNLINK.to_be_bytes());
        b.extend_from_slice(&100u32.to_be_bytes());
        b.extend_from_slice(&[0u8; 12]);
        b.extend_from_slice(&42u32.to_be_bytes()); // unlink_seqnum
        b.extend_from_slice(&[0u8; 24]);
        let mut r = PduReader::new();
        r.feed(&b);
        assert_eq!(
            r.next().unwrap(),
            Some(Pdu::Unlink { seqnum: 100, unlink_seqnum: 42 })
        );
    }

    #[test]
    fn rejects_corrupt_stream() {
        let mut r = PduReader::new();
        r.feed(&submit_bytes(1, DIR_OUT, 1, MAX_TRANSFER + 1, &[]));
        assert!(r.next().is_err());

        let mut r = PduReader::new();
        r.feed(&[0xffu8; HEADER_LEN]);
        assert!(r.next().is_err());
    }

    #[test]
    fn ret_pdus_have_header_size_and_payload() {
        let cmd = SubmitCmd {
            seqnum: 5,
            devid: 0x10002,
            direction: DIR_IN,
            ep: 1,
            transfer_buffer_length: 64,
            number_of_packets: 0,
            setup: [0; 8],
            data: vec![],
        };
        let ret = ret_submit(&cmd, 0, 3, b"xyz");
        assert_eq!(ret.len(), HEADER_LEN + 3);
        assert_eq!(be32(&ret[0..4]), RET_SUBMIT);
        assert_eq!(be32(&ret[4..8]), 5);
        assert_eq!(be32(&ret[24..28]), 3); // actual_length

        let ret = ret_unlink(100, ECONNRESET);
        assert_eq!(ret.len(), HEADER_LEN);
        assert_eq!(be32(&ret[0..4]), RET_UNLINK);
        assert_eq!(ret[20..24], ECONNRESET.to_be_bytes());
    }
}
