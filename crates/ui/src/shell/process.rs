//! Child-process checks for shells without prompt marks.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Whether `pid` has child processes (a command running in a shell without integration).
/// Reads the whole process table, so it is meant for one-off checks such as closing a pane.
pub(crate) fn has_children(pid: u32) -> bool {
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, ProcessRefreshKind::new());
    let parent = Pid::from_u32(pid);
    // conhost / OpenConsole hosts the pseudo console and is not something the user started.
    system.processes().values().any(|p| {
        p.parent() == Some(parent) && {
            let name = p.name().to_string_lossy().to_ascii_lowercase();
            !name.starts_with("conhost") && !name.starts_with("openconsole")
        }
    })
}
