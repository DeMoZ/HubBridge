# Proposal

## Why

Step 1 lets the desktop host see the USB devices on the phone's hub, but nothing can talk to
them yet. Step 2a adds the generic data path — open a device, claim interfaces, control and
bulk/interrupt transfers — so later consumers (serial bridge, USB/IP, HID) can be built on
one tested transport. Most of it is already on branch `feat/step2-usb-transfers` (commits
164b507, 6ffd18d) and was tested on a Galaxy S10e; this change records that work in
OpenSpec (adopted in this repo on 2026-11-27, after the code was written) and covers the
fix found on the real phone, which is not committed yet.

## What Changes

- Protocol v1 gains appended USB messages: `OpenDevice`, `CloseDevice`, `ClaimInterface`
  (optional kernel-driver detach), `ReleaseInterface`, `SetAlternate`, `Control`,
  `Transfer`, `ClearHalt`, `ResetDevice`; replies `Done`, `ControlDone`, `TransferDone`;
  event `DeviceClosed`; error codes `NoPermission`, `Busy`, `NotOpen`, `UsbFailed`.
  Additive, so v1 peers stay compatible.
- Agent core: one session owns a device at a time; requests are queued in arrival order and
  answered when they complete (up to 256 in flight per session); devices close when the
  session ends or the device is unplugged.
- Android: the app opens the `UsbDeviceConnection` and hands its usbfs fd to the Rust core,
  which drives it with `nusb`.
- Simulator and tests: a fake USB backend (bulk loopback device, Bulk-Only flash drive).
- Host: client API for the USB messages with several requests in flight; a read-only USB
  mass-storage client (Bulk-Only Transport + SCSI); CLI `hubbridge usb descriptors <id>`
  and `hubbridge usb storage <id> [--bench MiB] [--chunk-kib N] [--command-kib N]`.
- Pending fix from the phone test: enable nusb's `tokio` feature (opening a device
  panicked without it), answer the host with an error when USB handling panics, log Rust
  panics to logcat, and the bench size options.

## Capabilities

### New Capabilities
- `usb-device-access`: a paired host opening a USB device on the phone and exchanging
  control, bulk and interrupt transfers with it through the agent — ownership, ordering,
  errors, and closing.
- `usb-mass-storage`: the host reading a USB flash drive on the phone (identify, capacity,
  sequential read and speed measurement) over `usb-device-access`.

### Modified Capabilities
<!-- none: no specs existed before this change -->

## Impact

- Code: `crates/proto` (messages), `crates/agent` (`usb`, `nusb_backend`, `fake`, session
  loop), `crates/android` (JNI opener/closer, panic logging), Kotlin `BridgeService` /
  `NativeAgent` (open/close callbacks), `crates/host` (client USB API, `msc`, CLI),
  `crates/agent-sim`, tests `crates/host/tests/usb_e2e.rs`.
- Dependencies: `nusb` 0.2 (Android/Linux only) with the `tokio` feature.
- Behaviour on the phone: claiming with kernel-driver detach takes the interface from
  Android (a flash drive is unmounted while the host uses it).
- Docs: `docs/PROTOCOL.md` (USB data section), `docs/ROADMAP.md`.
