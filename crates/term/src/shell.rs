//! Finding the shells a session can run.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Shell family; decides the default arguments and how shell integration is injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// PowerShell 7+ (`pwsh`).
    Pwsh,
    /// Windows PowerShell 5.1 (`powershell.exe`).
    WindowsPowerShell,
    Cmd,
    Bash,
    Zsh,
    Fish,
    /// `sh`, `dash` and other POSIX shells without integration support.
    Sh,
    Other,
}

impl ShellKind {
    /// Guesses the family from the program's file name.
    pub fn from_program(program: &Path) -> Self {
        let stem = program
            .file_stem()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match stem.as_str() {
            "pwsh" | "pwsh-preview" => Self::Pwsh,
            "powershell" => Self::WindowsPowerShell,
            "cmd" => Self::Cmd,
            "bash" => Self::Bash,
            "zsh" => Self::Zsh,
            "fish" => Self::Fish,
            "sh" | "dash" | "ash" | "ksh" | "mksh" => Self::Sh,
            _ => Self::Other,
        }
    }

    pub fn is_powershell(self) -> bool {
        matches!(self, Self::Pwsh | Self::WindowsPowerShell)
    }
}

/// A shell program plus the arguments it starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub kind: ShellKind,
}

impl Shell {
    /// A shell with the default arguments for its family.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        let program = program.into();
        let kind = ShellKind::from_program(&program);
        let args = default_args(kind).iter().map(OsString::from).collect();
        Self {
            program,
            args,
            kind,
        }
    }

    /// A shell with exactly these arguments (from the user's config).
    pub fn with_args(program: impl Into<PathBuf>, args: Vec<OsString>) -> Self {
        let program = program.into();
        let kind = ShellKind::from_program(&program);
        Self {
            program,
            args,
            kind,
        }
    }

    /// Short name for tab titles and the settings list.
    pub fn display_name(&self) -> String {
        match self.kind {
            ShellKind::Pwsh => "PowerShell".into(),
            ShellKind::WindowsPowerShell => "Windows PowerShell".into(),
            ShellKind::Cmd => "Command Prompt".into(),
            _ => self
                .program
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.program.display().to_string()),
        }
    }
}

fn default_args(kind: ShellKind) -> &'static [&'static str] {
    match kind {
        ShellKind::Pwsh | ShellKind::WindowsPowerShell => &["-NoLogo"],
        // macOS terminals start login shells so /etc/zprofile (path_helper) runs; Linux ones do not.
        ShellKind::Bash | ShellKind::Zsh | ShellKind::Fish | ShellKind::Sh
            if cfg!(target_os = "macos") =>
        {
            &["-l"]
        }
        _ => &[],
    }
}

/// The shell new tabs open when the config names none.
///
/// Windows: pwsh, then Windows PowerShell, then cmd. Unix: `$SHELL`, then `/bin/sh`.
pub fn default_shell() -> Shell {
    available_shells()
        .into_iter()
        .next()
        .unwrap_or_else(|| Shell::new(fallback_program()))
}

/// Every shell found on this machine, best first.
pub fn available_shells() -> Vec<Shell> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !found.iter().any(|p| same_file(p, &path)) {
            found.push(path);
        }
    };

    if cfg!(windows) {
        for name in ["pwsh", "powershell"] {
            if let Ok(path) = which::which(name) {
                push(path);
            }
        }
        if let Some(comspec) = std::env::var_os("COMSPEC").map(PathBuf::from)
            && comspec.is_file()
        {
            push(comspec);
        } else if let Ok(path) = which::which("cmd") {
            push(path);
        }
    } else {
        if let Some(shell) = std::env::var_os("SHELL").map(PathBuf::from)
            && shell.is_file()
        {
            push(shell);
        }
        for path in etc_shells() {
            push(path);
        }
        let sh = PathBuf::from("/bin/sh");
        if sh.is_file() {
            push(sh);
        }
    }

    found.into_iter().map(Shell::new).collect()
}

fn fallback_program() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("cmd.exe")
    } else {
        PathBuf::from("/bin/sh")
    }
}

/// Interactive shells listed in /etc/shells that exist; nologin entries and duplicates under
/// /usr/bin vs /bin are dropped by the caller's same-file check.
fn etc_shells() -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string("/etc/shells") else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter(|line| !line.ends_with("nologin") && !line.ends_with("/false"))
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .collect()
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_from_program_name() {
        assert_eq!(
            ShellKind::from_program(Path::new(r"C:\x\pwsh.exe")),
            ShellKind::Pwsh
        );
        assert_eq!(
            ShellKind::from_program(Path::new("POWERSHELL.EXE")),
            ShellKind::WindowsPowerShell
        );
        assert_eq!(
            ShellKind::from_program(Path::new("/usr/bin/zsh")),
            ShellKind::Zsh
        );
        assert_eq!(
            ShellKind::from_program(Path::new("/bin/dash")),
            ShellKind::Sh
        );
        assert_eq!(ShellKind::from_program(Path::new("nu")), ShellKind::Other);
    }

    #[test]
    fn default_shell_exists() {
        let shell = default_shell();
        assert!(
            shell.program.is_file() || which::which(&shell.program).is_ok(),
            "{shell:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefers_powershell_over_cmd() {
        let shells = available_shells();
        let cmd = shells.iter().position(|s| s.kind == ShellKind::Cmd);
        let ps = shells.iter().position(|s| s.kind.is_powershell());
        if let (Some(cmd), Some(ps)) = (cmd, ps) {
            assert!(ps < cmd);
        }
    }
}
