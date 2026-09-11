//! The model catalog Codex bundles with a release, recalibrated against what
//! CAPI actually serves this seat.
//!
//! Codex resolves a model's window from its *catalog entry*: `context_window`
//! else `max_context_window`, with auto-compaction firing at
//! `min(auto_compact_token_limit, 90% of the window)`. The global
//! `model_context_window` / `model_auto_compact_token_limit` keys are clamped
//! to the entry's own `max_context_window`, so they can never raise a model
//! above what the bundled entry claims, and they apply to whichever model is
//! active. Rewriting the entries removes both limits at once.
//!
//! Pointing `model_catalog_json` at a local file has a second effect: Codex
//! then stops fetching `/models` itself, whose CAPI-shaped answer it cannot
//! parse (one error log every ~4m30s).

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::capi::Facts;

/// Auto-compaction fires at this fraction of each model's window. Fixed:
/// it is exactly Codex's own 90% ceiling, above which Codex clamps anyway.
pub const COMPACT_RATIO: f64 = 0.9;

/// The catalog bundled with a Codex release.
pub fn raw_url(version: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/openai/codex/rust-v{version}/codex-rs/models-manager/models.json"
    )
}

/// Which window a model is budgeted against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowChoice {
    /// Everything CAPI accepts. Above the standard tier this is billed at the
    /// long-context rate.
    #[default]
    Max,
    /// The standard-price tier ceiling.
    Base,
    /// An explicit count, clamped to what CAPI allows.
    Exact(u64),
}

/// `--model-window <slug>=<window>[:<compact>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    pub slug: String,
    pub window: u64,
    pub compact: Option<u64>,
}

/// One row of the calibration table, as recorded in `state.json`. Slugs and
/// token counts only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub slug: String,
    pub context_window: Option<u64>,
    pub max_context_window: Option<u64>,
    pub auto_compact_token_limit: Option<u64>,
    /// Largest prompt CAPI accepts (long-context tier when published).
    pub capi_max: Option<u64>,
    /// Standard-price ceiling; prompts above it cost roughly 2x.
    pub tier_base: Option<u64>,
    pub served: bool,
    pub policy: Option<String>,
    pub ws: bool,
}

impl Row {
    /// True when the entry was rewritten from CAPI's numbers.
    pub fn calibrated(&self) -> bool {
        self.served && self.capi_max.is_some()
    }
    /// True when this model can drift into the ~2x price tier.
    pub fn crosses_long_context(&self) -> bool {
        match (self.context_window, self.tier_base) {
            (Some(w), Some(b)) => w > b,
            _ => false,
        }
    }
}

/// A calibrated catalog, ready to be written verbatim.
#[derive(Debug, Clone)]
pub struct Calibrated {
    pub json: String,
    pub rows: Vec<Row>,
}

impl Calibrated {
    pub fn calibrated_count(&self) -> usize {
        self.rows.iter().filter(|r| r.calibrated()).count()
    }
    /// Slugs the bundled catalog has that this seat does not serve. They are
    /// left exactly as Codex shipped them.
    pub fn untouched(&self) -> Vec<&str> {
        self.rows
            .iter()
            .filter(|r| !r.calibrated())
            .map(|r| r.slug.as_str())
            .collect()
    }
}

/// `--context-window max|base|<tokens>`.
pub fn parse_window_choice(raw: &str) -> Result<WindowChoice> {
    let t = raw.trim();
    if t.eq_ignore_ascii_case("max") {
        return Ok(WindowChoice::Max);
    }
    if t.eq_ignore_ascii_case("base") {
        return Ok(WindowChoice::Base);
    }
    match t.replace('_', "").parse::<u64>() {
        Ok(n) if n >= 1000 => Ok(WindowChoice::Exact(n)),
        _ => bail!("invalid --context-window `{raw}`; expected max, base, or a count >= 1000"),
    }
}

/// `--model-window gpt-5.5=400000:320000`.
pub fn parse_override(raw: &str) -> Result<Override> {
    let (slug, rest) = raw.split_once('=').with_context(|| {
        format!("invalid --model-window `{raw}`; want <slug>=<window>[:<compact>]")
    })?;
    if slug.trim().is_empty() {
        bail!("invalid --model-window `{raw}`; the slug is empty");
    }
    let (window, compact) = match rest.split_once(':') {
        Some((w, c)) => (w, Some(c)),
        None => (rest, None),
    };
    let count = |s: &str, what: &str| -> Result<u64> {
        match s.trim().replace('_', "").parse::<u64>() {
            Ok(n) if n >= 1000 => Ok(n),
            _ => bail!("invalid --model-window `{raw}`; the {what} must be a count >= 1000"),
        }
    };
    Ok(Override {
        slug: slug.trim().to_string(),
        window: count(window, "window")?,
        compact: compact.map(|c| count(c, "compaction limit")).transpose()?,
    })
}

/// The window a calibrated entry gets, given what CAPI allows.
pub fn window_for(choice: WindowChoice, capi_max: u64, tier_base: Option<u64>) -> u64 {
    match choice {
        WindowChoice::Max => capi_max,
        WindowChoice::Base => tier_base.unwrap_or(capi_max).min(capi_max),
        WindowChoice::Exact(n) => n.min(capi_max),
    }
}

/// `floor(0.9 * window)`, kept strictly inside the window so auto-compaction
/// can still fire.
pub fn compact_for(window: u64) -> u64 {
    let raw = (window as f64 * COMPACT_RATIO).floor();
    let limit = if raw.is_finite() && raw >= 1.0 {
        raw as u64
    } else {
        1
    };
    limit.clamp(1, window.saturating_sub(1).max(1))
}

/// Downloads the bundled catalog. A failure here fails the install: there is
/// no fallback path any more.
pub fn download(client: &Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .with_context(|| format!("GET {url} failed (network or proxy?)"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("GET {url} returned HTTP {status}");
    }
    validate(&body)?;
    Ok(body)
}

/// Rejects anything that is not a `{"models": [...]}` document, so a proxy
/// login page never reaches `model_catalog_json`.
pub fn validate(json: &str) -> Result<usize> {
    let doc: Value = serde_json::from_str(json).context("catalog is not JSON")?;
    let models = doc
        .get("models")
        .and_then(Value::as_array)
        .context("catalog is not a Codex models.json (no `models` array)")?;
    if models.is_empty() {
        bail!("catalog has an empty `models` array");
    }
    Ok(models.len())
}

/// Rewrites every entry this seat serves so it states the real CAPI limits,
/// then applies `overrides`. Entries CAPI does not list are left alone, and no
/// field Codex does not define is ever added.
pub fn calibrate(
    json: &str,
    facts: &Facts,
    choice: WindowChoice,
    overrides: &[Override],
) -> Result<Calibrated> {
    let mut doc: Value = serde_json::from_str(json).context("catalog is not JSON")?;
    let models = doc
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .filter(|m| !m.is_empty())
        .context("catalog has no non-empty `models` array")?;

    let mut rows: Vec<Row> = Vec::with_capacity(models.len());
    for entry in models.iter_mut() {
        let Some(obj) = entry.as_object_mut() else {
            continue;
        };
        let Some(slug) = obj
            .get("slug")
            .or_else(|| obj.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let fact = facts.get(&slug);
        let mut row = Row {
            context_window: read_u64(obj, "context_window"),
            max_context_window: read_u64(obj, "max_context_window"),
            auto_compact_token_limit: read_u64(obj, "auto_compact_token_limit"),
            capi_max: fact.and_then(|f| f.capi_max),
            tier_base: fact.and_then(|f| f.tier_base),
            served: fact.is_some(),
            policy: fact.and_then(|f| f.policy.clone()),
            ws: fact.is_some_and(|f| f.ws),
            slug,
        };
        if let Some(capi_max) = row.capi_max {
            let window = window_for(choice, capi_max, row.tier_base);
            write_entry(obj, window, capi_max.max(window), compact_for(window));
            row.context_window = Some(window);
            row.max_context_window = Some(capi_max.max(window));
            row.auto_compact_token_limit = Some(compact_for(window));
        }
        rows.push(row);
    }

    for ov in overrides {
        let idx =
            rows.iter()
                .position(|r| r.slug == ov.slug)
                .with_context(|| {
                    format!(
                "--model-window {}=...: no catalog entry with that slug. This catalog has: {}",
                ov.slug,
                rows.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>().join(", ")
            )
                })?;
        let compact = ov.compact.unwrap_or_else(|| compact_for(ov.window));
        if compact >= ov.window {
            bail!(
                "--model-window {}={}:{compact}: the compaction limit must be below the window",
                ov.slug,
                ov.window
            );
        }
        if let Some(capi_max) = rows[idx].capi_max {
            if ov.window > capi_max {
                bail!(
                    "--model-window {}={}: CAPI accepts at most {capi_max} prompt tokens for this \
                     model on this seat, so every request would fail",
                    ov.slug,
                    ov.window
                );
            }
        }
        let max = rows[idx].capi_max.unwrap_or(ov.window).max(ov.window);
        let obj = models[idx]
            .as_object_mut()
            .context("catalog entry stopped being an object")?;
        write_entry(obj, ov.window, max, compact);
        rows[idx].context_window = Some(ov.window);
        rows[idx].max_context_window = Some(max);
        rows[idx].auto_compact_token_limit = Some(compact);
    }

    Ok(Calibrated {
        json: serde_json::to_string_pretty(&doc)? + "\n",
        rows,
    })
}

fn read_u64(obj: &Map<String, Value>, key: &str) -> Option<u64> {
    obj.get(key).and_then(Value::as_u64)
}

/// Select a real CAPI reviewer via Codex's native per-model override. Leave
/// slugs, model messages (including Guardian policy), and unserved entries alone.
/// Validate before replacing `catalog.json`, so failures cannot partially apply.
pub fn configure_auto_review(
    catalog: &mut Calibrated,
    facts: &Facts,
    reviewer: &str,
) -> Result<usize> {
    let mut doc: Value = serde_json::from_str(&catalog.json)?;
    let models = doc["models"]
        .as_array_mut()
        .context("catalog has no models")?;
    validate_auto_review(models, facts, reviewer)?;
    let mut count = 0;
    for model in models {
        if model["slug"]
            .as_str()
            .is_some_and(|slug| facts.contains_key(slug))
        {
            model["auto_review_model_override"] = Value::from(reviewer);
            count += 1;
        }
    }
    catalog.json = serde_json::to_string_pretty(&doc)? + "\n";
    Ok(count)
}

fn validate_auto_review(models: &[Value], facts: &Facts, reviewer: &str) -> Result<()> {
    let fact = facts
        .get(reviewer)
        .with_context(|| format!("{reviewer} is not served on this CAPI seat"))?;
    anyhow::ensure!(
        fact.policy_ok() && fact.ws,
        "{reviewer} must be enabled and support ws:/responses"
    );
    anyhow::ensure!(
        models.iter().any(|m| m["slug"].as_str() == Some(reviewer)),
        "{reviewer} is not in the Codex catalog"
    );
    for model in models {
        let Some(slug) = model["slug"].as_str() else {
            continue;
        };
        if !facts.contains_key(slug) {
            continue;
        }
        anyhow::ensure!(
            model.get("auto_review_model_override").is_some(),
            "catalog entry {slug} lacks auto_review_model_override; update Codex and use its \
             matching bundled catalog"
        );
    }
    Ok(())
}

fn write_entry(obj: &mut Map<String, Value>, window: u64, max: u64, compact: u64) {
    obj.insert("context_window".into(), Value::from(window));
    obj.insert("max_context_window".into(), Value::from(max));
    obj.insert("auto_compact_token_limit".into(), Value::from(compact));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capi::ModelFacts;

    fn facts() -> Facts {
        let mut f = Facts::new();
        f.insert(
            "gpt-6-astra".into(),
            ModelFacts {
                capi_max: Some(872_000),
                tier_base: Some(272_000),
                policy: Some("enabled".into()),
                ws: true,
            },
        );
        f.insert(
            "gpt-5.4-mini".into(),
            ModelFacts {
                capi_max: Some(272_000),
                tier_base: Some(272_000),
                policy: None,
                ws: true,
            },
        );
        f
    }

    const BUNDLED: &str = r#"{"models":[
        {"slug":"gpt-6-astra","context_window":272000,"max_context_window":872000,
         "auto_compact_token_limit":null,"tool_mode":"code_mode_only"},
        {"slug":"gpt-5.4-mini","context_window":272000,"max_context_window":272000},
        {"slug":"gpt-5.2","context_window":272000,"max_context_window":272000}]}"#;

    #[test]
    fn compaction_is_ninety_percent_and_stays_inside_the_window() {
        assert_eq!(compact_for(872_000), 784_800);
        assert_eq!(compact_for(272_000), 244_800);
        assert_eq!(compact_for(10), 9);
        // Never at or above the window, even at absurd sizes.
        assert_eq!(compact_for(1), 1);
        assert!(compact_for(1000) < 1000);
    }

    #[test]
    fn window_choice_picks_the_tier() {
        assert_eq!(
            window_for(WindowChoice::Max, 872_000, Some(272_000)),
            872_000
        );
        assert_eq!(
            window_for(WindowChoice::Base, 872_000, Some(272_000)),
            272_000
        );
        // No published base tier: `base` degrades to the CAPI ceiling.
        assert_eq!(window_for(WindowChoice::Base, 872_000, None), 872_000);
        // An explicit count is clamped to what CAPI accepts.
        assert_eq!(
            window_for(WindowChoice::Exact(400_000), 872_000, None),
            400_000
        );
        assert_eq!(
            window_for(WindowChoice::Exact(9_000_000), 872_000, None),
            872_000
        );
    }

    #[test]
    fn served_entries_are_rewritten_and_the_rest_are_left_alone() {
        let out = calibrate(BUNDLED, &facts(), WindowChoice::Max, &[]).unwrap();
        let doc: Value = serde_json::from_str(&out.json).unwrap();
        let astra = &doc["models"][0];
        assert_eq!(astra["context_window"], 872_000);
        assert_eq!(astra["max_context_window"], 872_000);
        assert_eq!(astra["auto_compact_token_limit"], 784_800);
        // Fields we do not own survive untouched.
        assert_eq!(astra["tool_mode"], "code_mode_only");
        // gpt-5.2 is not on this seat: byte-identical to what codex ships.
        assert_eq!(doc["models"][2]["context_window"], 272_000);
        assert_eq!(doc["models"][2].get("auto_compact_token_limit"), None);

        assert_eq!(out.calibrated_count(), 2);
        assert_eq!(out.untouched(), vec!["gpt-5.2"]);
        assert!(out.rows[0].crosses_long_context());
        assert!(!out.rows[1].crosses_long_context());
    }

    #[test]
    fn base_keeps_every_model_in_the_standard_price_tier() {
        let out = calibrate(BUNDLED, &facts(), WindowChoice::Base, &[]).unwrap();
        assert_eq!(out.rows[0].context_window, Some(272_000));
        // max_context_window still records what CAPI would allow.
        assert_eq!(out.rows[0].max_context_window, Some(872_000));
        assert_eq!(out.rows[0].auto_compact_token_limit, Some(244_800));
        assert!(!out.rows[0].crosses_long_context());
    }

    #[test]
    fn overrides_win_and_are_checked_against_capi() {
        let ov = parse_override("gpt-6-astra=400000:300000").unwrap();
        assert_eq!(ov.window, 400_000);
        assert_eq!(ov.compact, Some(300_000));
        let out = calibrate(BUNDLED, &facts(), WindowChoice::Max, &[ov]).unwrap();
        assert_eq!(out.rows[0].context_window, Some(400_000));
        assert_eq!(out.rows[0].auto_compact_token_limit, Some(300_000));
        assert_eq!(out.rows[0].max_context_window, Some(872_000));

        // Above what the seat allows, an unknown slug, and a compaction limit
        // that could never fire are all refused.
        let too_big = parse_override("gpt-6-astra=9000000").unwrap();
        assert!(calibrate(BUNDLED, &facts(), WindowChoice::Max, &[too_big]).is_err());
        let unknown = parse_override("nope=100000").unwrap();
        assert!(calibrate(BUNDLED, &facts(), WindowChoice::Max, &[unknown]).is_err());
        let silly = parse_override("gpt-6-astra=100000:100000").unwrap();
        assert!(calibrate(BUNDLED, &facts(), WindowChoice::Max, &[silly]).is_err());
    }

    #[test]
    fn parsing_rejects_nonsense() {
        assert!(parse_window_choice("max").is_ok());
        assert!(parse_window_choice("BASE").is_ok());
        assert_eq!(
            parse_window_choice("400_000").unwrap(),
            WindowChoice::Exact(400_000)
        );
        assert!(parse_window_choice("tiny").is_err());
        assert!(parse_window_choice("999").is_err());
        assert!(parse_override("gpt-5.5").is_err());
        assert!(parse_override("=100000").is_err());
        assert!(parse_override("gpt-5.5=abc").is_err());
    }

    #[test]
    fn only_a_real_catalog_validates() {
        assert_eq!(validate(BUNDLED).unwrap(), 3);
        assert!(validate("<html>proxy login</html>").is_err());
        assert!(validate(r#"{"models":[]}"#).is_err());
        assert!(validate(r#"{"data":[{"slug":"x"}]}"#).is_err());
    }

    #[test]
    fn the_raw_url_is_pinned_to_the_installed_codex_tag() {
        assert_eq!(
            raw_url("0.154.0"),
            "https://raw.githubusercontent.com/openai/codex/rust-v0.154.0/codex-rs/models-manager/models.json"
        );
    }
}
