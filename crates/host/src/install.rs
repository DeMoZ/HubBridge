//! Install the HubBridge Android app on a phone connected by USB (adb).
//!
//! Backs `hubbridge install-apk` now and the "Install on phone" button of the desktop app
//! later. The APK is taken from, in order:
//! 1. an explicit path;
//! 2. `$HUBBRIDGE_APK`;
//! 3. `HubBridge.apk` shipped next to the executable (or in the macOS bundle's `Resources`);
//! 4. in a dev checkout, the APK Gradle built (`android/app/build/outputs/apk/...`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::adb::{self, AdbDevice};

pub const APP_PACKAGE: &str = "com.demoz.hubbridge";
pub const APP_ACTIVITY: &str = "com.demoz.hubbridge/.MainActivity";
/// File name of the APK shipped with desktop releases.
pub const BUNDLED_APK: &str = "HubBridge.apk";

/// Where an APK was found.
#[derive(Debug, Clone)]
pub struct ApkSource {
    pub path: PathBuf,
    pub origin: &'static str,
}

/// Candidate locations, best first (existing or not).
pub fn apk_candidates(explicit: Option<&Path>) -> Vec<ApkSource> {
    let mut out = Vec::new();
    if let Some(p) = explicit {
        out.push(ApkSource {
            path: p.to_path_buf(),
            origin: "--apk",
        });
    }
    if let Some(p) = std::env::var_os("HUBBRIDGE_APK") {
        out.push(ApkSource {
            path: PathBuf::from(p),
            origin: "$HUBBRIDGE_APK",
        });
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(ApkSource {
            path: dir.join(BUNDLED_APK),
            origin: "bundled",
        });
        // macOS: HubBridge.app/Contents/MacOS/hubbridge → Contents/Resources/HubBridge.apk
        out.push(ApkSource {
            path: dir.join("../Resources").join(BUNDLED_APK),
            origin: "bundled",
        });
    }
    // Dev checkout: the APK Gradle built.
    let android = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../android/app/build/outputs/apk");
    out.push(ApkSource {
        path: android.join("release/app-release.apk"),
        origin: "dev build (release)",
    });
    out.push(ApkSource {
        path: android.join("debug/app-debug.apk"),
        origin: "dev build (debug)",
    });
    out
}

/// The first existing APK. An explicit path that does not exist is an error, not skipped.
pub fn find_apk(explicit: Option<&Path>) -> Result<ApkSource> {
    if let Some(p) = explicit
        && !p.is_file()
    {
        bail!("APK not found: {}", p.display());
    }
    // Among dev builds prefer the newest, so a fresh debug build beats an old release one.
    let candidates = apk_candidates(explicit);
    let mut found: Vec<ApkSource> = candidates.into_iter().filter(|c| c.path.is_file()).collect();
    if found.is_empty() {
        bail!(
            "no HubBridge APK found. Build it (`cd android && ./gradlew assembleDebug`), \
             put {BUNDLED_APK} next to the hubbridge executable, or pass --apk <file>"
        );
    }
    if found.iter().all(|c| c.origin.starts_with("dev build")) {
        found.sort_by_key(|c| std::cmp::Reverse(c.path.metadata().and_then(|m| m.modified()).ok()));
    }
    let mut best = found.swap_remove(0);
    if let Ok(p) = best.path.canonicalize() {
        best.path = p;
    }
    Ok(best)
}

/// What happened, for the UI.
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub device: AdbDevice,
    pub apk: ApkSource,
    /// Version installed before, if any.
    pub previous_version: Option<String>,
    pub version: Option<String>,
    /// The old app had to be removed first (different signing key): its pairings are gone.
    pub reinstalled: bool,
    pub launched: bool,
}

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub serial: Option<String>,
    pub apk: Option<PathBuf>,
    /// Open the app after installing.
    pub launch: bool,
    /// If the installed app is signed with another key, uninstall it and install again
    /// (this deletes its data: paired computers, agent id).
    pub replace_incompatible: bool,
}

/// `versionName` of the installed app, if installed.
pub async fn installed_version(serial: &str) -> Option<String> {
    let out = adb::adb(Some(serial), &["shell", "dumpsys", "package", APP_PACKAGE])
        .await
        .ok()?;
    out.lines()
        .find_map(|l| l.trim().strip_prefix("versionName="))
        .map(|v| v.trim().to_string())
}

/// Turn adb's `Failure [CODE: ...]` into advice.
pub fn explain_failure(text: &str) -> Option<&'static str> {
    const HINTS: &[(&str, &str)] = &[
        (
            "INSTALL_FAILED_UPDATE_INCOMPATIBLE",
            "the installed HubBridge is signed with another key (e.g. debug vs release). \
             Use --replace-incompatible to uninstall it first; the phone forgets its paired computers",
        ),
        (
            "INSTALL_FAILED_VERSION_DOWNGRADE",
            "the phone has a newer HubBridge; uninstall it first to go back",
        ),
        (
            "INSTALL_FAILED_USER_RESTRICTED",
            "the phone blocks installs over USB: allow \"Install via USB\" in Developer options \
             (Xiaomi/Oppo/Vivo) and confirm the prompt on the screen",
        ),
        (
            "INSTALL_FAILED_INSUFFICIENT_STORAGE",
            "not enough free space on the phone",
        ),
        (
            "INSTALL_FAILED_OLDER_SDK",
            "the phone's Android is too old: HubBridge needs Android 8.0 (API 26)+",
        ),
        (
            "INSTALL_FAILED_NO_MATCHING_ABIS",
            "this APK has no native library for the phone's CPU; rebuild with its ABI",
        ),
        (
            "INSTALL_FAILED_VERIFICATION_FAILURE",
            "Play Protect or the OEM verifier refused the APK; check the phone screen",
        ),
        ("INSTALL_CANCELED_BY_USER", "the install was cancelled on the phone"),
        (
            "INSTALL_FAILED_ABORTED",
            "the phone asked to confirm the install (Samsung/Xiaomi): accept it on the screen and try again",
        ),
    ];
    HINTS
        .iter()
        .find(|(code, _)| text.contains(code))
        .map(|(_, hint)| *hint)
}

const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

async fn adb_install(serial: &str, apk: &Path) -> Result<()> {
    let apk = apk.to_string_lossy();
    // -r replace, -g grant runtime permissions (notifications on Android 13+).
    // Play Protect may scan the APK for a while: allow 3 minutes.
    match adb::adb_with_timeout(Some(serial), &["install", "-r", "-g", &apk], INSTALL_TIMEOUT).await {
        Ok(out) if out.contains("Success") => Ok(()),
        Ok(out) => bail!("install did not report success: {}", out.trim()),
        Err(e) => {
            let text = format!("{e:#}");
            match explain_failure(&text) {
                Some(hint) => bail!("{text}\n  → {hint}"),
                None => Err(e),
            }
        }
    }
}

/// Install (or update) the app on the phone and optionally open it.
pub async fn install(opts: &InstallOptions) -> Result<InstallReport> {
    let device = adb::pick(opts.serial.as_deref()).await?;
    let apk = find_apk(opts.apk.as_deref())?;
    let previous_version = installed_version(&device.serial).await;

    let mut reinstalled = false;
    if let Err(e) = adb_install(&device.serial, &apk.path).await {
        // A timed-out install may still have finished on the phone.
        if format!("{e:#}").contains("timed out") && installed_version(&device.serial).await.is_some() {
            return finish(opts, device, apk, previous_version, false).await;
        }
        let incompatible = format!("{e:#}").contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE");
        if !(incompatible && opts.replace_incompatible) {
            return Err(e);
        }
        adb::adb(Some(&device.serial), &["uninstall", APP_PACKAGE])
            .await
            .context("uninstall the old app")?;
        adb_install(&device.serial, &apk.path).await?;
        reinstalled = true;
    }

    finish(opts, device, apk, previous_version, reinstalled).await
}

async fn finish(
    opts: &InstallOptions,
    device: AdbDevice,
    apk: ApkSource,
    previous_version: Option<String>,
    reinstalled: bool,
) -> Result<InstallReport> {
    let version = installed_version(&device.serial).await;
    // Keep the bridge service out of Doze / app standby (works over adb without root;
    // OEM battery managers may still need the user's tap).
    let _ = adb::adb(
        Some(&device.serial),
        &[
            "shell",
            "dumpsys",
            "deviceidle",
            "whitelist",
            &format!("+{APP_PACKAGE}"),
        ],
    )
    .await;
    let _ = adb::adb(
        Some(&device.serial),
        &[
            "shell",
            "cmd",
            "appops",
            "set",
            APP_PACKAGE,
            "RUN_ANY_IN_BACKGROUND",
            "allow",
        ],
    )
    .await;
    let launched = opts.launch
        && adb::adb(Some(&device.serial), &["shell", "am", "start", "-n", APP_ACTIVITY])
            .await
            .is_ok();
    Ok(InstallReport {
        device,
        apk,
        previous_version,
        version,
        reinstalled,
        launched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_known_failures() {
        let e = "adb install -r -g x.apk: adb: failed to install x.apk: Failure \
                 [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Existing package signatures do not match]";
        assert!(explain_failure(e).unwrap().contains("--replace-incompatible"));
        assert!(explain_failure("Failure [INSTALL_FAILED_USER_RESTRICTED: Install canceled by user]").is_some());
        assert!(explain_failure("something else").is_none());
    }

    #[test]
    fn explicit_missing_apk_is_an_error() {
        let err = find_apk(Some(Path::new("/nonexistent/HubBridge.apk"))).unwrap_err();
        assert!(err.to_string().contains("APK not found"));
    }
}
