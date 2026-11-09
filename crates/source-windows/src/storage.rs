//! Read-only volume capacity, without spawning a process or exposing paths.
use std::{io, path::Path};

pub fn available_bytes(path: &Path) -> io::Result<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                directory: *const u16,
                available: *mut u64,
                total: *mut u64,
                free: *mut u64,
            ) -> i32;
        }
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        wide.push(0);
        let mut available = 0;
        // The UTF-16 directory is NUL terminated; all output pointers are either
        // writable u64 storage or explicitly optional null pointers.
        if unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(available)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}
