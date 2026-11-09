//! DLL search-path hardening for the Windows executable (`maomiterminal`).
//!
//! By default `LoadLibrary("name.dll")` also looks in the application directory and the current
//! directory, so a DLL planted next to the exe (or in a folder it was launched from) would run
//! with our rights. Neither exe ships or loads a DLL of its own: everything GPUI and the CLI need
//! (D3D, DirectComposition, DirectWrite, ...) is a system DLL. So restrict the search to System32.
//!
//! This covers DLLs loaded after the call (including delay-loads). Static imports are resolved by
//! the loader before `main` and are not affected; the linker's `/DEPENDENTLOADFLAG` would cover
//! those if ever needed.

#![allow(
    unsafe_code,
    reason = "one call into kernel32 (SetDefaultDllDirectories)"
)]

/// Call first thing in `main`, before anything can load a library. No-op off Windows.
/// Failure (very old Windows without the API) is ignored: the process just keeps the default order.
pub fn restrict_to_system32() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::LibraryLoader::{
            LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
        };
        // SAFETY: a plain kernel32 call taking one flags argument, no pointers.
        let _ = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) };
    }
}
