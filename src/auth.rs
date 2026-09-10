//! Getting a GitHub OAuth token, and telling the user how to keep it.
//!
//! This module knows nothing about Codex: it produces a token string and
//! prints instructions. Nothing here writes the token to disk, to the
//! registry, or to a credential store - the environment variable is the
//! user's to set.

use std::io::{IsTerminal, Read, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::TOKEN_ENV;

/// The GitHub Copilot CLI OAuth app. Public by construction (a client id in a
/// device flow is not a secret) and already approved for Copilot seats.
pub const CLIENT_ID: &str = "Ov23ctDVkRmgkPke0Mmm";
/// Everything CAPI needs from the token is that it belongs to the seat holder.
pub const SCOPE: &str = "read:user";
pub const GITHUB_OAUTH: &str = "https://github.com";

/// Where a token came from, for the one line that says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Flag,
    Stdin,
    Environment,
    DeviceFlow,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Flag => "--token",
            Source::Stdin => "--token-stdin",
            Source::Environment => TOKEN_ENV,
            Source::DeviceFlow => "GitHub device flow",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub value: String,
    pub source: Source,
}

/// `gho_` prefix and length only. Never the token.
pub fn describe(token: &str) -> String {
    let prefix: String = token.chars().take(4).collect();
    format!("{}... ({} chars)", prefix, token.chars().count())
}

/// Resolves a token without asking the network, in precedence order:
/// `--token`, `--token-stdin`, then `$COPILOT_GITHUB_TOKEN`.
pub fn from_inputs(flag: Option<&str>, stdin: bool) -> Result<Option<Token>> {
    if let Some(raw) = flag {
        return Ok(Some(Token {
            value: check(raw)?,
            source: Source::Flag,
        }));
    }
    if stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("could not read the token from stdin")?;
        return Ok(Some(Token {
            value: check(&buf)?,
            source: Source::Stdin,
        }));
    }
    match std::env::var(TOKEN_ENV) {
        Ok(v) if !v.trim().is_empty() => Ok(Some(Token {
            value: check(&v)?,
            source: Source::Environment,
        })),
        _ => Ok(None),
    }
}

fn check(raw: &str) -> Result<String> {
    let t = raw.trim();
    if t.is_empty() {
        bail!("the supplied GitHub token is empty");
    }
    if t.contains(char::is_whitespace) {
        bail!("the supplied GitHub token contains whitespace");
    }
    Ok(t.to_string())
}

/// The token itself, once, plus the exact command that makes it stick. The
/// user runs that command; this tool does not.
pub fn print_token(token: &Token) {
    let mut out = std::io::stdout();
    let _ = writeln!(
        out,
        "\nGitHub token ({}), shown once:\n",
        token.source.as_str()
    );
    let _ = writeln!(out, "    {}={}", TOKEN_ENV, token.value);
    let _ = writeln!(
        out,
        "\nNothing on disk holds it. Set it as a user environment variable yourself:\n"
    );
    for line in set_commands(&token.value) {
        let _ = writeln!(out, "    {line}");
    }
    let _ = writeln!(
        out,
        "\nThen open a new shell so the variable is in the environment `codex` inherits."
    );
    let _ = out.flush();
}

/// The per-shell one-liners that set the variable for the current user.
pub fn set_commands(token: &str) -> Vec<String> {
    vec![
        format!(
            "PowerShell   [Environment]::SetEnvironmentVariable(\"{TOKEN_ENV}\", \"{token}\", \"User\")"
        ),
        format!("bash         echo 'export {TOKEN_ENV}={token}' >> ~/.profile"),
        format!("zsh          echo 'export {TOKEN_ENV}={token}' >> ~/.zshrc"),
    ]
}

/// The matching removal one-liners, printed by `uninstall`.
pub fn unset_commands() -> Vec<String> {
    vec![
        format!(
            "PowerShell   [Environment]::SetEnvironmentVariable(\"{TOKEN_ENV}\", $null, \"User\")"
        ),
        format!("bash         remove the `export {TOKEN_ENV}=...` line from ~/.profile"),
        format!("zsh          remove the `export {TOKEN_ENV}=...` line from ~/.zshrc"),
    ]
}

// ---------------------------------------------------------------------------
// Device flow
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AccessToken {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Standard GitHub OAuth device flow. Prompts on stderr so stdout stays
/// reserved for the result, and polls until the code is approved.
pub fn device_flow(client: &Client, oauth_base: &str, client_id: &str) -> Result<Token> {
    let base = oauth_base.trim_end_matches('/');
    let start_url = format!("{base}/login/device/code");
    let resp = client
        .post(&start_url)
        .header("accept", "application/json")
        .form(&[("client_id", client_id), ("scope", SCOPE)])
        .send()
        .with_context(|| format!("POST {start_url} failed (network or proxy?)"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("POST {start_url} returned HTTP {status} ({})", shape(&body));
    }
    // The body carries `device_code`, which is bearer-equivalent for the life
    // of the flow, so only its shape may appear in an error message.
    let start: DeviceCode = serde_json::from_str(&body).with_context(|| {
        format!(
            "POST {start_url} returned no device code ({})",
            shape(&body)
        )
    })?;

    let mut err = std::io::stderr();
    let _ = writeln!(err, "\n  Open       {}", start.verification_uri);
    let _ = writeln!(err, "  Enter code {}", start.user_code);
    let _ = writeln!(
        err,
        "  Scope      {SCOPE}   (app {client_id})\n  Waiting for approval; Ctrl-C aborts."
    );
    let _ = err.flush();

    let token_url = format!("{base}/login/oauth/access_token");
    let mut interval = Duration::from_secs(start.interval.unwrap_or(5).max(1));
    let deadline = Instant::now() + Duration::from_secs(start.expires_in.unwrap_or(900));
    loop {
        if Instant::now() >= deadline {
            bail!("the device code expired before it was approved");
        }
        sleep(interval);
        // Every transient failure below retries until the deadline: a 502 page
        // or a reset connection must not throw away a code the user may have
        // approved already.
        let Ok(resp) = client
            .post(&token_url)
            .header("accept", "application/json")
            .form(&[
                ("client_id", client_id),
                ("device_code", start.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
        else {
            continue;
        };
        let status = resp.status();
        if status.is_server_error() {
            continue;
        }
        let body = resp.text().unwrap_or_default();
        let parsed: AccessToken = match serde_json::from_str(&body) {
            Ok(p) => p,
            Err(_) if status.is_client_error() => {
                bail!("POST {token_url} returned HTTP {status} ({})", shape(&body))
            }
            Err(_) => continue,
        };
        if let Some(value) = parsed.access_token.filter(|t| !t.is_empty()) {
            if std::io::stderr().is_terminal() {
                let _ = writeln!(std::io::stderr(), "  Approved.");
            }
            return Ok(Token {
                value,
                source: Source::DeviceFlow,
            });
        }
        match parsed.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
            Some("expired_token") => bail!("the device code expired before it was approved"),
            Some("access_denied") => bail!("the authorization request was denied on github.com"),
            Some(other) => bail!(
                "device flow failed: {other} {}",
                parsed.error_description.unwrap_or_default()
            ),
            None => bail!("device flow returned neither a token nor an error"),
        }
    }
}

/// Describes a body without quoting it: an OAuth payload may hold a
/// `device_code` or an `access_token`.
fn shape(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(map)) => format!(
            "{} bytes of JSON, keys: {}",
            body.len(),
            map.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
        _ => format!("{} bytes, not a JSON object", body.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_shows_a_prefix_and_a_length_but_no_token() {
        let d = describe("gho_abcdefghijklmnop");
        assert!(d.starts_with("gho_"), "{d}");
        assert!(d.contains("20 chars"), "{d}");
        assert!(!d.contains("abcdefghij"), "{d}");
    }

    #[test]
    fn inputs_are_trimmed_and_validated() {
        let t = from_inputs(Some("  gho_x  "), false).unwrap().unwrap();
        assert_eq!(t.value, "gho_x");
        assert_eq!(t.source, Source::Flag);
        assert!(from_inputs(Some("   "), false).is_err());
        assert!(from_inputs(Some("gho a"), false).is_err());
    }

    #[test]
    fn the_set_and_unset_one_liners_name_the_variable() {
        let set = set_commands("gho_tok");
        assert!(set[0].contains("SetEnvironmentVariable") && set[0].contains("gho_tok"));
        assert!(set[1].contains("export COPILOT_GITHUB_TOKEN=gho_tok"));
        assert!(unset_commands()[0].contains("$null"));
    }

    #[test]
    fn body_shapes_never_quote_the_body() {
        let s = shape(r#"{"device_code":"secret","user_code":"AB-12"}"#);
        assert!(s.contains("keys: device_code, user_code"), "{s}");
        assert!(!s.contains("secret"), "{s}");
        assert!(shape("<html>").contains("not a JSON object"));
    }

    #[test]
    fn the_client_id_is_the_copilot_cli_app() {
        assert_eq!(CLIENT_ID, "Ov23ctDVkRmgkPke0Mmm");
        assert_eq!(SCOPE, "read:user");
    }
}
