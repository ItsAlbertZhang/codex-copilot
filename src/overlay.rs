//! The profile overlay: `$CODEX_HOME/<profile>.config.toml`.
//!
//! Codex layers this file on top of `config.toml` as a second `user` layer
//! (precedence 21 vs 20) only when `--profile <name>` is passed, and the two
//! are deep-merged per key: tables recurse, scalars and arrays replace. So
//! nothing here deletes anything from the base config - but the one array we
//! write, `shell_environment_policy.exclude`, does replace a base one.

use std::path::Path;

use crate::{PROVIDER_ID, TOKEN_ENV};

/// Everything the overlay text depends on.
#[derive(Debug, Clone)]
pub struct Params<'a> {
    pub profile: &'a str,
    pub model: &'a str,
    pub auto_review: bool,
    pub host: &'a str,
    pub codex_version: &'a str,
    pub catalog_path: &'a Path,
    pub calibrated: usize,
}

/// TOML basic-string escaping; the only characters we can meet are backslashes
/// (Windows paths) and quotes.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Renders the overlay. Deterministic: same params, same bytes, so re-running
/// `install` with the same inputs rewrites the file identically.
pub fn render(p: &Params) -> String {
    let id = PROVIDER_ID;
    format!(
        r#"# codex-copilot {installer} - Codex {codex} against GitHub Copilot CAPI over
# stateful ws:/responses. Managed file: `codex-copilot install` rewrites it.
# Active only for `codex --profile {profile}`; config.toml is never touched.
# Roll back with `codex-copilot uninstall`, or delete this file.

model = "{model}"
model_provider = "{id}"
# {calibrated} entries recalibrated to this seat's real CAPI limits. A local
# catalog also stops Codex polling /models, which it cannot parse from CAPI.
model_catalog_json = "{catalog}"
# CAPI does not stream reasoning summaries.
model_reasoning_summary = "none"
check_for_update_on_startup = false
{reviewer_config}

[model_providers.{id}]
# "OpenAI" is the literal Codex checks to enable remote compaction v2.
name = "OpenAI"
base_url = "{host}"
wire_api = "responses"
# The transport becomes wss://<host>/responses.
supports_websockets = true
stream_idle_timeout_ms = 300000
# Also bounds the first turn's wait for the startup prewarm. Do not shorten.
websocket_connect_timeout_ms = 10000
stream_max_retries = 5
# The bearer is read from the environment at request time; no token is stored.
env_key = "{TOKEN_ENV}"

# Identity headers the gateway expects; values mirror the official Copilot CLI.
[model_providers.{id}.http_headers]
"copilot-integration-id" = "{integration}"
"editor-version" = "{editor}"
"editor-plugin-version" = "{plugin}"
"x-github-api-version" = "{api_version}"
"openai-intent" = "{intent}"
"x-interaction-type" = "{interaction}"
# Static only: "user" over-reports premium counts, which is the safe direction.
"x-initiator" = "{initiator}"

[features]
remote_compaction_v2 = true
# name = "OpenAI" would otherwise enable image generation, which CAPI does not
# serve, and the plugin keys make the app-server fetch chatgpt.com at startup.
image_generation = false
plugins = false
remote_plugin = false
recommended_plugins = false
tool_suggest = false
apps = false

# Outbound telemetry off. Critical with name = "OpenAI": an enabled exporter
# would POST to an openai.com host while the process holds CAPI credentials.
[analytics]
enabled = false

[otel]
exporter = "none"
trace_exporter = "none"
metrics_exporter = "none"

[feedback]
enabled = false

# Keeps the token out of every subprocess Codex spawns for a tool call.
# ignore_default_excludes defaults to true, so this list is the only filter;
# being an array, it REPLACES any exclude list in your base config.toml.
[shell_environment_policy]
exclude = ["{TOKEN_ENV}"]
"#,
        installer = env!("CARGO_PKG_VERSION"),
        codex = esc(p.codex_version),
        profile = esc(p.profile),
        model = esc(p.model),
        reviewer_config = if p.auto_review {
            "# Use the approval model selected in the local catalog.\napprovals_reviewer = \"auto_review\""
        } else {
            ""
        },
        host = esc(p.host.trim_end_matches('/')),
        catalog = esc(&p.catalog_path.display().to_string()),
        calibrated = p.calibrated,
        integration = crate::capi::INTEGRATION_ID,
        editor = crate::capi::EDITOR_VERSION,
        plugin = crate::capi::EDITOR_PLUGIN_VERSION,
        api_version = crate::capi::API_VERSION,
        intent = crate::capi::OPENAI_INTENT,
        interaction = crate::capi::INTERACTION_TYPE,
        initiator = crate::capi::INITIATOR,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use toml::Value;

    fn rendered() -> (String, Value) {
        let path = PathBuf::from(r"C:\Users\x\.codex\copilot_config_toml\models-catalog.json");
        let text = render(&Params {
            profile: "copilot",
            model: "gpt-6-astra",
            auto_review: false,
            host: "https://api.enterprise.githubcopilot.com/",
            codex_version: "0.154.0",
            catalog_path: &path,
            calibrated: 7,
        });
        let parsed: Value = toml::from_str(&text).expect("overlay must parse as TOML");
        (text, parsed)
    }

    #[test]
    fn the_overlay_has_exactly_the_documented_keys() {
        let (_, doc) = rendered();
        assert_eq!(doc["model"].as_str(), Some("gpt-6-astra"));
        assert_eq!(doc["model_provider"].as_str(), Some("copilot"));
        assert_eq!(doc["model_reasoning_summary"].as_str(), Some("none"));
        assert_eq!(doc["check_for_update_on_startup"].as_bool(), Some(false));
        assert!(doc.get("approvals_reviewer").is_none());
        assert!(doc["model_catalog_json"]
            .as_str()
            .unwrap()
            .ends_with("models-catalog.json"));

        let p = &doc["model_providers"]["copilot"];
        assert_eq!(p["name"].as_str(), Some("OpenAI"));
        // The trailing slash is trimmed, or every URL Codex builds doubles it.
        assert_eq!(
            p["base_url"].as_str(),
            Some("https://api.enterprise.githubcopilot.com")
        );
        assert_eq!(p["wire_api"].as_str(), Some("responses"));
        assert_eq!(p["supports_websockets"].as_bool(), Some(true));
        assert_eq!(p["stream_idle_timeout_ms"].as_integer(), Some(300_000));
        assert_eq!(p["websocket_connect_timeout_ms"].as_integer(), Some(10_000));
        assert_eq!(p["stream_max_retries"].as_integer(), Some(5));
        assert_eq!(p["env_key"].as_str(), Some("COPILOT_GITHUB_TOKEN"));
        // env_key is mutually exclusive with an auth block; there must be none.
        assert!(p.get("auth").is_none());
        assert!(p.get("experimental_bearer_token").is_none());

        let h = &p["http_headers"];
        assert_eq!(
            h["copilot-integration-id"].as_str(),
            Some("copilot-developer-cli")
        );
        assert_eq!(
            h["editor-version"].as_str(),
            Some("copilot/1.0.84-canary.70")
        );
        assert_eq!(
            h["editor-plugin-version"].as_str(),
            Some("copilot-cli/1.0.84")
        );
        assert_eq!(h["x-github-api-version"].as_str(), Some("2026-08-01"));
        assert_eq!(h["openai-intent"].as_str(), Some("conversation-agent"));
        assert_eq!(h["x-interaction-type"].as_str(), Some("conversation-agent"));
        assert_eq!(h["x-initiator"].as_str(), Some("user"));

        assert_eq!(
            doc["features"]["remote_compaction_v2"].as_bool(),
            Some(true)
        );
        for off in [
            "image_generation",
            "plugins",
            "remote_plugin",
            "recommended_plugins",
            "tool_suggest",
            "apps",
        ] {
            assert_eq!(doc["features"][off].as_bool(), Some(false), "{off}");
        }
        assert_eq!(doc["analytics"]["enabled"].as_bool(), Some(false));
        assert_eq!(doc["feedback"]["enabled"].as_bool(), Some(false));
        for k in ["exporter", "trace_exporter", "metrics_exporter"] {
            assert_eq!(doc["otel"][k].as_str(), Some("none"), "{k}");
        }
        assert_eq!(
            doc["shell_environment_policy"]["exclude"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str),
            Some("COPILOT_GITHUB_TOKEN")
        );
    }

    #[test]
    fn windows_paths_survive_toml_escaping() {
        let (text, doc) = rendered();
        assert!(
            text.contains(r"C:\\Users\\x\\.codex"),
            "backslashes must be doubled"
        );
        assert_eq!(
            doc["model_catalog_json"].as_str(),
            Some(r"C:\Users\x\.codex\copilot_config_toml\models-catalog.json")
        );
    }

    #[test]
    fn no_token_and_no_helper_command_can_appear_in_the_file() {
        let (text, _) = rendered();
        assert!(
            !text.contains("command ="),
            "no auth helper is spawned any more"
        );
        assert!(!text.contains("keyring"));
        // The only mention of the variable is by name, never by value.
        assert_eq!(text.matches("COPILOT_GITHUB_TOKEN").count(), 2);
    }

    #[test]
    fn rendering_is_deterministic() {
        let (a, _) = rendered();
        let (b, _) = rendered();
        assert_eq!(a, b);
    }
}
