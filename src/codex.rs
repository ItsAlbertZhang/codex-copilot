//! Locating the installed Codex CLI and asking it for its version.
//!
//! On Windows what sits on `PATH` is usually an npm shim (`codex.cmd` /
//! `codex.ps1`), never `codex.exe`. `Command::new("codex")` only ever appends
//! `.exe`, so the shim would be invisible - hence the explicit extension walk
//! and the per-extension launcher.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

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

    /// Walks `PATH` for `codex`, trying a fixed extension order.
    pub fn discover() -> Result<Self> {
        let path_var = std::env::var_os("PATH")
            .ok_or_else(|| anyhow!("PATH is not set; cannot find codex"))?;
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for ext in EXTS {
                let candidate = if ext.is_empty() {
                    dir.join("codex")
                } else {
                    dir.join(format!("codex.{ext}"))
                };
                if is_runnable(&candidate) {
                    return Ok(Self {
                        launcher: launcher_for(&candidate),
                        path: candidate,
                    });
                }
            }
        }
        bail!(
            "`codex` was not found on PATH. Install the Codex CLI first \
             (`npm install -g @openai/codex`) or pass --codex-bin <path>."
        )
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

    #[test]
    fn a_missing_explicit_binary_is_reported() {
        let err = CodexBin::from_path(PathBuf::from("definitely/not/here")).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }
}
