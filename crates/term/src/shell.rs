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
    /// A WSL distro through `wsl.exe`; its shell runs inside Linux, so nothing is injected.
    Wsl,
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
            "wsl" => Self::Wsl,
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
            ShellKind::Wsl => match self.wsl_distro() {
                Some(distro) => format!("{distro} (WSL)"),
                None => "WSL".into(),
            },
            ShellKind::Bash if is_git_bash(&self.program) => "Git Bash".into(),
            _ => self
                .program
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.program.display().to_string()),
        }
    }

    /// The distro named by `-d` / `--distribution` in a WSL shell's arguments.
    fn wsl_distro(&self) -> Option<String> {
        let mut args = self.args.iter();
        while let Some(arg) = args.next() {
            if arg == "-d" || arg == "--distribution" {
                return args.next().map(|d| d.to_string_lossy().into_owned());
            }
        }
        None
    }
}

/// Git for Windows' bash lives in its install folder (`…\Git\bin\bash.exe`); the
/// `System32\bash.exe` launcher is WSL's.
fn is_git_bash(program: &Path) -> bool {
    program
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("git"))
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
    native_shells()
        .into_iter()
        .next()
        .unwrap_or_else(|| Shell::new(fallback_program()))
}

/// Every shell found on this machine, best first. On Windows that ends with Git Bash and the
/// WSL distros; the distro list is read once per run (it starts `wsl.exe`), so call this off
/// the UI thread first, e.g. at start.
pub fn available_shells() -> Vec<Shell> {
    let mut shells = native_shells();
    if cfg!(windows) {
        shells.extend(git_bash());
        shells.extend(wsl_distros().iter().cloned());
    }
    shells
}

/// The system's own shells: quick to find, so the default shell never waits on WSL.
fn native_shells() -> Vec<Shell> {
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

/// Git for Windows' bash, found next to `git` on the PATH or in the usual install folders.
/// Starts as a login shell, as Git Bash's own shortcut does, so `/etc/profile` sets up PATH.
fn git_bash() -> Option<Shell> {
    let from_git = which::which("git").ok().and_then(|git| {
        // `Git\cmd\git.exe` or `Git\mingw64\bin\git.exe`.
        git.ancestors()
            .skip(1)
            .take(3)
            .map(|dir| dir.join("bin").join("bash.exe"))
            .find(|bash| bash.is_file())
    });
    let installed = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .flat_map(|root| {
            [
                root.join("Git").join("bin").join("bash.exe"),
                root.join("Programs")
                    .join("Git")
                    .join("bin")
                    .join("bash.exe"),
            ]
        })
        .find(|bash| bash.is_file());
    let bash = from_git.or(installed)?;
    Some(Shell::with_args(bash, vec!["--login".into(), "-i".into()]))
}

/// One shell per installed WSL distro (`wsl -l -q`), read once per run. Docker Desktop's
/// internal distros are left out.
fn wsl_distros() -> &'static [Shell] {
    static DISTROS: std::sync::OnceLock<Vec<Shell>> = std::sync::OnceLock::new();
    DISTROS.get_or_init(|| {
        let Some(wsl) = std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("wsl.exe"))
            .filter(|wsl| wsl.is_file())
        else {
            return Vec::new();
        };
        list_wsl_distros(&wsl)
            .into_iter()
            .filter(|name| !name.starts_with("docker-desktop"))
            .map(|name| Shell::with_args(&wsl, vec!["-d".into(), name.into()]))
            .collect()
    })
}

/// Distro names from `wsl -l -q`, which prints UTF-16LE. Gives up after a few seconds, so a
/// stuck WSL service cannot hold the caller.
fn list_wsl_distros(wsl: &Path) -> Vec<String> {
    let mut command = std::process::Command::new(wsl);
    command
        .args(["-l", "-q"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW: no console flashes up for it.
        command.creation_flags(0x0800_0000);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(command.output());
    });
    let Ok(Ok(output)) = rx.recv_timeout(std::time::Duration::from_secs(5)) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_wsl_list(&output.stdout)
}

fn parse_wsl_list(stdout: &[u8]) -> Vec<String> {
    let units: Vec<u16> = stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
        .lines()
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}' || c == '\0'))
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
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
            // join() uses the host separator; a literal `C:\x\...` is one file name on Unix.
            ShellKind::from_program(&Path::new("x").join("pwsh.exe")),
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
        assert_eq!(
            ShellKind::from_program(Path::new("wsl.exe")),
            ShellKind::Wsl
        );
    }

    #[test]
    fn names_for_git_bash_and_wsl() {
        let git = Path::new("C:")
            .join("Program Files")
            .join("Git")
            .join("bin")
            .join("bash.exe");
        assert_eq!(Shell::new(git).display_name(), "Git Bash");
        assert_eq!(Shell::new("/bin/bash").display_name(), "bash");
        let wsl = Shell::with_args("wsl.exe", vec!["-d".into(), "Ubuntu".into()]);
        assert_eq!(wsl.display_name(), "Ubuntu (WSL)");
        assert_eq!(Shell::new("wsl.exe").display_name(), "WSL");
    }

    #[test]
    fn wsl_list_is_utf16() {
        let text = "\u{feff}Ubuntu\r\ndocker-desktop\r\n\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(parse_wsl_list(&bytes), ["Ubuntu", "docker-desktop"]);
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
