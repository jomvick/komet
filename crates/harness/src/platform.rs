//! Cross-platform helpers shared by the harness adapters and the engine.

/// Windows `CREATE_NO_WINDOW`.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Keep console children (git, agent CLIs, npm...) from opening a console
/// window. The managed engine runs detached with no console of its own, so on
/// Windows every console child would otherwise flash a new window. No-op
/// elsewhere.
pub trait NoConsole {
    fn no_console(&mut self) -> &mut Self;
}

impl NoConsole for std::process::Command {
    fn no_console(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

impl NoConsole for tokio::process::Command {
    fn no_console(&mut self) -> &mut Self {
        #[cfg(windows)]
        self.creation_flags(CREATE_NO_WINDOW);
        self
    }
}

/// The user's home directory: `HOME`, else `USERPROFILE` (Windows rarely sets
/// `HOME`). Env-based on purpose so tests can redirect it.
pub fn home_dir() -> Option<std::path::PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .find_map(|key| std::env::var_os(key).filter(|v| !v.is_empty()))
        .map(std::path::PathBuf::from)
}
