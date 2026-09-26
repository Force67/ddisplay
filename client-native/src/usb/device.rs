/// One forwarded USB device: a libusb handle plus per-endpoint worker threads
/// that execute the URBs the server's vhci sends us.
///
/// Ordering matters per endpoint (bulk streams, control sequencing), so every
/// endpoint gets its own FIFO worker. Across endpoints URBs complete freely;
/// the kernel matches replies by seqnum.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rusb::constants::*;
use rusb::{DeviceHandle, GlobalContext, TransferType};

use crate::protocol;
use crate::transport::TransportSender;
use super::proto::{self, Pdu, PduReader, SubmitCmd, DIR_IN};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const OUT_TIMEOUT: Duration = Duration::from_secs(30);
/// IN transfers wait in slices so pending URBs notice an unlink promptly.
const IN_SLICE: Duration = Duration::from_millis(500);

pub struct SharedDevice {
    token: u32,
    reader: PduReader,
    /// One URB queue per endpoint; key is the endpoint address (direction
    /// bit included, 0 for the shared control pipe).
    workers: HashMap<u8, mpsc::Sender<SubmitCmd>>,
    threads: Vec<JoinHandle<()>>,
    exec: Arc<Exec>,
    attach_json: Vec<u8>,
}

/// Everything a worker needs to run and answer URBs.
struct Exec {
    token: u32,
    handle: DeviceHandle<GlobalContext>,
    config: Mutex<ActiveConfig>,
    urbs: Mutex<UrbMaps>,
    /// Set on the first ENODEV; stops all workers answering further URBs.
    dead: AtomicBool,
    detach_sent: AtomicBool,
    /// Set when the SharedDevice is dropped; workers exit without replying.
    stop: AtomicBool,
    sender: TransportSender,
}

struct ActiveConfig {
    number: u8,
    claimed: Vec<u8>,
    /// Endpoint address (with direction bit) -> transfer type, across all
    /// alt settings of the active configuration.
    ep_types: HashMap<u8, TransferType>,
}

#[derive(Default)]
struct UrbMaps {
    /// Handed to a worker, no reply sent yet.
    pending: HashSet<u32>,
    /// URB seqnum -> the CMD_UNLINK seqnum that cancelled it.
    unlinked: HashMap<u32, u32>,
}

impl SharedDevice {
    /// Open and claim the first device matching vid:pid and build the attach
    /// announcement for it.
    pub fn open(
        vid: u16,
        pid: u16,
        token: u32,
        sender: TransportSender,
    ) -> anyhow::Result<Self> {
        let device = rusb::devices()?
            .iter()
            .find(|d| {
                d.device_descriptor()
                    .map(|desc| desc.vendor_id() == vid && desc.product_id() == pid)
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow::anyhow!("no device {vid:04x}:{pid:04x} found"))?;

        let handle = device.open().map_err(|e| {
            anyhow::anyhow!("open failed: {e} (Windows needs a WinUSB driver on the device, e.g. via Zadig)")
        })?;
        // Steal the device from its host-side driver while shared (Linux;
        // not supported and not needed elsewhere).
        let _ = handle.set_auto_detach_kernel_driver(true);

        let config = claim_active_config(&handle)?;

        let product = handle
            .read_product_string_ascii(&device.device_descriptor()?)
            .unwrap_or_default();
        let attach_json = serde_json::to_vec(&serde_json::json!({
            "token": token,
            "vendor_id": vid,
            "product_id": pid,
            "busnum": device.bus_number() as u32,
            "devnum": device.address() as u32,
            "speed": kernel_speed(device.speed()),
            "product": product,
        }))
        .unwrap();

        Ok(Self {
            token,
            reader: PduReader::new(),
            workers: HashMap::new(),
            threads: Vec::new(),
            exec: Arc::new(Exec {
                token,
                handle,
                config: Mutex::new(config),
                urbs: Mutex::new(UrbMaps::default()),
                dead: AtomicBool::new(false),
                detach_sent: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                sender,
            }),
            attach_json,
        })
    }

    pub fn attach_json(&self) -> &[u8] {
        &self.attach_json
    }

    pub fn is_dead(&self) -> bool {
        self.exec.dead.load(Ordering::Relaxed)
    }

    /// Ingest a chunk of the usbip stream and dispatch complete PDUs.
    /// An error means the stream is corrupt and the device must be dropped.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.reader.feed(chunk);
        while let Some(pdu) = self.reader.next()? {
            match pdu {
                Pdu::Submit(cmd) => {
                    self.exec.urbs.lock().unwrap().pending.insert(cmd.seqnum);
                    self.worker_for(&cmd)
                        .send(cmd)
                        .map_err(|_| "worker thread gone".to_string())?;
                }
                Pdu::Unlink { seqnum, unlink_seqnum } => {
                    let mut urbs = self.exec.urbs.lock().unwrap();
                    if urbs.pending.contains(&unlink_seqnum) {
                        urbs.unlinked.insert(unlink_seqnum, seqnum);
                    } else {
                        // Already completed; its RET_SUBMIT went out normally.
                        drop(urbs);
                        self.exec.send_pdu(proto::ret_unlink(seqnum, 0));
                    }
                }
            }
        }
        Ok(())
    }

    fn worker_for(&mut self, cmd: &SubmitCmd) -> &mpsc::Sender<SubmitCmd> {
        let key = endpoint_address(cmd);
        self.workers.entry(key).or_insert_with(|| {
            let (tx, rx) = mpsc::channel::<SubmitCmd>();
            let exec = self.exec.clone();
            let thread = std::thread::Builder::new()
                .name(format!("usb-{}-ep{:02x}", self.token, key))
                .spawn(move || {
                    while let Ok(cmd) = rx.recv() {
                        if exec.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        exec.run(cmd);
                    }
                })
                .expect("spawn usb worker");
            self.threads.push(thread);
            tx
        })
    }
}

/// Stop and join the workers so the last Exec reference, and with it the
/// DeviceHandle, goes away here: that releases the interfaces and gives the
/// device back to its host driver before a reconnect claims it again.
impl Drop for SharedDevice {
    fn drop(&mut self) {
        self.exec.stop.store(true, Ordering::Relaxed);
        self.workers.clear();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Endpoint address for worker routing: direction bit set for IN, 0 for the
/// (bidirectional, strictly ordered) control pipe.
fn endpoint_address(cmd: &SubmitCmd) -> u8 {
    if cmd.ep == 0 {
        0
    } else {
        cmd.ep as u8 | if cmd.direction == DIR_IN { 0x80 } else { 0 }
    }
}

impl Exec {
    fn run(&self, cmd: SubmitCmd) {
        if self.dead.load(Ordering::Relaxed) {
            return self.finish(&cmd, proto::ENODEV, 0, &[]);
        }
        if cmd.is_iso() {
            // Isochronous forwarding (audio, webcams) is not implemented.
            return self.finish(&cmd, proto::EINVAL, 0, &[]);
        }
        if cmd.ep == 0 {
            self.control(cmd)
        } else {
            self.stream(cmd)
        }
    }

    fn control(&self, cmd: SubmitCmd) {
        let bm = cmd.setup[0];
        let request = cmd.setup[1];
        let value = u16::from_le_bytes([cmd.setup[2], cmd.setup[3]]);
        let index = u16::from_le_bytes([cmd.setup[4], cmd.setup[5]]);
        let length = u16::from_le_bytes([cmd.setup[6], cmd.setup[7]]);

        // Requests that change libusb-visible state must go through the
        // libusb API instead of the raw pipe (mirrors the kernel stub driver).
        match (bm, request) {
            // SET_CONFIGURATION
            (0x00, 0x09) => return self.set_configuration(cmd, value as u8),
            // SET_INTERFACE
            (0x01, 0x0b) => {
                let r = self.handle.set_alternate_setting(index as u8, value as u8);
                return self.finish_result(&cmd, r.map(|_| 0));
            }
            // CLEAR_FEATURE(ENDPOINT_HALT)
            (0x02, 0x01) if value == 0 => {
                let r = self.handle.clear_halt(index as u8);
                return self.finish_result(&cmd, r.map(|_| 0));
            }
            _ => {}
        }

        if bm & 0x80 != 0 {
            let mut buf = vec![0u8; length as usize];
            match self.handle.read_control(bm, request, value, index, &mut buf, CONTROL_TIMEOUT) {
                Ok(n) => self.finish(&cmd, 0, n as u32, &buf[..n]),
                Err(e) => self.finish(&cmd, self.errno(e), 0, &[]),
            }
        } else {
            let r = self.handle.write_control(bm, request, value, index, &cmd.data, CONTROL_TIMEOUT);
            self.finish_result(&cmd, r);
        }
    }

    /// SET_CONFIGURATION needs interfaces released first, and everything
    /// re-claimed (plus a fresh endpoint map) afterwards.
    fn set_configuration(&self, cmd: SubmitCmd, number: u8) {
        let mut config = self.config.lock().unwrap();
        if config.number == number {
            return self.finish(&cmd, 0, 0, &[]);
        }
        for iface in config.claimed.drain(..) {
            let _ = self.handle.release_interface(iface);
        }
        if let Err(e) = self.handle.set_active_configuration(number) {
            return self.finish(&cmd, self.errno(e), 0, &[]);
        }
        match claim_active_config(&self.handle) {
            Ok(fresh) => {
                *config = fresh;
                self.finish(&cmd, 0, 0, &[]);
            }
            Err(e) => {
                tracing::warn!("[usb] reclaim after SET_CONFIGURATION({number}) failed: {e:#}");
                self.finish(&cmd, proto::EIO, 0, &[]);
            }
        }
    }

    fn stream(&self, cmd: SubmitCmd) {
        let ep = endpoint_address(&cmd);
        let ttype = self
            .config
            .lock()
            .unwrap()
            .ep_types
            .get(&ep)
            .copied()
            .unwrap_or(TransferType::Bulk);

        if cmd.direction == DIR_IN {
            // Stay pending (like a real URB) until the transfer completes, an
            // error, or an unlink; the slice bounds how late an unlink or stop
            // is noticed. Bytes read before a slice timed out stay in buf.
            let mut buf = vec![0u8; cmd.transfer_buffer_length as usize];
            let mut done = 0;
            loop {
                if self.stop.load(Ordering::Relaxed) {
                    return;
                }
                if self.dead.load(Ordering::Relaxed) {
                    return self.finish(&cmd, proto::ENODEV, 0, &[]);
                }
                if self.urbs.lock().unwrap().unlinked.contains_key(&cmd.seqnum) {
                    // finish() turns this into the RET_UNLINK reply.
                    return self.finish(&cmd, proto::ECONNRESET, 0, &[]);
                }
                let (rc, n) = self.read_slice(ep, ttype, &mut buf[done..]);
                done += n;
                match rc {
                    0 => return self.finish(&cmd, 0, done as u32, &buf[..done]),
                    LIBUSB_ERROR_TIMEOUT => continue,
                    rc => return self.finish(&cmd, self.errno(libusb_error(rc)), 0, &[]),
                }
            }
        } else {
            let timeout = match ttype {
                TransferType::Interrupt => CONTROL_TIMEOUT,
                _ => OUT_TIMEOUT,
            };
            let r = match ttype {
                TransferType::Interrupt => self.handle.write_interrupt(ep, &cmd.data, timeout),
                _ => self.handle.write_bulk(ep, &cmd.data, timeout),
            };
            self.finish_result(&cmd, r);
        }
    }

    /// One IN wait. Calls libusb directly because rusb reports a timeout
    /// after partial data as plain success, which would complete the URB
    /// short at a slice boundary. Returns the libusb code and bytes read.
    fn read_slice(&self, ep: u8, ttype: TransferType, buf: &mut [u8]) -> (i32, usize) {
        let transfer = match ttype {
            TransferType::Interrupt => rusb::ffi::libusb_interrupt_transfer,
            _ => rusb::ffi::libusb_bulk_transfer,
        };
        let mut transferred = 0;
        let rc = unsafe {
            transfer(
                self.handle.as_raw(),
                ep,
                buf.as_mut_ptr(),
                buf.len() as i32,
                &mut transferred,
                IN_SLICE.as_millis() as u32,
            )
        };
        (rc, transferred.max(0) as usize)
    }

    fn finish_result(&self, cmd: &SubmitCmd, r: rusb::Result<usize>) {
        match r {
            Ok(n) => self.finish(cmd, 0, n as u32, &[]),
            Err(e) => self.finish(cmd, self.errno(e), 0, &[]),
        }
    }

    /// Send the reply for a completed URB: RET_SUBMIT normally, RET_UNLINK
    /// when a CMD_UNLINK cancelled it while it ran.
    fn finish(&self, cmd: &SubmitCmd, status: i32, actual: u32, data: &[u8]) {
        let unlink_seq = {
            let mut urbs = self.urbs.lock().unwrap();
            urbs.pending.remove(&cmd.seqnum);
            urbs.unlinked.remove(&cmd.seqnum)
        };
        let pdu = match unlink_seq {
            Some(seq) => proto::ret_unlink(seq, proto::ECONNRESET),
            None => proto::ret_submit(cmd, status, actual, data),
        };
        self.send_pdu(pdu);
    }

    fn send_pdu(&self, pdu: Vec<u8>) {
        self.sender.send(protocol::encode_usb_data(self.token, &pdu));
    }

    /// Map a libusb error onto the -errno the kernel expects; losing the
    /// device also tells the server to detach the port.
    fn errno(&self, e: rusb::Error) -> i32 {
        match e {
            rusb::Error::Pipe => proto::EPIPE,
            rusb::Error::Timeout => proto::ETIMEDOUT,
            rusb::Error::InvalidParam | rusb::Error::NotSupported => proto::EINVAL,
            rusb::Error::Io => proto::EPROTO,
            rusb::Error::NoDevice => {
                self.dead.store(true, Ordering::Relaxed);
                if !self.detach_sent.swap(true, Ordering::Relaxed) {
                    eprintln!("[usb] token {}: device lost ({e})", self.token);
                    self.sender.send(protocol::encode_usb_detach(self.token));
                }
                proto::ENODEV
            }
            _ => proto::EIO,
        }
    }
}

/// The rusb error for a raw libusb code, as far as errno() tells them apart.
fn libusb_error(rc: i32) -> rusb::Error {
    match rc {
        LIBUSB_ERROR_PIPE => rusb::Error::Pipe,
        LIBUSB_ERROR_IO => rusb::Error::Io,
        LIBUSB_ERROR_NO_DEVICE => rusb::Error::NoDevice,
        LIBUSB_ERROR_INVALID_PARAM => rusb::Error::InvalidParam,
        LIBUSB_ERROR_NOT_SUPPORTED => rusb::Error::NotSupported,
        _ => rusb::Error::Other,
    }
}

/// Claim every interface of the active configuration (best effort; on
/// composite devices some interfaces may be held by host drivers) and map
/// its endpoints.
fn claim_active_config(handle: &DeviceHandle<GlobalContext>) -> anyhow::Result<ActiveConfig> {
    let config = handle.device().active_config_descriptor()?;
    let mut claimed = Vec::new();
    let mut ep_types = HashMap::new();
    for iface in config.interfaces() {
        match handle.claim_interface(iface.number()) {
            Ok(()) => claimed.push(iface.number()),
            Err(e) => tracing::warn!("[usb] claim interface {} failed: {e}", iface.number()),
        }
        for desc in iface.descriptors() {
            for ep in desc.endpoint_descriptors() {
                ep_types.insert(ep.address(), ep.transfer_type());
            }
        }
    }
    anyhow::ensure!(!claimed.is_empty(), "no interface could be claimed");
    Ok(ActiveConfig { number: config.number(), claimed, ep_types })
}

/// rusb speed -> kernel usb_device_speed value (what vhci's attach expects).
fn kernel_speed(speed: rusb::Speed) -> u32 {
    match speed {
        rusb::Speed::Low => 1,
        rusb::Speed::Full => 2,
        rusb::Speed::High => 3,
        rusb::Speed::Super => 5,
        rusb::Speed::SuperPlus => 6,
        _ => 3,
    }
}
