//! Process liveness, for status files that name the process that wrote them.
#![allow(unsafe_code)]

/// Whether process `pid` is still running. Unknown platforms say true, so callers only drop
/// what is surely stale.
pub fn is_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: the handle is checked before use and closed once.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            ok && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(unix)]
    {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
            || !std::path::Path::new("/proc/self").exists()
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn this_process_is_alive() {
        assert!(super::is_alive(std::process::id()));
    }
}
