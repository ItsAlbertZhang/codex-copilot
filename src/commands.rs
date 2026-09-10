//! The four commands, and the only place the auth half and the config half
//! meet.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::catalog::{self, Row};
use crate::codex::CodexBin;
use crate::{auth, capi, overlay, AuthArgs, InstallArgs, TOKEN_ENV};

/// What every command needs to know about where it is operating.
#[derive(Debug)]
pub struct Ctx {
    pub codex_home: PathBuf,
    pub profile: String,
    pub dry_run: bool,
    codex_bin: Option<PathBuf>,
    codex_version: Option<String>,
}

impl Ctx {
    pub fn new(
        codex_home: Option<PathBuf>,
        profile: String,
        dry_run: bool,
        codex_bin: Option<PathBuf>,
        codex_version: Option<String>,
    ) -> Result<Self> {
        if profile.is_empty() || profile.contains(['/', '\\', ':']) || profile.starts_with('.') {
            bail!("invalid --profile `{profile}`: it becomes a file name in CODEX_HOME");
        }
        let codex_home = match codex_home {
            Some(dir) => dir,
            None => dirs::home_dir()
                .context("could not find a home directory; pass --codex-home")?
                .join(".codex"),
        };
        let codex_home = std::path::absolute(&codex_home)
            .with_context(|| format!("could not resolve {}", codex_home.display()))?;
        Ok(Self {
            codex_home,
            profile,
            dry_run,
            codex_bin,
            codex_version,
        })
    }

    /// `$CODEX_HOME/<profile>.config.toml`.
    pub fn overlay_path(&self) -> PathBuf {
        self.codex_home
            .join(format!("{}.config.toml", self.profile))
    }
    /// `$CODEX_HOME/codex-copilot/<profile>/`.
    pub fn dir(&self) -> PathBuf {
        self.codex_home.join("codex-copilot").join(&self.profile)
    }
    pub fn catalog_path(&self) -> PathBuf {
        self.dir().join("models-catalog.json")
    }
    pub fn state_path(&self) -> PathBuf {
        self.dir().join("state.json")
    }

    /// The installed Codex version: the hidden override, else `codex --version`.
    fn version(&self) -> Result<String> {
        if let Some(v) = &self.codex_version {
            return Ok(v.clone());
        }
        match self.codex_bin.clone() {
            Some(p) => CodexBin::from_path(p),
            None => CodexBin::discover(),
        }?
        .version(&self.codex_home)
    }
}

/// `state.json`. No secrets: paths, versions and token counts.
#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    pub installed_at: String,
    pub codex_version: String,
    pub host: String,
    pub model: String,
    pub context_window: String,
    pub models: Vec<Row>,
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

/// Obtains a token and prints it. Knows nothing about Codex.
pub fn login(args: &AuthArgs) -> Result<()> {
    let client = capi::client()?;
    // `login` deliberately ignores an existing $COPILOT_GITHUB_TOKEN: its whole
    // job is to issue a new one.
    let token = resolve_token(&client, args, false)?;
    auth::print_token(&token);
    println!("\nThen: codex-copilot install");
    Ok(())
}

fn resolve_token(client: &Client, args: &AuthArgs, allow_env: bool) -> Result<auth::Token> {
    let from_inputs = auth::from_inputs(args.token.as_deref(), args.token_stdin)?;
    if let Some(t) = from_inputs {
        if allow_env || t.source != auth::Source::Environment {
            return Ok(t);
        }
    }
    auth::device_flow(
        client,
        args.github_oauth.as_deref().unwrap_or(auth::GITHUB_OAUTH),
        args.client_id.as_deref().unwrap_or(auth::CLIENT_ID),
    )
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

pub fn install(ctx: &Ctx, args: &InstallArgs) -> Result<()> {
    let choice = catalog::parse_window_choice(&args.context_window)?;
    let overrides = args
        .model_window
        .iter()
        .map(|s| catalog::parse_override(s))
        .collect::<Result<Vec<_>>>()?;

    let codex_version = ctx.version()?;
    let client = capi::client()?;
    let token = resolve_token(&client, &args.auth, true)?;
    auth::print_token(&token);

    // --- host -------------------------------------------------------------
    let hosts: Vec<String> = match (&args.host, args.host_list.is_empty()) {
        (Some(h), _) => vec![h.clone()],
        (None, false) => args.host_list.clone(),
        (None, true) => capi::DEFAULT_HOSTS.iter().map(|h| h.to_string()).collect(),
    };
    let probes = capi::discover(&client, &hosts, &token.value);
    let chosen = capi::pick(&probes).with_context(|| {
        format!(
            "no Copilot CAPI gateway accepted this token. Tried:\n{}\n\nA 401 here means the \
             token is not a GitHub OAuth token for a Copilot seat; a 403 means the seat has no \
             CAPI access.",
            probes
                .iter()
                .map(|p| format!("    {}  {}", p.host, p.verdict()))
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    let host = probes[chosen].host.clone();
    let facts = probes[chosen].facts.clone().unwrap_or_default();
    println!("\nCAPI host    {host}  ({} models)", facts.len());
    for p in probes.iter().take(chosen) {
        println!("  skipped    {}  {}", p.host, p.verdict());
    }

    // --- catalog ----------------------------------------------------------
    let url = args
        .catalog_url
        .clone()
        .unwrap_or_else(|| catalog::raw_url(&codex_version));
    let bundled = match &args.catalog {
        Some(path) => {
            let text = fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            catalog::validate(&text)
                .with_context(|| format!("{} is not a Codex models.json", path.display()))?;
            println!("Catalog      {}", path.display());
            text
        }
        None => catalog::download(&client, &url).with_context(|| {
            format!(
                "could not fetch the model catalog bundled with codex {codex_version}.\n    \
                 Download {url}\n    and pass it with --catalog <path>."
            )
        })?,
    };
    let calibrated = catalog::calibrate(&bundled, &facts, choice, &overrides)?;
    print_table(&calibrated.rows);
    let untouched = calibrated.untouched();
    if !untouched.is_empty() {
        println!(
            "  ({} entr{} left exactly as codex ships {}: {})",
            untouched.len(),
            if untouched.len() == 1 { "y" } else { "ies" },
            if untouched.len() == 1 { "it" } else { "them" },
            untouched.join(", ")
        );
    }

    // --- the model we are about to write ----------------------------------
    check_model(&args.model, &facts, &host);

    // --- write ------------------------------------------------------------
    let catalog_path = ctx.catalog_path();
    let overlay_text = overlay::render(&overlay::Params {
        profile: &ctx.profile,
        model: &args.model,
        host: &host,
        codex_version: &codex_version,
        catalog_path: &catalog_path,
        calibrated: calibrated.calibrated_count(),
    });
    let state = State {
        schema: 2,
        installed_at: iso8601(now_secs()),
        codex_version: codex_version.clone(),
        host: host.clone(),
        model: args.model.clone(),
        context_window: args.context_window.clone(),
        models: calibrated.rows.clone(),
    };

    println!();
    if ctx.dry_run {
        println!("Dry run, nothing written. Would write:");
        println!("  overlay    {}", ctx.overlay_path().display());
        println!("  catalog    {}", catalog_path.display());
        println!("  state      {}", ctx.state_path().display());
        println!("\n--- {} ---", ctx.overlay_path().display());
        print!("{overlay_text}");
        return Ok(());
    }
    fs::create_dir_all(ctx.dir())
        .with_context(|| format!("could not create {}", ctx.dir().display()))?;
    write(&catalog_path, &calibrated.json)?;
    write(&ctx.overlay_path(), &overlay_text)?;
    write(
        &ctx.state_path(),
        &(serde_json::to_string_pretty(&state)? + "\n"),
    )?;
    println!("Wrote  {}", ctx.overlay_path().display());
    println!("       {}", catalog_path.display());
    println!("       {}", ctx.state_path().display());

    println!("\nSet {TOKEN_ENV} as shown above, open a new shell, then:\n");
    println!("    codex --profile {}", ctx.profile);
    println!("    codex --profile {} exec \"...\"", ctx.profile);
    Ok(())
}

/// Warns, with a remedy, when the model about to be written is not usable.
fn check_model(model: &str, facts: &capi::Facts, host: &str) {
    let Some(f) = facts.get(model) else {
        println!(
            "\nWARNING  {model} is not in {host}/models. Pick one of the listed slugs with \
             --model, or ask the org admin to enable it."
        );
        return;
    };
    if !f.policy_ok() {
        println!(
            "\nWARNING  {model} has policy state `{}`. Enable it for your org/seat at \
             github.com/settings/copilot, or `install --model <other>`.",
            f.policy.as_deref().unwrap_or("unknown")
        );
    }
    if !f.ws {
        println!(
            "\nWARNING  {model} does not advertise {}. This profile uses the WebSocket \
             transport, so requests will fail; pick a model that lists it.",
            capi::WS_RESPONSES
        );
    }
}

fn print_table(rows: &[Row]) {
    println!(
        "\n  {:<22} {:>10} {:>10} {:>10} {:>10}  {:<4} policy",
        "model", "window", "compact", "capi max", "base tier", "ws"
    );
    for r in rows {
        let n = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |x| x.to_string());
        let tail = if !r.served {
            "not on this seat".to_string()
        } else {
            format!(
                "{:<4} {}{}",
                if r.ws { "yes" } else { "NO" },
                r.policy.as_deref().unwrap_or("-"),
                if r.crosses_long_context() {
                    "  (2x tier)"
                } else {
                    ""
                }
            )
        };
        println!(
            "  {:<22} {:>10} {:>10} {:>10} {:>10}  {tail}",
            r.slug,
            n(r.context_window),
            n(r.auto_compact_token_limit),
            n(r.capi_max),
            n(r.tier_base),
        );
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

pub fn status(ctx: &Ctx) -> Result<()> {
    let state: Option<State> = fs::read_to_string(ctx.state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let Some(state) = state else {
        println!(
            "Not installed for profile `{}` (no {}).\nRun: codex-copilot install",
            ctx.profile,
            ctx.state_path().display()
        );
        return Ok(());
    };

    println!("profile      {}", ctx.profile);
    println!(
        "installed    {}  (codex {})",
        state.installed_at, state.codex_version
    );
    println!("host         {}", state.host);
    println!(
        "model        {}  (--context-window {})",
        state.model, state.context_window
    );

    let mut problems: Vec<String> = Vec::new();
    let mut check = |label: &str, ok: bool, detail: String, fix: Option<String>| {
        println!(
            "{:<12} {} {detail}",
            label,
            if ok { "ok  " } else { "FAIL" }
        );
        if let Some(fix) = fix.filter(|_| !ok) {
            problems.push(fix);
        }
    };

    // The token lives only in the environment, and only the user puts it there.
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
    check(
        "token",
        token.is_some(),
        match &token {
            Some(t) => format!("{TOKEN_ENV} = {}", auth::describe(t)),
            None => format!("{TOKEN_ENV} is not set in this environment"),
        },
        Some(format!(
            "set {TOKEN_ENV} (codex-copilot login prints it and the one-liner)"
        )),
    );

    match ctx.version() {
        Ok(v) => check(
            "codex",
            v == state.codex_version,
            format!("{v} installed, catalog built for {}", state.codex_version),
            Some("re-run `codex-copilot install` so the catalog matches".into()),
        ),
        Err(e) => check(
            "codex",
            false,
            format!("{e}"),
            Some("install Codex or pass --codex-bin <path> to a working binary".into()),
        ),
    }

    let overlay_path = ctx.overlay_path();
    let overlay_ok = fs::read_to_string(&overlay_path)
        .ok()
        .and_then(|t| toml::from_str::<toml::Value>(&t).ok());
    check(
        "overlay",
        overlay_ok.is_some(),
        overlay_path.display().to_string(),
        Some("re-run `codex-copilot install`".into()),
    );

    let catalog_path = ctx.catalog_path();
    let entries = fs::read_to_string(&catalog_path)
        .ok()
        .and_then(|t| catalog::validate(&t).ok());
    check(
        "catalog",
        entries.is_some(),
        match entries {
            Some(n) => format!("{n} entries in {}", catalog_path.display()),
            None => format!("{} missing or unparseable", catalog_path.display()),
        },
        Some("re-run `codex-copilot install`".into()),
    );

    if let Some(token) = token {
        let probe = capi::probe_host(&capi::client()?, &state.host, &token);
        check(
            "capi",
            probe.ok(),
            format!("GET {}/models -> {}", state.host, probe.verdict()),
            Some("re-run `codex-copilot login`; the token may be revoked or expired".into()),
        );
        if let Some(facts) = probe.facts {
            let f = facts.get(&state.model);
            check(
                "model",
                f.is_some_and(|f| f.policy_ok() && f.ws),
                match f {
                    Some(f) => format!(
                        "{} policy={} ws:/responses={}",
                        state.model,
                        f.policy.as_deref().unwrap_or("none"),
                        if f.ws { "yes" } else { "no" }
                    ),
                    None => format!("{} is not served on this seat", state.model),
                },
                Some("pick another model: codex-copilot install --model <slug>".into()),
            );
        }
    } else {
        println!("capi         skipped (no token in this environment)");
    }

    if problems.is_empty() {
        println!("\nAll checks passed. `codex doctor` is not profile-aware, so verify by hand:");
        println!("    codex --profile {}", ctx.profile);
        println!("and send a short message; the first reply means the whole chain works.");
    } else {
        println!("\nTo fix:");
        for p in &problems {
            println!("  - {p}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

pub fn uninstall(ctx: &Ctx) -> Result<()> {
    let dir = ctx.dir();
    let overlay_path = ctx.overlay_path();
    let mut removed: Vec<String> = Vec::new();

    for path in [overlay_path.clone(), ctx.catalog_path(), ctx.state_path()] {
        if path.exists() {
            if ctx.dry_run {
                removed.push(format!("would remove {}", path.display()));
            } else {
                fs::remove_file(&path)
                    .with_context(|| format!("could not remove {}", path.display()))?;
                removed.push(format!("removed {}", path.display()));
            }
        }
    }
    if !ctx.dry_run && dir.exists() {
        // Only if empty: a file we did not write is a file we do not delete.
        let _ = fs::remove_dir(&dir);
        let _ = fs::remove_dir(dir.parent().unwrap_or(&dir));
    }

    if removed.is_empty() {
        println!("Nothing to remove for profile `{}`.", ctx.profile);
    } else {
        for line in removed {
            println!("{line}");
        }
    }
    println!("\n{TOKEN_ENV} is yours: this tool never set it. To clear it yourself:\n");
    for line in auth::unset_commands() {
        println!("    {line}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------

fn write(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("could not write {}", path.display()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `2026-09-10T12:34:56Z` from a Unix timestamp, without pulling in a date
/// crate. Civil-from-days after Howard Hinnant.
fn iso8601(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era * 400 + yoe + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> Ctx {
        Ctx::new(
            Some(dir.to_path_buf()),
            "copilot".into(),
            false,
            None,
            Some("0.154.0".into()),
        )
        .unwrap()
    }

    #[test]
    fn paths_are_derived_from_the_profile_name() {
        let c = ctx(Path::new("/home/.codex"));
        assert!(c.overlay_path().ends_with("copilot.config.toml"));
        assert!(c.dir().ends_with(Path::new("codex-copilot/copilot")));
        assert!(c.catalog_path().ends_with("models-catalog.json"));
        assert!(c.state_path().ends_with("state.json"));
    }

    #[test]
    fn a_profile_name_may_not_escape_codex_home() {
        for bad in ["", "a/b", r"a\b", "c:evil", ".hidden"] {
            assert!(
                Ctx::new(Some("/tmp".into()), bad.into(), false, None, None).is_err(),
                "{bad} was accepted"
            );
        }
        assert!(Ctx::new(Some("/tmp".into()), "cp2".into(), false, None, None).is_ok());
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_788_998_400), "2026-09-10T00:00:00Z");
        // A leap day, which the civil-from-days arithmetic has to get right.
        assert_eq!(iso8601(1_709_208_000), "2024-02-29T12:00:00Z");
    }
}
