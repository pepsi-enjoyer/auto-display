//! Restarting Explorer. Windows does not fully redraw the taskbar when the scaling changes while
//! signed in: it keeps its old size until Explorer starts again.

use std::path::PathBuf;
use std::ptr::null;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId};

use crate::{install, watch};

/// The process that owns the taskbar, if there is one.
fn taskbar_owner() -> Option<u32> {
    let class = watch::wide("Shell_TrayWnd");
    let hwnd = unsafe { FindWindowW(class.as_ptr(), null()) };
    if hwnd.is_null() {
        return None;
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    (pid != 0).then_some(pid)
}

fn wait_for_taskbar(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if taskbar_owner().is_some() {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    false
}

/// Stops the Explorer that draws the taskbar and makes sure a new one takes its place. Open File
/// Explorer windows close with it. Does nothing if there is no taskbar.
pub fn restart_explorer() -> Result<(), String> {
    let Some(pid) = taskbar_owner() else { return Ok(()) };
    let process = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return Err(format!("could not open Explorer: {}", std::io::Error::last_os_error()));
    }
    let stopped = unsafe { TerminateProcess(process, 1) } != 0;
    let error = std::io::Error::last_os_error();
    unsafe {
        WaitForSingleObject(process, 5000);
        CloseHandle(process);
    }
    if !stopped {
        return Err(format!("could not stop Explorer: {error}"));
    }
    // Windows usually starts a new one by itself. Starting a second would only open a folder window,
    // so give it a moment first.
    if wait_for_taskbar(Duration::from_secs(3)) {
        return Ok(());
    }
    let windir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    install::start_detached(&format!("\"{}\"", windir.join("explorer.exe").display()))
        .map_err(|e| format!("could not start Explorer: {e}"))?;
    if wait_for_taskbar(Duration::from_secs(15)) { Ok(()) } else { Err("Explorer did not bring the taskbar back".into()) }
}
