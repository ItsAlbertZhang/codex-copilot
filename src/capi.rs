//! GitHub Copilot API: identity headers, and the `GET <host>/models` probe
//! that both discovers the seat's gateway and describes every model on it.
//!
//! There is no `copilot_internal/v2/token` exchange here. The Copilot CLI OAuth
//! app is refused (403) at that endpoint, and CAPI accepts the raw GitHub token
//! as a bearer, so the exchange is dead weight.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::blocking::{Client, RequestBuilder};
use serde::Deserialize;

// Identity values mirroring the official GitHub Copilot CLI. The gateway keys
// model policy, rate limits and billing off these, so they are not cosmetic.
pub const INTEGRATION_ID: &str = "copilot-developer-cli";
pub const EDITOR_VERSION: &str = "copilot/1.0.84-canary.70";
pub const EDITOR_PLUGIN_VERSION: &str = "copilot-cli/1.0.84";
pub const API_VERSION: &str = "2026-08-01";
pub const OPENAI_INTENT: &str = "conversation-agent";
pub const INTERACTION_TYPE: &str = "conversation-agent";
pub const INITIATOR: &str = "user";
pub const USER_AGENT: &str = "copilot-cli/1.0.84";

/// The `supported_endpoints` entry Codex's WebSocket transport needs.
pub const WS_RESPONSES: &str = "ws:/responses";

/// Origins probed in order when `--host` was not given, most specific first.
/// `api.githubcopilot.com` is the catch-all and comes last.
pub const DEFAULT_HOSTS: [&str; 4] = [
    "https://api.enterprise.githubcopilot.com",
    "https://api.business.githubcopilot.com",
    "https://api.individual.githubcopilot.com",
    "https://api.githubcopilot.com",
];

/// The client for requests to a CAPI host, or to the GitHub OAuth base of the
/// device flow.
pub fn client() -> Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .context("could not build an HTTPS client")
}

/// The headers every CAPI request carries, bearer included.
fn identity(rb: RequestBuilder, token: &str) -> RequestBuilder {
    rb.bearer_auth(token)
        .header("accept", "application/json")
        .header("copilot-integration-id", INTEGRATION_ID)
        .header("editor-version", EDITOR_VERSION)
        .header("editor-plugin-version", EDITOR_PLUGIN_VERSION)
        .header("x-github-api-version", API_VERSION)
        .header("openai-intent", OPENAI_INTENT)
        .header("x-interaction-type", INTERACTION_TYPE)
        .header("x-initiator", INITIATOR)
}

/// What one `/models` entry says about a model, reduced to what `status`
/// acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelFacts {
    pub policy: Option<String>,
    /// Advertises `ws:/responses`.
    pub ws: bool,
}

impl ModelFacts {
    /// A missing policy block is an implicit pass, not a refusal.
    pub fn policy_ok(&self) -> bool {
        self.policy
            .as_deref()
            .is_none_or(|s| s.eq_ignore_ascii_case("enabled"))
    }
}

/// Model facts keyed by model id.
pub type Facts = BTreeMap<String, ModelFacts>;

#[derive(Debug, Deserialize)]
struct Entry {
    id: String,
    #[serde(default)]
    supported_endpoints: Vec<String>,
    #[serde(default)]
    policy: Option<Policy>,
}

#[derive(Debug, Deserialize)]
struct Policy {
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<Entry>,
}

/// Parses a `/models` body into per-model facts. Errors when the body is not a
/// models listing, which is how a captive portal or a 404 page is caught.
pub fn parse_models(body: &str) -> Result<Facts> {
    let parsed: ModelsResponse =
        serde_json::from_str(body).context("response is not a CAPI /models listing")?;
    anyhow::ensure!(!parsed.data.is_empty(), "/models returned an empty list");
    let mut out = Facts::new();
    for e in parsed.data {
        out.insert(
            e.id,
            ModelFacts {
                policy: e.policy.and_then(|p| p.state),
                ws: e.supported_endpoints.iter().any(|s| s == WS_RESPONSES),
            },
        );
    }
    Ok(out)
}

/// One host tried by the probe.
#[derive(Debug)]
pub struct Probe {
    pub host: String,
    /// HTTP status, absent when the request never completed.
    pub status: Option<u16>,
    /// Why this host was rejected. Empty on success.
    pub note: String,
    pub facts: Option<Facts>,
}

impl Probe {
    pub fn ok(&self) -> bool {
        self.facts.is_some()
    }
    /// `200` / `401` / `connect failed`, for the probe table.
    pub fn verdict(&self) -> String {
        match (self.status, self.ok()) {
            (Some(s), true) => format!("{s} ok"),
            (Some(s), false) => format!("{s} {}", self.note),
            (None, _) => self.note.clone(),
        }
    }
}

/// `GET <host>/models` with the identity headers and the raw GitHub token,
/// through [`client`].
pub fn probe_host(host: &str, token: &str) -> Probe {
    let host = host.trim_end_matches('/').to_string();
    let url = format!("{host}/models");
    let client = match client() {
        Ok(client) => client,
        Err(e) => {
            return Probe {
                host,
                status: None,
                note: format!("{e:#}"),
                facts: None,
            }
        }
    };
    let resp = match identity(client.get(&url), token).send() {
        Ok(r) => r,
        Err(e) => {
            return Probe {
                host,
                status: None,
                note: format!("request failed: {e}"),
                facts: None,
            }
        }
    };
    let status = resp.status().as_u16();
    let body = resp.text().unwrap_or_default();
    if status != 200 {
        return Probe {
            host,
            status: Some(status),
            note: "not this seat's gateway".to_string(),
            facts: None,
        };
    }
    match parse_models(&body) {
        Ok(facts) => Probe {
            host,
            status: Some(status),
            note: String::new(),
            facts: Some(facts),
        },
        Err(e) => Probe {
            host,
            status: Some(status),
            note: format!("{e}"),
            facts: None,
        },
    }
}

/// Walks `hosts` in preference order and stops at the first one that answers
/// 200 with a parseable model list. Every attempt is returned, so the failures
/// can be reported when none succeeds.
pub fn discover(hosts: &[String], token: &str) -> Vec<Probe> {
    let mut out = Vec::new();
    for host in hosts {
        let probe = probe_host(host, token);
        let done = probe.ok();
        out.push(probe);
        if done {
            break;
        }
    }
    out
}

/// Index of the first usable host. The candidate list is already in preference
/// order, so "first usable" is the preference.
pub fn pick(probes: &[Probe]) -> Option<usize> {
    probes.iter().position(Probe::ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_entry_says_its_policy_and_websocket_support() {
        let body = r#"{"object":"list","data":[
            {"id":"gpt-6-astra","supported_endpoints":["/responses","ws:/responses"],
             "policy":{"state":"enabled"}},
            {"id":"gpt-5.4-mini","supported_endpoints":["/responses"],
             "policy":{"state":"disabled"}},
            {"id":"bare-model"}]}"#;
        let f = parse_models(body).unwrap();
        assert!(f["gpt-6-astra"].ws && f["gpt-6-astra"].policy_ok());
        assert!(!f["gpt-5.4-mini"].ws && !f["gpt-5.4-mini"].policy_ok());
        // No policy block means no policy gate, not a refusal.
        assert!(f["bare-model"].policy_ok());
    }

    #[test]
    fn non_listings_are_rejected() {
        assert!(parse_models("<html>sign in</html>").is_err());
        assert!(parse_models(r#"{"data":[]}"#).is_err());
    }

    #[test]
    fn the_first_usable_host_wins_and_failures_do_not_count() {
        let probe = |host: &str, status: Option<u16>, ok: bool| Probe {
            host: host.to_string(),
            status,
            note: if ok { String::new() } else { "no".into() },
            facts: ok.then(Facts::new),
        };
        let probes = vec![
            probe("https://a", Some(401), false),
            probe("https://b", None, false),
            probe("https://c", Some(200), true),
            probe("https://d", Some(200), true),
        ];
        assert_eq!(pick(&probes), Some(2));
        assert_eq!(probes[2].host, "https://c");
        assert_eq!(probes[0].verdict(), "401 no");
        assert_eq!(probes[1].verdict(), "no");
        assert_eq!(probes[2].verdict(), "200 ok");
        assert_eq!(pick(&probes[..2]), None);
    }

    #[test]
    fn the_default_host_order_is_most_specific_first() {
        assert_eq!(DEFAULT_HOSTS[0], "https://api.enterprise.githubcopilot.com");
        assert_eq!(DEFAULT_HOSTS[3], "https://api.githubcopilot.com");
    }
}
