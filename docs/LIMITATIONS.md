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

## 4. Desktop: making devices appear to the OS

| Goal | Linux | Windows | macOS |
|---|---|---|---|
| Any USB device (USB/IP) | **In-box** `vhci-hcd` + `usbip` | **usbip-win2**: recent releases have attestation-signed drivers, no test mode. Our own kernel driver would need an EV certificate + Microsoft attestation signing. | **No client.** A user-space virtual host controller (`IOUSBHostControllerInterface`) needs the restricted entitlement `com.apple.developer.usb.host-controller-interface`, granted by Apple on request only. Kexts are deprecated and need Reduced Security on Apple Silicon. |
| Serial (CDC/FTDI/CP210x/CH34x) | PTY, no driver | Virtual COM needs a signed driver (e.g. com0com) — or a TCP/RFC 2217 port without one | PTY (`/dev/ttysNNN`), no driver |
| Keyboard / mouse | uinput | `SendInput`, no driver | `CGEvent`, Accessibility permission |
| Generic HID / gamepad | uhid | VHF (KMDF driver, signing) / ViGEm-style bus driver | HIDDriverKit dext: DriverKit entitlements from Apple + user approval in System Settings |
| Virtual monitor | evdi / dummy plug | **IddCx** UMDF driver: test-signing for development, attestation signing for distribution; signed open-source options exist (Virtual Display Driver, Parsec VDD) | `CGVirtualDisplay` — **private API** (no App Store, may break with OS updates); no public alternative |
| Screen capture | PipeWire | Windows.Graphics.Capture / Desktop Duplication, no driver | ScreenCaptureKit, Screen Recording permission |
| Webcam from the phone | v4l2loopback | **Windows 11 `MFCreateVirtualCamera`**, no driver (Windows 10: DirectShow filter only) | CoreMediaIO **Camera Extension**: system extension, Developer ID + notarization, user approval |

## 5. Video latency budget (estimate, 1080p60, good 5 GHz Wi-Fi)

capture 5–16 ms + encode 3–10 ms + network 2–10 ms (spikes more) + decode 5–15 ms +
display 8–16 ms ≈ **30–70 ms glass-to-glass**. 1080p60 H.264 at 10–20 Mbit/s fits any
5 GHz link; 4K60 HEVC at 30–50 Mbit/s is possible on Wi-Fi 6 but jitter shows. Over
Ethernet the network part drops below 1 ms.

Sources for the signing/entitlement facts: usbip-win2
[release v0.9.7.2](https://newreleases.io/project/github/vadimgrn/usbip-win2/release/v.0.9.7.2)
(attestation-signed drivers), Apple developer forum on
[`com.apple.developer.usb.host-controller-interface`](https://developer.apple.com/forums/thread/802495),
Microsoft docs on [test signing](https://learn.microsoft.com/windows-hardware/drivers/install/test-signing).
