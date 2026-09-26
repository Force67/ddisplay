/// USB forwarding, KVM-style: devices given with --share-usb are offered to
/// the server on every (re)connect and show up there as local USB hardware.
/// The server tunnels the kernel's usbip URB stream to us; `device` answers
/// it with libusb, `proto` speaks the PDU format.

mod device;
mod proto;

use std::collections::HashMap;

use crate::protocol;
use crate::transport::TransportSender;
use device::SharedDevice;

pub struct UsbManager {
    sender: TransportSender,
    shares: Vec<(u16, u16)>,
    devices: HashMap<u32, SharedDevice>,
    next_token: u32,
}

impl UsbManager {
    pub fn new(sender: TransportSender, shares: Vec<(u16, u16)>) -> Self {
        Self { sender, shares, devices: HashMap::new(), next_token: 1 }
    }

    /// (Re)open every shared device and offer it to the server. Runs per
    /// connect: a reconnect voids all server-side state, so old tokens die
    /// here and fresh ones are announced.
    pub fn on_connected(&mut self) {
        self.devices.clear();
        for (vid, pid) in self.shares.clone() {
            let token = self.next_token;
            self.next_token += 1;
            match SharedDevice::open(vid, pid, token, self.sender.clone()) {
                Ok(dev) => {
                    self.sender.send(protocol::encode_usb_attach(dev.attach_json()));
                    eprintln!("[usb] offering {vid:04x}:{pid:04x} (token {token})");
                    self.devices.insert(token, dev);
                }
                Err(e) => {
                    eprintln!("[usb] cannot share {vid:04x}:{pid:04x}: {e:#}");
                }
            }
        }
    }

    pub fn on_disconnected(&mut self) {
        self.devices.clear();
    }

    pub fn on_data(&mut self, token: u32, chunk: &[u8]) {
        let Some(dev) = self.devices.get_mut(&token) else {
            return;
        };
        if let Err(e) = dev.feed(chunk) {
            eprintln!("[usb] token {token}: corrupt stream ({e}); detaching");
            self.sender.send(protocol::encode_usb_detach(token));
            self.devices.remove(&token);
        } else if dev.is_dead() {
            // The worker already told the server; just drop our side.
            self.devices.remove(&token);
        }
    }

    pub fn on_detach(&mut self, token: u32) {
        if self.devices.remove(&token).is_some() {
            eprintln!("[usb] token {token}: detached by server");
        }
    }

    pub fn on_attached(&self, json: &[u8]) {
        #[derive(serde::Deserialize)]
        struct Attached {
            token: u32,
            port: u32,
        }
        if let Ok(a) = serde_json::from_slice::<Attached>(json) {
            eprintln!("[usb] token {}: attached on server vhci port {}", a.token, a.port);
        }
    }

    pub fn on_error(&mut self, json: &[u8]) {
        #[derive(serde::Deserialize)]
        struct UsbError {
            token: u32,
            error: String,
        }
        if let Ok(e) = serde_json::from_slice::<UsbError>(json) {
            eprintln!("[usb] token {}: server attach failed: {}", e.token, e.error);
            self.devices.remove(&e.token);
        }
    }
}

/// Parse a --share-usb spec like "046d:c52b".
pub fn parse_share_spec(spec: &str) -> anyhow::Result<(u16, u16)> {
    let (vid, pid) = spec
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected VID:PID, got \"{spec}\""))?;
    Ok((
        u16::from_str_radix(vid, 16).map_err(|_| anyhow::anyhow!("bad vendor id \"{vid}\""))?,
        u16::from_str_radix(pid, 16).map_err(|_| anyhow::anyhow!("bad product id \"{pid}\""))?,
    ))
}

/// Print the connected USB devices (--list-usb).
pub fn list_devices() {
    let devices = match rusb::devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot enumerate USB devices: {e}");
            return;
        }
    };
    for device in devices.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        let product = device
            .open()
            .ok()
            .and_then(|h| h.read_product_string_ascii(&desc).ok())
            .unwrap_or_default();
        println!(
            "bus {:03} dev {:03}  {:04x}:{:04x}  {:?}  {}",
            device.bus_number(),
            device.address(),
            desc.vendor_id(),
            desc.product_id(),
            device.speed(),
            product,
        );
    }
}
