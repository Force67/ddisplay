/// Forwarded USB devices, KVM-style: a device plugged into the client shows
/// up as a local USB device in the session.
///
/// The kernel side is usbip's virtual host controller (vhci-hcd). Attaching
/// hands the kernel one end of a loopback TCP pair via sysfs; the kernel then
/// speaks the usbip URB protocol over it. This module never parses that
/// protocol — it pumps the byte stream between the kernel socket and
/// MSG_USB_DATA frames. The client implements the device side (libusb).

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::protocol::{self, UsbAttachRequest};

const VHCI_DIR: &str = "/sys/devices/platform/vhci_hcd.0";

/// Kernel usb_device_speed values from USB_SPEED_SUPER on need an "ss" hub port.
const SPEED_SUPER: u32 = 5;

/// vhci status value for a free port (VDEV_ST_NULL).
const PORT_FREE: u32 = 4;

/// Serializes free-port lookup + attach so concurrent attaches can't pick
/// the same port.
static ATTACH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The USB devices one client connection has attached, keyed by the client's
/// token. Dropping this (connection teardown) detaches every port.
pub struct ConnectionUsb {
    /// Server -> client message channel (bounded; backpressures the kernel reads).
    usb_tx: mpsc::Sender<Vec<u8>>,
    ports: HashMap<u32, Port>,
}

struct Port {
    hub_port: u32,
    writer: mpsc::Sender<Vec<u8>>,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
    /// Set once the kernel closed its socket: the port is free again and may
    /// already belong to another connection, so it must not be detached.
    released: Arc<AtomicBool>,
}

impl Drop for Port {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.writer_task.abort();
        if !self.released.load(Ordering::Acquire) {
            detach_port(self.hub_port);
        }
    }
}

impl ConnectionUsb {
    pub fn new(usb_tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self { usb_tx, ports: HashMap::new() }
    }

    /// Attach an offered device to a free vhci port and start pumping its
    /// stream. Replies UsbAttached or UsbError to the client either way.
    pub async fn attach(&mut self, req: UsbAttachRequest) {
        let token = req.token;
        self.ports.remove(&token);
        self.ports.retain(|_, p| !p.released.load(Ordering::Acquire));

        let (hub_port, sock) = match vhci_attach(&req).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "[usb] attach {:04x}:{:04x} failed: {e:#}",
                    req.vendor_id, req.product_id,
                );
                self.reject(token, &format!("{e:#}")).await;
                return;
            }
        };
        tracing::info!(
            "[usb] {:04x}:{:04x} \"{}\" attached on vhci port {}",
            req.vendor_id, req.product_id, req.product, hub_port,
        );

        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(64);
        let (mut rd, mut wr) = sock.into_split();

        let usb_tx = self.usb_tx.clone();
        let released = Arc::new(AtomicBool::new(false));
        let reader_released = released.clone();
        let reader_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if usb_tx.send(protocol::encode_usb_data(token, &buf[..n])).await.is_err() {
                            return;
                        }
                    }
                }
            }
            // The kernel closed its end (port error or detach): the port is
            // already gone, so just tell the client to stop serving URBs.
            reader_released.store(true, Ordering::Release);
            let _ = usb_tx.send(protocol::encode_usb_detach(token)).await;
        });

        let writer_task = tokio::spawn(async move {
            while let Some(data) = write_rx.recv().await {
                if wr.write_all(&data).await.is_err() {
                    break;
                }
            }
        });

        let _ = self.usb_tx.send(protocol::encode_usb_attached(token, hub_port)).await;
        self.ports.insert(token, Port { hub_port, writer: write_tx, reader_task, writer_task, released });
    }

    pub async fn reject(&self, token: u32, reason: &str) {
        let _ = self.usb_tx.send(protocol::encode_usb_error(token, reason)).await;
    }

    /// Forward a chunk of the client's usbip stream to the kernel socket.
    pub async fn data(&mut self, token: u32, data: Vec<u8>) {
        if let Some(port) = self.ports.get(&token) {
            let _ = port.writer.send(data).await;
        }
    }

    pub fn detach(&mut self, token: u32) {
        if let Some(port) = self.ports.remove(&token) {
            tracing::info!("[usb] detached vhci port {} (token {})", port.hub_port, token);
        }
    }
}

async fn vhci_attach(req: &UsbAttachRequest) -> anyhow::Result<(u32, TcpStream)> {
    let _guard = ATTACH_LOCK.lock().await;
    ensure_vhci()?;
    let hub_port = free_port(req.speed)?;
    let (local, kernel) = loopback_pair().await?;

    let kernel = kernel.into_std()?;
    kernel.set_nonblocking(false)?;
    let devid = (req.busnum << 16) | (req.devnum & 0xffff);
    let line = format!("{} {} {} {}", hub_port, kernel.as_raw_fd(), devid, req.speed);
    std::fs::write(format!("{VHCI_DIR}/attach"), &line).map_err(|e| {
        anyhow::anyhow!("writing {VHCI_DIR}/attach failed: {e} (needs root or usbip sysfs access)")
    })?;
    // The kernel took its own reference on the socket; our fd can go.
    drop(kernel);
    Ok((hub_port, local))
}

fn ensure_vhci() -> anyhow::Result<()> {
    if Path::new(VHCI_DIR).exists() {
        return Ok(());
    }
    let _ = std::process::Command::new("modprobe").arg("vhci-hcd").status();
    if Path::new(VHCI_DIR).exists() {
        return Ok(());
    }
    anyhow::bail!("vhci_hcd is not available (modprobe vhci-hcd failed; install the usbip kernel modules)")
}

fn free_port(speed: u32) -> anyhow::Result<u32> {
    let status = std::fs::read_to_string(format!("{VHCI_DIR}/status"))?;
    parse_free_port(&status, speed)
}

/// Pick a free vhci port whose hub matches the device speed.
///
/// Status lines are "hub port sta spd dev sockfd local_busid" after a header
/// row; a port is free while its status is VDEV_ST_NULL.
fn parse_free_port(status: &str, speed: u32) -> anyhow::Result<u32> {
    let want_hub = if speed >= SPEED_SUPER { "ss" } else { "hs" };
    for line in status.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (Some(hub), Some(port), Some(sta)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if hub != want_hub {
            continue;
        }
        if sta.parse::<u32>() == Ok(PORT_FREE) {
            if let Ok(port) = port.parse::<u32>() {
                return Ok(port);
            }
        }
    }
    anyhow::bail!("no free {want_hub} port on vhci_hcd.0")
}

/// A connected loopback TCP pair. The kernel needs a real TCP socket, and its
/// end must stay ESTABLISHED, so this is the usual vhci tunneling trick.
async fn loopback_pair() -> anyhow::Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let local = TcpStream::connect(addr).await?;
    let local_addr = local.local_addr()?;
    let (kernel, peer) = listener.accept().await?;
    anyhow::ensure!(peer == local_addr, "unexpected peer on vhci loopback socket");
    local.set_nodelay(true)?;
    kernel.set_nodelay(true)?;
    Ok((local, kernel))
}

fn detach_port(hub_port: u32) {
    // Fails harmlessly when the kernel already released the port.
    let _ = std::fs::write(format!("{VHCI_DIR}/detach"), hub_port.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "\
hub port sta spd dev      sockfd local_busid
hs  0000 006 003 00010002 000003 1-1
hs  0001 004 000 00000000 000000 0-0
hs  0002 004 000 00000000 000000 0-0
ss  0003 004 000 00000000 000000 0-0
ss  0004 006 005 00020003 000004 2-2
";

    #[test]
    fn picks_first_free_port_of_matching_hub() {
        assert_eq!(parse_free_port(STATUS, 3).unwrap(), 1);
        assert_eq!(parse_free_port(STATUS, 5).unwrap(), 3);
    }

    #[test]
    fn errors_when_hub_is_full() {
        let all_used = "\
hub port sta spd dev      sockfd local_busid
hs  0000 006 003 00010002 000003 1-1
";
        assert!(parse_free_port(all_used, 3).is_err());
        assert!(parse_free_port("", 3).is_err());
    }
}
