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
use hubbridge_agent::{Agent, AgentCallbacks, AgentConfig};
use hubbridge_proto::{ConfigInfo, UsbDeviceInfo};
use jni::objects::{GlobalRef, JObject, JString};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jint, jstring};
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

/// Calls back into the Kotlin `NativeAgent.Callbacks` object from Rust threads.
struct JavaCallbacks {
    vm: JavaVM,
    target: GlobalRef,
}

impl JavaCallbacks {
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

fn init_logging() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("HubBridge"),
    );
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
    callbacks: Arc<dyn AgentCallbacks>,
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
    agent.set_callbacks(callbacks);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("hubbridge")
        .enable_all()
        .build()?;
    // Loopback only = reachable just through `adb forward`; LAN = Wi-Fi/Ethernet too.
    // `[::]` is dual-stack on Linux/Android (IPv4 + IPv6); fall back to IPv4 if IPv6 is off.
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
}
