//! Human-readable output.

use std::fmt::Write;
use std::time::Duration;

use hubbridge_proto::usb::{class_name, clean_str};
use hubbridge_proto::{AgentInfo, TransferType, UsbDeviceInfo};

fn bcd(v: u16) -> String {
    format!("{}.{:02x}", v >> 8, v & 0xff)
}

pub fn device(d: &UsbDeviceInfo) -> String {
    let mut s = String::new();
    let maker = d
        .manufacturer
        .as_deref()
        .map(|m| format!(" — {}", clean_str(m)))
        .unwrap_or_default();
    let perm = if d.has_permission { "granted" } else { "not granted" };
    let _ = writeln!(s, "[{}] {}{}  (permission: {perm})", d.id, d.label(), maker);
    let mut facts = vec![d.path.clone(), format!("class {:02x} {}", d.class, class_name(d.class))];
    if let Some(v) = d.bcd_usb {
        facts.push(format!("USB {}", bcd(v)));
    }
    if let Some(v) = d.bcd_device {
        facts.push(format!("rev {}", bcd(v)));
    }
    if let Some(sn) = &d.serial {
        facts.push(format!("serial {sn}"));
    }
    if let Some(raw) = &d.raw_descriptors {
        facts.push(format!("{} descriptor bytes", raw.len()));
    }
    let _ = writeln!(s, "    {}", facts.join(" · "));
    if let Ok(serial) = crate::serial::detect(d, 0) {
        let _ = writeln!(s, "    serial: {}", serial.label());
    }
    for c in &d.configurations {
        let name = c
            .name
            .as_deref()
            .map(|n| format!(" \"{}\"", clean_str(n)))
            .unwrap_or_default();
        let _ = writeln!(
            s,
            "    config {}{}: {} mA, attributes 0x{:02x}",
            c.value, name, c.max_power_ma, c.attributes
        );
        for i in &c.interfaces {
            let name = i
                .name
                .as_deref()
                .map(|n| format!(" \"{}\"", clean_str(n)))
                .unwrap_or_default();
            let _ = writeln!(
                s,
                "      interface {}.{} {} {:02x}/{:02x}/{:02x}{}",
                i.number,
                i.alternate,
                class_name(i.class),
                i.class,
                i.subclass,
                i.protocol,
                name
            );
            for e in &i.endpoints {
                let kind = match e.transfer_type() {
                    TransferType::Control => "control",
                    TransferType::Isochronous => "isochronous",
                    TransferType::Bulk => "bulk",
                    TransferType::Interrupt => "interrupt",
                };
                let dir = if e.is_in() { "IN " } else { "OUT" };
                let interval = match e.transfer_type() {
                    TransferType::Interrupt | TransferType::Isochronous => format!(", interval {}", e.interval),
                    _ => String::new(),
                };
                let _ = writeln!(
                    s,
                    "        endpoint 0x{:02x} {dir} {kind}, {} bytes{interval}",
                    e.address, e.max_packet_size
                );
            }
        }
    }
    s
}

pub fn agent_info(i: &AgentInfo) -> String {
    format!(
        "{} ({})\n  id       {}\n  os       {}\n  app      {}\n  protocol v{}\n  USB host {}",
        clean_str(&i.name),
        i.model,
        i.agent_id,
        i.os,
        i.app_version,
        i.proto_version,
        if i.usb_host {
            "yes"
        } else {
            "no (this phone cannot host USB devices)"
        }
    )
}

pub fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

/// min / avg / max / jitter (mean absolute difference of consecutive samples).
pub fn rtt_summary(samples: &[Duration]) -> String {
    if samples.is_empty() {
        return "no replies".into();
    }
    let v: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    let min = v.iter().cloned().fold(f64::MAX, f64::min);
    let max = v.iter().cloned().fold(0.0, f64::max);
    let avg = v.iter().sum::<f64>() / v.len() as f64;
    let jitter = if v.len() > 1 {
        v.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (v.len() - 1) as f64
    } else {
        0.0
    };
    format!(
        "{} replies: min {min:.2} / avg {avg:.2} / max {max:.2} ms, jitter {jitter:.2} ms",
        v.len()
    )
}
