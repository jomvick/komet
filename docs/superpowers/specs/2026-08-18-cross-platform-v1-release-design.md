# Komet cross-platform (Windows) + v1.0 release — design

Date: 2026-08-18
Status: Approved (brainstorming) — implementation order: refactors cross-platform first, then packaging, CI, README, release.

## Summary

Close the Windows gap so Komet is a full desktop product on **Windows + Linux +
macOS**, then ship **v1.0** with installable assets (tarballs, AppImage, dmg,
msi, exe) and per-OS install docs in the README.

Windows today cross-compiles `x86_64-pc-windows-gnu` (UI included) but nothing
is distributed: the daemon (launchd/systemd), auto-updater (unix symlinks),
credential handling (macOS Keychain vs file), harness signal handling and
notifications are all unix/macOS-oriented (ARCHITECTURE.md §8).

Scope is **full parity**: daemon as a native SCM service, MSI (WiX) installer +
integrated auto-update, Windows Credential Manager, toast notifications, CI
signing. No change to Linux/macOS behavior. The two pre-existing PTY-over-relay
test failures and the `komet-sync` clippy `--all-targets` issue are **out of
scope** (pre-existing, not introduced here).

## Decisions (brainstorming)

- Targets: desktop 3 OS. Windows on `x86_64-pc-windows-msvc`.
- Windows daemon: **native SCM service**, running under the **logged-in user
  account** (access to `%USERPROFILE%` for `.claude/.credentials.json`), the
  same shape as systemd --user / launchd.
- Installer + updater: **MSI (WiX Toolset)**; auto-update re-runs the MSI
  silently (`msiexec /i … /qn`). No symlink-based swap on Windows.
- Claude credentials on Windows: **Windows Credential Manager** (wincred) as a
  fallback/source beside the primary `.credentials.json` file — same read
  precedence as the macOS Keychain branch today.
- CI: add a `windows-latest` job; sign Authenticode with certificates supplied
  as GitHub secrets (`WINDOWS_CERT_P12` / `WINDOWS_CERT_PASSWORD`).
- Release: bump workspace version to `1.0.0`, tag `v1.0.0`; publish assets
  (tar.gz + AppImage, dmg, msi + exe) to the GitHub release **and** R2
  (`komet.sh/releases/*` + `manifest.json` + `latest.txt`).
- README: add an "Install" section covering Linux, macOS and Windows.

## Per-crate changes

### `apps/komet` — daemon as a Windows service

`apps/komet/src/daemon.rs` gains a `#[cfg(target_os = "windows")]` branch in the
existing `DaemonCommand` (Install/Uninstall/Start/Stop/Restart/Status):

- `Install`: `sc create Komet … binPath= "<exe> --service"`, configures
  `obj= <domain>\<user>` / `type= interact` so the service runs as the logged-in
  user (auto-Logon-like SCM credentials stored at install time).
- `Uninstall`: `sc delete Komet`.
- `Start/Stop/Restart/Status`: `net start` / `net stop` / `sc query`.
- A new `--service` entrypoint uses `windows-service` (`StartServiceCtrlDispatcher`)
  to run `komet headless` under SCM control with a stop handler.

Target-dependency: `windows-service` (add under `[target.'cfg(windows)'.dependencies]`).

`apps/komet/src/auth_cli.rs` Windows path (l.273 cfg unix today): read/write the
Claude credentials via the new `engine` wincred module instead of the file-only
path (or keep file-first, credential-manager fallback — same precedence as
macOS).

### `crates/engine` — credentials, lock, device name

- `crates/engine/src/agent_accounts.rs`: add a `wincred` module
  (`#[cfg(target_os = "windows")]`) mirroring the macOS `keychain` module
  signatures (`read_credentials`, `write_credentials`) via `CredReadW` /
  `CredWriteW` (crate `wincred`). Keep `.credentials.json` as primary store;
  use the credential manager as fallback when the file is absent.
- `crates/engine/src/instance_lock.rs`: Windows path already exists
  (`share_mode(0)`), keep.
- `crates/engine/src/lib.rs` `native_friendly_device_name` (l.1069-1086): already
  handles Windows via `COMPUTERNAME` — verify only.

### `crates/harness` — signal handling

`crates/harness/src/lib.rs` `send_signal` (l.259+): replace the
`#[cfg(not(unix))]` no-op (l.271-274) with a Windows implementation using
`windows-sys` `TerminateProcess` for SIGKILL and `GenerateConsoleCtrlEvent` for
SIGTERM/SIGINT where a console is attached (fall back to `TerminateProcess`).

`crates/harness/Cargo.toml`: add `[target.'cfg(windows)'.dependencies]` windows-sys.

`crates/harness/src/shell_env.rs`: remains unix-only (login-shell PATH snapshot;
Windows PATH comes from the environment directly).

### `crates/update` — Windows apply/relaunch

`crates/update/src/lib.rs`:
- `apply_headless` (l.331-347): Windows branch — download the MSI and run
  `msiexec /i <file> /qn /norestart`, wait for exit; no symlink swap.
- `relaunch_app_after_exit` (l.437+): Windows branch — `cmd /c start "" "<exe>"`.

### `crates/ui` — notifications

`crates/ui/src/notify.rs` (l.255-256 no-op for non macos/linux): add
`#[cfg(target_os = "windows")] post_impl` using `winrt-notification` (toast via
`ToastNotificationManager`) or PowerShell `[Windows.UI.Notifications]` fallback.
`sound.rs` already has a PowerShell player on Windows (l.69) — keep.

## Packaging

### `scripts/package-windows.sh` (new)

1. `cargo build --release --target x86_64-pc-windows-msvc -p komet`.
2. Assemble a WiX MSI from `dist/windows/installer.wxs` (new): installs
   `komet.exe` into `Program Files\Komet`, registers uninstall keys, optionally
   installs the `Komet` service.
3. Extract `komet.exe` as a standalone portable asset.
4. `signtool sign` the exe + msi when `WINDOWS_CERT_P12` / password are set
   (base64 p12 imported via `certutil`/`openssl` + `signtool`).

Output: `target/package/komet-<ver>-windows-x86_64.msi` and `….exe`.

### `scripts/package-appimage.sh` (new)

Linux AppImage via `appimagetool`:
1. Build release (reuse `package-linux.sh` outputs).
2. Assemble `AppDir` (`komet`, `komet.desktop`, `komet.png`, `AppRun`).
3. `appimagetool` → `komet-<ver>-linux-<arch>.AppImage`.

Requires `libfuse2`/`fuse` and `appimagetool` on the runner (apt install in CI).

## CI — `.github/workflows/release.yml`

- Add a `windows` job on `windows-latest`:
  - install WiX (`choco install wixtoolset` or `dotnet tool`),
  - install MSVC target via `rust-toolchain.toml`/rustup,
  - import `WINDOWS_CERT_P12` (env from secrets) and run
    `scripts/package-windows.sh`,
  - `upload-artifact` the msi + exe.
- Linux job: also run `scripts/package-appimage.sh` per matrix arch and upload
  the AppImage alongside the tarball.
- macOS job: change `continue-on-error: true` → blocking (Keychain cfg paths
  must type-check for v1.0), keep signing/notarization.
- `publish`: already globs `dist/*`; the new artifacts flow through untouched.
  Update the R2 upload comment nothing needed — `dist/*` covers everything.

## README

Rewrite the install section (README.md l.7-45) into a per-OS "Install" section:
- **Linux**: `curl -fsSL https://komet.sh/install.sh | sh` (recommended),
  AppImage (portable), or tarball from Releases; `komet status` / `komet update` /
  `komet daemon …`.
- **macOS**: `komet-<ver>-macos-arm64.dmg` from Releases, drag to Applications;
  auto-update built in; source build + `komet daemon install` (launchd) as an
  alternative.
- **Windows**: `komet-<ver>-windows-x86_64.msi` from Releases (installs to
  Program Files, registers the `Komet` service); portable `.exe` as an
  alternative; `komet update` re-runs the MSI.

## Release v1.0

- Bump `Cargo.toml` `[workspace.package]` version → `1.0.0`.
- `version_newer` compares numerically → `1.0.0` > `0.2.2`, updater works.
- Push tag `v1.0.0` → release workflow builds all assets, creates the GitHub
  release, uploads to R2 + `manifest.json` + `latest.txt`.

## Risks / notes

- macOS job has never type-checked Keychain cfg against the Apple SDK — flipping
  it to blocking may surface errors; budget a fix commit.
- AppImage needs a FUSE runtime on the runner and glibc-compatible strip
  (already `lto=thin` + `strip=symbols`).
- SCM service-as-user requires storing the user password in SCM at install time;
  acceptable for a desktop product (equivalent to `sc create … obj=`).
- Windows tests not run in this iteration (no windows runner for cargo test in
  the base workflow); CI at minimum type-checks `--target x86_64-pc-windows-msvc`.
