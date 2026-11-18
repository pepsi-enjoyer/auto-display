use std::fs;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::thread::sleep;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};

use crate::{log, watch};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "auto-display";
const DETACHED_PROCESS: u32 = 0x8;

/// Copies this executable to %LOCALAPPDATA%\auto-display, starts it at sign-in and starts it now.
pub fn install() -> Result<(), String> {
    watch::stop_running();
    let dir = log::data_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let target = dir.join("auto-display.exe");
    let current = std::env::current_exe().map_err(|e| e.to_string())?;

    let same = fs::canonicalize(&current).ok() == fs::canonicalize(&target).ok();
    if !same {
        let mut last_err = None;
        for _ in 0..10 {
            match fs::copy(&current, &target) {
                Ok(_) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    sleep(Duration::from_millis(300));
                }
            }
        }
        if let Some(e) = last_err {
            return Err(format!("could not copy to {}: {e}", target.display()));
        }
    }

    crate::config::ensure()?;
    let command = format!("\"{}\" watch", target.display());
    let data = watch::wide(&command);
    let rc = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            watch::wide(RUN_KEY).as_ptr(),
            watch::wide(RUN_VALUE).as_ptr(),
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    if rc != 0 {
        return Err(format!("could not add the sign-in entry (error {rc})"));
    }

    start_detached(&command).map_err(|e| format!("could not start {}: {e}", target.display()))?;
    update_user_path(&dir, true)?;

    log::line(&format!("Installed to {} and started. It will run at every sign-in.", target.display()));
    Ok(())
}

fn update_user_path(dir: &Path, add: bool) -> Result<(), String> {
    let dir = dir.to_str().ok_or("installation directory is not valid Unicode")?;
    if add && dir.contains(';') {
        return Err("cannot add the installation directory to PATH: it contains a semicolon".into());
    }

    let key = watch::wide("Environment");
    let name = watch::wide("Path");
    let mut buffer = vec![0u16; 32_768]; // Maximum environment value plus its terminator.
    let mut len = (buffer.len() * 2) as u32;
    let mut kind = REG_SZ;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND, &mut kind, buffer.as_mut_ptr().cast(), &mut len,
        )
    };
    let path = match rc {
        ERROR_SUCCESS => String::from_utf16(&buffer[..len as usize / 2])
            .map_err(|e| format!("could not read the user PATH: {e}"))?,
        ERROR_FILE_NOT_FOUND if !add => return Ok(()),
        ERROR_FILE_NOT_FOUND => {
            kind = REG_SZ;
            String::new()
        }
        _ => return Err(format!("could not read the user PATH (error {rc})")),
    };
    let Some(path) = updated_path(path.trim_end_matches('\0'), dir, add) else {
        return Ok(());
    };
    let data = watch::wide(&path);
    if data.len() > buffer.len() {
        return Err("could not add the installation directory: the user PATH would be too long".into());
    }
    let rc = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), kind, data.as_ptr().cast(), (data.len() * 2) as u32,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!("could not write the user PATH (error {rc})"));
    }

    // Let Explorer pick up the change for new shells without a sign-out.
    let notified = unsafe {
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, key.as_ptr() as isize, SMTO_ABORTIFHUNG, 1000, null_mut())
    };
    if notified == 0 {
        log::line(
            "Updated the user PATH but could not notify all windows. If a new shell still has the old PATH, sign out and back in.",
        );
    }
    Ok(())
}

fn updated_path(path: &str, dir: &str, add: bool) -> Option<String> {
    let entries = path.split(';');
    let matches_dir = |entry: &str| entry.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(dir);
    match (add, entries.clone().any(matches_dir)) {
        (true, false) if path.is_empty() => Some(dir.into()),
        (true, false) => Some(format!("{path};{dir}")),
        (false, true) => Some(entries.filter(|entry| !matches_dir(entry)).collect::<Vec<_>>().join(";")),
        _ => None,
    }
}

/// Starts the watcher without handing it any of our handles. std::process::Command lets the child
/// inherit the caller's console pipes, which makes a shell wait for the watcher to exit.
pub fn start_detached(command_line: &str) -> Result<(), String> {
    let mut cmd = watch::wide(command_line);
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(null(), cmd.as_mut_ptr(), null(), null(), 0, DETACHED_PROCESS, null(), null(), &si, &mut pi)
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    unsafe {
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let was_running = watch::stop_running();
    let rc = unsafe {
        RegDeleteKeyValueW(HKEY_CURRENT_USER, watch::wide(RUN_KEY).as_ptr(), watch::wide(RUN_VALUE).as_ptr())
    };
    // 2 = ERROR_FILE_NOT_FOUND: there was no entry to remove.
    if rc != 0 && rc != 2 {
        return Err(format!("could not remove the sign-in entry (error {rc})"));
    }
    for name in ["auto-display.exe", "auto-display.exe.old", "auto-display.exe.new"] {
        let _ = fs::remove_file(log::data_dir().join(name));
    }
    update_user_path(&log::data_dir(), false)?;
    log::line(&format!(
        "Uninstalled{}. Log kept at {}.",
        if was_running { " and stopped the running copy" } else { "" },
        log::path().display()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::updated_path;

    const DIR: &str = r"C:\User Data\auto-display";

    #[test]
    fn adds_to_path_without_expanding_or_reordering_entries() {
        let before = r"%TOOLS%\bin;C:\Program Files\Tool";
        assert_eq!(updated_path(before, DIR, true), Some(format!("{before};{DIR}")));
        assert_eq!(updated_path("", DIR, true), Some(DIR.into()));
    }

    #[test]
    fn skips_registration_if_the_path_already_contains_the_directory() {
        let before = r"C:\bin;c:\user data\Auto-Display\;C:\tools";
        assert_eq!(updated_path(before, DIR, true), None);
    }

    #[test]
    fn removes_only_the_install_directory_from_path() {
        let before = format!(r"%TOOLS%\bin;{DIR};{DIR}-other;;C:\tools");
        let after = format!(r"%TOOLS%\bin;{DIR}-other;;C:\tools");
        assert_eq!(updated_path(&before, DIR, false), Some(after.clone()));
        assert_eq!(updated_path(&after, DIR, false), None);
    }
}
