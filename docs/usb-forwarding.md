# USB forwarding

Status: **implemented** on `feature/usb-streaming`. Needs validation with real
hardware end to end (a flash drive and a HID device are the reference cases).

Goal: KVM-style USB redirection. A device plugged into the client machine
shows up as a local USB device in the remote session, so security keys,
flash drives or serial adapters work as if they were plugged into the server.

## How it works

The transport reuses what the Linux kernel already ships for USB over
network: usbip. The server never interprets USB traffic, it only moves bytes.

- **Server.** On `UsbAttach` the server picks a free port on the usbip
  virtual host controller (`vhci-hcd`), creates a loopback TCP pair, and
  hands one end to the kernel through
  `/sys/devices/platform/vhci_hcd.0/attach`. From then on the kernel speaks
  the usbip URB protocol over that socket and enumerates the device like any
  hotplug. `server/src/usb.rs` pumps the byte stream between the socket and
  the WebSocket connection, framed as `MSG_USB_DATA` chunks. Ports live and
  die with their connection: a disconnect detaches every port the client had.
- **Wire.** `MSG_USB_ATTACH` (0x30) offers a device (JSON with vid/pid,
  bus/device number, speed). The server answers `MSG_USB_ATTACHED` (0x31) or
  `MSG_USB_ERROR` (0x34). `MSG_USB_DATA` (0x33) carries usbip stream chunks
  both ways, tagged with the client-chosen token. `MSG_USB_DETACH` (0x32)
  tears down from either side. Chunk boundaries carry no meaning; each side
  reassembles PDUs from the byte stream, so TCP-style framing survives the
  WebSocket transport.
- **Client.** `client-native/src/usb/` implements the usbip device side on
  libusb (rusb, vendored). `proto.rs` parses `CMD_SUBMIT`/`CMD_UNLINK` and
  encodes the `RET_` replies (big-endian, 48-byte headers). `device.rs`
  claims the device and runs one FIFO worker thread per endpoint, so per
  endpoint ordering holds while endpoints proceed independently. Control
  requests that change libusb-visible state (`SET_CONFIGURATION`,
  `SET_INTERFACE`, `CLEAR_FEATURE(ENDPOINT_HALT)`) go through the libusb API
  instead of the raw pipe, mirroring the kernel stub driver. IN transfers
  wait in 500ms slices so a pending URB notices its unlink promptly.

## Usage

```sh
# see what is connected
ddisplay-client --list-usb

# forward a device (repeatable)
ddisplay-client --server <host>:9550 --share-usb 046d:c52b
```

Devices are offered on every (re)connect and released on disconnect, which
also returns them to their local drivers.

## Requirements

- Server: the `vhci-hcd` kernel module (part of the usbip modules shipped
  with the kernel) and write access to its sysfs attach file, which in
  practice means running the server as root. The server modprobes the module
  on demand and reports a clear error to the client otherwise.
- Windows client: the device must be bound to a WinUSB-compatible driver
  (Zadig does this per device). Devices held by a functional Windows driver,
  e.g. an active keyboard, cannot be claimed.
- Linux client: permission to open the device node (a udev rule granting the
  user access to the vid:pid), plus kernel driver auto-detach, which the
  client requests by itself.

## Limitations

- No isochronous endpoints, so webcams and audio devices are rejected with
  an error status per URB. Bulk, interrupt and control transfers work.
- A forwarded device is claimed exclusively on the client while shared.
- Readonly viewers cannot attach devices (same gate as input injection).
- An unlink for an URB queued behind a blocked transfer on the same endpoint
  is answered only once that transfer finishes its timeout slice.

## To validate

1. Flash drive: `--share-usb`, confirm it enumerates in the session
   (`lsusb`, mount), copy files both ways, detach and confirm the client OS
   gets it back.
2. HID (a spare mouse via Zadig on Windows): confirm input arrives in the
   session and that unplugging mid-session detaches cleanly server-side.
3. Reconnect: kill the client during a transfer, confirm the vhci port
   detaches (device disappears in the session) and re-offering on reconnect
   works.
4. Permission failure: run the server without root and confirm the client
   logs the attach error instead of hanging.
