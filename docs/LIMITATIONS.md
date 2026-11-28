# What works out of the box, and what does not

Read this before buying hardware or promising features. "Out of the box" means a
non-rooted Android phone and stock macOS / Windows without test-signing or reduced security.

## 1. Physical topology

| Fact | Consequence |
|---|---|
| Most phones have **one** USB-C port. When it hosts the hub, the phone is the USB *host*. | The same port cannot be a USB *device* to the PC at the same time. **ADB over a USB cable and a hub on the phone are mutually exclusive.** A hub cannot join two hosts. |
| "Wired" mode in practice | A **USB Ethernet adapter in the hub** (most Android kernels have CDC-ECM/NCM, ASIX, RTL8152 drivers): the phone gets a LAN link, the agent listens on it like on Wi-Fi. This is the recommended low-latency wired link. |
| `adb forward` | Useful for development: phone cabled to the PC (no hub), or ADB over Wi-Fi (wireless debugging, Android 11+), or phones with **two** USB-C ports (ASUS ROG Phone, some Lenovo Legion — one port to the PC, one to the hub; check the model). |
| USB host-to-host "bridge" cables | Need vendor drivers on both ends; Android has none. Not supported. |
| Power | The phone powers the hub and devices unless the hub has **USB-PD pass-through**. Phones limit OTG current (often 0.5–1.5 A). Hard drives, capture cards and many devices need a PD hub with a charger. Phone + PD hub = the phone must support charging while in host mode (most recent phones do; some do not). |
| USB speed | Many mid-range phones have a **USB 2.0** port (480 Mbit/s raw, ~35 MB/s real) even with a USB-C connector. A USB 3 hub then runs at USB 2. |

## 2. Android (non-rooted)

Works out of the box:

- Enumerate devices behind the hub, read IDs/class/interfaces/endpoints (`UsbManager`).
- With the user's permission: open the device, read raw descriptors and serial, claim
  interfaces, **control / bulk / interrupt** transfers.
- Through the usbfs file descriptor of an opened device (`getFileDescriptor()`), native code
  can do everything usbfs offers: async URBs, **isochronous** transfers, resets — without root.
  libusb and our Rust core use this.

Limits and gotchas:

- **A permission dialog per device.** It persists only if the app is opened from the
  `USB_DEVICE_ATTACHED` intent with "always" ticked (keyed by VID/PID); it resets after
  reinstall. A host can trigger the dialog (`hubbridge grant <id>`), but someone must tap Allow
  on the phone.
- **No isochronous transfers in the Java API.** Audio and most webcams/capture dongles need
  native usbfs I/O (step 2/3, in Rust).
- **Kernel drivers own some classes**: HID (input), USB audio, UVC (when the external camera
  HAL is on), mass storage (vold), CDC-ECM/NCM Ethernet. `claimInterface(intf, force = true)`
  detaches the kernel driver; the phone then loses that device itself (e.g. a keyboard stops
  typing on the phone). The hub itself is never exported.
- **Some OEMs switch OTG off** by default or after ~10 minutes idle (several ColorOS / MIUI
  builds): the user must enable it in settings.
- **Background:** the service runs as a `connectedDevice` foreground service with a
  notification (Android 14+ requires a type and a prerequisite permission; we hold
  `CHANGE_WIFI_STATE`). OEM battery managers (Xiaomi, Huawei, Samsung, Oppo) may still kill
  it: ask the user to exempt the app from battery optimization.
- **Wi-Fi power save:** `WIFI_MODE_FULL_LOW_LATENCY` helps only while the app is in the
  foreground with the screen on; with the screen off expect RTT spikes of 50–300 ms. For
  steady latency keep the screen on (or use Ethernet).
- **Local network access:** newer Android releases are adding a runtime permission for local
  network access tied to the target SDK. The app targets API 36 now; raising `targetSdk` will
  require requesting it.
- **No raw video input on the USB-C port.** DisplayPort Alt Mode on phones is **output
  only**. To get HDMI *into* the phone you need a **UVC capture dongle** (HDMI → USB, e.g.
  MS2109 class: 1080p30 MJPEG/YUY2 over USB 2; USB 3 cards need a USB 3 phone port).
- **UVC via Camera2** works only on Android 14+ devices where the OEM enabled external
  cameras; otherwise we decode UVC in user space on the usbfs fd.
- **Video out to the hub's HDMI** works only on phones that support DP Alt Mode (Samsung
  DeX models, recent Pixels, some Motorola/Xiaomi/ASUS) or with DisplayLink hubs (through
  DisplayLink's own app). Some phones only mirror, which leaves no secondary display for the
  `Presentation` API.
- **Measured on a Samsung SM-G970F (Android 12), 2026-11-28**, monitor on the USB-C port and through a
  charging USB-C hub, picture drawn by a `Presentation` from HubBridge's foreground service:
  - **Samsung DeX must be off.** With DeX on, the monitor keeps showing the DeX desktop although the app's
    window on the HDMI display is created and drawing; Android reports no error.
  - With DeX off the picture stays while HubBridge is in the background or another app is in front.
  - **The phone must be unlocked with its screen on.** The power button locks the phone and turns the monitor
    dark; after screen on and unlock the picture comes back by itself. A lock screen with the screen on, and
    the same with DeX on, were not tested.
  - Turning DeX off or replugging the monitor brings the display back under a **new display id** and cancels
    the app's window: the picture has to be started again.
  - The monitor reported 1280×720@60 while DeX was on and 2560×1600@60 without it (the same monitor, with and
    without the hub).
  - H.264 decoder: `OMX.Exynos.avc.dec`, which reports `FEATURE_LowLatency`.
- **iOS is out of scope**: no general USB host API (only MFi accessories / ExternalAccessory).

## 3. Network

| Link | Practical TCP throughput | RTT (median / spikes) |
|---|---|---|
| Wi-Fi 4, 2.4 GHz | 30–100 Mbit/s | 3–10 ms / 50–300 ms |
| Wi-Fi 5, 5 GHz | 200–600 Mbit/s | 2–5 ms / 20–200 ms |
| Wi-Fi 6/6E | 400–900 Mbit/s | 2–4 ms / 10–100 ms |
| USB Ethernet 100M (USB 2 adapter) | ~90 Mbit/s | < 1 ms |
| USB Ethernet 1G (USB 3 phone + adapter) | 300–900 Mbit/s | < 1 ms |
| `adb forward` over a USB 2 cable | 100–300 Mbit/s | 1–3 ms |

- If the PC is also on Wi-Fi, traffic crosses the air twice (phone → AP → PC): halve the
  numbers. The phone's own hotspot with the PC on it avoids the AP.
- **USB/IP over the network adds one RTT per synchronous transfer.** Fine: USB-serial, HID
  (effective polling 100–250 Hz instead of 1000 Hz), MCU programmers, most vendor-specific
  bulk devices. Slow: mass storage (works, well below USB speed). Poor: isochronous
  audio/video over USB/IP — use class-level streaming (encode on the phone) instead.
- **mDNS is blocked** on many guest, hotel and corporate networks; "client isolation" blocks
  all phone ↔ PC traffic. Fallbacks: type the IP; use the phone's hotspot; use Ethernet.
- Everything on the LAN is untrusted: pairing uses SPAKE2 and every session is encrypted and
  mutually authenticated (Noise). An attacker on the LAN gets one PIN guess per connection,
  three per PIN.

### Serial over Wi-Fi

`hubbridge serial` drives CDC-ACM, CP210x, FTDI (FT232BM and newer) and CH34x devices from the
desktop, over the generic USB transfers (the phone runs no chip code). What that costs:

- **Opening and changing the line** takes 3 to 7 control requests, one after the other: about
  3 to 7 round trips (10 to 60 ms on good Wi-Fi). Setting a baud rate is rare, so this does
  not matter; changing it right after writing waits for the written bytes to leave first.
- **Request/response latency** is about one round trip plus the device's own time. Writes are
  pipelined, received data costs one one-way trip because the next IN transfer is already
  queued.
- **Sustained receive on UART chips is the real limit.** CP210x and CH34x answer an IN token
  with whatever they have, so the queued IN transfers are used up within microseconds and
  the endpoint has nothing queued for about one round trip while the host's resubmissions
  travel. The chip buffers meanwhile (CP2102 576 bytes, FT232R 256 bytes): 576 bytes last
  about 50 ms at 115200 baud, 6 ms at 921600 and under 2 ms at 3 Mbaud. A Wi-Fi spike longer
  than that loses bytes unless RTS/CTS is wired (`--flow rtscts`); the counters show chip
  overruns where the chip reports them. FTDI holds a transfer until 62 bytes or the latency
  timer and so absorbs gaps better; native CDC-ACM devices (TinyUSB, ESP32-S3,
  RP2040 boards) hold data in firmware until the host asks, so they slow down instead of
  losing data.
- **FTDI idle traffic**: an idle FTDI chip answers an IN transfer with a 2-byte status packet
  every latency-timer period (16 ms by default, `--ftdi-latency-ms`): about 60 small
  messages per second each way.
- **CP210x modem status** is polled (200 ms) only while an RFC 2217 client asks for it.
- **60 s IN timeout**: the agent caps transfer timeouts at 60 s, so an idle line gets all its
  IN transfers cancelled and resubmitted every minute (data is never lost to that; the chip
  buffers for one round trip).
- A bridge ends when the device is unplugged or the session drops; reconnecting after a
  dropped session is step 4.
- Not supported: XON/XOFF and DTR/DSR flow control, 9-bit data, FTDI bit-bang/MPSSE, FT8U232AM,
  Prolific PL2303, CH343/CH9102 vendor mode (they enumerate as CDC-ACM and use that driver),
  two ports of one multi-port FTDI at the same time (`--port N` picks one).
- The remedy for high-baud loss, if measurements on real adapters need it, is a generic IN
  stream in the agent (chip-agnostic, queued on the phone); it is not built. Measure first:
  `hubbridge serial <id> --bench-echo` on an adapter with TX wired to RX.

## 4. Desktop: making devices appear to the OS

| Goal | Windows | macOS |
|---|---|---|
| Any USB device (USB/IP) | **usbip-win2**: recent releases have attestation-signed drivers, no test mode. Our own kernel driver would need an EV certificate + Microsoft attestation signing. | **No client.** A user-space virtual host controller (`IOUSBHostControllerInterface`) needs the restricted entitlement `com.apple.developer.usb.host-controller-interface`, granted by Apple on request only. Kexts are deprecated and need Reduced Security on Apple Silicon. |
| Serial (CDC/FTDI/CP210x/CH34x) | **No virtual COM port** (needs a signed driver such as com0com, not shipped): TCP / RFC 2217 (pyserial `rfc2217://`, esptool) works; for a COM port in other software use com0com + hub4com yourself | PTY (`/dev/ttysNNN`), no driver; also TCP / RFC 2217 |
| Keyboard / mouse | `SendInput`, no driver | `CGEvent`, Accessibility permission |
| Generic HID / gamepad | VHF (KMDF driver, signing) / ViGEm-style bus driver | HIDDriverKit dext: DriverKit entitlements from Apple + user approval in System Settings |
| Virtual monitor | **IddCx** UMDF driver: test-signing for development, attestation signing for distribution; signed open-source options exist (Virtual Display Driver, Parsec VDD) | `CGVirtualDisplay` — **private API** (no App Store, may break with OS updates); no public alternative |
| Screen capture | Windows.Graphics.Capture / Desktop Duplication, no driver | ScreenCaptureKit, Screen Recording permission |
| Webcam from the phone | **Windows 11 `MFCreateVirtualCamera`**, no driver (Windows 10: DirectShow filter only) | CoreMediaIO **Camera Extension**: system extension, Developer ID + notarization, user approval |

### Serial front ends: what a PTY can and cannot do

- **PTY (macOS only)**: `hubbridge serial <id>` prints the `/dev/...` path; any program that
  opens it sees a raw terminal. Baud rate, data bits, parity, stop bits and RTS/CTS that the
  program sets with `tcsetattr` are applied to the chip before the data it writes after the
  change (the termios of the PTY is looked at every 100 ms and before every chunk it
  writes). On macOS arbitrary rates (250000, 921600, 3000000) work through `tcsetattr`.
  `tcflush` on the PTY purges the matching direction.
- **What a PTY cannot do**: modem lines are not visible (`TIOCMGET` and `TIOCMSET` fail on a
  PTY), so DTR/RTS toggling such as esptool's auto-reset does not work; and programs that set
  rates above 230400 on macOS with `IOSSIOSPEED` (pyserial does) get `ENOTTY` on a PTY. Use
  `--baud N --lock-line` (the bridge sets the rate, the program's settings are ignored), or
  RFC 2217.
- **RFC 2217 (`--tcp PORT --rfc2217`, macOS and Windows)**: carries baud rate, line format,
  RTS/CTS, DTR/RTS, break, purge and modem/line state notifications. Clients:
  `pyserial` (`serial.serial_for_url("rfc2217://127.0.0.1:PORT")`), `esptool --port
  rfc2217://127.0.0.1:PORT`. XON/XOFF is not supported (the flow in effect is reported).
- **Raw TCP (`--tcp PORT`)**: bytes as they are, no line control (use `--baud`).
- **Windows**: no PTY. RFC 2217 clients (pyserial, esptool) work directly; a COM port for
  programs that need one takes com0com + hub4com (or similar), not shipped.
- The TCP port listens on 127.0.0.1 only unless you give an address, and has no
  authentication; one client at a time.
- Bytes the device sends while no program has the PTY open stay in the bridge's receive
  buffer (up to 1 MiB, then they are dropped and counted as "dropped") and are read by the
  next program that opens the PTY.
- **Timing of a speed change**: the PTY's termios is polled (every 100 ms and before each
  chunk the program writes), so a program that writes data and then changes the rate
  with `tcsetattr(TCSADRAIN)` can see the new rate applied to bytes it wrote before the change
  (the bridge cannot tell when the program's output queue drained). Wait for a reply before
  switching the rate, or use RFC 2217, where the order of commands and data is exact.
- A failed or cancelled OUT transfer is counted ("failed to send") and reported; three in a
  row end the bridge.

## 5. Released builds

- **The macOS app and CLI need macOS 13 or later** (Apple Silicon). v0.1.0 was built for
  macOS 11 or later.
- **The macOS app is not notarized.** It is signed ad hoc (no Apple Developer ID), so a
  downloaded copy is stopped on its first start ("Not Opened", "Apple could not verify…") until
  you press **Open Anyway** in System Settings → Privacy & Security, or remove the quarantine
  flag with `xattr -dr com.apple.quarantine /Applications/HubBridge.app`. The CLI is signed the
  same way.
- **macOS may ask for Local Network access again after an update**: an ad-hoc signed app has no
  stable signing identity. Not yet checked with a real update.
- **adb after quitting the app:** an adb server started by the packaged app may lose local
  network access once the app quits (ADB over Wi-Fi stops working until the adb server is
  restarted, for example with `adb kill-server`). Spawning it "disclaimed" is on the roadmap.
- **The released phone app is signed with the project's release key.** A development build
  (debug key) cannot be updated to it in place: uninstall it once, which forgets the paired
  computers. Later releases update in place.
- There are no Windows downloads: Windows runs from source.

## 6. Video latency budget (estimate, 1080p60, good 5 GHz Wi-Fi)

capture 5–16 ms + encode 3–10 ms + network 2–10 ms (spikes more) + decode 5–15 ms +
display 8–16 ms ≈ **30–70 ms glass-to-glass**. 1080p60 H.264 at 10–20 Mbit/s fits any
5 GHz link; 4K60 HEVC at 30–50 Mbit/s is possible on Wi-Fi 6 but jitter shows. Over
Ethernet the network part drops below 1 ms.

Sources for the signing/entitlement facts: usbip-win2
[release v0.9.7.2](https://newreleases.io/project/github/vadimgrn/usbip-win2/release/v.0.9.7.2)
(attestation-signed drivers), Apple developer forum on
[`com.apple.developer.usb.host-controller-interface`](https://developer.apple.com/forums/thread/802495),
Microsoft docs on [test signing](https://learn.microsoft.com/windows-hardware/drivers/install/test-signing).
