use std::fs;
use std::ptr::null;
use std::thread::sleep;
use std::time::Duration;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
use windows_sys::Win32::System::Threading::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};

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

    log::line(&format!("Installed to {} and started. It will run at every sign-in.", target.display()));
    Ok(())
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
    log::line(&format!(
        "Uninstalled{}. Log kept at {}.",
        if was_running { " and stopped the running copy" } else { "" },
        log::path().display()
    ));
    Ok(())
}
