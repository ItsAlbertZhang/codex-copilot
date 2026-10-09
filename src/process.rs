//! Detect running Codex processes (a directory swap under a live Codex is
//! unsafe: Windows refuses to rename directories with open files, and an
//! app-server keeps writing into the old directory).
//!
//! Shells out to the platform's process lister instead of linking a process
//! crate; the parsing lives in pure functions so it is tested on captured
//! output.

use std::process::Command;

use anyhow::{bail, Context, Result};

/// `ps` arguments on Unix. `-A -o`: the POSIX spelling of `-axo` (`-x` is a
/// BSD option), which both the macOS and the procps `ps` accept. `-ww`: no
/// width limit. macOS prints the full executable path as `comm` and cuts it
/// at the terminal width, which it takes from `$COLUMNS` or any descriptor
/// that is a terminal (stdin is inherited) even with stdout piped; a cut
/// path's basename would no longer read `codex`.
const PS_ARGS: &[&str] = &["-ww", "-A", "-o", "pid=,comm="];

/// The Codex binaries a swap must not run under: the CLI (whose `app-server`
/// subcommand serves the desktop app) and the standalone
/// app-server Codex 0.160 also ships.
const CODEX_BINARIES: &[&str] = &["codex", "codex-app-server"];

/// Targets Codex publishes release binaries for. People run the downloaded
/// asset, `<binary>-<triple>[.exe]`, without renaming it.
const RELEASE_TRIPLES: &[&str] = &[
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
];

/// Linux keeps only this many bytes of an executable's name as `comm`
/// (`TASK_COMM_LEN` less the terminating NUL), which is what `ps` prints.
const COMM_LEN: usize = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
}

/// Processes running a Codex binary ([`is_codex_image`]).
pub fn running_codex() -> Result<Vec<ProcessInfo>> {
    if cfg!(windows) {
        list("tasklist", &["/FO", "CSV", "/NH"]).map(|out| parse_tasklist_csv(&out))
    } else {
        list("ps", PS_ARGS).map(|out| parse_ps(&out))
    }
}

/// Runs a process lister and returns its stdout. A missing tool or a
/// non-zero exit is an error; the caller decides whether that blocks a swap.
fn list(program: &str, args: &[&str]) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        // The service binary has no console; without this flag every call
        // would flash one up.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().with_context(|| {
        format!("could not run `{program}` to look for running Codex processes")
    })?;
    if !output.status.success() {
        bail!(
            "`{program}` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    // tasklist prints in the OEM code page; image names we care about are
    // ASCII, so a lossy decode is enough.
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `name`, an image name or the last component of an executable's
/// path, is a Codex binary ([`CODEX_BINARIES`]): `codex[.exe]` or
/// `codex-app-server[.exe]`, a release asset `<binary>-<triple>[.exe]`
/// ([`RELEASE_TRIPLES`]), or one of those cut to Linux's [`COMM_LEN`].
/// Case-insensitive everywhere: a case-insensitive file system (macOS,
/// Windows) runs `Codex` as well. Never matches `codex-copilot*` (this tool
/// and its relay) or helpers Codex starts for itself, such as
/// `codex-code-mode-host`.
pub fn is_codex_image(name: &str) -> bool {
    codex_images().any(|image| {
        image.eq_ignore_ascii_case(name)
            || (image.len() > COMM_LEN
                && name.len() == COMM_LEN
                && image[..COMM_LEN].eq_ignore_ascii_case(name))
    })
}

/// Every full image name [`is_codex_image`] accepts.
fn codex_images() -> impl Iterator<Item = String> {
    CODEX_BINARIES
        .iter()
        .flat_map(|binary| {
            std::iter::once((*binary).to_owned()).chain(
                RELEASE_TRIPLES
                    .iter()
                    .map(move |triple| format!("{binary}-{triple}")),
            )
        })
        .flat_map(|stem| [format!("{stem}.exe"), stem])
}

/// Codex rows of `tasklist /FO CSV /NH`:
/// `"codex.exe","1234","Console","1","123,456 K"`.
pub fn parse_tasklist_csv(output: &str) -> Vec<ProcessInfo> {
    output
        .lines()
        .filter_map(|line| {
            let fields = csv_fields(line.trim());
            let (name, pid) = (fields.first()?, fields.get(1)?);
            if !is_codex_image(name) {
                return None;
            }
            Some(ProcessInfo {
                pid: pid.trim().parse().ok()?,
                name: name.clone(),
            })
        })
        .collect()
}

/// Splits one CSV record: fields may be quoted, quotes inside a quoted field
/// are doubled, and the memory column contains a comma.
fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    fields.push(field);
    fields
}

/// Codex rows of `ps -ww -A -o pid=,comm=` ([`PS_ARGS`]):
/// `  1234 /usr/local/bin/codex`. macOS prints the whole executable path
/// (which may contain spaces), Linux the bare name truncated to
/// [`COMM_LEN`]; either way the basename must be a Codex image
/// ([`is_codex_image`]).
pub fn parse_ps(output: &str) -> Vec<ProcessInfo> {
    output
        .lines()
        .filter_map(|line| {
            let (pid, comm) = line.trim().split_once(char::is_whitespace)?;
            let comm = comm.trim();
            let name = comm.rsplit('/').next().unwrap_or(comm);
            if !is_codex_image(name) {
                return None;
            }
            Some(ProcessInfo {
                pid: pid.parse().ok()?,
                name: name.to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_images_are_codex_and_the_release_assets() {
        for name in ["codex", "codex.exe", "CODEX.EXE", "Codex"] {
            assert!(is_codex_image(name), "{name}");
        }
        for triple in RELEASE_TRIPLES {
            let asset = format!("codex-{triple}");
            for name in [
                asset.clone(),
                format!("{asset}.exe"),
                asset.to_uppercase(),
                // What Linux `ps` prints for it.
                asset[..COMM_LEN].to_owned(),
                asset[..COMM_LEN].to_uppercase(),
            ] {
                assert!(is_codex_image(&name), "{name}");
            }
            // Only a cut at exactly the `comm` length is a truncation.
            assert!(!is_codex_image(&asset[..COMM_LEN - 1]), "{asset}");
            assert!(!is_codex_image(&asset[..COMM_LEN + 1]), "{asset}");
        }
        assert!(is_codex_image("codex-x86_64-un"));
        assert!(is_codex_image("codex-aarch64-p"));
    }

    #[test]
    fn the_standalone_app_server_is_codex_too() {
        for name in [
            "codex-app-server",
            "codex-app-server.exe",
            "CODEX-APP-SERVER.EXE",
            "Codex-App-Server",
            // What Linux `ps` prints for it: 16 characters cut to 15.
            "codex-app-serve",
            "CODEX-APP-SERVE",
        ] {
            assert!(is_codex_image(name), "{name}");
        }
        for triple in RELEASE_TRIPLES {
            let asset = format!("codex-app-server-{triple}");
            for name in [
                asset.clone(),
                format!("{asset}.exe"),
                asset.to_uppercase(),
                asset[..COMM_LEN].to_owned(),
            ] {
                assert!(is_codex_image(&name), "{name}");
            }
            assert!(!is_codex_image(&asset[..COMM_LEN - 1]), "{asset}");
            // One character longer than `comm` is the bare binary's name.
            assert_eq!(&asset[..COMM_LEN + 1], "codex-app-server");
        }
        for name in [
            "codex-app-serv",
            "codex-app-server-",
            "codex-app-server-old.exe",
            "codex-app-server-x86_64-pc-windows-msvc-old.exe",
            "my-codex-app-server",
            "codex-app",
        ] {
            assert!(!is_codex_image(name), "{name}");
        }
    }

    #[test]
    fn codex_images_exclude_this_tool_and_codex_helpers() {
        for name in [
            "codex-copilot",
            "codex-copilot.exe",
            "codex-copilot-serve",
            "codex-copilot-serve.exe",
            "CODEX-COPILOT.EXE",
            // `comm` of codex-copilot-serve and codex-code-mode-host.
            "codex-copilot-s",
            "codex-code-mode",
            "codex-code-mode-host",
            "codex-code-mode-host.exe",
            "codex-x86_64-pc-windows-msvc-old.exe",
            "codex-",
            "codexd",
            "mycodex.exe",
            "codex.exe.bak",
            "node",
            "",
        ] {
            assert!(!is_codex_image(name), "{name}");
        }
    }

    #[test]
    fn tasklist_rows_match_codex_images_only() {
        let output = concat!(
            "\r\n",
            "\"System Idle Process\",\"0\",\"Services\",\"0\",\"8 K\"\r\n",
            "\"codex.exe\",\"1234\",\"Console\",\"1\",\"123,456 K\"\r\n",
            "\"CODEX.EXE\",\"88\",\"Console\",\"1\",\"1,024 K\"\r\n",
            "\"codex-copilot.exe\",\"2222\",\"Console\",\"1\",\"9,000 K\"\r\n",
            "\"codex-copilot-serve.exe\",\"3333\",\"Console\",\"1\",\"9,000 K\"\r\n",
            "\"codex-code-mode-host.exe\",\"3334\",\"Console\",\"1\",\"9,000 K\"\r\n",
            "\"node.exe\",\"4444\",\"Console\",\"1\",\"50,000 K\"\r\n",
            "\"mycodex.exe\",\"5555\",\"Console\",\"1\",\"1 K\"\r\n",
            "\"codex\",\"6666\",\"Console\",\"1\",\"1 K\"\r\n",
            "\"codex-x86_64-pc-windows-msvc.exe\",\"7777\",\"Console\",\"1\",\"1 K\"\r\n",
            "\"codex-app-server.exe\",\"8888\",\"Console\",\"1\",\"1 K\"\r\n",
            "\"codex-app-server-aarch64-pc-windows-msvc.exe\",\"9999\",\"Console\",\"1\",\"1 K\"\r\n",
        );
        let found = parse_tasklist_csv(output);
        let found: Vec<(u32, &str)> = found.iter().map(|p| (p.pid, p.name.as_str())).collect();
        assert_eq!(
            found,
            [
                (1234, "codex.exe"),
                (88, "CODEX.EXE"),
                (6666, "codex"),
                (7777, "codex-x86_64-pc-windows-msvc.exe"),
                (8888, "codex-app-server.exe"),
                (9999, "codex-app-server-aarch64-pc-windows-msvc.exe"),
            ]
        );
    }

    #[test]
    fn tasklist_noise_is_ignored() {
        // What tasklist prints when a filter matches nothing, plus junk.
        let output = "INFO: No tasks are running which match the specified criteria.\r\n\
                      \"codex.exe\",\"not-a-pid\"\r\n\
                      \"codex.exe\"\r\n";
        assert!(parse_tasklist_csv(output).is_empty());
        assert!(parse_tasklist_csv("").is_empty());
    }

    #[test]
    fn csv_fields_handle_quotes_and_commas() {
        assert_eq!(
            csv_fields(r#""a ""b""","1,2",c"#),
            vec![r#"a "b""#, "1,2", "c"]
        );
        assert_eq!(csv_fields(""), vec![""]);
    }

    #[test]
    fn ps_rows_match_codex_image_basenames_only() {
        let output = "    1 /sbin/launchd\n\
                      \x20 412 /usr/local/bin/codex\n\
                      \x20 413 codex\n\
                      \x20 414 /Applications/My App.app/Contents/MacOS/codex\n\
                      \x20 415 /Users/me/Downloads/codex-aarch64-apple-darwin\n\
                      \x20 416 codex-x86_64-un\n\
                      \x20 417 /usr/local/bin/Codex\n\
                      \x20 418 codex-app-serve\n\
                      \x20 419 /Applications/Codex.app/Contents/Resources/codex-app-server\n\
                      \x20 500 /usr/local/bin/codex-copilot\n\
                      \x20 501 codex-copilot-s\n\
                      \x20 502 /opt/codex/bin/node\n\
                      \x20 503 codexd\n\
                      \x20 504 codex-code-mode\n\
                      \x20 505 /Applications/Codex.app/Contents/Resources/codex-code-mode-host\n\
                      garbage\n";
        let found = parse_ps(output);
        let found: Vec<(u32, &str)> = found.iter().map(|p| (p.pid, p.name.as_str())).collect();
        assert_eq!(
            found,
            [
                (412, "codex"),
                (413, "codex"),
                (414, "codex"),
                (415, "codex-aarch64-apple-darwin"),
                (416, "codex-x86_64-un"),
                (417, "Codex"),
                (418, "codex-app-serve"),
                (419, "codex-app-server"),
            ]
        );
    }

    #[test]
    fn ps_asks_for_unlimited_width_for_long_macos_paths() {
        // Without -ww, macOS would cut a path like this at the terminal width.
        assert_eq!(PS_ARGS.first(), Some(&"-ww"));
        let long = format!(
            "  777 /Users/someone/Library/Application Support/{}/node_modules/@openai/codex/\
             vendor/aarch64-apple-darwin/codex/codex\n",
            "x".repeat(200)
        );
        let found = parse_ps(&long);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].pid, 777);
    }

    #[test]
    fn the_live_lister_runs() {
        // Only checks that the platform tool is there and its output parses;
        // whether a Codex happens to be running is not ours to assert.
        running_codex().unwrap();
    }
}
