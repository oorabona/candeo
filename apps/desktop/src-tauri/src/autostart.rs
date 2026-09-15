//! Launching Candeo at login, hidden in the notification area (#103).
//!
//! A firmware effect survives a reboot; an effect Candeo runs only comes back
//! once Candeo runs again.
//!
//! # The system's entry is the only record
//!
//! The Windows `Run` registry value, or an XDG autostart file. Nothing is kept in
//! `settings.json`: the system's own tools change the same entry (Task Manager's
//! startup apps, a desktop's session settings), and a copy would disagree with
//! them.
//!
//! No `tauri-plugin-autostart`: an entry is one registry value or one short
//! file, and the registry is already read for the diagnostic.
//!
//! # Not from a development build
//!
//! It would register a binary under `target/`, which the next build replaces
//! and `cargo clean` deletes.

use std::path::PathBuf;

use serde::Serialize;

use crate::{CmdResult, Failure};

/// The argument the entry passes: start without a window.
pub const HIDDEN: &str = "--hidden";

/// The name of the entry, in the registry and as a file name.
const NAME: &str = "candeo";

/// True when this process was launched by the entry.
pub fn launched_hidden() -> bool {
    std::env::args().skip(1).any(|arg| arg == HIDDEN)
}

/// Whether this build may write an entry.
fn available() -> bool {
    !cfg!(debug_assertions) && cfg!(any(windows, target_os = "linux"))
}

/// The entry's state, as Settings reads it.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LaunchAtLogin {
    pub enabled: bool,
    /// False in a development build, and on a system Candeo writes no entry for.
    pub available: bool,
}

fn status() -> LaunchAtLogin {
    LaunchAtLogin {
        enabled: platform::enabled(),
        available: available(),
    }
}

#[tauri::command]
pub fn get_launch_at_login() -> LaunchAtLogin {
    status()
}

/// Writes or removes the entry.
#[tauri::command]
pub fn set_launch_at_login(on: bool) -> CmdResult<LaunchAtLogin> {
    if !available() {
        return Err(Failure::unexpected(
            "launch at login is written only by a release build on Windows or Linux",
        ));
    }
    let written = if on {
        executable().and_then(|exe| platform::enable(&exe))
    } else {
        platform::disable()
    };
    written.map_err(|e| {
        tracing::warn!("launch at login not changed: {e}");
        Failure::unexpected(e)
    })?;
    tracing::info!(on, "launch at login changed");
    Ok(status())
}

/// The file the entry starts.
///
/// An AppImage runs from a mount that changes at every launch: the entry must
/// start the image itself, which the runtime names in `APPIMAGE`.
fn executable() -> Result<PathBuf, String> {
    #[cfg(target_os = "linux")]
    if let Some(image) = std::env::var_os("APPIMAGE") {
        return Ok(PathBuf::from(image));
    }
    std::env::current_exe().map_err(|e| format!("executable path not read: {e}"))
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
        RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
    };

    use super::{HIDDEN, NAME};

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    /// Where Task Manager records a startup app turned off, without removing it
    /// from `Run`.
    const APPROVED: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn enabled() -> bool {
        let (run, name) = (wide(RUN), wide(NAME));
        let mut size = 0u32;
        // SAFETY: both strings end in NUL; a null buffer only asks for the size.
        let present = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                run.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        } == 0;
        if !present {
            return false;
        }

        let approved = wide(APPROVED);
        let mut flags = [0u8; 12];
        let mut size = flags.len() as u32;
        // SAFETY: `size` is the buffer's size in bytes; both strings end in NUL.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                approved.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_BINARY,
                std::ptr::null_mut(),
                flags.as_mut_ptr().cast(),
                &mut size,
            )
        };
        status != 0 || super::approved(&flags[..size as usize])
    }

    pub fn enable(exe: &Path) -> Result<(), String> {
        let command = wide(&format!("\"{}\" {HIDDEN}", exe.display()));
        let (run, name) = (wide(RUN), wide(NAME));
        // SAFETY: the data is `command`, NUL included, its size given in bytes.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                run.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                command.as_ptr().cast(),
                (command.len() * 2) as u32,
            )
        };
        if status != 0 {
            return Err(format!("Run value not written: error {status}"));
        }
        // Turned on from Candeo is a decision: an earlier "off" from Task Manager
        // would otherwise keep the new entry from running.
        delete(APPROVED)
    }

    pub fn disable() -> Result<(), String> {
        delete(RUN)?;
        delete(APPROVED)
    }

    fn delete(key: &str) -> Result<(), String> {
        let (key_w, name) = (wide(key), wide(NAME));
        // SAFETY: both strings end in NUL.
        let status =
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key_w.as_ptr(), name.as_ptr()) };
        match status {
            0 | ERROR_FILE_NOT_FOUND => Ok(()),
            _ => Err(format!("value not removed from {key}: error {status}")),
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::path::{Path, PathBuf};

    use super::NAME;

    /// `$XDG_CONFIG_HOME/autostart`, or `~/.config/autostart`.
    fn entry() -> Option<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(config.join("autostart").join(format!("{NAME}.desktop")))
    }

    pub fn enabled() -> bool {
        entry()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .is_some_and(|content| !super::entry_turned_off(&content))
    }

    pub fn enable(exe: &Path) -> Result<(), String> {
        let path = entry().ok_or("no configuration folder: neither XDG_CONFIG_HOME nor HOME")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("autostart folder not created: {e}"))?;
        }
        std::fs::write(&path, super::desktop_entry(&exe.to_string_lossy()))
            .map_err(|e| format!("autostart entry not written: {e}"))
    }

    pub fn disable() -> Result<(), String> {
        let Some(path) = entry() else { return Ok(()) };
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("autostart entry not removed: {e}"))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use std::path::Path;

    pub fn enabled() -> bool {
        false
    }

    pub fn enable(_exe: &Path) -> Result<(), String> {
        Err("launch at login is not written on this system".into())
    }

    pub fn disable() -> Result<(), String> {
        Ok(())
    }
}

/// Whether Task Manager leaves a startup app on: the first byte of its record is
/// even when on (`02`, `06`), odd when turned off (`03`, `07`).
#[cfg_attr(not(windows), allow(dead_code))]
fn approved(record: &[u8]) -> bool {
    !matches!(record.first(), Some(flag) if flag & 1 == 1)
}

/// The XDG autostart entry starting `exe` hidden.
///
/// `Exec` quotes the path, escaping what the specification reserves inside
/// quotes; a backslash is then escaped once more, as any string value is.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn desktop_entry(exe: &str) -> String {
    let mut quoted = String::from('"');
    for c in exe.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    let exec = quoted.replace('\\', "\\\\");
    format!(
        "[Desktop Entry]\nType=Application\nName={NAME}\nExec={exec} {HIDDEN}\nX-GNOME-Autostart-enabled=true\n"
    )
}

/// An entry a desktop's session settings turned off without deleting it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn entry_turned_off(content: &str) -> bool {
    content.lines().any(|line| {
        matches!(
            line.trim(),
            "Hidden=true" | "X-GNOME-Autostart-enabled=false"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_manager_turning_a_startup_app_off_is_read() {
        assert!(approved(&[0x02, 0, 0, 0]));
        assert!(approved(&[0x06, 0, 0, 0]));
        assert!(!approved(&[0x03, 0, 0, 0]));
        assert!(!approved(&[0x07, 0, 0, 0]));
        // No record: nobody turned it off.
        assert!(approved(&[]));
    }

    #[test]
    fn the_autostart_entry_quotes_the_executable() {
        let entry = desktop_entry("/opt/can deo/candeo");
        assert!(
            entry.contains("\nExec=\"/opt/can deo/candeo\" --hidden\n"),
            "{entry}"
        );
        assert!(entry.starts_with("[Desktop Entry]\n"));

        // `$` is escaped inside the quotes, and that backslash once more as a string.
        let entry = desktop_entry("/home/someone/$bin/candeo");
        assert!(
            entry.contains(r#"Exec="/home/someone/\\$bin/candeo" --hidden"#),
            "{entry}"
        );
    }

    #[test]
    fn an_entry_turned_off_by_the_desktop_is_read() {
        assert!(entry_turned_off("[Desktop Entry]\nHidden=true\n"));
        assert!(entry_turned_off(
            "[Desktop Entry]\nX-GNOME-Autostart-enabled=false\n"
        ));
        assert!(!entry_turned_off(&desktop_entry("/usr/bin/candeo")));
    }
}
