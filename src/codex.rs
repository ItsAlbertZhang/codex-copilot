//! Locating the installed Codex CLI and asking it for its version.
//!
//! On Windows what sits on `PATH` is usually an npm shim (`codex.cmd` /
//! `codex.ps1`), never `codex.exe`. `Command::new("codex")` only ever appends
//! `.exe`, so the shim would be invisible - hence the explicit extension walk
//! and the per-extension launcher.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launcher {
    /// A real executable.
    Direct,
    /// A Windows batch shim: needs `cmd.exe /C`.
    Batch,
    /// A PowerShell shim.
    PowerShell,
}

#[derive(Debug, Clone)]
pub struct CodexBin {
    pub path: PathBuf,
    launcher: Launcher,
}

/// Extensions accepted when walking `PATH`, most-preferred first. On Windows an
/// extensionless hit is npm's POSIX shell script, which CreateProcess cannot
/// run, so it is skipped.
#[cfg(windows)]
const EXTS: &[&str] = &["exe", "com", "cmd", "bat", "ps1"];
#[cfg(not(windows))]
const EXTS: &[&str] = &[""];

fn is_runnable(path: &Path) -> bool {
    // Under %LOCALAPPDATA%\Microsoft\WindowsApps a "missing" app is a zero-byte
    // reparse point that exists but cannot be executed.
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name))
        .find(|c| is_runnable(c))
}

/// How to install Codex, for messages.
#[cfg(windows)]
const INSTALL: &str =
    "`irm https://chatgpt.com/codex/install.ps1 | iex`, or `npm install -g @openai/codex`";
#[cfg(not(windows))]
const INSTALL: &str =
    "`curl -fsSL https://chatgpt.com/codex/install.sh | sh`, or `npm install -g @openai/codex`";

/// Where the standalone installer puts the `codex` command: under
/// `$CODEX_INSTALL_DIR`, else `~/.local/bin` (`install.sh`) or
/// `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin` (`install.ps1`).
fn standalone_commands() -> Vec<PathBuf> {
    let name = if cfg!(windows) { "codex.exe" } else { "codex" };
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("CODEX_INSTALL_DIR").filter(|dir| !dir.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|dir| !dir.is_empty()) {
            dirs.push(Path::new(&local).join(r"Programs\OpenAI\Codex\bin"));
        }
    } else if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local").join("bin"));
    }
    dirs.into_iter().map(|dir| dir.join(name)).collect()
}

/// `link` and its target, when `link` is a symlink (or junction) whose
/// target is gone.
fn dangling(link: &Path) -> Option<(PathBuf, PathBuf)> {
    let is_link = std::fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link || link.exists() {
        return None;
    }
    let target = std::fs::read_link(link).ok()?;
    Some((link.to_path_buf(), target))
}

fn powershell_exe() -> PathBuf {
    on_path("pwsh.exe")
        .or_else(|| on_path("powershell.exe"))
        .unwrap_or_else(|| PathBuf::from("powershell.exe"))
}

fn launcher_for(path: &Path) -> Launcher {
    match path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("cmd" | "bat") => Launcher::Batch,
        Some("ps1") => Launcher::PowerShell,
        _ => Launcher::Direct,
    }
}

impl CodexBin {
    /// An explicit path from `--codex-bin` / `$CODEX_BIN`.
    pub fn from_path(path: PathBuf) -> Result<Self> {
        if !path.is_file() {
            bail!("codex binary not found at {}", path.display());
        }
        let launcher = launcher_for(&path);
        Ok(Self { path, launcher })
    }

    /// Walks `PATH` for `codex`, trying a fixed extension order, then the
    /// standalone installer's own command location: the installer adds that
    /// to PATH through the shell profile, so a shell started before it ran
    /// does not have it yet.
    pub fn discover() -> Result<Self> {
        let mut candidates = Vec::new();
        if let Some(path_var) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path_var) {
                if dir.as_os_str().is_empty() {
                    continue;
                }
                for ext in EXTS {
                    candidates.push(if ext.is_empty() {
                        dir.join("codex")
                    } else {
                        dir.join(format!("codex.{ext}"))
                    });
                }
            }
        }
        let standalone = standalone_commands();
        candidates.extend(standalone.iter().cloned());
        if let Some(path) = candidates.iter().find(|c| is_runnable(c)) {
            return Ok(Self {
                launcher: launcher_for(path),
                path: path.clone(),
            });
        }
        // install.ps1 links the command's directory rather than the command.
        let broken = candidates
            .iter()
            .map(PathBuf::as_path)
            .chain(standalone.iter().filter_map(|c| c.parent()))
            .find_map(dangling);
        if let Some((link, target)) = broken {
            bail!(
                "`codex` is installed but broken: {} links to {}, which does not exist. The \
                 standalone Codex installer links the command into $CODEX_HOME/packages, so \
                 moving that directory breaks it; `override` / `unoverride` of codex-copilot \
                 2.0.0 and 2.0.1 did. If an override made by such a version is active, \
                 `codex-copilot unoverride` puts it back. Otherwise reinstall Codex ({INSTALL}), \
                 or pass --codex-bin <path>.",
                link.display(),
                target.display()
            );
        }
        bail!("`codex` was not found on PATH. Install the Codex CLI first ({INSTALL}) or pass --codex-bin <path>.")
    }

    /// Runs `codex --version` and extracts the semver. `CODEX_HOME` is pinned
    /// to the home being installed into: codex drops scratch files under it,
    /// and a `--codex-home` run must not write to the real one.
    pub fn version(&self, codex_home: &Path) -> Result<String> {
        let mut cmd = match self.launcher {
            Launcher::Direct => Command::new(&self.path),
            Launcher::Batch => {
                let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
                let mut c = Command::new(comspec);
                c.arg("/C").arg(&self.path);
                c
            }
            Launcher::PowerShell => {
                let mut c = Command::new(powershell_exe());
                c.arg("-NoProfile")
                    .arg("-NonInteractive")
                    .arg("-ExecutionPolicy")
                    .arg("Bypass")
                    .arg("-File")
                    .arg(&self.path);
                c
            }
        };
        let out = cmd
            .arg("--version")
            .env("CODEX_HOME", codex_home)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run {} --version", self.path.display()))?;
        if !out.status.success() {
            bail!(
                "{} --version exited with {}: {}",
                self.path.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        parse_version(&stdout)
            .with_context(|| format!("no version in `codex --version` output: {stdout:?}"))
    }
}

/// Pulls `0.154.0` out of `codex-cli 0.154.0`.
pub fn parse_version(s: &str) -> Option<String> {
    s.split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .map(|t| t.trim().trim_start_matches('v'))
        .find(|t| {
            // Split any prerelease suffix off first, so `0.155.0-alpha.1` still
            // reads as a three-part version.
            let core = t.split(['-', '+']).next().unwrap_or(t);
            let parts: Vec<&str> = core.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_banners_parse() {
        assert_eq!(
            parse_version("codex-cli 0.154.0\n").as_deref(),
            Some("0.154.0")
        );
        assert_eq!(parse_version("codex v2.0.10").as_deref(), Some("2.0.10"));
        assert_eq!(
            parse_version("codex-cli 0.155.0-alpha.1").as_deref(),
            Some("0.155.0-alpha.1")
        );
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }

    #[test]
    fn shims_get_the_launcher_they_need() {
        assert_eq!(launcher_for(Path::new("codex.cmd")), Launcher::Batch);
        assert_eq!(launcher_for(Path::new("codex.BAT")), Launcher::Batch);
        assert_eq!(launcher_for(Path::new("codex.ps1")), Launcher::PowerShell);
        assert_eq!(launcher_for(Path::new("codex.exe")), Launcher::Direct);
        assert_eq!(launcher_for(Path::new("/usr/bin/codex")), Launcher::Direct);
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_is_named_with_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("gone").join("bin").join("codex");
        let link = dir.path().join("codex");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!is_runnable(&link));
        assert_eq!(dangling(&link), Some((link.clone(), target)));

        // A link to a directory that is gone, like install.ps1's `bin`.
        let bin = dir.path().join("bin");
        std::os::unix::fs::symlink(dir.path().join("gone"), &bin).unwrap();
        assert_eq!(dangling(&bin), Some((bin, dir.path().join("gone"))));

        // A plain file, or nothing at all, is not a dangling link.
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        assert_eq!(dangling(&plain), None);
        assert_eq!(dangling(&dir.path().join("missing")), None);
    }

    #[test]
    fn a_missing_explicit_binary_is_reported() {
        let err = CodexBin::from_path(PathBuf::from("definitely/not/here")).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }
}
