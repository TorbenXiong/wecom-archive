//! Windows process coordination for the portable executable updater.
use std::io;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Command;
#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
#[cfg(windows)]
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

#[cfg(windows)]
pub fn hide(command: &mut Command) -> &mut Command {
    command.creation_flags(0x0800_0000)
}

#[cfg(not(windows))]
pub fn hide(command: &mut Command) -> &mut Command {
    command
}

#[cfg(windows)]
pub fn wait_for_exit(
    pid: u32,
    timeout_ms: u32,
    ready: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, false, pid)
            .map_err(|_| io::Error::other("cannot open update parent"))?;
        if let Err(error) = ready() {
            let _ = CloseHandle(handle);
            return Err(error);
        }
        let status = WaitForSingleObject(handle, timeout_ms);
        let _ = CloseHandle(handle);
        if status != WAIT_OBJECT_0 {
            return Err(io::Error::other("update parent did not exit"));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn wait_for_exit(
    _pid: u32,
    _timeout_ms: u32,
    ready: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    ready()
}
