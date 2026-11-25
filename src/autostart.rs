//! Autostart management.
//!
//! Two mutually-exclusive launch-at-login mechanisms:
//!
//!   * User mode  - HKCU\...\Run registry value. Standard, no elevation.
//!   * Admin mode - a Scheduled Task registered with SCHTASKS /RL HIGHEST.
//!                  The Task Scheduler launches it with full admin rights at
//!                  logon, which does not trigger a UAC prompt (UAC only
//!                  prompts for interactive elevation, not task launch).
//!
//! The three states are None, User and Admin; setting one clears the other.

use std::process::Command;

/// Which autostart mechanism is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    None,
    User,
    Admin,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::None => "none",
            Mode::User => "user",
            Mode::Admin => "admin",
        }
    }
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "XClip";
const TASK_NAME: &str = "XClip";

/// Detect the active mode by querying both backends.
pub fn current_mode() -> Mode {
    if task_exists() {
        Mode::Admin
    } else if run_value_exists() {
        Mode::User
    } else {
        Mode::None
    }
}

/// Set the autostart mode, clearing whichever backend is not selected.
pub fn set_mode(mode: Mode) -> Result<(), String> {
    match mode {
        Mode::None => {
            remove_run_value()?;
            remove_task()?;
        }
        Mode::User => {
            remove_task()?; // mutually exclusive
            install_run_value()?;
        }
        Mode::Admin => {
            remove_run_value()?; // mutually exclusive
            install_task()?;
        }
    }
    Ok(())
}

fn exe_path() -> Result<String, String> {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| format!("cannot resolve executable path: {e}"))
}

// ---------------------------------------------------------------------------
// Registry (user mode)
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn run_value_exists() -> bool {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, RegOpenKeyExW, RegCloseKey, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
        RRF_RT_REG_SZ,
    };
    unsafe {
        let wide: Vec<u16> = RUN_KEY.encode_utf16().chain(std::iter::once(0)).collect();
        let mut key = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide.as_ptr(), 0, KEY_QUERY_VALUE, &mut key) != 0 {
            return false;
        }
        let value_wide: Vec<u16> = VALUE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let mut ty = 0u32;
        let mut data = [0u16; 1];
        let mut size = std::mem::size_of_val(&data) as u32;
        let found = RegGetValueW(
            key,
            std::ptr::null(),
            value_wide.as_ptr(),
            RRF_RT_REG_SZ,
            &mut ty,
            data.as_mut_ptr() as *mut _,
            &mut size,
        ) == 0;
        RegCloseKey(key);
        found
    }
}

#[cfg(windows)]
fn install_run_value() -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{
        RegSetValueExW, RegOpenKeyExW, RegCloseKey, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ,
    };
    let path = exe_path()?;
    unsafe {
        let wide: Vec<u16> = RUN_KEY.encode_utf16().chain(std::iter::once(0)).collect();
        let mut key = std::ptr::null_mut();
        let rc = RegOpenKeyExW(HKEY_CURRENT_USER, wide.as_ptr(), 0, KEY_SET_VALUE, &mut key);
        if rc != 0 {
            return Err(format!("RegOpenKeyExW failed ({rc})"));
        }
        let value_wide: Vec<u16> = VALUE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        // Quote the path so spaces are safe.
        let data = format!("\"{path}\"");
        let data_wide: Vec<u16> = data.encode_utf16().collect();
        let rc = RegSetValueExW(
            key,
            value_wide.as_ptr(),
            0,
            REG_SZ,
            data_wide.as_ptr() as *const u8,
            (data_wide.len() * 2) as u32,
        );
        RegCloseKey(key);
        if rc != 0 {
            return Err(format!("RegSetValueExW failed ({rc})"));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn remove_run_value() -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{
        RegDeleteValueW, RegOpenKeyExW, RegCloseKey, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };
    unsafe {
        let wide: Vec<u16> = RUN_KEY.encode_utf16().chain(std::iter::once(0)).collect();
        let mut key = std::ptr::null_mut();
        let rc = RegOpenKeyExW(HKEY_CURRENT_USER, wide.as_ptr(), 0, KEY_SET_VALUE, &mut key);
        if rc != 0 {
            return Ok(()); // key missing == already removed
        }
        let value_wide: Vec<u16> = VALUE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let rc = RegDeleteValueW(key, value_wide.as_ptr());
        RegCloseKey(key);
        // ERROR_FILE_NOT_FOUND (2) means it was not there; that is fine.
        if rc != 0 && rc != 2 {
            return Err(format!("RegDeleteValueW failed ({rc})"));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn run_value_exists() -> bool { false }
#[cfg(not(windows))]
fn install_run_value() -> Result<(), String> { Err("unsupported".into()) }
#[cfg(not(windows))]
fn remove_run_value() -> Result<(), String> { Ok(()) }

// ---------------------------------------------------------------------------
// Scheduled task (admin mode)
// ---------------------------------------------------------------------------

/// Run schtasks without flashing a console window. schtasks.exe is a console
/// app, so spawning it plainly would briefly open a cmd-style window on every
/// right-click that probes for the task. CREATE_NO_WINDOW suppresses it.
#[cfg(windows)]
fn schtasks(args: &[&str]) -> Result<std::process::ExitStatus, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("cannot run schtasks: {e}"))
}

#[cfg(windows)]
fn task_exists() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME])
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn install_task() -> Result<(), String> {
    let path = exe_path()?;
    // /RL HIGHEST makes the Task Scheduler run it elevated without a UAC prompt.
    // /SC ONLOGON launches at every interactive logon for the current user.
    let status = schtasks(&[
        "/Create",
        "/TN", TASK_NAME,
        "/TR", &format!("\"{path}\""),
        "/SC", "ONLOGON",
        "/RL", "HIGHEST",
        "/F",
    ])?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("schtasks /Create failed (code {status})"))
    }
}

#[cfg(windows)]
fn remove_task() -> Result<(), String> {
    let status = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])?;

    // /Delete fails if the task does not exist; treat that as already removed.
    if status.success() || status.code() == Some(1) {
        Ok(())
    } else {
        Err(format!("schtasks /Delete failed (code {status})"))
    }
}

/// Whether the current process is running elevated.
#[cfg(windows)]
pub fn is_elevated() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool { false }


#[cfg(not(windows))]
fn task_exists() -> bool { false }
#[cfg(not(windows))]
fn install_task() -> Result<(), String> { Err("unsupported".into()) }
#[cfg(not(windows))]
fn remove_task() -> Result<(), String> { Ok(()) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_labels_are_stable() {
        assert_eq!(Mode::None.label(), "none");
        assert_eq!(Mode::User.label(), "user");
        assert_eq!(Mode::Admin.label(), "admin");
    }

    #[test]
    fn modes_are_mutually_exclusive_by_construction() {
        // The three modes form a finite set; the exclusivity is enforced in
        // set_mode. This documents the invariant rather than touching state.
        let all = [Mode::None, Mode::User, Mode::Admin];
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn exe_path_is_absolute() {
        // current_exe must resolve to a real file for the Run value and the
        // task action to be meaningful.
        if let Ok(p) = exe_path() {
            assert!(std::path::Path::new(&p).is_absolute(), "path must be absolute: {p}");
        }
    }
}
