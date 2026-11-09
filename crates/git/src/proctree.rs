//! Killing a git command's whole process tree. Cancelling or timing out a command must also
//! stop what git started (ssh, Git Credential Manager, hooks, filters), not just `git.exe`.
//!
//! Windows: a Job Object holding the child (and so everything it spawns afterwards), created
//! with kill-on-close. Unix: the child leads its own process group (see [`prepare`]) and the
//! group gets SIGKILL.
//!
//! A [`ProcessTree`] kills the tree when dropped, which is how cancelling (dropping the future)
//! and timeouts work. A command that ran to its end calls [`ProcessTree::release`] first, so
//! daemons git left behind on purpose (`gc --auto`, the fsmonitor daemon) keep running.

#![allow(
    unsafe_code,
    reason = "Job Object FFI; the Unix side uses rustix's safe API"
)]

use async_process::Child;

/// Makes the child lead its own process group (Unix) so the group can be killed as one.
pub(crate) fn prepare(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// The processes started by one spawned command.
pub(crate) struct ProcessTree {
    inner: imp::Tree,
    armed: bool,
}

impl ProcessTree {
    /// Call right after spawning. If the platform refuses (e.g. the process already exited),
    /// the tree is empty and only [`Child::kill`] applies.
    ///
    /// On Windows a process the child started before this call is not in the job; for a git
    /// that has only just begun that window is a few instructions wide.
    pub fn attach(child: &Child) -> Self {
        Self {
            inner: imp::Tree::attach(child),
            armed: true,
        }
    }

    /// Kills every process of the tree now.
    pub fn kill(&self) {
        self.inner.kill();
    }

    /// The command ended on its own: dropping no longer kills what it left running.
    pub fn release(mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        if self.armed {
            self.inner.kill();
        }
        self.inner.close(self.armed);
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle as _;

    use async_process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    /// A Job Object handle (kept as an integer so the future holding it stays `Send`), or 0.
    pub struct Tree(usize);

    /// Sets the job's limit flags.
    fn set_flags(job: HANDLE, flags: u32) -> bool {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = flags;
        // SAFETY: `info` is a live, correctly sized struct for this information class.
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
        }
    }

    impl Tree {
        pub fn attach(child: &Child) -> Self {
            // SAFETY: plain Win32 calls; the job handle is closed on every failure path and
            // otherwise owned by the returned value. The child's handle outlives the call.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Self(0);
                }
                let ok = set_flags(job, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
                    && AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) != 0;
                if !ok {
                    tracing::debug!("could not put git in a job object; cancel kills git only");
                    CloseHandle(job);
                    return Self(0);
                }
                Self(job as usize)
            }
        }

        pub fn kill(&self) {
            if self.0 != 0 {
                // SAFETY: the handle is a job we own.
                unsafe { TerminateJobObject(self.0 as HANDLE, 1) };
            }
        }

        /// Closes the job. Unless `kill_remaining`, the kill-on-close flag is cleared first so
        /// whatever is still running survives.
        pub fn close(&self, kill_remaining: bool) {
            if self.0 != 0 {
                let job = self.0 as HANDLE;
                if !kill_remaining {
                    set_flags(job, 0);
                }
                // SAFETY: the handle is a job we own and never use again.
                unsafe { CloseHandle(job) };
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use async_process::Child;
    use rustix::process::{Pid, Signal, kill_process_group};

    /// The child's process group (its pid, see [`super::prepare`]).
    pub struct Tree(Option<Pid>);

    impl Tree {
        pub fn attach(child: &Child) -> Self {
            Self(i32::try_from(child.id()).ok().and_then(Pid::from_raw))
        }

        pub fn kill(&self) {
            if let Some(pgid) = self.0 {
                _ = kill_process_group(pgid, Signal::KILL);
            }
        }

        pub fn close(&self, _kill_remaining: bool) {}
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use async_process::Child;

    pub struct Tree;

    impl Tree {
        pub fn attach(_child: &Child) -> Self {
            Self
        }
        pub fn kill(&self) {}
        pub fn close(&self, _kill_remaining: bool) {}
    }
}
