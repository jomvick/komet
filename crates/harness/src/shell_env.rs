//! Login-shell PATH snapshot.
//!
//! GUI/service launches (Dock, Finder, launchd, systemd) never run the user's
//! shell init, so the daemon's own PATH misses everything the shell shapes:
//! nvm's shell function, fnm multishells, asdf/mise shims, custom npm
//! prefixes, nix profiles, `~/.zshrc` exports. The hardcoded known-location
//! lists in the resolvers cover the common managers, but the only fix that
//! works for *any* setup is asking the user's actual shell: spawn it once as
//! an interactive login shell, have it print its environment between markers,
//! and keep the PATH it reports. If `codex`/`claude` runs in their terminal,
//! it resolves here too.
//!
//! The snapshot is captured once per process (cached, including a negative
//! result) and is defensive about hostile shell init:
//! - `-lic` first (interactive login — nvm and friends load in rc files),
//!   falling back to `-lc` if that produces nothing (some rc files hang or
//!   `exec` a multiplexer when interactive).
//! - Output is read on a side thread into a shared buffer; the poll loop
//!   returns as soon as the end marker appears, so init that blocks *after*
//!   printing (or grandchildren inheriting the pipe) can't wedge us.
//! - A hard per-attempt timeout kills the shell.
//!
//! Set `KOMET_NO_LOGIN_SHELL=1` to disable the snapshot entirely.

use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

static CACHE: OnceLock<Option<OsString>> = OnceLock::new();

/// The PATH the user's login shell reports, captured once and cached for the
/// life of the process. `None` when disabled, non-unix, no usable shell, or
/// the shell never produced a parseable snapshot.
pub fn login_shell_path() -> Option<&'static OsStr> {
    #[cfg(unix)]
    {
        CACHE.get_or_init(unix::capture).as_deref()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// OSC 133 shell integration (step 5 of the terminal story)
// ---------------------------------------------------------------------------

/// OSC 133 command markers for the terminals Komet opens.
///
/// The terminal parser (`komet-term`) can only slice a transcript into command
/// blocks if the *shell* emits `A`/`B`/`C`/`D` markers. This module generates
/// the init code that makes it do so, per real shell, and describes how the
/// launch must change to load it. Nothing here is visual: every emitted
/// sequence is an OSC string the parser consumes and the screen never shows.
///
/// Rules this module lives by:
/// - **Detect the real shell first.** The snippet is chosen by the resolved
///   launch path's basename (`bash`, `zsh`, `fish`). Anything else (`sh`,
///   `dash`, `csh`, `nu`, …) gets NO injection — emitting bash init code to a
///   shell that cannot parse it is corruption, not a no-op. Those terminals
///   keep working through the parser's silence fallback, which is honest about
///   knowing no command text or exit code.
/// - **Chain, never clobber.** A user's `PS1`, `PROMPT_COMMAND`, `precmd` or
///   fish event handlers survive: ours are prepended/appended around theirs,
///   and the invisible `B` marker is *appended* to PS1 (guarded so a re-painted
///   prompt does not accumulate duplicates).
/// - **PowerShell / Windows: deliberately NOT DONE.** No snippet is generated
///   for it (plan returns `None`), and that is a declared gap, not a silent
///   absence: `docs/PARITY.md`-style follow-up owns the PowerShell
///   (`PSReadLine`) port.
///
/// `KOMET_SHELL_INTEGRATION=0` disables injection entirely (escape hatch for a
/// snippet that wedges someone's rc — the shells then fall back to silence).
pub mod integration {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    /// How a Komet terminal's shell launch must change to load the snippet.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ShellIntegration {
        /// Extra arguments appended to the shell command line.
        pub extra_args: Vec<String>,
        /// True when the login flag (`-l`) must NOT be passed because the
        /// injected init emulates it (bash `--rcfile` reads no profile while
        /// `-l` is absent, so the wrapper sources the user's profile files
        /// itself). The snippet files are written once per process.
        pub drop_login_flag: bool,
        /// Environment for the shell child process.
        pub env: Vec<(String, String)>,
    }

    /// The per-process directory holding the snippet files. Created lazily;
    /// files are tiny, idempotent, and live in the temp dir (cleaned at exit
    /// by the OS alongside everything else in TMPDIR).
    fn snippet_dir() -> Option<&'static Path> {
        static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
        DIR.get_or_init(|| {
            let dir = std::env::temp_dir()
                .join(format!("komet-shell-integration-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok()?;
            Some(dir)
        })
        .as_deref()
    }

    fn write_snippet(name: &str, contents: &str) -> Option<PathBuf> {
        let path = snippet_dir()?.join(name);
        std::fs::write(&path, contents).ok()?;
        Some(path)
    }

    /// The integration plan for the shell about to be launched, or `None`
    /// when the shell gets no injection (unsupported/disabled — callers must
    /// then launch exactly as before).
    pub fn plan(shell_path: &str) -> Option<ShellIntegration> {
        plan_flagged(shell_path, !integration_disabled())
    }

    fn integration_disabled() -> bool {
        std::env::var_os("KOMET_SHELL_INTEGRATION")
            .is_some_and(|v| v.eq_ignore_ascii_case("0") || v.eq_ignore_ascii_case("false"))
    }

    /// `plan` with the escape hatch decided by the caller (tests use this
    /// instead of mutating the process environment).
    fn plan_flagged(shell_path: &str, enabled: bool) -> Option<ShellIntegration> {
        if !enabled {
            return None;
        }
        #[cfg(unix)]
        {
            let name = Path::new(shell_path)
                .file_name()?
                .to_string_lossy()
                .to_string();
            // Stripped/renamed shells (`bash.exe`, a login shell invoked as
            // `-bash`) keep matching on the trimmed basename.
            let name = name.trim_start_matches('-').trim_end_matches(".exe");
            match name {
                "bash" => bash_plan(),
                "zsh" => zsh_plan(),
                "fish" => fish_plan(),
                _ => None,
            }
        }
        #[cfg(not(unix))]
        {
            // NOT DONE on Windows: the PowerShell port is a declared gap
            // (see the module docs), not a silent absence.
            let _ = shell_path;
            None
        }
    }

    /// Emitted on every real shell: `\e]133;…\a` stays invisible to the grid.
    /// `Cwd=` rides the A/D markers so a block knows where it ran.
    fn bash_integration() -> String {
        r#"# Komet OSC 133 integration — sourced last, chains, never clobbers.
if [ -n "${KOMET_OSC133_LOADED:-}" ]; then return 0 2>/dev/null; fi
KOMET_OSC133_LOADED=1

__komet_osc133_pre() {
  local __komet_st=$?
  printf '\033]133;D;%s;Cwd=%s\007' "$__komet_st" "$PWD"
  printf '\033]133;A;Cwd=%s\007' "$PWD"
  # B (prompt end) is APPENDED to PS1 as a zero-width sequence. Guarded so a
  # prompt tool that repaints PS1 every prompt does not accumulate markers.
  # The OSC MUST ride its own BEL (`\007`): an unterminated OSC string swallows
  # every byte up to the next ESC, i.e. the echoed command line itself, and the
  # grid then never shows what was typed.
  case "$PS1" in
    *']133;B'*) ;;
    *) PS1="${PS1}\\[\\033]133;B\\007\\]" ;;
  esac
}

# C (output start) rides PS0: bash expands PS0 right after the command line is
# read and before it runs — no DEBUG trap, no false C from PROMPT_COMMAND
# segments. The user's own PS0 is kept, ours appended (OSC bytes are not
# echoed by PS0's writer, so no width bookkeeping is needed).
PS0="${PS0}$(printf '\033]133;C\007')"

# Chain PROMPT_COMMAND with ours FIRST: it must capture $? before anything
# else in the prompt pipeline can overwrite it.
if [ -n "${PROMPT_COMMAND:-}" ]; then
  PROMPT_COMMAND="__komet_osc133_pre;$PROMPT_COMMAND"
else
  PROMPT_COMMAND="__komet_osc133_pre"
fi
"#
        .to_string()
    }

    fn bash_wrapper(integration: &Path) -> String {
        format!(
            r#"# Komet: interactive bash with shell integration. `--rcfile` bypasses the
# login sequence, so emulate it: /etc/profile, then the first profile that
# exists, and ~/.bashrc only when no profile chain would have sourced it.
[ -f /etc/profile ] && . /etc/profile
__komet_profile=""
for __komet_f in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
  if [ -f "$__komet_f" ]; then . "$__komet_f"; __komet_profile=1; break; fi
done
if [ -z "$__komet_profile" ] && [ -f "$HOME/.bashrc" ]; then . "$HOME/.bashrc"; fi
. "{}"
"#,
            integration.display()
        )
    }

    fn bash_plan() -> Option<ShellIntegration> {
        let integration = write_snippet("komet-bash-integration.sh", &bash_integration())?;
        let wrapper = write_snippet("komet-bash-wrapper.sh", &bash_wrapper(&integration))?;
        Some(ShellIntegration {
            extra_args: vec!["--rcfile".into(), wrapper.display().to_string()],
            drop_login_flag: true,
            env: Vec::new(),
        })
    }

    /// Launch arguments for bash in the exact order bash accepts them: on
    /// bash 5.3 a LONG option written after a short flag (`-i --rcfile F`) is
    /// rejected with `bash: --: invalid option`, so the plan's `--rcfile` must
    /// come first. `interactive_first` is only for the tests, which cannot use
    /// a PTY and must ask for interactivity by hand; the engine's PTY launch
    /// needs neither flag and passes the plan's arguments as they are.
    #[cfg(test)]
    fn bash_args(plan: &ShellIntegration, interactive: bool) -> Vec<String> {
        let mut args = plan.extra_args.clone();
        if interactive {
            args.push("-i".into());
        }
        args
    }

    /// zsh: hooks via `add-zsh-hook`, which chains with the user's own
    /// precmd/preexec functions (and preserves $status across them). The
    /// launch swaps ZDOTDIR for a stub dir that sources the user's files
    /// first — every startup file zsh would read still is read.
    fn zsh_integration() -> String {
        r#"# Komet OSC 133 integration — sourced after the user's .zshrc.
if [ -n "${KOMET_OSC133_LOADED:-}" ]; then return 0 2>/dev/null; fi
KOMET_OSC133_LOADED=1
autoload -U add-zsh-hook

__komet_osc133_pre() {
  local __komet_st=$?
  printf '\033]133;D;%s;Cwd=%s\007' "$__komet_st" "$PWD"
  printf '\033]133;A;Cwd=%s\007' "$PWD"
  case "$PS1" in
    *']133;B'*) ;;
    *) PS1="${PS1}%{\033]133;B\a%}" ;;
  esac
}
__komet_osc133_preexec() { printf '\033]133;C\007' }
add-zsh-hook precmd __komet_osc133_pre
add-zsh-hook preexec __komet_osc133_preexec
"#
        .to_string()
    }

    /// One startup file of the stub ZDOTDIR: source the user's own version of
    /// it (if any), then run `extra` (only `.zshrc` has any).
    fn zsh_stub(name: &str, inner: &str, extra: &str) -> String {
        format!(
            concat!(
                "# Komet ZDOTDIR stub: the user's own {} still runs first.\n",
                "__komet_user_zdotdir=\"${{KOMET_USER_ZDOTDIR:-$HOME}}\"\n",
                "[ -f \"$__komet_user_zdotdir/{}\" ] && source \"$__komet_user_zdotdir/{}\"\n",
                "{}",
            ),
            name, inner, inner, extra
        )
    }

    fn zsh_plan() -> Option<ShellIntegration> {
        let dir = snippet_dir()?;
        let rc_extra = format!(
            "source \"{}\"\n{}",
            dir.join("komet-zsh-integration.sh").display(),
            zsh_integration()
        );
        // The dir itself must exist before the stubs land in it.
        std::fs::create_dir_all(dir.join("zsh")).ok()?;
        let zdotdir = dir.join("zsh");
        for (file, inner) in [
            (".zshenv", ".zshenv"),
            (".zprofile", ".zprofile"),
            (".zlogin", ".zlogin"),
        ] {
            std::fs::write(zdotdir.join(file), zsh_stub(file, inner, "")).ok()?;
        }
        std::fs::write(
            zdotdir.join(".zshrc"),
            zsh_stub(".zshrc", ".zshrc", &rc_extra),
        )
        .ok()?;
        std::fs::write(zdotdir.join("komet-zsh-integration.sh"), zsh_integration()).ok()?;
        let mut env = vec![("ZDOTDIR".to_string(), zdotdir.display().to_string())];
        // Only forward a real ZDOTDIR; with none set the stub falls back to
        // $HOME, which is exactly where zsh would have looked.
        if let Ok(user_zdotdir) = std::env::var("ZDOTDIR") {
            env.push(("KOMET_USER_ZDOTDIR".to_string(), user_zdotdir));
        }
        Some(ShellIntegration {
            extra_args: Vec::new(),
            drop_login_flag: false,
            env,
        })
    }

    /// fish: `--init-command` composes with the user's config without any
    /// file swap, and event handlers chain by construction (a user's own
    /// fish_precmd still runs — both subscribe to the event).
    ///
    /// fish also owns the login flag here: its `-l` means `--list-bindings`
    /// (print the key map and EXIT), so the plain `-l` the launcher adds by
    /// default would kill the shell at birth. The plan therefore drops it and
    /// passes fish's long form `--login` itself, in the argument order fish
    /// accepts.
    fn fish_plan() -> Option<ShellIntegration> {
        let path = write_snippet(
            "komet-fish-integration.fish",
            r#"# Komet OSC 133 integration — event handlers, chained by construction.
function __komet_osc133_pre --event fish_precmd
    printf '\033]133;D;%d;Cwd=%s\007' $status $PWD
    printf '\033]133;A;Cwd=%s\007' $PWD
end
function __komet_osc133_preexec --event fish_preexec
    printf '\033]133;C\007'
end
"#,
        )?;
        Some(ShellIntegration {
            extra_args: vec![
                "--login".into(),
                "--init-command".into(),
                format!("source {}", path.display()),
            ],
            drop_login_flag: true,
            env: Vec::new(),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn which(name: &str) -> Option<String> {
            let dirs = std::env::var_os("PATH")?;
            std::env::split_paths(&dirs)
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
                .map(|candidate| candidate.display().to_string())
        }

        /// Drive a scripted interactive shell session and return everything
        /// it wrote. Interactive shells put the PROMPT (and PS0) on stderr
        /// and command output on stdout — one PTY merges them, so the test
        /// merges them too.
        fn run_session<A: AsRef<std::ffi::OsStr>>(
            program: &str,
            args: &[A],
            env: &[(&str, &str)],
            stdin_cmds: &str,
        ) -> String {
            use std::io::Write as _;
            use std::process::{Command, Stdio};
            let mut child = Command::new(program)
                .args(args)
                .envs(env.iter().map(|(k, v)| (k, v.to_string())))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(stdin_cmds.as_bytes())
                .expect("type into the shell");
            let out = child.wait_with_output().expect("shell exits");
            // No status assertion: these scripts end on `false` on purpose (a
            // non-zero exit must reach the markers), and every shell exits with
            // the last command's code. What matters is the emitted stream.
            let mut merged = String::from_utf8_lossy(&out.stdout).into_owned();
            merged.push_str(&String::from_utf8_lossy(&out.stderr));
            merged
        }

        fn owned_env(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
            pairs
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect()
        }

        #[test]
        fn unsupported_shells_get_no_injection() {
            // Emitting bash init into a shell that cannot parse it is
            // corruption — `sh`/`dash`/`nu` must plan nothing.
            assert_eq!(plan_flagged("/bin/sh", true), None);
            assert_eq!(plan_flagged("/usr/bin/dash", true), None);
            assert_eq!(plan_flagged("/usr/local/bin/nu", true), None);
        }

        #[test]
        fn the_escape_hatch_disables_everything() {
            // `KOMET_SHELL_INTEGRATION=0` is the declared off-switch; the
            // plan then disappears for shells that would otherwise inject.
            assert_eq!(plan_flagged("/bin/bash", false), None);
            assert!(plan_flagged("/bin/bash", true).is_some());
        }

        #[cfg(unix)]
        #[test]
        fn bash_plan_wraps_rcfile_and_drops_the_login_flag() {
            if which("bash").is_none() {
                eprintln!("no bash on this machine");
                return;
            }
            let p = plan_flagged("/bin/bash", true).expect("bash plan");
            assert!(p.drop_login_flag);
            assert_eq!(p.extra_args.len(), 2);
            assert_eq!(p.extra_args[0], "--rcfile");
            assert!(
                std::fs::read_to_string(&p.extra_args[1])
                    .expect("wrapper exists")
                    .contains("komet-bash-integration.sh")
            );
        }

        /// The two invariants this step must not trade away: a personalised
        /// PS1 survives, and so does a personalised PROMPT_COMMAND — while
        /// the markers arrive around them, including the NON-ZERO exit code
        /// of `false`.
        #[cfg(unix)]
        #[test]
        fn bash_injection_keeps_a_custom_ps1_and_prompt_command_and_marks_exit() {
            let Some(bash) = which("bash") else {
                eprintln!("no bash on this machine");
                return;
            };
            let home = tempfile::tempdir().expect("fake HOME");
            std::fs::write(
                home.path().join(".bashrc"),
                "PS1='MYPROMPT> '\nPROMPT_COMMAND='echo pc-ran'\n",
            )
            .expect("write bashrc");
            let p = plan_flagged("/bin/bash", true).expect("bash plan");
            let mut env = vec![("HOME".to_string(), home.path().display().to_string())];
            env.extend(p.env.iter().cloned());
            // `false` first: its exit code must arrive as D;1. bash reads
            // --rcfile only when interactive, hence -i (the PTY launch is
            // interactive by construction).
            let args = bash_args(&p, true);
            let out = run_session(&bash, &args, &owned_env(&env), "false\ntrue\nexit\n");
            assert!(out.contains("MYPROMPT> "), "the user's PS1 survived: {out}");
            assert!(out.contains("pc-ran"), "the user's PROMPT_COMMAND ran");
            assert!(
                out.contains("\u{1b}]133;D;1;Cwd="),
                "the failing command's exit code reached the stream: {out}"
            );
            assert!(
                out.contains("\u{1b}]133;D;0;Cwd="),
                "and the succeeding one's"
            );
            assert!(out.contains("\u{1b}]133;A;Cwd="), "prompt start marked");
            assert!(out.contains("\u{1b}]133;B"), "prompt end marked");
            assert!(out.contains("\u{1b}]133;C"), "output start marked");
        }

        /// Same stream through the real parser: after the shell emits its
        /// markers the screen must show NONE of them (OSC strings are
        /// consumed by the grid) and everything else must survive.
        #[cfg(unix)]
        #[test]
        fn bash_markers_leave_no_visible_characters_on_the_grid() {
            let Some(bash) = which("bash") else {
                eprintln!("no bash on this machine");
                return;
            };
            let home = tempfile::tempdir().expect("fake HOME");
            std::fs::write(home.path().join(".bashrc"), "PS1='> '\n").expect("write bashrc");
            let p = plan_flagged("/bin/bash", true).expect("bash plan");
            let mut env = vec![("HOME".to_string(), home.path().display().to_string())];
            env.extend(p.env.iter().cloned());
            let args = bash_args(&p, true);
            let out = run_session(&bash, &args, &owned_env(&env), "echo komet-visible\nexit\n");
            // Feed the raw stream through the emulator: what it renders as
            // text must contain the echo and no marker bytes.
            let mut emu = komet_term::Emulator::with_scrollback(80, 24, 100);
            emu.feed(out.as_bytes());
            let screen = emu
                .screen_text()
                .iter()
                .map(|line| line.trim_end())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                screen.contains("komet-visible"),
                "output survived: {screen}"
            );
            assert!(
                !screen.contains("133;"),
                "no marker bytes on screen: {screen}"
            );
            let blocks = emu.command_blocks();
            // Command TEXT must NOT be asserted here: with stdin a PIPE bash
            // has no readline echo of the typed line (nothing between `B` and
            // `C` carries `echo komet-visible`), and `run_session` concatenates
            // stderr after stdout, so prompt, `C` and output never arrive in
            // screen order — the parser would then legitimately read the
            // prompt line as the command (`command: Some(">")`). Command
            // capture on the PRODUCTION launch (a real PTY, kernel echo
            // between `B` and `C`) is proven by komet-engine's
            // `terminals::bash_injection_on_a_real_pty_reports_the_typed_
            // command_to_agents`. What a pipe CAN honestly prove is the claim
            // of this test's name: the markers reached the parser, were
            // consumed off the grid, and still sliced a marker-backed block
            // carrying the exit code.
            assert!(
                blocks
                    .iter()
                    .any(|b| b.marker_backed && b.exit_code == Some(0)),
                "the parser consumed the off-screen markers into an exit-coded block: {blocks:?}"
            );
        }

        #[cfg(unix)]
        #[test]
        fn zsh_injection_chains_precmd_and_marks_exit() {
            let Some(zsh) = which("zsh") else {
                eprintln!("no zsh on this machine");
                return;
            };
            let home = tempfile::tempdir().expect("fake HOME");
            // A classic (non-hook) precmd: zsh runs it AND add-zsh-hook
            // runs ours — the user's prompt machinery must keep working.
            std::fs::write(
                home.path().join(".zshrc"),
                "PS1='MYZPROMPT> '\nprecmd() { echo usr-ran; }\n",
            )
            .expect("write zshrc");
            let p = plan_flagged("zsh", true).expect("zsh plan");
            let mut env = vec![("HOME".to_string(), home.path().display().to_string())];
            env.extend(p.env.iter().cloned());
            let out = run_session(&zsh, &["-i"], &owned_env(&env), "false\nexit\n");
            assert!(out.contains("MYZPROMPT> "), "the user's prompt survived");
            assert!(out.contains("usr-ran"), "the user's own precmd still ran");
            assert!(
                out.contains("\u{1b}]133;D;1;Cwd="),
                "zsh carried the exit code: {out}"
            );
            assert!(out.contains("\u{1b}]133;A;Cwd="));
            assert!(out.contains("\u{1b}]133;C"), "preexec emitted C");
        }

        #[cfg(unix)]
        #[test]
        fn fish_injection_emits_markers_without_clobbering_prompt() {
            let Some(fish) = which("fish") else {
                eprintln!("no fish on this machine");
                return;
            };
            let config = tempfile::tempdir().expect("fake config");
            let p = plan_flagged("fish", true).expect("fish plan");
            let mut env = vec![
                (
                    "XDG_CONFIG_HOME".to_string(),
                    config.path().display().to_string(),
                ),
                ("HOME".to_string(), config.path().display().to_string()),
            ];
            env.extend(p.env.iter().cloned());
            let mut args = vec!["--interactive".to_string()];
            args.extend(p.extra_args.iter().cloned());
            let out = run_session(&fish, &args, &owned_env(&env), "false\nexit\n");
            assert!(
                out.contains("\u{1b}]133;D;1"),
                "fish carried the exit code: {out}"
            );
            assert!(out.contains("\u{1b}]133;A;Cwd="));
            assert!(out.contains("\u{1b}]133;C"));
        }
    }
}

/// Kick off the snapshot on a background thread so the first harness resolve
/// doesn't pay the shell-startup latency inline. Call at daemon startup.
pub fn prewarm() {
    #[cfg(unix)]
    {
        let _ = std::thread::Builder::new()
            .name("komet-shell-env".into())
            .spawn(|| {
                let _ = login_shell_path();
            });
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsString;
    use std::io::Read;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const BEGIN_MARKER: &str = "__KOMET_SHELL_ENV_BEGIN__";
    const END_MARKER: &str = "__KOMET_SHELL_ENV_END__";
    /// Enough for any sane environment; a runaway rc file can't OOM us.
    const MAX_OUTPUT: usize = 2 * 1024 * 1024;
    const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
    /// After the shell exits, wait this long for the pipe to flush.
    const EXIT_FLUSH_GRACE: Duration = Duration::from_millis(250);

    pub(super) fn capture() -> Option<OsString> {
        if std::env::var_os("KOMET_NO_LOGIN_SHELL").is_some_and(|v| !v.is_empty()) {
            return None;
        }
        let shell = user_shell()?;
        snapshot_path(&shell, ATTEMPT_TIMEOUT)
    }

    /// The user's shell: `$SHELL`, then the passwd entry, then well-known
    /// defaults. Non-executables and nologin shells are skipped.
    fn user_shell() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(s) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
            candidates.push(PathBuf::from(s));
        }
        // systemd/launchd services often start without SHELL — passwd has it.
        if let Some(p) = passwd_shell() {
            candidates.push(p);
        }
        candidates.push(PathBuf::from("/bin/zsh"));
        candidates.push(PathBuf::from("/bin/bash"));
        candidates.push(PathBuf::from("/bin/sh"));
        candidates.into_iter().find(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string());
            let blocked = matches!(name.as_deref(), Some("nologin" | "false") | None);
            !blocked && is_executable(p)
        })
    }

    fn passwd_shell() -> Option<PathBuf> {
        // SAFETY: getpwuid's static buffer is only read here, and callers are
        // serialized through the OnceLock init above.
        unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if pw.is_null() || (*pw).pw_shell.is_null() {
                return None;
            }
            let shell = std::ffi::CStr::from_ptr((*pw).pw_shell);
            (!shell.to_bytes().is_empty())
                .then(|| PathBuf::from(std::ffi::OsStr::from_bytes(shell.to_bytes())))
        }
    }

    fn is_executable(p: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }

    /// Flag sets to try, most-loaded first. csh/tcsh reject `-l` combined with
    /// `-c`; fish runs config.fish for every invocation, so `-l` alone loads
    /// everything without interactive-mode side effects.
    fn attempt_flag_sets(shell: &Path) -> Vec<Vec<&'static str>> {
        let name = shell
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        match name.as_str() {
            "csh" | "tcsh" => vec![vec!["-c"]],
            "fish" => vec![vec!["-l", "-c"], vec!["-c"]],
            _ => vec![vec!["-l", "-i", "-c"], vec!["-l", "-c"]],
        }
    }

    /// Run `<shell> <flags> 'echo BEGIN; env; echo END'` per flag set until one
    /// yields a parseable PATH.
    pub(super) fn snapshot_path(shell: &Path, timeout: Duration) -> Option<OsString> {
        let script = format!("echo {BEGIN_MARKER}; env; echo {END_MARKER}");
        for flags in attempt_flag_sets(shell) {
            let output = run_and_capture(shell, &flags, &script, timeout);
            if let Some(path) = parse_snapshot_path(&output) {
                return Some(path);
            }
        }
        None
    }

    /// Spawn the shell and collect stdout until the end marker appears, the
    /// child exits (plus a flush grace), or the timeout kills it. The reader
    /// lives on its own thread appending into a shared buffer, so a shell that
    /// blocks after printing — or a grandchild that inherits the pipe and
    /// never closes it — can't hang us on EOF.
    fn run_and_capture(shell: &Path, flags: &[&str], script: &str, timeout: Duration) -> Vec<u8> {
        let mut cmd = std::process::Command::new(shell);
        cmd.args(flags)
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Let rc files detect (and skip work for) this probe, mirroring
            // VSCODE_RESOLVING_ENVIRONMENT; TERM=dumb quiets fancy prompts.
            .env("KOMET_RESOLVING_ENVIRONMENT", "1")
            .env("TERM", "dumb");
        // An interactive shell opens `/dev/tty` and calls `tcsetpgrp`. When the
        // daemon was started from a terminal, it sits in a background process
        // group there, so the kernel stops the *whole group* with SIGTTOU —
        // including the thread that would enforce ATTEMPT_TIMEOUT. A new
        // session gives the probe no controlling terminal to fight over.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let Ok(mut child) = cmd.spawn() else {
            return Vec::new();
        };
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        if let Some(mut stdout) = child.stdout.take() {
            let buf = Arc::clone(&buf);
            let _ = std::thread::Builder::new()
                .name("komet-shell-env-read".into())
                .spawn(move || {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match stdout.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let mut b = buf
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                if b.len() >= MAX_OUTPUT {
                                    break;
                                }
                                b.extend_from_slice(&chunk[..n]);
                            }
                        }
                    }
                });
        }
        let deadline = Instant::now() + timeout;
        let mut exited_at: Option<Instant> = None;
        let mut scanned = 0usize;
        loop {
            {
                let b = buf
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Only scan the unscanned tail (minus marker-length overlap).
                let from = scanned.saturating_sub(END_MARKER.len());
                if find_subslice(&b[from..], END_MARKER.as_bytes()).is_some() {
                    break;
                }
                scanned = b.len();
            }
            match exited_at {
                Some(at) if at.elapsed() >= EXIT_FLUSH_GRACE => break,
                Some(_) => {}
                None => match child.try_wait() {
                    Ok(Some(_)) => exited_at = Some(Instant::now()),
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => break,
                },
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if exited_at.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let b = buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        b.clone()
    }

    /// Extract PATH from the `env` dump between the LAST begin marker and the
    /// first end marker after it (rc noise printed before our command — or a
    /// marker echoed by init itself — lands before the real one).
    fn parse_snapshot_path(output: &[u8]) -> Option<OsString> {
        let begin = rfind_subslice(output, BEGIN_MARKER.as_bytes())?;
        let after = &output[begin + BEGIN_MARKER.len()..];
        let end = find_subslice(after, END_MARKER.as_bytes())?;
        for line in after[..end].split(|b| *b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(value) = line.strip_prefix(b"PATH=")
                && !value.is_empty()
            {
                return Some(OsString::from_vec(value.to_vec()));
            }
        }
        None
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .rposition(|window| window == needle)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn fake_shell(dir: &Path, body: &str) -> PathBuf {
            let path = dir.join("fake-shell");
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        /// A fake $SHELL skeleton: consume flags, exec the `-c` payload.
        const RUN_PAYLOAD: &str = r#"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-c" ]; then shift; exec /bin/sh -c "$1"; fi
  shift
done
exit 1
"#;

        #[test]
        fn parses_path_between_markers() {
            let output = format!(
                "rc noise\n{BEGIN_MARKER}\nHOME=/home/u\nPATH=/custom/bin:/usr/bin\nX=y\n{END_MARKER}\ntrailing"
            );
            let path = parse_snapshot_path(output.as_bytes()).unwrap();
            assert_eq!(path, OsString::from("/custom/bin:/usr/bin"));
        }

        #[test]
        fn ignores_marker_echoed_by_init() {
            // rc noise that happens to contain the begin marker but no PATH
            // after it must not shadow the real snapshot.
            let output =
                format!("{BEGIN_MARKER}\ngarbage\n{BEGIN_MARKER}\nPATH=/real/bin\n{END_MARKER}\n");
            let path = parse_snapshot_path(output.as_bytes()).unwrap();
            assert_eq!(path, OsString::from("/real/bin"));
        }

        #[test]
        fn snapshots_path_from_fake_shell() {
            let dir = tempfile::tempdir().unwrap();
            let shell = fake_shell(
                dir.path(),
                &format!(
                    "#!/bin/sh\nPATH=\"/komet-test/custom/bin:/usr/bin:/bin\"; export PATH\n{RUN_PAYLOAD}"
                ),
            );
            let path = snapshot_path(&shell, Duration::from_secs(10)).unwrap();
            let path = path.to_string_lossy();
            assert!(path.starts_with("/komet-test/custom/bin:"), "got: {path}");
        }

        #[test]
        fn falls_back_when_interactive_attempt_hangs() {
            let dir = tempfile::tempdir().unwrap();
            // Simulates rc files that wedge only in interactive mode (`exec
            // tmux` and friends): sleep forever when -i is present.
            let shell = fake_shell(
                dir.path(),
                &format!(
                    "#!/bin/sh\ncase \" $* \" in *\" -i \"*) sleep 60;; esac\nPATH=\"/komet-test/fallback/bin:/bin\"; export PATH\n{RUN_PAYLOAD}"
                ),
            );
            let start = Instant::now();
            let path = snapshot_path(&shell, Duration::from_millis(400)).unwrap();
            assert!(
                path.to_string_lossy()
                    .starts_with("/komet-test/fallback/bin"),
                "got: {}",
                path.to_string_lossy()
            );
            // First attempt burned ~400ms then was killed; the whole resolve
            // must not have waited out the sleep.
            assert!(start.elapsed() < Duration::from_secs(5));
        }

        #[test]
        fn gives_up_on_a_shell_that_never_answers() {
            let dir = tempfile::tempdir().unwrap();
            let shell = fake_shell(dir.path(), "#!/bin/sh\nsleep 60\n");
            let start = Instant::now();
            assert!(snapshot_path(&shell, Duration::from_millis(300)).is_none());
            assert!(start.elapsed() < Duration::from_secs(5));
        }
    }
}
