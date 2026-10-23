//! Shell integration: makes shells report their cwd (OSC 7 / 9;9) and prompt state (OSC 133).
//!
//! The user's own startup files still run; the snippets wrap the prompt after them.
//! pwsh gets the snippet through `-EncodedCommand`; bash and zsh through startup files written
//! to a temp folder (`--rcfile` / `ZDOTDIR`); cmd through the `PROMPT` variable.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use portable_pty::CommandBuilder;

use crate::shell::{Shell, ShellKind};

/// Wraps `prompt` and the Enter key. Uses `[char]27` because Windows PowerShell 5.1 has no `` `e ``.
const PWSH: &str = r#"
if ($null -eq $global:__MaomiPrompt) {
  $global:__MaomiPrompt = $function:prompt
  $global:__MaomiHistoryId = -1
  $global:__MaomiStarted = $false
  function global:prompt {
    $ok = $?
    $code = $global:LASTEXITCODE
    $e = [char]27; $a = [char]7; $out = ''
    if ($global:__MaomiStarted) {
      $last = Get-History -Count 1
      if ($null -ne $last -and $last.Id -ne $global:__MaomiHistoryId) {
        $global:__MaomiHistoryId = $last.Id
        $exit = if ($ok) { 0 } elseif ($code) { $code } else { 1 }
        $out += "$e]133;D;$exit$a"
      } else { $out += "$e]133;D$a" }
    }
    $global:__MaomiStarted = $true
    $loc = $executionContext.SessionState.Path.CurrentLocation
    if ($loc.Provider.Name -eq 'FileSystem') { $out += "$e]9;9;`"$($loc.ProviderPath)`"$a" }
    $out += "$e]133;A$a"
    $out += & $global:__MaomiPrompt
    $out += "$e]133;B$a"
    $global:LASTEXITCODE = $code
    return $out
  }
  if (Get-Module PSReadLine) {
    Set-PSReadLineKeyHandler -Chord Enter -ScriptBlock {
      [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
      [Console]::Write("$([char]27)]133;C$([char]7)")
    }
  }
}
"#;

/// Sourced through `--rcfile`, so it loads the user's files itself.
const BASH_RC: &str = r#"
if [ -n "$MAOMI_LOGIN_SHELL" ]; then
  unset MAOMI_LOGIN_SHELL
  [ -r /etc/profile ] && . /etc/profile
  for __maomi_f in ~/.bash_profile ~/.bash_login ~/.profile; do
    [ -r "$__maomi_f" ] && { . "$__maomi_f"; break; }
  done
  unset __maomi_f
else
  [ -r ~/.bashrc ] && . ~/.bashrc
fi

if [ -z "$__maomi_loaded" ]; then
  __maomi_loaded=1
  __maomi_started=
  __maomi_user_pc="$PROMPT_COMMAND"
  __maomi_precmd() {
    local ret=$?
    [ -n "$__maomi_started" ] && printf '\e]133;D;%s\a' "$ret"
    __maomi_started=1
    printf '\e]7;file://%s%s\a' "${HOSTNAME:-localhost}" "${PWD//\%/%25}"
    if [ -n "$__maomi_user_pc" ]; then
      (exit $ret)
      eval "$__maomi_user_pc"
    fi
    case "$PS1" in
      *'133;B'*) ;;
      *) PS1='\[\e]133;A\a\]'"$PS1"'\[\e]133;B\a\]' ;;
    esac
    return $ret
  }
  PROMPT_COMMAND=__maomi_precmd
  # PS0 is printed after a command is read and before it runs (bash 4.4+).
  PS0="${PS0}"'\e]133;C\a'
fi
"#;

/// zsh reads these from `ZDOTDIR`; each sources the user's file of the same name.
const ZSH_ENV: &str = r#"
__maomi_zdotdir="$ZDOTDIR"
ZDOTDIR="${MAOMI_USER_ZDOTDIR:-$HOME}"
[ -r "$ZDOTDIR/.zshenv" ] && . "$ZDOTDIR/.zshenv"
MAOMI_USER_ZDOTDIR="$ZDOTDIR"
ZDOTDIR="$__maomi_zdotdir"
"#;

const ZSH_PROFILE: &str = r#"
__maomi_zdotdir="$ZDOTDIR"
ZDOTDIR="$MAOMI_USER_ZDOTDIR"
[ -r "$ZDOTDIR/.zprofile" ] && . "$ZDOTDIR/.zprofile"
ZDOTDIR="$__maomi_zdotdir"
"#;

/// Last of our files: restores the user's `ZDOTDIR`, so `.zlogin` and later reads use theirs.
const ZSH_RC: &str = r#"
ZDOTDIR="$MAOMI_USER_ZDOTDIR"
unset MAOMI_USER_ZDOTDIR __maomi_zdotdir
[ -r "$ZDOTDIR/.zshrc" ] && . "$ZDOTDIR/.zshrc"
[ "$ZDOTDIR" = "$HOME" ] && unset ZDOTDIR

__maomi_started=
__maomi_precmd() {
  local ret=$?
  [ -n "$__maomi_started" ] && printf '\e]133;D;%s\a' "$ret"
  __maomi_started=1
  printf '\e]7;file://%s%s\a' "${HOST:-localhost}" "${PWD//\%/%25}"
  [[ "$PS1" == *'133;B'* ]] || PS1=$'%{\e]133;A\a%}'"$PS1"$'%{\e]133;B\a%}'
}
__maomi_preexec() { printf '\e]133;C\a'; }
autoload -Uz add-zsh-hook
add-zsh-hook precmd __maomi_precmd
add-zsh-hook preexec __maomi_preexec
"#;

/// cmd has no hooks; `$e]9;9;$P$e\` in `PROMPT` reports the cwd on every prompt.
const CMD_PROMPT: &str = r"$e]9;9;$P$e\$P$G";

/// Adds the integration for `shell` to `cmd`. Returns false when the shell has none or the
/// startup files could not be written; the session then runs without it.
pub(crate) fn inject(shell: &Shell, cmd: &mut CommandBuilder) -> bool {
    match shell.kind {
        ShellKind::Pwsh | ShellKind::WindowsPowerShell => {
            // `-NoExit` keeps the shell open after the snippet; profiles load before it runs.
            if !shell.args.iter().any(|a| a.eq_ignore_ascii_case("-NoExit")) {
                cmd.arg("-NoExit");
            }
            cmd.arg("-EncodedCommand");
            cmd.arg(encode_utf16_base64(PWSH));
            true
        }
        ShellKind::Bash => {
            let Some(dir) = script_dir() else {
                return false;
            };
            // `--rcfile` is ignored by login shells, so `-l` becomes our env flag instead.
            let args = cmd.get_argv_mut();
            let had_login = args.iter().any(|a| a == "-l" || a == "--login");
            args.retain(|a| a != "-l" && a != "--login");
            args.insert(1, OsString::from("--rcfile"));
            args.insert(2, dir.join("bashrc").into_os_string());
            if had_login {
                cmd.env("MAOMI_LOGIN_SHELL", "1");
            }
            true
        }
        ShellKind::Zsh => {
            let Some(dir) = script_dir() else {
                return false;
            };
            if let Some(user) = std::env::var_os("ZDOTDIR") {
                cmd.env("MAOMI_USER_ZDOTDIR", user);
            }
            cmd.env("ZDOTDIR", dir.join("zsh"));
            true
        }
        ShellKind::Cmd => {
            if std::env::var_os("PROMPT").is_none() {
                cmd.env("PROMPT", CMD_PROMPT);
            }
            true
        }
        ShellKind::Fish | ShellKind::Sh | ShellKind::Wsl | ShellKind::Other => false,
    }
}

/// Writes the bash/zsh startup files once per process.
fn script_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("maomiterminal-shell-{}", env!("CARGO_PKG_VERSION")));
        let zsh = dir.join("zsh");
        let written = std::fs::create_dir_all(&zsh)
            .and_then(|()| std::fs::write(dir.join("bashrc"), BASH_RC))
            .and_then(|()| std::fs::write(zsh.join(".zshenv"), ZSH_ENV))
            .and_then(|()| std::fs::write(zsh.join(".zprofile"), ZSH_PROFILE))
            .and_then(|()| std::fs::write(zsh.join(".zshrc"), ZSH_RC));
        match written {
            Ok(()) => Some(dir),
            Err(err) => {
                tracing::warn!(%err, "could not write shell integration scripts");
                None
            }
        }
    })
    .as_deref()
}

/// `-EncodedCommand` takes base64 of UTF-16LE.
fn encode_utf16_base64(script: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_utf16() {
        // "ab" → 61 00 62 00
        assert_eq!(encode_utf16_base64("ab"), "YQBiAA==");
        assert_eq!(encode_utf16_base64("abc"), "YQBiAGMA");
    }

    #[test]
    fn bash_rcfile_replaces_login_flag() {
        let shell = Shell::with_args("/bin/bash", vec!["-l".into()]);
        let mut cmd = CommandBuilder::new(&shell.program);
        cmd.args(&shell.args);
        assert!(inject(&shell, &mut cmd));
        let argv = cmd.get_argv();
        assert_eq!(argv[1], "--rcfile");
        assert!(!argv.iter().any(|a| a == "-l"));
        assert_eq!(cmd.get_env("MAOMI_LOGIN_SHELL"), Some("1".as_ref()));
    }
}
