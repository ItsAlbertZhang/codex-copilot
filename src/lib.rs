//! `codex-copilot` 2.x - a local Responses relay between an installed OpenAI
//! Codex CLI and GitHub Copilot CAPI. It relays `/responses` over both of
//! Codex's transports: the WebSocket (`GET /responses` upgraded) and HTTP
//! SSE (`POST /responses`, which Codex falls back to for the rest of a
//! session after repeated WebSocket failures).
//!
//! Copilot's backend emits a different opaque `item.id` on every streamed
//! event of the same output item (and a different `response.id` on
//! `created` / `in_progress` / `completed`), which breaks Codex's assumption
//! that `output_item.added` and `output_item.done` share one id. No
//! configuration fixes that, so the relay rewrites ids on the way down, on
//! either transport.
//!
//! Module map:
//!
//! * [`proxy`] - the relay itself (axum + tokio-tungstenite): the WebSocket
//!   bridge, the HTTP SSE relay and a passthrough for everything else.
//! * [`daemon`] - run the relay in the foreground or as a detached
//!   background process (`start`), and find / stop a running one.
//! * [`home`] - the dedicated CODEX_HOME (`~/.codex-copilot`), its state
//!   file, and the `override` directory swap.
//! * [`config`] - the managed `config.toml` written into that home.
//! * [`process`] - detect running `codex` processes before a swap.
//! * [`auth`] / [`capi`] / [`codex`] - unchanged from 1.x: GitHub OAuth,
//!   CAPI identity headers and gateway probe, codex lookup.
//! * [`commands`] / [`cli`] - the subcommands.

pub mod auth;
pub mod capi;
pub mod cli;
pub mod codex;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod home;
pub mod process;
pub mod proxy;

/// The variable Codex reads the CAPI bearer from (`env_key` in config.toml).
/// The user owns it: nothing here ever writes it. Codex sends it to the relay
/// as `Authorization: Bearer`, and the relay forwards it unchanged.
pub const TOKEN_ENV: &str = "COPILOT_GITHUB_TOKEN";
/// `model_provider` id, and the `[model_providers.<id>]` table name.
pub const PROVIDER_ID: &str = "copilot";
/// Default `model` written to config.toml.
pub const DEFAULT_MODEL: &str = "gpt-6-astra";
/// Default `model_reasoning_effort` written to config.toml.
pub const DEFAULT_REASONING_EFFORT: &str = "ultra";
/// Default `model_context_window`. Codex clamps it to each model's
/// `max_context_window`, so this just means "use the model's maximum".
pub const DEFAULT_CONTEXT_WINDOW: i64 = 1_000_000;
/// Model the relay substitutes for `codex-auto-review` (the reviewer Codex
/// picks when no catalog override exists, which CAPI does not serve).
pub const DEFAULT_REVIEW_MODEL: &str = "gpt-6-luna";
/// The reviewer slug Codex sends without a catalog override.
pub const CODEX_AUTO_REVIEW: &str = "codex-auto-review";
/// Loopback address the relay listens on by default.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:12899";
/// Default upstream gateway when none is probed or given.
pub const DEFAULT_UPSTREAM: &str = "https://api.enterprise.githubcopilot.com";
/// Directory name of the dedicated Codex home, next to `~/.codex`.
pub const COPILOT_HOME_NAME: &str = ".codex-copilot";
/// Directory name of the regular Codex home.
pub const CODEX_HOME_NAME: &str = ".codex";
/// State file written into the dedicated home. It travels with the directory
/// through `override`, which is how the tool knows which directory is which.
pub const STATE_FILE: &str = "codex-copilot.json";
/// Prefix of relay-generated item ids. Deliberately contains no `_`: Codex
/// strips any item id without an `A_B` shape before sending history upstream
/// (`core/src/client.rs:1010`), so these ids never reach Copilot.
pub const ITEM_ID_PREFIX: &str = "copilot";

/// Validates a gateway given with `flag` (`--upstream`, `--host`, `--hosts`)
/// and returns its origin, `scheme://host[:port]` with no trailing slash.
///
/// The relay appends paths (`/responses`, `/models`) to this string, and
/// derives a `wss://` URL from it, so anything but a bare origin corrupts the
/// request URL: `https://host?x=1` would become `wss://host?x=1/responses`.
/// Refused: a scheme other than http(s), an empty host, credentials, a query,
/// a fragment, and a path other than `/`.
pub fn check_origin(flag: &str, raw: &str) -> anyhow::Result<String> {
    let reject = |why: &str| {
        anyhow::anyhow!(
            "{flag} must be an http(s) origin like {DEFAULT_UPSTREAM} ({why}), got {raw:?}"
        )
    };
    let url = reqwest::Url::parse(raw.trim()).map_err(|err| reject(&err.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(reject("the scheme must be http or https"));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(reject("the host is empty"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(reject("credentials are not allowed"));
    }
    if url.query().is_some() {
        return Err(reject("a query is not allowed"));
    }
    if url.fragment().is_some() {
        return Err(reject("a fragment is not allowed"));
    }
    if !matches!(url.path(), "" | "/") {
        return Err(reject("a path is not allowed"));
    }
    Ok(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::check_origin;

    #[test]
    fn an_origin_is_normalised_without_the_trailing_slash() {
        let ok = |raw: &str| check_origin("--upstream", raw).unwrap();
        assert_eq!(
            ok("https://api.githubcopilot.com"),
            "https://api.githubcopilot.com"
        );
        assert_eq!(
            ok(" https://api.githubcopilot.com/ "),
            "https://api.githubcopilot.com"
        );
        assert_eq!(ok("http://127.0.0.1:9"), "http://127.0.0.1:9");
        assert_eq!(ok("http://[::1]:9/"), "http://[::1]:9");
        assert_eq!(ok("HTTPS://Example.COM:8443"), "https://example.com:8443");
    }

    #[test]
    fn anything_but_a_bare_origin_is_refused() {
        for bad in [
            "",
            "   ",
            "api.githubcopilot.com",
            "ftp://example.com",
            "wss://example.com",
            "https://",
            "https://host?x=1",
            "https://host/?",
            "https://host/?x=1",
            "https://host#frag",
            "https://host/#",
            "https://user@host",
            "https://user:pw@host",
            "https://host/v1",
            "https://host//",
            "https://host:notaport",
            "https://ho st",
        ] {
            let err = check_origin("--upstream", bad).unwrap_err().to_string();
            assert!(err.contains("--upstream"), "{bad:?}: {err}");
            assert!(err.contains("http(s) origin"), "{bad:?}: {err}");
        }
    }
}
