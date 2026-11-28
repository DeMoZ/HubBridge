//! Updating the app from GitHub releases, as in UsbMemoryChecker and the other DeMoZ tools, on both systems.
//!
//! The latest release comes from the GitHub API; it is searched for the archive of this system
//! (`…-macos-arm64.zip` with `HubBridge.app` or `…-windows-x64.zip` with `HubBridge.exe`). The downloaded archive is
//! checked against the SHA-256 GitHub publishes for every release file and unpacked into a temporary folder; a small
//! script replaces the app after it has quit and starts the new version.

use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::net::{self, command};

pub const REPO: &str = "DeMoZ/HubBridge";
const WORK_PREFIX: &str = "HubBridge-update-";
const EXE: &str = "HubBridge.exe";
const APP: &str = "HubBridge.app";
const BUNDLE_ID: &str = "com.demoz.hubbridge";
const NO_APP: &str = "the update archive has no HubBridge app inside";

/// The suffix of the archive for this system.
pub fn asset_suffix() -> &'static str {
    if cfg!(windows) {
        "-windows-x64.zip"
    } else {
        "-macos-arm64.zip"
    }
}

pub fn latest_api() -> String {
    std::env::var("HUBBRIDGE_UPDATES_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("https://api.github.com/repos/{REPO}/releases/latest"))
}

/// The list of all releases, for the browser.
pub fn releases_page() -> String {
    format!("https://github.com/{REPO}/releases")
}

pub type Version = (u32, u32, u32);

pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.').map(|p| p.parse::<u32>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

pub fn current() -> Version {
    parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0))
}

pub fn format_version(v: Version) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// The update file of this system in a release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub url: String,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// `None`: the release has no file for this system.
    pub file: Option<Asset>,
    pub page_url: String,
    pub notes: String,
}

/// GitHub's "latest release" answer: tag, the archive for this system and its SHA-256. `None`: not a release.
pub fn parse_release(json: &str, suffix: &str) -> Option<Release> {
    let v: Value = serde_json::from_str(json).ok()?;
    let version = parse_version(v.get("tag_name")?.as_str()?)?;
    let file = v
        .get("assets")?
        .as_array()?
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.to_lowercase().ends_with(suffix))
        })
        .and_then(|asset| {
            Some(Asset {
                url: asset.get("browser_download_url")?.as_str()?.to_string(),
                // "sha256:<hex>"
                sha256: asset
                    .get("digest")
                    .and_then(Value::as_str)
                    .and_then(|d| d.strip_prefix("sha256:"))
                    .map(|h| h.to_lowercase()),
            })
        });
    Some(Release {
        version,
        file,
        page_url: v
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        notes: release_notes(v.get("body").and_then(Value::as_str).unwrap_or_default()),
    })
}

/// The release description for the update window: without the "Установка" / "Install" sections and heading marks.
fn release_notes(body: &str) -> String {
    let body = ["\n## Установка", "\n## Install"]
        .iter()
        .fold(body, |b, h| b.split(h).next().unwrap_or(b));
    body.lines()
        .map(|l| l.trim_start_matches('#').trim_start())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

pub fn latest() -> Result<Release, String> {
    let data = net::get(&latest_api(), 20)?;
    parse_release(&String::from_utf8_lossy(&data), asset_suffix())
        .ok_or_else(|| "the answer does not describe a release".to_string())
}

/// What an update replaces: `HubBridge.app` on a Mac, the folder with the .exe on Windows.
/// `None`: a development build (`cargo run`), or the program does not run from an .app.
pub fn install_target() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        return None;
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    if cfg!(target_os = "macos") {
        let app = exe.ancestors().nth(3)?;
        return (app.extension()? == "app").then(|| app.to_path_buf());
    }
    exe.parent().map(Path::to_path_buf)
}

/// Whether the place of the app can be written to (on a Mac: the folder that holds the .app).
pub fn can_write(target: &Path) -> bool {
    let folder = if cfg!(target_os = "macos") {
        target.parent()
    } else {
        Some(target)
    };
    let Some(folder) = folder else { return false };
    let probe = folder.join(format!(".hubbridge-update-check-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Downloads the archive, checks its SHA-256 and unpacks it. Returns the unpacked app (the .app or the .exe folder).
pub fn download(release: &Release) -> Result<PathBuf, String> {
    let asset = release
        .file
        .as_ref()
        .ok_or("the release has no download for this system")?;
    let expected = asset
        .sha256
        .as_ref()
        .ok_or("the release does not publish a SHA-256 for this file")?;
    let work = std::env::temp_dir().join(format!("{WORK_PREFIX}{}", format_version(release.version)));
    prepare(&work, expected, |zip| net::download(&asset.url, zip))
}

/// Fills a fresh `work` folder with the checked and unpacked archive; on failure the folder is removed.
fn prepare(work: &Path, expected: &str, fetch: impl FnOnce(&Path) -> Result<(), String>) -> Result<PathBuf, String> {
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(work).map_err(|e| e.to_string())?;
    let zip = work.join("update.zip");
    let result = fetch(&zip)
        .and_then(|()| verify(&zip, expected))
        .and_then(|()| unpack(&zip, work));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(work);
    }
    result
}

/// Removes the download folder of an unpacked update that will not be installed.
pub fn discard(source: &Path) {
    let work = work_dir(source);
    if work != source {
        let _ = std::fs::remove_dir_all(work);
    }
}

fn verify(zip: &Path, expected: &str) -> Result<(), String> {
    let actual = sha256_file(zip).map_err(|e| e.to_string())?;
    if actual != expected {
        return Err(format!(
            "the downloaded archive does not match the published SHA-256 ({actual} ≠ {expected})"
        ));
    }
    Ok(())
}

/// Unpacks the archive into `work` and returns the HubBridge app inside it.
fn unpack(zip: &Path, work: &Path) -> Result<PathBuf, String> {
    let unpacked = work.join("app");
    std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    let status = if cfg!(windows) {
        command("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!(
                "Expand-Archive -LiteralPath {} -DestinationPath {} -Force",
                ps_quote(zip),
                ps_quote(&unpacked)
            ))
            .status()
    } else {
        // ditto keeps the signature and the attributes of the .app
        command("/usr/bin/ditto")
            .arg("-x")
            .arg("-k")
            .arg(zip)
            .arg(&unpacked)
            .status()
    };
    if !status.map(|s| s.success()).unwrap_or(false) {
        return Err("could not unpack the update archive".into());
    }
    let _ = std::fs::remove_file(zip);
    let found = if cfg!(windows) {
        find(&unpacked, &|p| p.file_name().is_some_and(|n| n == EXE)).and_then(|p| p.parent().map(Path::to_path_buf))
    } else {
        find(&unpacked, &|p| p.is_dir() && p.file_name().is_some_and(|n| n == APP))
            .filter(|app| bundle_id(app).as_deref() == Some(BUNDLE_ID))
    };
    found.ok_or_else(|| NO_APP.into())
}

fn bundle_id(app: &Path) -> Option<String> {
    let out = command("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn find(dir: &Path, pred: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if pred(&p) {
            return Some(p);
        }
        if p.is_dir()
            && p.extension().is_none_or(|e| e != "app")
            && let Some(found) = find(&p, pred)
        {
            return Some(found);
        }
    }
    None
}

/// A single-quoted PowerShell string. PowerShell also ends it at a typographic single quote, so those are doubled too.
fn ps_quote(p: &Path) -> String {
    let mut quoted = String::from("'");
    for c in p.to_string_lossy().chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            quoted.push(c);
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

/// The download folder (HubBridge-update-<version>), which the script removes after copying.
fn work_dir(source: &Path) -> PathBuf {
    source
        .ancestors()
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(WORK_PREFIX))
        })
        .unwrap_or(source)
        .to_path_buf()
}

/// The script for a Mac: waits for the program (`pid`) to quit, swaps the .app for the new one and starts it.
/// If the swap fails, it puts the old one back and starts it with `--update-failed`. A move from another volume can
/// leave a partial .app: it is removed only after the old one was really moved aside.
pub fn mac_script(source: &Path, target: &Path, pid: u32) -> String {
    let old = target.with_extension("app.old");
    let (s, t, o, w) = (
        sh_quote(source),
        sh_quote(target),
        sh_quote(&old),
        sh_quote(&work_dir(source)),
    );
    format!(
        "while kill -0 {pid} 2>/dev/null; do sleep 0.2; done\n\
         sleep 0.5\n\
         rm -rf {o}\n\
         if [ ! -e {o} ] && mv {t} {o}; then\n\
         \x20 if mv {s} {t}; then\n\
         \x20   rm -rf {o} {w}\n\
         \x20   xattr -dr com.apple.quarantine {t} 2>/dev/null\n\
         \x20   open {t}\n\
         \x20   exit 0\n\
         \x20 fi\n\
         \x20 rm -rf {t} && mv {o} {t}\n\
         fi\n\
         rm -rf {w}\n\
         open {t} --args --update-failed\n"
    )
}

/// The script for Windows: waits for the program to quit, copies the new files over it and starts it.
pub fn windows_script(source: &Path, target: &Path, pid: u32) -> String {
    let exe = target.join(EXE);
    let work = ps_quote(&work_dir(source));
    format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue\r\n\
         Start-Sleep -Milliseconds 500\r\n\
         robocopy {} {} /E /R:10 /W:1 /NFL /NDL /NJH /NJS /NP | Out-Null\r\n\
         $failed = $LASTEXITCODE -ge 8\r\n\
         Remove-Item -LiteralPath {work} -Recurse -Force -ErrorAction SilentlyContinue\r\n\
         if ($failed) {{ Start-Process -FilePath {exe} -ArgumentList '--update-failed'; exit 1 }}\r\n\
         Start-Process -FilePath {exe}\r\n",
        ps_quote(source),
        ps_quote(target),
        exe = ps_quote(&exe),
    )
}

/// With a byte order mark: without one Windows PowerShell 5.1 reads the file in the ANSI code page.
fn write_ps_script(path: &Path, script: &str) -> std::io::Result<()> {
    std::fs::write(path, format!("\u{feff}{script}"))
}

/// Starts the replace script; the program has to quit after this.
pub fn start_install(source: &Path, target: &Path) -> Result<(), String> {
    let pid = std::process::id();
    if cfg!(windows) {
        let script = std::env::temp_dir().join("HubBridge-update.ps1");
        write_ps_script(&script, &windows_script(source, target, pid)).map_err(|e| e.to_string())?;
        command("powershell.exe")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-WindowStyle",
                "Hidden",
                "-File",
            ])
            .arg(&script)
            .spawn()
            .map_err(|e| e.to_string())?;
    } else {
        let script = std::env::temp_dir().join("HubBridge-update.sh");
        std::fs::write(&script, mac_script(source, target, pid)).map_err(|e| e.to_string())?;
        command("/bin/bash").arg(&script).spawn().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATEST: &str = r###"{"tag_name":"v0.4.0","html_url":"https://github.com/DeMoZ/HubBridge/releases/tag/v0.4.0",
        "body":"## What's new\n- Faster\n## Install\nDownload","assets":[
        {"name":"HubBridge-macOS-v0.4.0.dmg","browser_download_url":"https://x/dmg"},
        {"name":"HubBridge-0.4.0-macos-arm64.zip","browser_download_url":"https://x/mac.zip","digest":"sha256:ABCDEF"},
        {"name":"hubbridge-0.4.0-macos-arm64.tar.gz","browser_download_url":"https://x/cli.tar.gz"},
        {"name":"HubBridge-0.4.0.apk","browser_download_url":"https://x/apk"}]}"###;

    #[test]
    fn release_for_each_system() {
        let mac = parse_release(LATEST, "-macos-arm64.zip").unwrap();
        assert_eq!(mac.version, (0, 4, 0));
        assert_eq!(
            mac.file,
            Some(Asset {
                url: "https://x/mac.zip".into(),
                sha256: Some("abcdef".into())
            })
        );
        assert_eq!(mac.notes, "What's new\n- Faster");
        assert_eq!(mac.page_url, "https://github.com/DeMoZ/HubBridge/releases/tag/v0.4.0");
        let win = parse_release(LATEST, "-windows-x64.zip").unwrap();
        assert_eq!(win.version, (0, 4, 0));
        assert_eq!(win.file, None);
        let no_digest = r#"{"tag_name":"v1.0.0","assets":[{"name":"HubBridge-1.0.0-windows-x64.zip","browser_download_url":"https://x/win.zip"}]}"#;
        let win = parse_release(no_digest, "-windows-x64.zip").unwrap();
        assert_eq!(win.file.unwrap().sha256, None);
        assert!(parse_release("{}", "-windows-x64.zip").is_none());
        assert!(parse_release(r#"{"assets":[]}"#, "-macos-arm64.zip").is_none());
        assert!(parse_release("not json", "-macos-arm64.zip").is_none());
    }

    #[test]
    fn notes_without_install_sections() {
        let body =
            "## What's new\n- Languages\n\n## Что нового\n- Языки\n\n## Installation\n- dmg\n\n## Установка\n- dmg";
        assert_eq!(release_notes(body), "What's new\n- Languages\n\nЧто нового\n- Языки");
        assert_eq!(
            release_notes("## What's new\r\n- x\r\n\r\n## Install\r\n- dmg"),
            "What's new\n- x"
        );
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.4"), Some((0, 4, 0)));
        assert_eq!(parse_version("x"), None);
        assert!(parse_version("v0.10.0") > parse_version("0.9.9"));
    }

    #[test]
    fn sha256_of_file() {
        let p = std::env::temp_dir().join(format!("hubbridge_sha_{}", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        let sum = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_file(&p).unwrap(), sum);
        assert!(verify(&p, sum).is_ok());
        let err = verify(&p, &"0".repeat(64)).unwrap_err();
        assert!(err.contains("does not match the published SHA-256"), "{err}");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn download_needs_a_digest() {
        let release = Release {
            version: (9, 9, 9),
            file: Some(Asset {
                url: "https://invalid.invalid/HubBridge.zip".into(),
                sha256: None,
            }),
            page_url: String::new(),
            notes: String::new(),
        };
        assert_eq!(
            download(&release).unwrap_err(),
            "the release does not publish a SHA-256 for this file"
        );
    }

    #[test]
    fn scripts_quote_paths() {
        let src = Path::new("/tmp/HubBridge-update-0.4.0/app/HubBridge.app");
        let s = mac_script(src, Path::new("/Applications/HubBridge.app"), 42);
        assert!(s.contains("kill -0 42"));
        assert!(s.contains("'/Applications/HubBridge.app.old'"));
        assert!(s.contains("rm -rf '/Applications/HubBridge.app.old' '/tmp/HubBridge-update-0.4.0'"));
        // the paths are built with Path::join, so the check does not depend on the system's separator
        let work = std::env::temp_dir().join("HubBridge-update-0.4.0");
        let target = std::env::temp_dir().join("Rom's tools");
        let w = windows_script(&work.join("app"), &target, 7);
        assert!(w.contains("Wait-Process -Id 7"));
        assert!(w.contains(&ps_quote(&target.join(EXE))));
        assert!(w.contains("Rom''s tools"));
        let remove = format!("Remove-Item -LiteralPath {}", ps_quote(&work));
        assert!(w.find(&remove).unwrap() < w.find("if ($failed)").unwrap(), "{w}");
        assert_eq!(
            ps_quote(Path::new("a'b\u{2018}c\u{2019}d\u{201a}e\u{201b}f")),
            "'a''b\u{2018}\u{2018}c\u{2019}\u{2019}d\u{201a}\u{201a}e\u{201b}\u{201b}f'"
        );
    }

    #[test]
    fn windows_script_file_has_a_byte_order_mark() {
        let p = std::env::temp_dir().join(format!("hubbridge_bom_{}.ps1", std::process::id()));
        let target = Path::new("C:/Users/Роман/HubBridge");
        let script = windows_script(&target.join("app"), target, 7);
        write_ps_script(&p, &script).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]), Some(script.as_bytes()));
        assert!(script.contains("Роман"));
    }

    #[test]
    fn a_failed_download_leaves_no_folder() {
        let work = std::env::temp_dir().join(format!("hubbridge_prepare_{}", std::process::id()));
        let garbage = |zip: &Path| std::fs::write(zip, b"abc").map_err(|e| e.to_string());
        let sum = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        assert_eq!(prepare(&work, sum, |_| Err("offline".into())).unwrap_err(), "offline");
        assert!(!work.exists(), "after a failed download");
        let err = prepare(&work, &"0".repeat(64), garbage).unwrap_err();
        assert!(err.contains("does not match"), "{err}");
        assert!(!work.exists(), "after a digest mismatch");
        assert!(prepare(&work, sum, garbage).is_err());
        assert!(!work.exists(), "after an unpack failure");
    }

    #[test]
    fn discard_removes_only_a_download_folder() {
        let root = std::env::temp_dir().join(format!("hubbridge_discard_{}", std::process::id()));
        let work = root.join("HubBridge-update-0.2.0");
        let source = work.join("app/HubBridge.app");
        let other = root.join("other/HubBridge.app");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        discard(&other);
        assert!(other.exists());
        discard(&source);
        assert!(!work.exists() && root.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(target_os = "macos")]
    mod mac {
        use super::*;

        struct TempDir(PathBuf);

        impl TempDir {
            fn new(name: &str) -> Self {
                let dir = std::env::temp_dir().join(format!("hubbridge_test_{name}_{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                TempDir(dir)
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// A fake bundle `name` with the bundle id `id`, zipped as the release script does it.
        fn zipped_bundle(root: &Path, name: &str, id: &str) -> PathBuf {
            let app = root.join("build").join(name);
            std::fs::create_dir_all(app.join("Contents")).unwrap();
            std::fs::write(
                app.join("Contents/Info.plist"),
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
                     <key>CFBundleIdentifier</key><string>{id}</string></dict></plist>\n"
                ),
            )
            .unwrap();
            let zip = root.join("update.zip");
            let status = command("/usr/bin/ditto")
                .args(["-c", "-k", "--keepParent"])
                .arg(&app)
                .arg(&zip)
                .status()
                .unwrap();
            assert!(status.success());
            zip
        }

        #[test]
        fn unpack_returns_the_hubbridge_app() {
            let tmp = TempDir::new("unpack_ok");
            let zip = zipped_bundle(&tmp.0, "HubBridge.app", "com.demoz.hubbridge");
            let app = unpack(&zip, &tmp.0.join("work")).unwrap();
            assert_eq!(app, tmp.0.join("work/app/HubBridge.app"));
            assert!(app.join("Contents/Info.plist").is_file());
        }

        #[test]
        fn unpack_refuses_another_app_name() {
            let tmp = TempDir::new("unpack_name");
            let zip = zipped_bundle(&tmp.0, "Other.app", "com.demoz.hubbridge");
            assert_eq!(unpack(&zip, &tmp.0.join("work")).unwrap_err(), NO_APP);
        }

        #[test]
        fn unpack_refuses_another_bundle_id() {
            let tmp = TempDir::new("unpack_id");
            let zip = zipped_bundle(&tmp.0, "HubBridge.app", "com.example.x");
            assert_eq!(unpack(&zip, &tmp.0.join("work")).unwrap_err(), NO_APP);
        }

        /// Runs the replace script with every `open` turned into a no-op.
        fn run_script(source: &Path, target: &Path) {
            run_changed_script(source, target, |script| script);
        }

        /// `run_script` with a step of the script replaced, to make it fail.
        fn run_changed_script(source: &Path, target: &Path, change: impl FnOnce(String) -> String) {
            let mut gone = command("/usr/bin/true").spawn().unwrap();
            let pid = gone.id();
            gone.wait().unwrap();
            let script = mac_script(source, target, pid).replace("open ", ": ");
            assert!(!script.contains("open "));
            command("/bin/bash").arg("-c").arg(change(script)).status().unwrap();
        }

        fn bundle_with_marker(app: &Path, marker: &str) {
            std::fs::create_dir_all(app).unwrap();
            std::fs::write(app.join(marker), b"").unwrap();
        }

        #[test]
        fn mac_script_swaps_the_app() {
            let tmp = TempDir::new("swap");
            let work = tmp.0.join("HubBridge-update-0.2.0");
            let source = work.join("app/HubBridge.app");
            let target = tmp.0.join("Apps/HubBridge.app");
            bundle_with_marker(&source, "new");
            bundle_with_marker(&target, "old");
            run_script(&source, &target);
            assert!(target.join("new").is_file());
            assert!(!target.join("old").exists());
            assert!(!tmp.0.join("Apps/HubBridge.app.old").exists());
            assert!(!work.exists());
        }

        fn replaced(script: &str, step: &str, with: &str) -> String {
            assert_eq!(script.matches(step).count(), 1, "`{step}` in {script}");
            script.replace(step, with)
        }

        #[test]
        fn mac_script_removes_a_partly_moved_app() {
            let tmp = TempDir::new("partial");
            let work = tmp.0.join("HubBridge-update-0.2.0");
            let source = work.join("app/HubBridge.app");
            let target = tmp.0.join("Apps/HubBridge.app");
            bundle_with_marker(&source, "new");
            bundle_with_marker(&target, "old");
            run_changed_script(&source, &target, |script| {
                // As a move from another volume that stops halfway.
                let step = format!("mv {} {}", sh_quote(&source), sh_quote(&target));
                let partial = format!("{{ mkdir {t} && touch {t}/partial; false; }}", t = sh_quote(&target));
                replaced(&script, &step, &partial)
            });
            assert!(target.join("old").is_file());
            assert!(!target.join("partial").exists());
            assert!(!tmp.0.join("Apps/HubBridge.app.old").exists());
            assert!(!work.exists());
        }

        #[test]
        fn mac_script_keeps_the_app_it_could_not_move_aside() {
            let tmp = TempDir::new("aside");
            let work = tmp.0.join("HubBridge-update-0.2.0");
            let source = work.join("app/HubBridge.app");
            let target = tmp.0.join("Apps/HubBridge.app");
            let old = target.with_extension("app.old");
            bundle_with_marker(&source, "new");
            bundle_with_marker(&target, "old");
            run_changed_script(&source, &target, |script| {
                let step = format!("mv {} {}", sh_quote(&target), sh_quote(&old));
                replaced(&script, &step, "false")
            });
            assert!(target.join("old").is_file());
            assert!(!target.join("new").exists());
            assert!(!work.exists());

            // A leftover .app.old that cannot be removed: `mv` would put the app inside it.
            bundle_with_marker(&source, "new");
            bundle_with_marker(&old, "stale");
            run_changed_script(&source, &target, |script| {
                let step = format!("rm -rf {}\n", sh_quote(&old));
                replaced(&script, &step, "")
            });
            assert!(target.join("old").is_file());
            assert!(!old.join("HubBridge.app").exists());
            assert!(!work.exists());
        }

        #[test]
        fn mac_script_restores_the_old_app_when_the_swap_fails() {
            let tmp = TempDir::new("restore");
            // the new app is missing, so moving it into place fails
            let work = tmp.0.join("HubBridge-update-0.2.0");
            let source = work.join("app/HubBridge.app");
            bundle_with_marker(&work.join("app/Other.app"), "left over");
            let target = tmp.0.join("Apps/HubBridge.app");
            bundle_with_marker(&target, "old");
            run_script(&source, &target);
            assert!(target.join("old").is_file());
            assert!(!tmp.0.join("Apps/HubBridge.app.old").exists());
            assert!(!work.exists());
            assert!(tmp.0.exists());
        }
    }
}
