//! Standalone or installed.
//!
//! The download is one exe that runs from wherever it landed. Started from
//! Downloads, it offers to install itself — a copy in
//! %LOCALAPPDATA%\Programs\NotDiscord, a Start Menu shortcut, an entry in
//! Apps & features — the way AnyDesk does, because a standalone exe collects
//! copies (switchb: "you can end up with multiple copies"). Per-user, so no
//! admin prompt, and the self-updater can write there. Started again from
//! Downloads once installed, it opens the installed copy instead of
//! becoming a second one.
//!
//! `--portable` opts out of all of it; `--uninstall` is what Apps & features
//! runs. Settings and the login live in %APPDATA%\NotDiscord either way, so
//! installing carries them over and uninstalling leaves them.

use std::path::{Path, PathBuf};

const APP_NAME: &str = "NotDiscord";
const EXE_NAME: &str = "NotDiscord.exe";

/// What the command line asked for.
#[derive(Debug, PartialEq)]
pub enum Launch {
    Normal,
    /// Never hand off to an installed copy, never offer to install.
    Portable,
    /// Apps & features: confirm, then remove.
    Uninstall,
    /// The temp helper that deletes the install folder after the installed
    /// exe has exited (a running exe can't delete itself).
    UninstallCleanup(PathBuf),
}

pub fn launch_mode() -> Launch {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--portable") => Launch::Portable,
        Some("--uninstall") => Launch::Uninstall,
        Some("--uninstall-cleanup") => match args.get(1) {
            Some(dir) => Launch::UninstallCleanup(PathBuf::from(dir)),
            None => Launch::Normal,
        },
        _ => Launch::Normal,
    }
}

/// A test seam: with NOTDISCORD_INSTALL_SANDBOX set, everything an install
/// touches (folder, shortcuts, registry) lands under that directory or a
/// test registry key, so a debug build can be installed and uninstalled
/// without touching the real Start Menu.
fn sandbox() -> Option<PathBuf> {
    std::env::var_os("NOTDISCORD_INSTALL_SANDBOX").map(PathBuf::from)
}

pub fn install_dir() -> Option<PathBuf> {
    match sandbox() {
        Some(s) => Some(s.join("Programs").join(APP_NAME)),
        None => dirs::data_local_dir().map(|d| d.join("Programs").join(APP_NAME)),
    }
}

pub fn installed_exe() -> Option<PathBuf> {
    install_dir().map(|d| d.join(EXE_NAME))
}

fn start_menu_shortcut() -> Option<PathBuf> {
    match sandbox() {
        Some(s) => Some(s.join("StartMenu").join(format!("{APP_NAME}.lnk"))),
        None => dirs::data_dir().map(|d| {
            d.join("Microsoft").join("Windows").join("Start Menu").join("Programs").join(format!("{APP_NAME}.lnk"))
        }),
    }
}

fn desktop_shortcut() -> Option<PathBuf> {
    match sandbox() {
        Some(s) => Some(s.join("Desktop").join(format!("{APP_NAME}.lnk"))),
        None => dirs::desktop_dir().map(|d| d.join(format!("{APP_NAME}.lnk"))),
    }
}

fn uninstall_key() -> &'static str {
    if sandbox().is_some() {
        r"Software\NotDiscord\TestUninstall"
    } else {
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\NotDiscord"
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// True when this process is the installed copy.
pub fn running_installed() -> bool {
    match (std::env::current_exe(), installed_exe()) {
        (Ok(me), Some(installed)) => same_file(&me, &installed),
        _ => false,
    }
}

/// "0.95.4" -> (0, 95, 4); anything unparseable sorts lowest.
fn version_tuple(v: &str) -> (u64, u64, u64) {
    let mut parts = v.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// The installed copy's version, from its Apps & features entry — which the
/// installed copy keeps current on every start (see `refresh_registration`).
#[cfg(windows)]
pub fn installed_version() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    let key = winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey(uninstall_key()).ok()?;
    key.get_value::<String, _>("DisplayVersion").ok()
}

#[cfg(not(windows))]
pub fn installed_version() -> Option<String> {
    None
}

/// Whether the offer to install applies to this process at all: Windows,
/// not the installed copy, not `--portable`, not a dev run.
fn eligible() -> bool {
    if !cfg!(windows) || launch_mode() != Launch::Normal || running_installed() {
        return false;
    }
    // A dev profile or a debug build is a test, not a friend's first launch
    // — unless the test is of this very feature.
    if std::env::var_os("NOTDISCORD_INSTALL_PROMPT").is_some() {
        return true;
    }
    !(cfg!(debug_assertions) || std::env::var_os("NOTDISCORD_CONFIG_DIR").is_some())
}

/// Started from somewhere other than the install folder while an installed
/// copy at least as new exists: start that one instead. True means the
/// caller should exit. Must run before the single-instance mutex is taken,
/// or the copy we start would find it held and give up.
pub fn hand_off_to_installed() -> bool {
    if !eligible() {
        return false;
    }
    let Some(installed) = installed_exe().filter(|p| p.is_file()) else {
        return false;
    };
    // A newer download than what's installed is someone updating by hand;
    // it runs, and offers to install over the old one.
    let theirs = installed_version().map(|v| version_tuple(&v)).unwrap_or_default();
    if theirs < version_tuple(env!("CARGO_PKG_VERSION")) {
        return false;
    }
    std::process::Command::new(&installed).spawn().is_ok()
}

/// Whether to ask "Install NotDiscord?" on this launch.
pub fn should_offer() -> bool {
    if !eligible() {
        return false;
    }
    // "Not now" is remembered for this particular file; a fresh download
    // asks again.
    let declined = crate::api::load_settings().install_declined_for;
    let me = std::env::current_exe().ok().map(|p| p.display().to_string());
    declined.is_none() || declined != me
}

pub fn decline() {
    let mut settings = crate::api::load_settings();
    settings.install_declined_for = std::env::current_exe().ok().map(|p| p.display().to_string());
    crate::api::save_settings(&settings);
}

/// True when an installed copy exists and this download is newer than it —
/// the prompt then reads as an update rather than a first install.
pub fn would_update() -> Option<String> {
    let installed = installed_version()?;
    installed_exe().filter(|p| p.is_file())?;
    (version_tuple(&installed) < version_tuple(env!("CARGO_PKG_VERSION"))).then_some(installed)
}

/// The folder this exe is running from, for the prompt's "you're running
/// NotDiscord straight from …".
pub fn running_from() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.display().to_string()))
        .unwrap_or_default()
}

/// Copy this exe into the install folder, add the shortcuts and the Apps &
/// features entry. Returns the installed exe, for the caller to start.
#[cfg(windows)]
pub fn install(with_desktop_shortcut: bool) -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|e| format!("can't find my own exe: {e}"))?;
    let dir = install_dir().ok_or("no local app data folder")?;
    let exe = dir.join(EXE_NAME);
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    // Windows lets a running exe be renamed but not overwritten. Move any
    // old copy aside first; the app sweeps NotDiscord.old-*.exe on start.
    if exe.exists() {
        let aside = dir.join(format!("NotDiscord.old-{}.exe", std::process::id()));
        let _ = std::fs::remove_file(&aside);
        std::fs::rename(&exe, &aside)
            .map_err(|_| "the installed NotDiscord is still running — close it and try again".to_string())?;
    }
    std::fs::copy(&me, &exe).map_err(|e| format!("couldn't copy into {}: {e}", dir.display()))?;

    if let Some(lnk) = start_menu_shortcut() {
        shortcut(&exe, &lnk)?;
    }
    if with_desktop_shortcut {
        if let Some(lnk) = desktop_shortcut() {
            shortcut(&exe, &lnk)?;
        }
    }
    register(&exe)?;
    Ok(exe)
}

#[cfg(not(windows))]
pub fn install(_with_desktop_shortcut: bool) -> Result<PathBuf, String> {
    Err("installing is a Windows thing".into())
}

#[cfg(windows)]
fn shortcut(exe: &Path, lnk: &Path) -> Result<(), String> {
    if let Some(parent) = lnk.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("couldn't create {}: {e}", parent.display()))?;
    }
    let mut link = mslnk::ShellLink::new(exe).map_err(|e| format!("shortcut: {e:?}"))?;
    link.set_name(Some(APP_NAME.into()));
    link.set_icon_location(Some(exe.display().to_string()));
    link.set_working_dir(exe.parent().map(|d| d.display().to_string()));
    link.create_lnk(lnk).map_err(|e| format!("couldn't write {}: {e:?}", lnk.display()))
}

/// The Apps & features entry: name, version, size, where, how to remove.
#[cfg(windows)]
fn register(exe: &Path) -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    let (key, _) = winreg::RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(uninstall_key())
        .map_err(|e| format!("registry: {e}"))?;
    let dir = exe.parent().map(|d| d.display().to_string()).unwrap_or_default();
    let size_kb = std::fs::metadata(exe).map(|m| (m.len() / 1024) as u32).unwrap_or(0);
    let set = |name: &str, value: &str| key.set_value(name, &value).map_err(|e| format!("registry {name}: {e}"));
    set("DisplayName", APP_NAME)?;
    set("DisplayVersion", env!("CARGO_PKG_VERSION"))?;
    set("Publisher", APP_NAME)?;
    set("InstallLocation", &dir)?;
    set("DisplayIcon", &exe.display().to_string())?;
    set("UninstallString", &format!("\"{}\" --uninstall", exe.display()))?;
    set("URLInfoAbout", "https://notdiscord.switchbhost.com")?;
    key.set_value("EstimatedSize", &size_kb).map_err(|e| format!("registry: {e}"))?;
    key.set_value("NoModify", &1u32).map_err(|e| format!("registry: {e}"))?;
    key.set_value("NoRepair", &1u32).map_err(|e| format!("registry: {e}"))?;
    Ok(())
}

/// A self-update replaces the installed exe without going through
/// `install`, so the installed copy re-stamps its entry whenever the
/// version there is stale. Also what `hand_off_to_installed` reads.
pub fn refresh_registration() {
    #[cfg(windows)]
    {
        if !running_installed() {
            return;
        }
        if installed_version().as_deref() != Some(env!("CARGO_PKG_VERSION")) {
            if let Some(exe) = installed_exe() {
                let _ = register(&exe);
            }
        }
    }
}

/// `--uninstall`: ask, then remove the shortcuts and the entry, hand the
/// folder to a helper in %TEMP% (this exe can't delete itself), and exit.
#[cfg(windows)]
pub fn uninstall_interactive() -> ! {
    use winapi::um::winuser::{MessageBoxW, IDYES, MB_ICONQUESTION, MB_YESNO};
    let text: Vec<u16> = "Remove NotDiscord from this computer?\n\nYour login and settings stay in %APPDATA%\\NotDiscord, so reinstalling picks up where you left off.\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "Uninstall NotDiscord\0".encode_utf16().collect();
    // A test can't click a native message box; the sandbox implies yes.
    if sandbox().is_none() {
        let answer = unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONQUESTION) };
        if answer != IDYES {
            std::process::exit(0);
        }
    }
    for lnk in [start_menu_shortcut(), desktop_shortcut()].into_iter().flatten() {
        let _ = std::fs::remove_file(lnk);
    }
    {
        use winreg::enums::HKEY_CURRENT_USER;
        let _ = winreg::RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(uninstall_key());
    }
    if let (Some(dir), Ok(me)) = (install_dir(), std::env::current_exe()) {
        // The helper is left behind in %TEMP%; Windows cleans that up.
        let helper = std::env::temp_dir().join(format!("NotDiscord-uninstall-{}.exe", std::process::id()));
        if std::fs::copy(&me, &helper).is_ok() {
            let _ = std::process::Command::new(&helper).arg("--uninstall-cleanup").arg(&dir).spawn();
        }
    }
    std::process::exit(0);
}

#[cfg(not(windows))]
pub fn uninstall_interactive() -> ! {
    std::process::exit(0);
}

/// The helper: wait for the installed exe to let go, then delete the folder.
pub fn uninstall_cleanup(dir: &Path) -> ! {
    // Only ever the install folder — never whatever a stray argument names.
    if install_dir().as_deref() == Some(dir) {
        for _ in 0..40 {
            if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(version_tuple("0.95.10") > version_tuple("0.95.9"));
        assert!(version_tuple("1.0.0") > version_tuple("0.99.99"));
        assert_eq!(version_tuple("v0.95.4"), (0, 95, 4));
        assert_eq!(version_tuple("garbage"), (0, 0, 0));
    }
}
