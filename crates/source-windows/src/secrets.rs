//! Current-user protection shared by the workstation and collector.
pub fn protect(bytes: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        crate::dpapi::protect(bytes).map_err(|_| "DPAPI operation failed".into())
    }
    #[cfg(not(windows))]
    {
        let _ = bytes;
        Err("server private key protection requires Windows DPAPI".into())
    }
}

pub fn unprotect(bytes: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        crate::dpapi::unprotect(bytes)
            .map(|value| value.to_vec())
            .map_err(|_| "DPAPI operation failed".into())
    }
    #[cfg(not(windows))]
    {
        let _ = bytes;
        Err("server private key protection requires Windows DPAPI".into())
    }
}
