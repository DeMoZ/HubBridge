//! Network through the system `curl` (present on macOS and Windows 10+), so the app needs no TLS
//! implementation of its own. Used only for donations and updates; the bridge itself does not use it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A command without a console window on Windows.
pub fn command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

fn curl() -> Command {
    let mut c = command(if cfg!(windows) { "curl.exe" } else { "/usr/bin/curl" });
    // The GitHub API refuses requests without a User-Agent.
    c.args(["-fsSL", "--proto", "=https", "-A", "HubBridge"])
        .stdin(Stdio::null());
    c
}

/// Downloads a small answer (JSON) as a whole.
pub fn get(url: &str, timeout_secs: u32) -> Result<Vec<u8>, String> {
    let out = curl()
        .args(["--max-time", &timeout_secs.to_string(), url])
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("curl exited with code {:?}", out.status.code())
        } else {
            err
        })
    }
}

/// Downloads a file to disk.
pub fn download(url: &str, to: &Path) -> Result<(), String> {
    let out = curl()
        .args(["--max-time", "900", "-o"])
        .arg(to)
        .arg(url)
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Opens a page in the browser. https:// only.
pub fn open_url(url: &str) -> std::io::Result<()> {
    if !is_https(url) {
        return Err(std::io::Error::other("only https:// links are allowed"));
    }
    if cfg!(windows) {
        command("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()
            .map(|_| ())
    } else {
        command("/usr/bin/open").arg(url).spawn().map(|_| ())
    }
}

pub fn is_https(url: &str) -> bool {
    url.len() > "https://".len()
        && url.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"))
        && !url[8..].starts_with('/')
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Where the window keeps its files: the HubBridge config folder, or `$HUBBRIDGE_HOME`.
pub fn data_dir() -> PathBuf {
    hubbridge_host::store::default_dir().unwrap_or_else(|_| std::env::temp_dir().join("HubBridge"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_links() {
        assert!(is_https("https://pay.example/a"));
        assert!(is_https("HTTPS://pay.example/a"));
        assert!(!is_https("http://pay.example/a"));
        assert!(!is_https("https:///etc/passwd"));
        assert!(!is_https("javascript:alert(1)"));
        assert!(!is_https("https://a b"));
        assert!(!is_https("https://"));
        assert!(!is_https("ссылка на оплату"));
    }
}
