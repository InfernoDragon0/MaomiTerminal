//! Keeps terminals responsive while other programs load every core (a full `cargo build`):
//! the PTY threads, the shell and its console host run one step above normal priority.
//! Programs started from the shell still get normal priority, since Windows does not pass
//! above-normal on to child processes.
#![allow(unsafe_code)]

/// Raises the calling thread (the PTY reader and writer).
pub(crate) fn raise_current_thread() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        // SAFETY: the pseudo handle of the calling thread is always valid and needs no close.
        if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) } == 0 {
            tracing::debug!("could not raise the PTY thread priority");
        }
    }
}

/// Raises the shell `pid` and the console hosts (ConPTY) that carry its input and output.
/// Walks the process list, so call it off the UI thread.
pub(crate) fn raise_shell(pid: u32) {
    #[cfg(windows)]
    {
        // A busy machine would otherwise keep this thread waiting before the raise happens.
        raise_current_thread();
        raise_process(pid);
        for host in console_hosts(pid) {
            raise_process(host);
        }
    }
    #[cfg(not(windows))]
    let _ = pid;
}

#[cfg(windows)]
fn raise_process(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_SET_INFORMATION, SetPriorityClass,
    };
    // SAFETY: the handle is checked before use and closed once.
    unsafe {
        let handle = OpenProcess(PROCESS_SET_INFORMATION, 0, pid);
        if handle.is_null() {
            return;
        }
        if SetPriorityClass(handle, ABOVE_NORMAL_PRIORITY_CLASS) == 0 {
            tracing::debug!(pid, "could not raise the process priority");
        }
        CloseHandle(handle);
    }
}

/// conhost / OpenConsole processes started by this app (ConPTY) or by the shell. Uses a
/// Toolhelp snapshot: one cheap walk of the list, where a full sysinfo refresh takes seconds
/// while the CPU is busy.
#[cfg(windows)]
fn console_hosts(shell: u32) -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    let parents = [std::process::id(), shell];
    let mut hosts = Vec::new();
    // SAFETY: the snapshot handle is checked and closed once; the entry is plain data whose
    // size is set as the API requires.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return hosts;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            if parents.contains(&entry.th32ParentProcessID) && is_console_host(&entry.szExeFile) {
                hosts.push(entry.th32ProcessID);
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    hosts
}

/// `exe` is a NUL-terminated UTF-16 file name.
#[cfg(windows)]
fn is_console_host(exe: &[u16]) -> bool {
    let len = exe.iter().position(|&c| c == 0).unwrap_or(exe.len());
    let name = String::from_utf16_lossy(&exe[..len]).to_ascii_lowercase();
    name.starts_with("conhost") || name.starts_with("openconsole")
}

#[cfg(all(test, windows))]
mod tests {
    use std::time::{Duration, Instant};

    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, GetPriorityClass, NORMAL_PRIORITY_CLASS, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    use crate::{PtySession, SessionOptions, Shell};

    fn priority_class(pid: u32) -> u32 {
        // SAFETY: the handle is checked before use and closed once.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            assert!(!handle.is_null(), "cannot open process {pid}");
            let class = GetPriorityClass(handle);
            CloseHandle(handle);
            class
        }
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn child_named(parent: u32, name: &str) -> Option<u32> {
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessesToUpdate::All, ProcessRefreshKind::new());
        let parent = Pid::from_u32(parent);
        system
            .processes()
            .values()
            .find(|p| {
                p.parent() == Some(parent) && p.name().to_string_lossy().eq_ignore_ascii_case(name)
            })
            .map(|p| p.pid().as_u32())
    }

    #[test]
    fn shell_and_console_host_run_above_normal_but_programs_do_not() {
        let session = PtySession::spawn(SessionOptions {
            shell: Some(Shell::with_args("cmd.exe", vec!["/Q".into()])),
            shell_integration: false,
            ..SessionOptions::default()
        })
        .expect("spawn shell");
        let shell = session.pid().expect("shell pid");
        wait_until("the shell to be raised", || {
            priority_class(shell) == ABOVE_NORMAL_PRIORITY_CLASS
        });
        // The hosts are raised right after the shell.
        wait_until("the console host to be raised", || {
            let hosts = super::console_hosts(shell);
            !hosts.is_empty()
                && hosts
                    .iter()
                    .all(|&h| priority_class(h) == ABOVE_NORMAL_PRIORITY_CLASS)
        });

        session.write("ping -n 30 127.0.0.1\r");
        let mut ping = None;
        wait_until("ping to start", || {
            ping = child_named(shell, "ping.exe");
            ping.is_some()
        });
        assert_eq!(priority_class(ping.unwrap()), NORMAL_PRIORITY_CLASS);
        session.kill();
    }
}
