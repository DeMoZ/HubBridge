//! JNI bindings: run the HubBridge agent core inside the Android app.
//!
//! Kotlin side: `com.demoz.hubbridge.NativeAgent` (an `object`, so the functions below get
//! the singleton as `this`). Data crosses the boundary as JSON strings: simple, and the
//! volume (device lists, status) is tiny. USB data in step 2 will use the usbfs file
//! descriptor from `UsbDeviceConnection.getFileDescriptor()` directly in Rust instead of
//! crossing JNI per packet.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use hubbridge_agent::video::{FrameResult, SinkStats, VideoSink};
use hubbridge_agent::{Agent, AgentCallbacks, AgentConfig};
use hubbridge_proto::{
    ConfigInfo, DisplayInfo, DisplayList, DisplayMode, MirrorEnd, UsbDeviceInfo, VideoConfig, VideoFrame,
};
use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jint, jlong, jstring};
use jni::{JNIEnv, JavaVM};
use serde::Deserialize;

struct Running {
    rt: tokio::runtime::Runtime,
    agent: Agent,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// One device as `UsbScanner.kt` describes it.
#[derive(Deserialize)]
struct AndroidDevice {
    id: u32,
    path: String,
    vendor_id: u16,
    product_id: u16,
    class: u8,
    subclass: u8,
    protocol: u8,
    manufacturer: Option<String>,
    product: Option<String>,
    serial: Option<String>,
    has_permission: bool,
    #[serde(default)]
    configurations: Vec<ConfigInfo>,
    raw_descriptors_hex: Option<String>,
}

/// USB string descriptors often carry padding spaces; empty means absent.
fn clean(s: Option<String>) -> Option<String> {
    s.map(|v| hubbridge_proto::usb::clean_str(&v).to_string())
        .filter(|v| !v.is_empty())
}

impl From<AndroidDevice> for UsbDeviceInfo {
    fn from(mut a: AndroidDevice) -> Self {
        for c in &mut a.configurations {
            c.name = clean(c.name.take());
            for i in &mut c.interfaces {
                i.name = clean(i.name.take());
            }
        }
        let mut d = UsbDeviceInfo {
            id: a.id,
            path: a.path,
            vendor_id: a.vendor_id,
            product_id: a.product_id,
            class: a.class,
            subclass: a.subclass,
            protocol: a.protocol,
            bcd_usb: None,
            bcd_device: None,
            manufacturer: clean(a.manufacturer),
            product: clean(a.product),
            serial: clean(a.serial),
            configurations: a.configurations,
            has_permission: a.has_permission,
            raw_descriptors: a.raw_descriptors_hex.and_then(|h| hex::decode(h).ok()),
        };
        if let Err(e) = d.apply_raw_descriptors() {
            log::warn!("device {}: bad raw descriptors: {e}", d.id);
        }
        d
    }
}

/// Parse the JSON array `UsbScanner.kt` produces.
pub fn parse_devices(json: &str) -> Result<Vec<UsbDeviceInfo>> {
    let list: Vec<AndroidDevice> = serde_json::from_str(json).context("device list JSON")?;
    Ok(list.into_iter().map(Into::into).collect())
}

/// The display list as `DisplayWatcher.kt` describes it.
#[derive(Deserialize)]
struct AndroidDisplays {
    desktop_mode: bool,
    displays: Vec<AndroidDisplay>,
}

#[derive(Deserialize)]
struct AndroidDisplay {
    id: u32,
    name: String,
    width: u32,
    height: u32,
    refresh_mhz: u32,
    modes: Vec<AndroidDisplayMode>,
    presentation: bool,
    secure: bool,
}

#[derive(Deserialize)]
struct AndroidDisplayMode {
    width: u32,
    height: u32,
    refresh_mhz: u32,
}

/// Parse the JSON `DisplayWatcher.kt` produces.
pub fn parse_displays(json: &str) -> Result<DisplayList> {
    let list: AndroidDisplays = serde_json::from_str(json).context("display list JSON")?;
    Ok(DisplayList {
        desktop_mode: list.desktop_mode,
        displays: list
            .displays
            .into_iter()
            .map(|d| DisplayInfo {
                id: d.id,
                name: d.name.trim().to_string(),
                width: d.width,
                height: d.height,
                refresh_mhz: d.refresh_mhz,
                modes: d
                    .modes
                    .into_iter()
                    .map(|m| DisplayMode {
                        width: m.width,
                        height: m.height,
                        refresh_mhz: m.refresh_mhz,
                    })
                    .collect(),
                presentation: d.presentation,
                secure: d.secure,
            })
            .collect(),
    })
}

/// Calls back into the Kotlin `NativeAgent.Callbacks` object from Rust threads.
struct JavaCallbacks {
    vm: JavaVM,
    target: GlobalRef,
}

impl JavaCallbacks {
    /// Call a Java method returning `int`; `None` if the call failed.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    fn call_int(&self, method: &str, sig: &str, args: &[jni::objects::JValue]) -> Option<i32> {
        let mut env = self.vm.attach_current_thread_permanently().ok()?;
        match env
            .call_method(self.target.as_obj(), method, sig, args)
            .and_then(|v| v.i())
        {
            Ok(v) => Some(v),
            Err(_) => {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
                None
            }
        }
    }

    fn call(&self, method: &str, sig: &str, args: &[jni::objects::JValue]) {
        let Ok(mut env) = self.vm.attach_current_thread_permanently() else {
            return;
        };
        if env.call_method(self.target.as_obj(), method, sig, args).is_err() {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
    }
}

impl AgentCallbacks for JavaCallbacks {
    fn permission_requested(&self, id: u32) {
        self.call("onPermissionRequested", "(I)V", &[jni::objects::JValue::Int(id as i32)]);
    }

    fn status_changed(&self) {
        self.call("onStatusChanged", "()V", &[]);
    }
}

// --- video output: `MirrorOutput.kt` ----------------------------------------------------

/// `onVideoFrame` flags.
const FRAME_KEY: jint = 1;
const FRAME_CONFIG: jint = 2;

/// Why `onMirrorStart` failed, in words: the codes of `MirrorOutput.kt`.
fn start_result(code: jint) -> Result<(), String> {
    Err(match code {
        0 => return Ok(()),
        1 => "Samsung DeX is on: turn DeX off on the phone (the monitor then mirrors the screen), then start again",
        2 => "the phone cannot show a picture on this display (is the monitor still attached?)",
        3 => "the phone did not open a window on the display: unlock the phone and start again",
        4 => "the phone has no H.264 decoder for this picture size",
        5 => "the phone's video decoder did not start",
        _ => "the phone could not start the video output",
    }
    .to_string())
}

/// What `onVideoFrame` answers.
fn frame_result(code: Option<jint>) -> FrameResult {
    match code {
        Some(0) => FrameResult::Queued,
        Some(1) => FrameResult::NoInput,
        Some(2) => FrameResult::NeedKeyframe,
        _ => FrameResult::Failed(MirrorEnd::DecoderFailed, "the phone's video decoder failed".into()),
    }
}

/// The reason codes of `mirrorFailed` in `MirrorOutput.kt`.
fn end_reason(code: jint) -> MirrorEnd {
    match code {
        1 => MirrorEnd::DecoderFailed,
        2 => MirrorEnd::DecoderStalled,
        3 => MirrorEnd::DisplayRemoved,
        _ => MirrorEnd::SurfaceLost,
    }
}

/// The decoder counters as `MirrorDecoder.kt` writes them.
#[derive(Deserialize)]
struct AndroidSinkStats {
    decoded: u64,
    dropped: u64,
    bad: u64,
    decode_us: u32,
}

fn parse_sink_stats(json: &str) -> SinkStats {
    serde_json::from_str::<AndroidSinkStats>(json).map_or_else(
        |_| SinkStats::default(),
        |s| SinkStats {
            decoded: s.decoded,
            dropped: s.dropped,
            bad: s.bad,
            decode_us: s.decode_us,
        },
    )
}

/// The agent's video output is the app: a window on the monitor and a `MediaCodec` decoder.
/// The agent calls it from threads that may block.
struct AndroidSink(Arc<JavaCallbacks>);

impl AndroidSink {
    /// Run JNI calls with a local reference frame: the calling threads live long, and every
    /// frame makes references.
    fn with_env<T>(&self, call: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<T>) -> Option<T> {
        let mut env = self.0.vm.attach_current_thread_permanently().ok()?;
        let target = self.0.target.as_obj();
        match env.with_local_frame(8, |env| call(env, target)) {
            Ok(v) => Some(v),
            Err(_) => {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
                None
            }
        }
    }
}

impl VideoSink for AndroidSink {
    fn start(&self, stream: u64, display: &DisplayInfo, config: &VideoConfig) -> Result<(), String> {
        let args = [
            JValue::Long(stream as jlong),
            JValue::Int(display.id as jint),
            JValue::Int(config.width as jint),
            JValue::Int(config.height as jint),
            JValue::Int(config.fps as jint),
        ];
        let code = self.with_env(|env, target| env.call_method(target, "onMirrorStart", "(JIIII)I", &args)?.i());
        start_result(code.unwrap_or(-1))
    }

    fn frame(&self, stream: u64, frame: &VideoFrame) -> FrameResult {
        let flags = if frame.keyframe { FRAME_KEY } else { 0 } | if frame.config { FRAME_CONFIG } else { 0 };
        let code = self.with_env(|env, target| {
            let data = env.byte_array_from_slice(&frame.data)?;
            let args = [
                JValue::Long(stream as jlong),
                JValue::Object(&data),
                JValue::Long(frame.pts_us as jlong),
                JValue::Int(flags),
            ];
            env.call_method(target, "onVideoFrame", "(J[BJI)I", &args)?.i()
        });
        frame_result(code)
    }

    fn stats(&self, stream: u64) -> SinkStats {
        self.with_env(|env, target| {
            let json = env
                .call_method(
                    target,
                    "onMirrorStats",
                    "(J)Ljava/lang/String;",
                    &[JValue::Long(stream as jlong)],
                )?
                .l()?;
            let json = JString::from(json);
            Ok(parse_sink_stats(&String::from(env.get_string(&json)?)))
        })
        .unwrap_or_default()
    }

    fn stop(&self, stream: u64) {
        self.0.call("onMirrorStop", "(J)V", &[JValue::Long(stream as jlong)]);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_mirrorFailed<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    stream: jlong,
    reason: jint,
    message: JString<'l>,
) {
    let message = jstr(&mut env, &message);
    with_agent(|a| a.mirror_failed(stream as u64, end_reason(reason), &message));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_requestKeyframe<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
    stream: jlong,
) {
    with_agent(|a| a.request_keyframe(stream as u64));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_setPhoneLocked<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
    locked: jboolean,
) {
    with_agent(|a| a.set_phone_locked(locked != 0));
}

/// USB through the app: Kotlin opens the `UsbDeviceConnection` and returns its fd
/// (`onOpenDevice`: fd, or -1 no permission, -2 no such device, -3 open failed); we work on
/// a duplicate and tell Kotlin when to close the connection (`onCloseDevice`).
#[cfg(target_os = "android")]
fn usb_backend(cb: Arc<JavaCallbacks>) -> Arc<dyn hubbridge_agent::usb::UsbBackend> {
    use hubbridge_agent::usb::UsbError;
    use std::os::fd::BorrowedFd;
    let open_cb = Arc::clone(&cb);
    let opener = Arc::new(move |id: u32| {
        match open_cb.call_int("onOpenDevice", "(I)I", &[jni::objects::JValue::Int(id as i32)]) {
            Some(fd) if fd >= 0 => {
                // Safety: Kotlin keeps the connection (and so the fd) open until onCloseDevice.
                let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
                borrowed
                    .try_clone_to_owned()
                    .map_err(|e| UsbError::Failed(format!("dup usbfs fd: {e}")))
            }
            Some(-1) => Err(UsbError::NoPermission(
                "allow USB access for this device on the phone".into(),
            )),
            Some(-2) => Err(UsbError::NotFound),
            _ => Err(UsbError::Failed("the phone could not open the device".into())),
        }
    });
    let closer = Arc::new(move |id: u32| cb.call("onCloseDevice", "(I)V", &[jni::objects::JValue::Int(id as i32)]));
    hubbridge_agent::nusb_backend::NusbBackend::new(opener, closer)
}

fn init_logging() {
    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("HubBridge"),
        );
        // Rust panics go to stderr, which Android drops: send them to logcat too.
        static HOOK: std::sync::Once = std::sync::Once::new();
        HOOK.call_once(|| {
            let default = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                log::error!("panic: {info}");
                default(info);
            }));
        });
    }
}

fn jstr(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(Into::into).unwrap_or_default()
}

fn new_jstring(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

fn with_agent<T>(f: impl FnOnce(&Agent) -> T) -> Option<T> {
    RUNNING.lock().unwrap().as_ref().map(|r| f(&r.agent))
}

#[allow(clippy::too_many_arguments)]
fn start(
    state_dir: String,
    name: String,
    model: String,
    os: String,
    app_version: String,
    usb_host: bool,
    port: u16,
    lan: bool,
    callbacks: Arc<JavaCallbacks>,
) -> Result<u16> {
    stop();
    let agent = Agent::new(AgentConfig {
        state_dir: state_dir.into(),
        name,
        model,
        os,
        app_version,
        usb_host,
    })?;
    #[cfg(target_os = "android")]
    agent.set_usb_backend(usb_backend(Arc::clone(&callbacks)));
    agent.set_video_sink(Arc::new(AndroidSink(Arc::clone(&callbacks))));
    agent.set_callbacks(callbacks);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("hubbridge")
        .enable_all()
        .build()?;
    // Loopback only = reachable just through `adb forward`; LAN = Wi-Fi/Ethernet too.
    // `[::]` is dual-stack on Android (IPv4 + IPv6); fall back to IPv4 if IPv6 is off.
    let listener = if lan {
        let v6 = SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
        match rt.block_on(tokio::net::TcpListener::bind(v6)) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("bind {v6}: {e}; IPv4 only");
                let v4 = SocketAddr::from(([0, 0, 0, 0], port));
                rt.block_on(tokio::net::TcpListener::bind(v4))
                    .with_context(|| format!("bind {v4}"))?
            }
        }
    } else {
        let lo = SocketAddr::from(([127, 0, 0, 1], port));
        rt.block_on(tokio::net::TcpListener::bind(lo))
            .with_context(|| format!("bind {lo}"))?
    };
    let bound = listener.local_addr()?.port();
    let serve = agent.clone();
    rt.spawn(async move {
        if let Err(e) = serve.serve(listener).await {
            log::error!("agent server stopped: {e:#}");
        }
    });
    *RUNNING.lock().unwrap() = Some(Running { rt, agent });
    Ok(bound)
}

fn stop() {
    if let Some(r) = RUNNING.lock().unwrap().take() {
        r.agent.shutdown();
        r.rt.shutdown_timeout(Duration::from_secs(1));
    }
}

/// Returns the bound port, or -1 on error (logged).
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_start<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    state_dir: JString<'l>,
    name: JString<'l>,
    model: JString<'l>,
    os: JString<'l>,
    app_version: JString<'l>,
    usb_host: jboolean,
    port: jint,
    lan: jboolean,
    callbacks: JObject<'l>,
) -> jint {
    init_logging();
    let args = (
        jstr(&mut env, &state_dir),
        jstr(&mut env, &name),
        jstr(&mut env, &model),
        jstr(&mut env, &os),
        jstr(&mut env, &app_version),
    );
    let cb = match (env.get_java_vm(), env.new_global_ref(callbacks)) {
        (Ok(vm), Ok(target)) => Arc::new(JavaCallbacks { vm, target }),
        _ => return -1,
    };
    match start(
        args.0,
        args.1,
        args.2,
        args.3,
        args.4,
        usb_host != 0,
        port as u16,
        lan != 0,
        cb,
    ) {
        Ok(p) => p as jint,
        Err(e) => {
            log::error!("start failed: {e:#}");
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_stop<'l>(_env: JNIEnv<'l>, _this: JObject<'l>) {
    stop();
}

/// Opens a pairing window; returns the PIN to show, or "" if the agent is not running.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_startPairing<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
) -> jstring {
    let pin = with_agent(|a| a.start_pairing()).unwrap_or_default();
    new_jstring(&mut env, &pin)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_cancelPairing<'l>(_env: JNIEnv<'l>, _this: JObject<'l>) {
    with_agent(|a| a.cancel_pairing());
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_setDevices<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    json: JString<'l>,
) {
    let json = jstr(&mut env, &json);
    match parse_devices(&json) {
        Ok(list) => {
            with_agent(|a| a.set_devices(list));
        }
        Err(e) => log::error!("setDevices: {e:#}"),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_setDisplays<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    json: JString<'l>,
) {
    let json = jstr(&mut env, &json);
    match parse_displays(&json) {
        Ok(list) => {
            with_agent(|a| a.set_displays(list));
        }
        Err(e) => log::error!("setDisplays: {e:#}"),
    }
}

/// Agent status as JSON (see `hubbridge_agent::AgentStatus`), or "" if not running.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_status<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
) -> jstring {
    let json = with_agent(|a| serde_json::to_string(&a.status()).unwrap_or_default()).unwrap_or_default();
    new_jstring(&mut env, &json)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_demoz_hubbridge_NativeAgent_forgetHost<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    host_id: JString<'l>,
) -> jboolean {
    let id = jstr(&mut env, &host_id);
    match with_agent(|a| a.forget_host(&id)) {
        Some(Ok(true)) => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scanner_json() {
        let raw = hex::encode(hubbridge_proto::fixtures::cp2102_raw());
        let json = format!(
            r#"[{{"id":1002,"path":"/dev/bus/usb/001/003","vendor_id":4292,"product_id":60000,
            "class":0,"subclass":0,"protocol":0,"manufacturer":"Silicon Labs \u0000 ","product":"CP2102",
            "serial":null,"has_permission":true,
            "configurations":[{{"value":1,"name":null,"attributes":128,"max_power_ma":100,
              "interfaces":[{{"number":0,"alternate":0,"class":255,"subclass":0,"protocol":0,"name":"UART",
              "endpoints":[{{"address":129,"attributes":2,"max_packet_size":64,"interval":0}}]}}]}}],
            "raw_descriptors_hex":"{raw}"}},
            {{"id":7,"path":"/dev/bus/usb/001/007","vendor_id":1,"product_id":2,"class":3,"subclass":0,
            "protocol":0,"manufacturer":null,"product":null,"serial":null,"has_permission":false,
            "raw_descriptors_hex":null}}]"#
        );
        let list = parse_devices(&json).unwrap();
        assert_eq!(list.len(), 2);
        let uart = &list[0];
        assert_eq!(uart.bcd_usb, Some(0x0200));
        // Raw descriptors win (2 endpoints), interface name from Android is kept.
        let intf = &uart.configurations[0].interfaces[0];
        assert_eq!(intf.endpoints.len(), 2);
        assert_eq!(intf.name.as_deref(), Some("UART"));
        assert!(list[1].configurations.is_empty());
        assert_eq!(uart.manufacturer.as_deref(), Some("Silicon Labs"));
    }

    /// Values of the SM-G970F with its monitor through a hub, DeX off (2026-11-28).
    #[test]
    fn parses_display_json() {
        let mode = |w, h, r| format!(r#"{{"width":{w},"height":{h},"refresh_mhz":{r}}}"#);
        let modes = [
            mode(2560, 1600, 60011),
            mode(1920, 1200, 59885),
            mode(1280, 720, 60000),
            mode(720, 480, 59940),
            mode(640, 480, 59940),
        ]
        .join(",");
        let json = format!(
            r#"{{"desktop_mode":false,"displays":[{{"id":10,"name":"HDMI Screen","width":2560,"height":1600,
            "refresh_mhz":60011,"modes":[{modes}],"presentation":true,"secure":true}}]}}"#
        );
        let list = parse_displays(&json).unwrap();
        assert!(!list.desktop_mode);
        let [d] = list.displays.as_slice() else {
            panic!("{list:?}")
        };
        assert_eq!((d.id, d.name.as_str()), (10, "HDMI Screen"));
        assert_eq!((d.width, d.height, d.refresh_mhz), (2560, 1600, 60_011));
        assert_eq!(d.modes.len(), 5);
        assert_eq!(
            (d.modes[2].width, d.modes[2].height, d.modes[2].refresh_mhz),
            (1280, 720, 60_000)
        );
        assert!(d.presentation && d.secure);
        assert!(
            parse_displays(r#"{"desktop_mode":true,"displays":[]}"#)
                .unwrap()
                .desktop_mode
        );
        assert!(parse_displays(r#"{"displays":[]}"#).is_err());
        // The Kotlin side writes exactly these names.
        let kotlin = include_str!("../../../android/app/src/main/kotlin/com/demoz/hubbridge/DisplayWatcher.kt");
        for field in [
            "desktop_mode",
            "displays",
            "id",
            "name",
            "width",
            "height",
            "refresh_mhz",
            "modes",
            "presentation",
            "secure",
        ] {
            assert!(
                kotlin.contains(&format!(".put(\"{field}\"")),
                "DisplayWatcher.kt: {field}"
            );
        }
    }

    #[test]
    fn video_output_codes() {
        assert_eq!(start_result(0), Ok(()));
        assert!(start_result(1).unwrap_err().contains("turn DeX off"));
        for code in [2, 3, 4, 5, -1, 99] {
            assert!(!start_result(code).unwrap_err().is_empty(), "{code}");
        }
        assert_eq!(frame_result(Some(0)), FrameResult::Queued);
        assert_eq!(frame_result(Some(1)), FrameResult::NoInput);
        assert_eq!(frame_result(Some(2)), FrameResult::NeedKeyframe);
        for failed in [Some(3), None] {
            assert!(matches!(
                frame_result(failed),
                FrameResult::Failed(MirrorEnd::DecoderFailed, _)
            ));
        }
        assert_eq!(end_reason(0), MirrorEnd::SurfaceLost);
        assert_eq!(end_reason(1), MirrorEnd::DecoderFailed);
        assert_eq!(end_reason(2), MirrorEnd::DecoderStalled);
        assert_eq!(end_reason(3), MirrorEnd::DisplayRemoved);
    }

    #[test]
    fn parses_decoder_stats_json() {
        let got = parse_sink_stats(r#"{"decoded":90,"dropped":2,"bad":1,"decode_us":4200}"#);
        assert_eq!(
            got,
            SinkStats {
                decoded: 90,
                dropped: 2,
                bad: 1,
                decode_us: 4200
            }
        );
        assert_eq!(parse_sink_stats(""), SinkStats::default());
        let kotlin = include_str!("../../../android/app/src/main/kotlin/com/demoz/hubbridge/MirrorDecoder.kt");
        for field in ["decoded", "dropped", "bad", "decode_us"] {
            assert!(
                kotlin.contains(&format!(".put(\"{field}\"")),
                "MirrorDecoder.kt: {field}"
            );
        }
    }
}
