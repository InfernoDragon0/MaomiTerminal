//! Child-process checks: busy shells without prompt marks, and the running command's name.

use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System};

/// Whether `pid` has child processes (a command running in a shell without integration).
/// Reads the whole process table, so it is meant for one-off checks such as closing a pane.
pub(crate) fn has_children(pid: u32) -> bool {
    let system = process_table();
    let parent = Pid::from_u32(pid);
    system
        .processes()
        .values()
        .any(|p| p.parent() == Some(parent) && !is_console_host(p))
}

/// Name of the newest program `pid` started, without `.exe`, for the pane header while a
/// command runs. None while the shell runs the command itself (a pwsh cmdlet, a builtin).
/// Reads the whole process table, so call it off the UI thread.
pub(crate) fn newest_child_name(pid: u32) -> Option<String> {
    let system = process_table();
    let parent = Pid::from_u32(pid);
    let newest = system
        .processes()
        .values()
        .filter(|p| p.parent() == Some(parent) && !is_console_host(p))
        .max_by_key(|p| p.start_time())?;
    let name = newest.name().to_string_lossy();
    let name = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(&name);
    Some(name.to_string())
}

fn process_table() -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, ProcessRefreshKind::new());
    system
}

/// conhost / OpenConsole hosts the pseudo console and is not something the user started.
fn is_console_host(p: &Process) -> bool {
    let name = p.name().to_string_lossy().to_ascii_lowercase();
    name.starts_with("conhost") || name.starts_with("openconsole")
}
