# Design

## Context

- Step 1 (main): an encrypted session per host (`crates/link`), an agent core in Rust that
  runs inside the Android app through JNI (`crates/agent`, `crates/android`), and a device
  list fed by Kotlin's `UsbManager`. Request/response frames carry a `req` id.
- Android gives an app access to a USB device only through `UsbManager.openDevice()` after
  a permission dialog; the resulting `UsbDeviceConnection` exposes the usbfs file
  descriptor. The Java API has no isochronous transfers and costs a JNI hop per call.
- Most of the code exists on `feat/step2-usb-transfers` and passed a real-phone test after
  one fix (see proposal). This design documents the decisions already taken, so the tasks
  can be checked against them.

## Goals / Non-Goals

**Goals:**
- One generic, ordered, pipelined transfer path that later consumers reuse.
- Testable without hardware (fakes in CI), working on a real phone over Wi-Fi.

**Non-Goals:**
- Isochronous transfers (audio, most webcams) — later, same backend.
- OS integration on the desktop (USB/IP, virtual serial, HID) — steps 2b/2c.
- Writing to mass storage.

## Decisions

1. **Rust drives usbfs through `nusb`, not Java `UsbDeviceConnection`.** Kotlin opens the
   connection and returns `getFileDescriptor()`; Rust duplicates the fd and wraps it with
   `nusb::Device::from_fd`. Async URBs, many in flight, no JNI per packet, isochronous
   possible later. *Alternatives:* Java API (no iso, JNI per call); raw usbfs ioctls by
   hand (more code, nusb already does it).
2. **nusb needs a runtime feature.** Its open/claim calls are blocking syscalls; awaiting
   them requires the `smol` or `tokio` feature, otherwise they panic. We use `tokio`
   (already our runtime). Found only on the phone: the fake backend never touched nusb.
3. **Backend interface `UsbBackend` / `UsbDevice`** with boxed futures, implemented by the
   nusb backend (Android/Linux) and a fake backend. *Alternative:* `async_trait` — an extra
   dependency for the same thing.
4. **Ordering: queue synchronously, wait asynchronously.** The session loop calls
   `transfer()`/`control()` in arrival order; the call queues the request on the endpoint
   right away and returns a future for the result, which runs in its own task. Spawning
   first and queueing inside the task would let tasks reorder transfers on one endpoint.
5. **One worker task per endpoint** owns the nusb `Endpoint`: submits as requests arrive,
   matches completions FIFO, enforces timeouts by `cancel_all` (usbfs has no per-URB
   timeout), runs `clear_halt` between transfers. IN lengths are rounded up to whole
   packets (usbfs requirement) and the data is truncated back.
6. **Ownership per session** in a map device id → session; closed on `CloseDevice`, on
   session end, on unplug (`DeviceClosed` event). Only one host can drive a device.
7. **Back-pressure** with a 256-permit semaphore per session; answers go through an
   unbounded channel so finished tasks can always release permits (no deadlock while the
   loop waits for a permit).
8. **Panics answer the host.** Each request future runs in its own task; a join error
   becomes `Error Internal`. On Android a panic hook also logs to logcat (stderr is lost).
9. **Mass-storage client on the host**, not on the phone: Bulk-Only + SCSI over the generic
   path, so it exercises exactly what other consumers will use. The data phase of a
   READ(10) is submitted as several 64 KiB transfers at once (one round trip per command,
   not per packet); short data → stall handling and early-CSW detection per BOT.
10. **Protocol stays v1**: new messages and error codes are appended (postcard variant
    index), old peers fail cleanly with `Unsupported`.

## Risks / Trade-offs

- [Detaching the kernel driver unmounts a drive / steals a keyboard from the phone] → only
  when the host asks (`detach_kernel_driver`), released on close; documented in CLI help.
- [Timeout cancels every pending transfer on that endpoint, not just the late one] →
  acceptable for now; the host sees `Cancelled` and retries; per-URB discard later if needed.
- [Real-phone throughput 2.3–2.6 MB/s from a flash drive while Wi-Fi does 21.7 MB/s] →
  measurements show ~70 ms per SCSI command and little effect of transfer size, pointing at
  the drive/hub; to confirm by reading the same drive on the Mac. Command pipelining
  (CBW + data + CSW in flight) is a later optimisation.
- [A CSW that arrives inside a queued data chunk] → detected by signature and tag.
- [The fake backend cannot catch nusb/Android-specific bugs] → keep a real-phone check in
  the tasks for every backend change.

## Migration Plan

Additive protocol; agent and host update independently. Install the new APK on the phone
(`hubbridge install-apk`, also over Wi-Fi ADB); pairing is kept. Rollback: older APK/host —
USB requests then get `Unsupported`.
