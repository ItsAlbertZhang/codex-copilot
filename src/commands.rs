//! The subcommands. Everything user-facing is printed here; the modules they
//! call (home, config, daemon, process, auth, capi, codex) return data.
//!
//! Output follows one shape: a 12-column label, then the value, then (for
//! `status`) an `ok` / `WARN` / `FAIL` verdict.

use std::fmt::Display;
use std::fs;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::cli::{AuthArgs, InstallArgs, RelayOpts, StartArgs, StopArgs, UninstallArgs};
use crate::codex::CodexBin;
use crate::config::{self, ConfigOptions, CONFIG_FILE};
use crate::daemon::{self, Health, RelayArgs};
use crate::home::{self, Homes, Located, State};
use crate::proxy;
use crate::{
    auth, capi, check_origin, process, CODEX_AUTO_REVIEW, DEFAULT_CONTEXT_WINDOW, DEFAULT_LISTEN,
    DEFAULT_MODEL, DEFAULT_REVIEW_MODEL, DEFAULT_UPSTREAM, PROVIDER_ID, STATE_FILE, TOKEN_ENV,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// One `/healthz` probe.
const PROBE: Duration = Duration::from_secs(2);
/// `POST /shutdown` until the port refuses connections.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// A freshly started relay answering `/healthz`.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// The README section with the per-platform login-time recipes.
const LOGIN_RECIPES: &str = "Starting the relay at login";
/// Stands in for a token the user passed with --token / --token-stdin, which
/// is never printed back.
const TOKEN_PLACEHOLDER: &str = "<your token>";

/// What every command may need: where the homes are and where codex is.
#[derive(Debug, Clone)]
pub struct Ctx {
    home_dir: Option<PathBuf>,
    codex_bin: Option<PathBuf>,
}

impl Ctx {
    /// `home_dir` is the parent of `.codex` / `.codex-copilot` (default: the
    /// user's home directory); `codex_bin` an explicit codex binary.
    pub fn new(home_dir: Option<PathBuf>, codex_bin: Option<PathBuf>) -> Self {
        Self {
            home_dir,
            codex_bin,
        }
    }

    fn homes(&self) -> Result<Homes> {
        Homes::new(self.home_dir.clone())
    }

    /// `codex --version` with CODEX_HOME pinned to `home`, or to a throwaway
    /// directory when there is no home to point it at yet.
    fn codex_version(&self, home: Option<&Path>) -> Result<(PathBuf, String)> {
        let bin = match &self.codex_bin {
            Some(path) => CodexBin::from_path(path.clone())?,
            None => CodexBin::discover()?,
        };
        let version = match home {
            Some(home) => bin.version(home)?,
            None => {
                let scratch = tempfile::tempdir().context("could not create a temp directory")?;
                bin.version(scratch.path())?
            }
        };
        Ok((bin.path, version))
    }
}

fn line(label: &str, value: impl Display) {
    println!("{label:<12} {value}");
}

fn warning(text: impl Display) {
    println!("\nWARNING  {text}");
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

/// Obtains a token and prints it. Knows nothing about Codex.
pub fn login(args: &AuthArgs) -> Result<()> {
    // `login` deliberately ignores an existing $COPILOT_GITHUB_TOKEN: its whole
    // job is to issue a new one.
    let token = match auth::from_inputs(args.token.as_deref(), args.token_stdin)? {
        Some(token) if token.source != auth::Source::Environment => token,
        _ => device_flow(args)?,
    };
    auth::print_token(&token);
    println!("\nThen: codex-copilot install");
    Ok(())
}

fn device_flow(args: &AuthArgs) -> Result<auth::Token> {
    let base = args.github_oauth.as_deref().unwrap_or(auth::GITHUB_OAUTH);
    auth::device_flow(
        &capi::client()?,
        base,
        args.client_id.as_deref().unwrap_or(auth::CLIENT_ID),
    )
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

pub fn install(ctx: &Ctx, args: &InstallArgs) -> Result<()> {
    let homes = ctx.homes()?;
    let located = homes.locate()?;
    if located == Located::Overridden {
        bail!(
            "an override is active ({} is the codex-copilot home); run `codex-copilot \
             unoverride` first",
            homes.codex().display()
        );
    }
    let home = homes.copilot_home(located);
    let (listen, loopback) = check_listen(&args.listen)?;
    let host = args
        .host
        .as_deref()
        .map(|host| check_origin("--host", host))
        .transpose()?;
    let candidates = candidate_hosts(&args.hosts)?;
    // `None` keeps what config.toml already says (a `/model` choice).
    let model = not_empty("--model", args.model.as_deref())?;
    let effort = not_empty("--reasoning-effort", args.reasoning_effort.as_deref())?;
    let review_model = args.review_model.trim().to_string();

    line("Home", home.display());
    // The previous state only says where a relay started with the previous
    // settings would listen. A file that does not parse is about to be
    // replaced, but say so.
    let previous = match home::read_state(&home) {
        Ok(previous) => previous,
        Err(err) => {
            warning(format_args!(
                "{err:#}\nInstall goes on and rewrites it. A relay started with the previous \
                 settings is not found by `codex-copilot stop` unless it runs on {listen}; stop \
                 it with `codex-copilot stop --listen <its address>`."
            ));
            None
        }
    };

    // --- token ---------------------------------------------------------------
    let token = match auth::from_inputs(args.auth.token.as_deref(), args.auth.token_stdin)? {
        Some(token) => Some(token),
        // The token only feeds the probe; Codex sends its own at runtime.
        None if host.is_some() => None,
        None => {
            let token = device_flow(&args.auth)?;
            auth::print_token(&token);
            println!();
            Some(token)
        }
    };
    match &token {
        Some(token) => line(
            "Token",
            format_args!(
                "{} from {}",
                auth::describe(&token.value),
                token.source.as_str()
            ),
        ),
        None => line("Token", format_args!("none ({TOKEN_ENV} is not set)")),
    }

    // --- upstream ------------------------------------------------------------
    let (upstream, facts) = match (host, &token) {
        (Some(host), Some(token)) => {
            let probe = capi::probe_host(&host, &token.value);
            let verdict = probe.verdict();
            match probe.facts {
                Some(facts) => {
                    line("Upstream", format_args!("{host}  ({} models)", facts.len()));
                    (host, Some(facts))
                }
                None => {
                    line("Upstream", format_args!("{host}  (--host)"));
                    warning(format_args!(
                        "GET {host}/models -> {verdict}. Continuing because --host was given; Codex \
                         requests will fail the same way if the token or the gateway is wrong."
                    ));
                    (host, None)
                }
            }
        }
        (Some(host), None) => {
            line(
                "Upstream",
                format_args!(
                    "{host}  (--host; probe skipped: no token. The relay forwards whatever \
                     token Codex sends at runtime.)"
                ),
            );
            (host, None)
        }
        (None, Some(token)) => {
            let probes = capi::discover(&candidates, &token.value);
            let chosen = capi::pick(&probes).with_context(|| {
                format!(
                    "no Copilot CAPI gateway accepted this token. Tried:\n{}\n\nA 401 here means \
                     the token is not a GitHub OAuth token for a Copilot seat; a 403 means the \
                     seat has no CAPI access.",
                    probes
                        .iter()
                        .map(|p| format!("    {}  {}", p.host, p.verdict()))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })?;
            let mut probes = probes;
            let found = probes.remove(chosen);
            let facts = found.facts.unwrap_or_default();
            line(
                "Upstream",
                format_args!("{}  ({} models)", found.host, facts.len()),
            );
            for skipped in probes.iter().take(chosen) {
                println!("  skipped    {}  {}", skipped.host, skipped.verdict());
            }
            (found.host, Some(facts))
        }
        // Without --host a token always exists: the device flow ran.
        (None, None) => bail!("no GitHub token to probe the Copilot gateways with"),
    };

    // --- codex -----------------------------------------------------------------
    let codex = if args.dry_run {
        ctx.codex_version(None)
    } else {
        fs::create_dir_all(&home)
            .with_context(|| format!("could not create {}", home.display()))?;
        ctx.codex_version(Some(&home))
    };
    let codex_version = match codex {
        Ok((path, version)) => {
            line("Codex", format_args!("{version}  ({})", path.display()));
            Some(version)
        }
        Err(err) => {
            line("Codex", "not found");
            warning(format_args!(
                "{err:#}\nThe relay and the config do not need it now; Codex itself has to be \
                 installed before you run it on the dedicated home."
            ));
            None
        }
    };

    // --- what gets configured ----------------------------------------------------
    let opts = ConfigOptions {
        model,
        reasoning_effort: effort,
        context_window: DEFAULT_CONTEXT_WINDOW,
        // What Codex connects to: loopback for a wildcard --listen.
        listen: daemon::connect_addr(&listen),
        yolo: !args.no_yolo,
    };
    let plan = plan_config(&home, &opts)?;
    // Without --model a model already in config.toml is kept, so the plan
    // says which one Codex will run.
    let model = plan.root_str("model").unwrap_or_default();
    let effort = plan.root_str("model_reasoning_effort").unwrap_or_default();
    line(
        "Model",
        format_args!("{model}  (reasoning effort {effort})"),
    );
    if let Some(facts) = &facts {
        check_model(&model, facts, &upstream);
    }
    if args.no_yolo {
        line(
            "Yolo",
            "off (--no-yolo): Codex's approval and sandbox defaults apply, and \
             approvals_reviewer = \"auto_review\" answers approval requests",
        );
    } else {
        line(
            "Yolo",
            "on (codex --yolo): no approval prompts; commands run without a sandbox.",
        );
        line("", "Pass --no-yolo to keep Codex's approvals and sandbox.");
    }
    if review_model.is_empty() {
        line(
            "Review model",
            format_args!("none: the relay passes {CODEX_AUTO_REVIEW} through unchanged"),
        );
        if args.no_yolo {
            warning(format_args!(
                "CAPI does not serve {CODEX_AUTO_REVIEW}, so approval reviews will fail. Pass \
                 --review-model <slug> (default {DEFAULT_REVIEW_MODEL})."
            ));
        }
    } else {
        line(
            "Review model",
            format_args!(
                "{review_model} replaces {CODEX_AUTO_REVIEW}{}",
                if args.no_yolo {
                    ""
                } else {
                    " (only consulted with --no-yolo)"
                }
            ),
        );
        if let Some(facts) = &facts {
            check_review_model(&review_model, facts, &upstream);
        }
    }
    if !loopback {
        warning(format_args!(
            "--listen {listen} is not a loopback address. The relay has no authentication of \
             its own: anything that reaches the port can use it with its own Copilot token, \
             and stop it with POST /shutdown."
        ));
    }

    let state = State {
        version: VERSION.to_string(),
        installed_at: home::now_rfc3339(),
        listen: listen.clone(),
        upstream: upstream.clone(),
        review_model: review_model.clone(),
        yolo: !args.no_yolo,
        codex_version,
    };

    if args.dry_run {
        return print_dry_run(&home, &plan, &state);
    }

    // --- write -------------------------------------------------------------------
    println!();
    plan.write()?;
    line(
        "Wrote",
        format_args!("{}  ({})", plan.path.display(), plan.verb()),
    );
    home::write_state(&home, &state)?;
    line("", home.join(STATE_FILE).display());

    // Nothing is probed or stopped here, but `stop` now looks on the new
    // address, so name the old one.
    let moved = previous.filter(|old| old.listen != listen);
    next_steps(
        &home,
        token.as_ref(),
        args,
        &review_model,
        moved.as_ref().map(|old| old.listen.as_str()),
    );
    Ok(())
}

/// Validates `--listen`. Returns it trimmed, and whether it is loopback.
///
/// Only an IP literal is accepted: the relay binds a single address, while
/// Codex's base_url is written from this text, so a name such as `localhost`
/// (`::1` and `127.0.0.1`) could send Codex to an address nothing listens on.
fn check_listen(raw: &str) -> Result<(String, bool)> {
    let listen = raw.trim();
    let addr: SocketAddr = listen.parse().map_err(|_| {
        anyhow!(
            "--listen {listen:?} is not an IP address and port: the relay binds one address and \
             Codex's base_url names it, so use an IP literal such as {DEFAULT_LISTEN}"
        )
    })?;
    if addr.port() == 0 {
        bail!("--listen needs a fixed port: Codex's base_url points at it");
    }
    Ok((listen.to_string(), is_loopback(addr)))
}

/// Whether only this machine reaches `addr`. The relay's own exposure
/// warning decides, so `install`, `uninstall --purge` and the relay agree
/// (`[::ffff:127.0.0.1]` is loopback too).
fn is_loopback(addr: SocketAddr) -> bool {
    daemon::exposure_warning(addr).is_none()
}

/// The gateways probed, in order, when `--host` is not given: the hidden
/// `--hosts` list (empty items skipped), else [`capi::DEFAULT_HOSTS`].
fn candidate_hosts(hosts: &[String]) -> Result<Vec<String>> {
    if hosts.is_empty() {
        return Ok(capi::DEFAULT_HOSTS.iter().map(|h| h.to_string()).collect());
    }
    let hosts: Vec<String> = hosts
        .iter()
        .filter(|host| !host.trim().is_empty())
        .map(|host| check_origin("--hosts", host))
        .collect::<Result<_>>()?;
    if hosts.is_empty() {
        bail!("--hosts lists no gateway");
    }
    Ok(hosts)
}

/// A flag's value, trimmed; `None` when the flag was not given.
fn not_empty(flag: &str, value: Option<&str>) -> Result<Option<String>> {
    let Some(value) = value.map(str::trim) else {
        return Ok(None);
    };
    if value.is_empty() {
        bail!("{flag} may not be empty");
    }
    Ok(Some(value.to_string()))
}

/// Warns, with a remedy, when the model about to be written is not usable.
fn check_model(model: &str, facts: &capi::Facts, host: &str) {
    let Some(f) = facts.get(model) else {
        warning(format_args!(
            "{model} is not in {host}/models. Pick one of the listed slugs with --model, or ask \
             the org admin to enable it."
        ));
        return;
    };
    if !f.policy_ok() {
        warning(format_args!(
            "{model} has policy state `{}`. Enable it for your org/seat at \
             github.com/settings/copilot, or `install --model <other>`.",
            f.policy.as_deref().unwrap_or("unknown")
        ));
    }
    if !f.ws {
        warning(format_args!(
            "{model} does not advertise {}. Codex is configured for the WebSocket transport, \
             so each session fails its WebSocket attempts before it falls back to HTTP; pick a \
             model that lists it.",
            capi::WS_RESPONSES
        ));
    }
}

fn check_review_model(model: &str, facts: &capi::Facts, host: &str) {
    match facts.get(model) {
        Some(f) if f.policy_ok() && f.ws => {}
        Some(_) => warning(format_args!(
            "review model {model} is disabled on this seat or lacks {}; pick another with \
             --review-model.",
            capi::WS_RESPONSES
        )),
        None => warning(format_args!(
            "review model {model} is not in {host}/models; pick another with --review-model."
        )),
    }
}

/// The config.toml `install` is about to write.
struct ConfigPlan {
    path: PathBuf,
    existing: Option<String>,
    text: String,
}

impl ConfigPlan {
    fn verb(&self) -> &'static str {
        match &self.existing {
            None => "created",
            Some(old) if *old == self.text => "unchanged",
            Some(_) => "updated",
        }
    }

    /// A root string of the planned file, e.g. the `model` Codex will use.
    fn root_str(&self, key: &str) -> Option<String> {
        let doc: toml::Value = toml::from_str(&self.text).ok()?;
        doc.get(key)?.as_str().map(str::to_owned)
    }

    /// Writes the file unless it already says exactly this, so a running
    /// Codex never sees an unchanged file move.
    fn write(&self) -> Result<()> {
        if self.existing.as_deref() == Some(self.text.as_str()) {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        home::atomic_write(&self.path, &self.text)
    }
}

/// [`config::render`] for a new file, [`config::apply`] for an existing one.
fn plan_config(home: &Path, opts: &ConfigOptions) -> Result<ConfigPlan> {
    let path = home.join(CONFIG_FILE);
    let existing = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("could not read {}", path.display())),
    };
    let text = match &existing {
        Some(old) => config::apply(old, opts)
            .with_context(|| format!("could not update {}", path.display()))?,
        None => config::render(opts),
    };
    Ok(ConfigPlan {
        path,
        existing,
        text,
    })
}

/// The URL a client on this machine reaches the relay listening on `listen`
/// at, which is what Codex's base_url says: loopback for a wildcard address,
/// which is not connectable everywhere.
fn relay_url(listen: &str) -> String {
    format!("http://{}", daemon::connect_addr(listen))
}

fn print_dry_run(home: &Path, plan: &ConfigPlan, state: &State) -> Result<()> {
    let state_path = home.join(STATE_FILE);
    let verb = match plan.verb() {
        "created" => "create",
        "updated" => "update",
        _ => "keep (unchanged)",
    };
    println!("\nDry run, nothing written. Would:");
    println!("  {verb:<8} {}", plan.path.display());
    println!("  {:<8} {}", "write", state_path.display());
    println!("\n--- {} ---", plan.path.display());
    print!("{}", plan.text);
    println!("--- {} ---", state_path.display());
    println!(
        "{}",
        serde_json::to_string_pretty(state).context("could not serialize the state")?
    );
    Ok(())
}

/// How a running relay differs from this binary and the given arguments.
fn mismatch(health: &Health, upstream: &str, review_model: &str) -> Option<String> {
    let mut differences = Vec::new();
    if health.version != VERSION {
        differences.push(format!("is version {} (this is {VERSION})", health.version));
    }
    if health.upstream != upstream {
        differences.push(format!("forwards to {}", health.upstream));
    }
    let running_review = health.review_model.as_deref().unwrap_or("");
    if running_review != review_model {
        differences.push(format!(
            "substitutes {}",
            if running_review.is_empty() {
                "no review model"
            } else {
                running_review
            }
        ));
    }
    (!differences.is_empty()).then(|| differences.join(", "))
}

/// `moved_from` is the listen address of the previous install, when this one
/// changed it.
fn next_steps(
    home: &Path,
    token: Option<&auth::Token>,
    args: &InstallArgs,
    review_model: &str,
    moved_from: Option<&str>,
) {
    println!("\nNext steps:\n");
    let mut step = 1;
    if !token_in_env() {
        match token {
            Some(token) if token.source == auth::Source::DeviceFlow => {
                println!(
                    "  {step}. Set {TOKEN_ENV} as shown above, then open a new shell (restart \
                     desktop clients)."
                );
            }
            // A token passed with --token / --token-stdin is never echoed:
            // --token-stdin exists to keep it out of logs.
            Some(_) => {
                println!(
                    "  {step}. {TOKEN_ENV} is not set in this shell. Set it as a user \
                     environment variable to the token you passed, then open a new shell \
                     (restart desktop clients):\n"
                );
                for command in auth::set_commands_template(TOKEN_PLACEHOLDER) {
                    println!("         {command}");
                }
            }
            None => println!(
                "  {step}. {TOKEN_ENV} is not set in this shell. `codex-copilot login` gets a \
                 token and prints the one-liners that set it."
            ),
        }
        println!();
        step += 1;
    }
    println!(
        "  {step}. Start the relay. Codex cannot connect while it is not running:\n\n\
         \x20        codex-copilot start\n\n\
         \x20    A relay that already runs keeps the settings it was started with: run \
         `codex-copilot stop`,\n     then `codex-copilot start`. `codex-copilot` alone runs \
         it in the foreground instead.\n     To start it at login, see \"{LOGIN_RECIPES}\" in \
         the README."
    );
    if let Some(old) = moved_from {
        println!(
            "     The listen address moved from {old}: a relay started with the previous \
             settings still runs\n     there; `codex-copilot stop --listen {old}` stops it."
        );
    }
    println!();
    step += 1;
    println!("  {step}. Run Codex on the dedicated home:\n");
    println!(
        "         bash/zsh     CODEX_HOME={} codex",
        sh_quote(&home.display().to_string())
    );
    println!(
        "         PowerShell   $env:CODEX_HOME = {}; codex",
        ps_quote(home)
    );
    println!(
        "\n     or make it the default home of every Codex client (plain `codex`, the desktop \
         app,\n     editor integrations): codex-copilot override"
    );
    if args.no_yolo {
        println!(
            "\nApproval requests go to Codex's automatic reviewer (approvals_reviewer = \
             \"auto_review\");"
        );
        if review_model.is_empty() {
            println!("the relay passes {CODEX_AUTO_REVIEW} through unchanged.");
        } else {
            println!(
                "the relay sends {review_model} in place of {CODEX_AUTO_REVIEW}. Reviewer \
                 inference consumes additional CAPI usage."
            );
        }
    }
}

fn token_in_env() -> bool {
    std::env::var(TOKEN_ENV).is_ok_and(|value| !value.trim().is_empty())
}

/// A PowerShell literal string.
fn ps_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

/// A POSIX shell word: as is when it is safe, single-quoted otherwise.
fn sh_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Ok,
    Warn,
    Fail,
}

/// Prints checks as they run and remembers what to fix.
#[derive(Default)]
struct Checks {
    fixes: Vec<String>,
    failed: usize,
}

impl Checks {
    fn check(&mut self, label: &str, verdict: Verdict, detail: impl Display, fix: Option<String>) {
        let tag = match verdict {
            Verdict::Ok => "ok  ",
            Verdict::Warn => "WARN",
            Verdict::Fail => "FAIL",
        };
        println!("{label:<12} {tag} {detail}");
        if verdict == Verdict::Fail {
            self.failed += 1;
        }
        if let Some(fix) = fix.filter(|_| verdict != Verdict::Ok) {
            if !self.fixes.contains(&fix) {
                self.fixes.push(fix);
            }
        }
    }

    fn finish(self, all_good: &str) -> Result<()> {
        if !self.fixes.is_empty() {
            println!("\nTo fix:");
            for fix in &self.fixes {
                println!("  - {fix}");
            }
        }
        match self.failed {
            0 => {
                if self.fixes.is_empty() {
                    println!("\n{all_good}");
                }
                Ok(())
            }
            1 => bail!("1 check failed"),
            n => bail!("{n} checks failed"),
        }
    }
}

pub fn status(ctx: &Ctx, probe: bool) -> Result<()> {
    let homes = ctx.homes()?;
    let located = homes.locate()?;
    let mut checks = Checks::default();
    let home = homes.copilot_home(located);
    match located {
        Located::None => line("Home", home.display()),
        Located::Overridden => line(
            "Home",
            format_args!(
                "{}  (override active: plain `codex` uses it; `codex-copilot unoverride` swaps \
                 back)",
                home.display()
            ),
        ),
        Located::Normal => line(
            "Home",
            format_args!(
                "{}  (no override; {} is your regular Codex home)",
                home.display(),
                homes.codex().display()
            ),
        ),
    }
    let state = match located {
        Located::None => Ok(None),
        _ => home::read_state(&home),
    };
    let installed = state.as_ref().ok().cloned().flatten();
    if let Some(state) = &installed {
        line(
            "Installed",
            format_args!("{}  by codex-copilot {}", state.installed_at, state.version),
        );
        line("Listen", &state.listen);
        line("Upstream", &state.upstream);
        line(
            "Review model",
            if state.review_model.is_empty() {
                "none"
            } else {
                state.review_model.as_str()
            },
        );
        line("Yolo", if state.yolo { "on" } else { "off" });
    }
    match daemon::log_file() {
        Ok(path) => line("Relay log", path.display()),
        Err(err) => line("Relay log", format_args!("{err:#}")),
    }
    println!();

    // First, since nothing works without it. Where `start` and `stop` look:
    // the installed address, else the default one.
    let listen = installed
        .as_ref()
        .map_or(DEFAULT_LISTEN, |state| state.listen.as_str());
    let running = check_relay(&mut checks, listen, installed.as_ref());

    let state = match state {
        Ok(Some(state)) => state,
        Ok(None) if located == Located::None => {
            checks.check(
                "state",
                Verdict::Fail,
                format_args!(
                    "not installed: neither {} nor {} contains {STATE_FILE}",
                    homes.copilot().display(),
                    homes.codex().display()
                ),
                Some("run `codex-copilot install`".into()),
            );
            return checks.finish("");
        }
        Ok(None) => bail!("{} disappeared", home.join(STATE_FILE).display()),
        Err(err) => {
            checks.check(
                "state",
                Verdict::Fail,
                format_args!("{err:#}"),
                Some("re-run `codex-copilot install`".into()),
            );
            return checks.finish("");
        }
    };

    if state.version != VERSION {
        checks.check(
            "state",
            Verdict::Warn,
            format_args!(
                "written by codex-copilot {}, this is {VERSION}",
                state.version
            ),
            Some("re-run `codex-copilot install` to update the home".into()),
        );
    }

    let model = check_config(&mut checks, &home, &state);

    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    match &token {
        Some(token) => checks.check(
            "token",
            Verdict::Ok,
            format_args!("{TOKEN_ENV} is set ({})", auth::describe(token.trim())),
            None,
        ),
        // Only a warning: this shell is not where Codex runs. A desktop
        // client or a new terminal may well have the variable this one lacks.
        None => checks.check(
            "token",
            Verdict::Warn,
            format_args!(
                "{TOKEN_ENV} is not visible in this shell; Codex needs it in its own environment"
            ),
            Some(format!(
                "set {TOKEN_ENV} as a user environment variable (`codex-copilot login` prints it \
                 and the one-liner), then open a new shell and restart desktop clients"
            )),
        ),
    }

    let codex_home = home.is_dir().then_some(home.as_path());
    match ctx.codex_version(codex_home) {
        Ok((path, version)) => {
            let at_install = match &state.codex_version {
                Some(old) if *old != version => format!(", {old} at install"),
                _ => String::new(),
            };
            checks.check(
                "codex",
                Verdict::Ok,
                format_args!("{version}  ({}{at_install})", path.display()),
                None,
            );
        }
        Err(err) => checks.check(
            "codex",
            Verdict::Warn,
            format_args!("{err:#}"),
            Some("install Codex, or pass --codex-bin <path>".into()),
        ),
    }

    if probe {
        if let Some(found) = &running {
            check_proxy(&mut checks, found, proxy::system_proxy(&found.upstream));
        }
        match &token {
            Some(token) => probe_upstream(&mut checks, &state, &model, token.trim()),
            None => checks.check(
                "capi",
                Verdict::Fail,
                format_args!("--probe needs {TOKEN_ENV}"),
                None,
            ),
        }
    }

    checks.finish(
        "All checks passed. The real end-to-end check: start Codex on the dedicated home and \
         send one short message.",
    )
}

/// The `relay` check: what answers `/healthz` on `listen`, compared with the
/// install `state` when there is one. Returns the running relay's health,
/// for the `proxy` line of `--probe`.
fn check_relay(checks: &mut Checks, listen: &str, state: Option<&State>) -> Option<Health> {
    match daemon::health(listen, PROBE) {
        Ok(Some(found)) => {
            let detail = format!(
                "{} running on {} (pid {}), upstream {}, proxy {}",
                found.version,
                relay_url(listen),
                found.pid,
                found.upstream,
                found.proxy.as_deref().unwrap_or("none")
            );
            match state.and_then(|s| mismatch(&found, &s.upstream, &s.review_model)) {
                None => checks.check("relay", Verdict::Ok, detail, None),
                Some(difference) => checks.check(
                    "relay",
                    Verdict::Warn,
                    format_args!("{detail}; it {difference}"),
                    Some(
                        "`codex-copilot stop`, then `codex-copilot start`, restarts the relay \
                         with this binary and the installed settings"
                            .into(),
                    ),
                ),
            }
            Some(found)
        }
        Ok(None) => {
            // Without an install there is nothing for it to serve yet; the
            // state check below fails anyway.
            let verdict = if state.is_some() {
                Verdict::Fail
            } else {
                Verdict::Warn
            };
            checks.check(
                "relay",
                verdict,
                format_args!("not running on {listen}, so Codex cannot connect"),
                Some(
                    "`codex-copilot start` starts the relay in the background (`codex-copilot` \
                     alone runs it in the foreground)"
                        .into(),
                ),
            );
            None
        }
        Err(err) => {
            checks.check(
                "relay",
                Verdict::Fail,
                format_args!("{err:#}"),
                Some("free the port, or pick another with `codex-copilot install --listen`".into()),
            );
            None
        }
    }
}

/// Checks that config.toml routes Codex through the relay. Returns the
/// configured model, for `--probe`.
fn check_config(checks: &mut Checks, home: &Path, state: &State) -> String {
    let path = home.join(CONFIG_FILE);
    let fix = Some("re-run `codex-copilot install`".to_string());
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            checks.check(
                "config",
                Verdict::Fail,
                format_args!("could not read {}: {err}", path.display()),
                fix,
            );
            return DEFAULT_MODEL.to_string();
        }
    };
    let doc: toml::Value = match toml::from_str(&text) {
        Ok(doc) => doc,
        Err(err) => {
            checks.check(
                "config",
                Verdict::Fail,
                format_args!("{} does not parse: {err}", path.display()),
                fix,
            );
            return DEFAULT_MODEL.to_string();
        }
    };
    let model = doc
        .get("model")
        .and_then(toml::Value::as_str)
        .unwrap_or(DEFAULT_MODEL)
        .to_string();
    let provider = doc
        .get("model_providers")
        .and_then(|providers| providers.get(PROVIDER_ID));
    let base_url = provider
        .and_then(|p| p.get("base_url"))
        .and_then(toml::Value::as_str);
    let websockets = provider
        .and_then(|p| p.get("supports_websockets"))
        .and_then(toml::Value::as_bool);
    let selected = doc.get("model_provider").and_then(toml::Value::as_str);

    // What install writes: loopback for a wildcard listen address.
    let expected = relay_url(&state.listen);
    let mut problems = Vec::new();
    if selected != Some(PROVIDER_ID) {
        problems.push(format!(
            "model_provider is {}, not \"{PROVIDER_ID}\"",
            selected.map_or("unset".to_string(), |s| format!("{s:?}"))
        ));
    }
    if base_url != Some(expected.as_str()) {
        problems.push(format!(
            "[model_providers.{PROVIDER_ID}] base_url is {}, not \"{expected}\"",
            base_url.map_or("unset".to_string(), |s| format!("{s:?}"))
        ));
    }
    if websockets != Some(true) {
        problems.push(format!(
            "[model_providers.{PROVIDER_ID}] supports_websockets is not true"
        ));
    }
    if problems.is_empty() {
        checks.check(
            "config",
            Verdict::Ok,
            format_args!(
                "{}: model {model}, base_url {expected}, websockets",
                path.display()
            ),
            None,
        );
    } else {
        checks.check(
            "config",
            Verdict::Fail,
            format_args!("{}: {}", path.display(), problems.join("; ")),
            fix,
        );
    }
    model
}

/// The `proxy` line of `status --probe`: the proxy the running relay reaches
/// its upstream through (from `/healthz`), next to the one a relay started
/// from this shell would use (`shell`, credentials masked; `None` for
/// direct). The relay read its own environment when it started, which need
/// not be this shell's, so a difference is a WARN.
fn check_proxy(checks: &mut Checks, found: &Health, shell: Option<String>) {
    let relay = match &found.proxy {
        Some(proxy) => format!("the relay reaches {} through {proxy}", found.upstream),
        None => format!("the relay reaches {} directly", found.upstream),
    };
    if shell == found.proxy {
        checks.check("proxy", Verdict::Ok, relay, None);
        return;
    }
    checks.check(
        "proxy",
        Verdict::Warn,
        format_args!(
            "{relay}, but from this shell it would go {}",
            shell.map_or("directly".to_string(), |proxy| format!("through {proxy}"))
        ),
        Some(
            "the relay reads HTTPS_PROXY, HTTP_PROXY, ALL_PROXY, NO_PROXY and the system proxy \
             settings when it starts: run `codex-copilot stop`, then `codex-copilot start` from \
             a shell that has the right ones (a login entry gets the login environment)"
                .to_string(),
        ),
    );
}

fn probe_upstream(checks: &mut Checks, state: &State, model: &str, token: &str) {
    let probe = capi::probe_host(&state.upstream, token);
    let Some(facts) = &probe.facts else {
        checks.check(
            "capi",
            Verdict::Fail,
            format_args!("GET {}/models -> {}", state.upstream, probe.verdict()),
            Some(capi_fix(probe.status, &state.upstream)),
        );
        return;
    };
    checks.check(
        "capi",
        Verdict::Ok,
        format_args!(
            "GET {}/models -> {} ({} models)",
            state.upstream,
            probe.verdict(),
            facts.len()
        ),
        None,
    );
    let (verdict, detail) = model_verdict(model, facts);
    checks.check(
        "model",
        verdict,
        detail,
        Some("pick another model: `codex-copilot install --model <slug>`".into()),
    );
    if !state.review_model.is_empty() {
        // The review model only ever warns: Codex runs without it.
        let (verdict, detail) = model_verdict(&state.review_model, facts);
        checks.check(
            "review",
            if verdict == Verdict::Ok {
                Verdict::Ok
            } else {
                Verdict::Warn
            },
            detail,
            Some("pick another: `codex-copilot install --review-model <slug>`".into()),
        );
    }
}

/// The `status --probe` verdict for one model slug, with the line to print.
///
/// A model that is absent or blocked by policy cannot work, so it fails. One
/// that only lacks `ws:/responses` still works: Codex retries the WebSocket,
/// then falls back to HTTP SSE for the rest of the session (which the relay
/// serves). That is a warning, since every session starts slower.
fn model_verdict(slug: &str, facts: &capi::Facts) -> (Verdict, String) {
    let Some(f) = facts.get(slug) else {
        return (Verdict::Fail, format!("{slug} is not served on this seat"));
    };
    let detail = format!(
        "{slug}: policy {}, {} {}",
        f.policy.as_deref().unwrap_or("none"),
        capi::WS_RESPONSES,
        if f.ws { "yes" } else { "no" }
    );
    if !f.policy_ok() {
        (Verdict::Fail, detail)
    } else if !f.ws {
        (
            Verdict::Warn,
            format!(
                "{detail}; sessions start slower because Codex retries the WebSocket before falling back to HTTP SSE"
            ),
        )
    } else {
        (Verdict::Ok, detail)
    }
}

/// What to do about a failed `GET <upstream>/models`, by what came back.
fn capi_fix(status: Option<u16>, upstream: &str) -> String {
    match status {
        Some(401) => "the gateway refused the token (401): re-run `codex-copilot login`; the \
                      token may be revoked or expired, or not be for a Copilot seat"
            .to_string(),
        Some(403) => "the seat has no CAPI access on this gateway (403): check the Copilot \
                      seat, or re-run `codex-copilot install` if the seat's gateway changed"
            .to_string(),
        None => format!(
            "{upstream} could not be reached: check the network connection, a proxy \
             (HTTPS_PROXY) or a firewall; re-run `codex-copilot install` if the seat's gateway \
             changed"
        ),
        Some(_) => format!(
            "{upstream} did not answer with a model list: it may be down or not a CAPI gateway; \
             re-run `codex-copilot install` if the seat's gateway changed"
        ),
    }
}

// ---------------------------------------------------------------------------
// override / unoverride
// ---------------------------------------------------------------------------

pub fn override_home(ctx: &Ctx, force: bool) -> Result<()> {
    swap(ctx, force, true)
}

pub fn unoverride_home(ctx: &Ctx, force: bool) -> Result<()> {
    swap(ctx, force, false)
}

fn swap(ctx: &Ctx, force: bool, to_override: bool) -> Result<()> {
    let homes = ctx.homes()?;
    let ready = match homes.locate()? {
        Located::Normal => to_override,
        Located::Overridden => !to_override,
        Located::None => false,
    };
    // When the layout is wrong, `home` refuses with the right message before
    // touching anything; the process check would only be noise.
    if ready && !force {
        refuse_if_codex_runs(
            "Windows cannot rename a directory while a program has a file open in it, and a \
             running Codex (an app-server in particular) would keep writing into the directory \
             it opened, which after the swap belongs to the other home.",
        )?;
    }
    let report = if to_override {
        home::override_homes(&homes)?
    } else {
        home::unoverride_homes(&homes)?
    };
    for step in &report.steps {
        println!("{step}");
    }

    let (codex, copilot) = (homes.codex(), homes.copilot());
    let width = codex
        .display()
        .to_string()
        .len()
        .max(copilot.display().to_string().len());
    let row = |path: &Path, what: &str| {
        println!("  {:<width$}  {what}", path.display().to_string());
    };
    println!("\nNow:");
    if to_override {
        row(
            &codex,
            "the codex-copilot home: plain `codex` and every Codex client run on Copilot",
        );
        if copilot.exists() {
            row(&copilot, "your regular Codex home");
        } else {
            row(&copilot, "(absent: there was no regular Codex home)");
        }
        println!(
            "\nRestart running Codex clients (desktop app, editor integrations) so \
             they pick up the swapped home. They need {TOKEN_ENV} in their environment.\n\
             `codex-copilot unoverride` swaps back."
        );
        println!(
            "\nEvery Codex entry point now goes through the relay, so start it at login: the \
             README section \"{LOGIN_RECIPES}\" has per-platform recipes."
        );
    } else {
        if codex.exists() {
            row(&codex, "your regular Codex home");
        } else {
            row(&codex, "(absent: there was no regular Codex home)");
        }
        row(
            &copilot,
            "the codex-copilot home: run Codex on it with CODEX_HOME",
        );
        println!(
            "\nRestart running Codex clients so they pick up your regular home again.\n\
             `codex-copilot override` swaps again."
        );
    }
    Ok(())
}

/// Fails while a Codex process runs; `why` says what that would break.
fn refuse_if_codex_runs(why: &str) -> Result<()> {
    let running = process::running_codex()
        .context("could not check for running codex processes; --force skips the check")?;
    if running.is_empty() {
        return Ok(());
    }
    bail!(
        "Codex is running: {}.\n\
         Close every Codex client first (terminals, the desktop app, editor \
         integrations), then retry. {why} --force skips this check.",
        running_list(&running)
    )
}

/// `codex.exe (pid 1234), codex-app-server (pid 99)`: each process under the
/// image name the process lister reported.
fn running_list(running: &[process::ProcessInfo]) -> String {
    running
        .iter()
        .map(|p| format!("{} (pid {})", p.name, p.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

/// Stops the relay on `--listen`, else the installed address, else the
/// default one, and waits until its port refuses connections. Nothing
/// running there is not an error: `stop` is idempotent. The install is
/// left as it is.
pub fn stop(ctx: &Ctx, args: &StopArgs) -> Result<()> {
    let listen = match &args.listen {
        Some(listen) => listen.trim().to_string(),
        None => installed_state(ctx, |err| {
            warning(format_args!(
                "{err:#}; stopping a relay on {DEFAULT_LISTEN}"
            ))
        })?
        .map_or_else(|| DEFAULT_LISTEN.to_string(), |state| state.listen),
    };
    daemon::resolve_listen(&listen)?;
    let stopped = daemon::stop(&listen, STOP_TIMEOUT)
        .with_context(|| format!("left whatever listens on {listen} alone"))?;
    match stopped {
        Some(found) => {
            line(
                "Relay",
                format_args!("stopped {} (pid {}) on {listen}", found.version, found.pid),
            );
            println!("\n`codex-copilot start` starts it again.");
        }
        None => line("Relay", format_args!("not running on {listen}")),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

pub fn uninstall(ctx: &Ctx, args: &UninstallArgs) -> Result<()> {
    let homes = ctx.homes()?;
    let located = homes.locate()?;
    if located == Located::Overridden {
        bail!(
            "an override is active ({} is the codex-copilot home); run `codex-copilot \
             unoverride` first",
            homes.codex().display()
        );
    }
    let home = homes.copilot();
    // Before anything is removed, so a refusal leaves the install as it was.
    // A home `purge` would not delete anyway needs no check.
    let purging =
        args.purge && home.exists() && (located == Located::Normal || looks_like_relay_home(&home));
    if purging && !args.force {
        refuse_if_codex_runs(
            "Windows cannot delete a file a program has open, which would leave the home \
             half-deleted, and a running Codex would keep writing into the home it opened.",
        )?;
    }
    // Read before the file goes: where a relay for this install would run.
    // A broken state file is removed all the same.
    let state = match located {
        Located::Normal => home::read_state(&home).unwrap_or(None),
        _ => None,
    };
    let listen = state
        .as_ref()
        .map_or(DEFAULT_LISTEN, |state| state.listen.as_str());

    if located == Located::Normal {
        let path = home.join(STATE_FILE);
        match fs::remove_file(&path) {
            Ok(()) => line("Removed", path.display()),
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("could not remove {}", path.display()))
            }
        }
    }

    if args.purge {
        purge(&home, located)?;
    } else if located == Located::None {
        println!("\ncodex-copilot is not installed; nothing else to do.");
    } else if home.exists() {
        println!(
            "\nKept {} (config.toml, sessions, everything Codex keeps there); its config.toml \
             still points Codex at the relay. `codex-copilot uninstall --purge` deletes it.",
            home.display()
        );
    }

    // Files only: a running relay is left alone, but not unmentioned.
    if let Ok(Some(found)) = daemon::health(listen, PROBE) {
        println!(
            "\nA codex-copilot relay still runs on {listen} ({}, pid {}); `codex-copilot stop \
             --listen {listen}` stops it.",
            found.version, found.pid
        );
    }

    println!("\n{TOKEN_ENV} is yours: this tool never set it. To clear it yourself:\n");
    for command in auth::unset_commands() {
        println!("    {command}");
    }
    Ok(())
}

/// Deletes the dedicated home. Without a state file the directory is only
/// trusted to be ours when its config carries the provider table `install`
/// writes, so a directory that is something else is never deleted on name
/// alone.
fn purge(home: &Path, located: Located) -> Result<()> {
    if !home.exists() {
        println!("\nNothing to purge: {} does not exist.", home.display());
        return Ok(());
    }
    if located == Located::None && !looks_like_relay_home(home) {
        println!(
            "\nNot deleting {}: it has no {STATE_FILE} and its config.toml lacks the \
             [model_providers.{PROVIDER_ID}] table codex-copilot writes. Delete it yourself if \
             you are sure.",
            home.display()
        );
        return Ok(());
    }
    fs::remove_dir_all(home).with_context(|| {
        format!(
            "could not delete {} (is a program using it?)",
            home.display()
        )
    })?;
    line(
        "Deleted",
        format_args!(
            "{} (config, sessions and everything else in it)",
            home.display()
        ),
    );
    Ok(())
}

/// Whether `home`'s config.toml carries the provider table `install` writes,
/// every fingerprint key of it: a user's own `[model_providers.copilot]`
/// pointing at a proxy of theirs on 127.0.0.1 must not pass.
fn looks_like_relay_home(home: &Path) -> bool {
    let Ok(text) = fs::read_to_string(home.join(CONFIG_FILE)) else {
        return false;
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
        return false;
    };
    let Some(provider) = doc
        .get("model_providers")
        .and_then(|providers| providers.get(PROVIDER_ID))
    else {
        return false;
    };
    let string = |key: &str| provider.get(key).and_then(toml::Value::as_str);
    // `install` writes `http://` + the connectable address, an IP literal.
    let relay = string("base_url")
        .and_then(|url| url.strip_prefix("http://"))
        .and_then(|addr| addr.parse::<SocketAddr>().ok())
        .is_some_and(is_loopback);
    relay
        && string("name") == Some("OpenAI")
        && string("wire_api") == Some("responses")
        && string("env_key") == Some(TOKEN_ENV)
        && provider
            .get("supports_websockets")
            .and_then(toml::Value::as_bool)
            == Some(true)
}

// ---------------------------------------------------------------------------
// the relay: foreground (no subcommand), background (`start`)
// ---------------------------------------------------------------------------

/// No subcommand: the relay in the foreground, logging to stderr; with the
/// hidden `--background`, the detached instance `start` spawns, logging to
/// its file. Unset options come from the install state, so both run what
/// `install` configured Codex for.
pub fn relay(ctx: &Ctx, opts: &RelayOpts, background: bool) -> Result<()> {
    if background {
        let (args, note) = match resolve_relay(ctx, opts) {
            Ok(resolved) => resolved,
            Err(err) => {
                daemon::log_background_error(&format!("{err:#}"));
                return Err(err);
            }
        };
        return daemon::run_background(&args, note);
    }
    daemon::log_to_stderr();
    let (args, note) = resolve_relay(ctx, opts)?;
    if let Some(note) = note {
        tracing::warn!("{note}");
    }
    // The proxy variables of this shell apply: the relay starts here.
    daemon::run_foreground(&args)
}

/// Starts the relay in the background: refuses when one already answers on
/// the address, spawns this executable detached, and (unless `--no-wait`)
/// waits until it answers `/healthz`.
pub fn start(ctx: &Ctx, args: &StartArgs) -> Result<()> {
    let (relay, note) = resolve_relay(ctx, &args.relay)?;
    if let Some(note) = note {
        warning(note);
    }
    let cfg = daemon::proxy_config(&relay)?;
    if cfg.listen.port() == 0 {
        bail!(
            "--listen needs a fixed port: `start` waits for the relay there, and Codex's \
             base_url points at it"
        );
    }
    let url = relay_url(&relay.listen);
    match daemon::health(&relay.listen, PROBE) {
        Ok(Some(found)) => bail!(
            "a codex-copilot relay already runs on {url} ({}, pid {}). It keeps the settings it \
             was started with: `codex-copilot stop`, then `codex-copilot start`, restarts it.",
            found.version,
            found.pid
        ),
        Ok(None) => {}
        Err(err) => {
            return Err(err.context(format!(
                "{} is taken by something that is not a codex-copilot relay; free the port, or \
                 pick another with --listen",
                relay.listen
            )))
        }
    }
    let log = daemon::log_file()?;
    let mut started = daemon::spawn_background(&relay)?;
    if args.no_wait {
        line(
            "Relay",
            format_args!("starting on {url} (pid {})", started.child.id()),
        );
    } else {
        let health = daemon::wait_started(&relay.listen, &mut started.child, START_TIMEOUT)
            .map_err(|err| {
                err.context(format!(
                    "the relay did not start; its log is {}",
                    log.display()
                ))
            })?;
        line(
            "Relay",
            format_args!("{} running on {url} (pid {})", health.version, health.pid),
        );
    }
    line("Log", log.display());
    if let Some(text) = daemon::exposure_warning(cfg.listen) {
        warning(text);
    }
    if let Some(text) = daemon::job_warning(&started) {
        warning(text);
    }
    Ok(())
}

/// The relay options: each one given on the command line, else the
/// installed value, else the crate default. The second value is a warning
/// for an install state that could not be read (the defaults then apply);
/// the caller reports it where its output goes. The state is not even
/// looked for when every option is given, which is how `start` runs the
/// background relay.
fn resolve_relay(ctx: &Ctx, opts: &RelayOpts) -> Result<(RelayArgs, Option<String>)> {
    let complete = opts.listen.is_some() && opts.upstream.is_some() && opts.review_model.is_some();
    let mut note = None;
    let state = if complete {
        None
    } else {
        installed_state(ctx, |err| {
            note = Some(format!(
                "{err:#}; options not given on the command line get the defaults instead of the \
                 installed values"
            ))
        })?
    };
    let pick = |given: &Option<String>, installed: fn(&State) -> &String, default: &str| {
        given
            .clone()
            .or_else(|| state.as_ref().map(|s| installed(s).clone()))
            .unwrap_or_else(|| default.to_string())
    };
    let args = RelayArgs {
        listen: pick(&opts.listen, |s| &s.listen, DEFAULT_LISTEN)
            .trim()
            .to_string(),
        upstream: pick(&opts.upstream, |s| &s.upstream, DEFAULT_UPSTREAM),
        review_model: pick(
            &opts.review_model,
            |s| &s.review_model,
            DEFAULT_REVIEW_MODEL,
        ),
    };
    Ok((args, note))
}

/// The install state, for the commands that only need the installed
/// settings (the relay, `start`, `stop`). `Ok(None)` when codex-copilot is
/// not installed. A state file that cannot be read goes to `unreadable` (its
/// error names the file) and then counts as absent, so the defaults apply;
/// a layout [`Homes::locate`] refuses, such as both homes holding a state
/// file, is an error: neither file can be trusted then.
fn installed_state(ctx: &Ctx, unreadable: impl FnOnce(anyhow::Error)) -> Result<Option<State>> {
    let homes = ctx.homes()?;
    let located = homes.locate()?;
    if located == Located::None {
        return Ok(None);
    }
    match home::read_state(&homes.copilot_home(located)) {
        Ok(state) => Ok(state),
        Err(err) => {
            unreadable(err);
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_without_websocket_only_warns() {
        let model = |policy: Option<&str>, ws| capi::ModelFacts {
            policy: policy.map(String::from),
            ws,
        };
        let facts: capi::Facts = [
            ("good".to_string(), model(Some("enabled"), true)),
            ("slow".to_string(), model(None, false)),
            ("blocked".to_string(), model(Some("disabled"), true)),
            ("blocked-slow".to_string(), model(Some("disabled"), false)),
        ]
        .into();
        assert_eq!(model_verdict("good", &facts).0, Verdict::Ok);
        let (verdict, line) = model_verdict("slow", &facts);
        assert_eq!(verdict, Verdict::Warn);
        assert!(line.contains("retries the WebSocket"), "{line}");
        assert!(line.contains("HTTP SSE"), "{line}");
        assert_eq!(model_verdict("blocked", &facts).0, Verdict::Fail);
        assert_eq!(model_verdict("blocked-slow", &facts).0, Verdict::Fail);
        assert_eq!(model_verdict("absent", &facts).0, Verdict::Fail);
    }

    #[test]
    fn listen_must_be_a_fixed_ip_and_port() {
        assert_eq!(
            check_listen(" 127.0.0.1:12899 ").unwrap(),
            ("127.0.0.1:12899".to_string(), true)
        );
        assert!(check_listen("[::1]:5000").unwrap().1);
        assert!(!check_listen("0.0.0.0:5000").unwrap().1);
        assert!(!check_listen("[::]:5000").unwrap().1);
        assert!(!check_listen("192.168.1.5:5000").unwrap().1);
        let zero = check_listen("127.0.0.1:0").unwrap_err().to_string();
        assert!(zero.contains("fixed port"), "{zero}");
        assert!(check_listen("127.0.0.1").is_err());
        assert!(check_listen("not an address").is_err());
    }

    /// A name may resolve to several addresses; the relay binds one of them
    /// while Codex's base_url keeps the name.
    #[test]
    fn listen_refuses_a_host_name() {
        for name in ["localhost:12899", "my-pc.local:5000"] {
            let err = check_listen(name).unwrap_err().to_string();
            assert!(
                err.contains("use an IP literal such as 127.0.0.1:12899"),
                "{err}"
            );
        }
    }

    /// One loopback predicate for `install` and the relay's own warning: an
    /// IPv4-mapped loopback address is loopback.
    #[test]
    fn a_mapped_loopback_address_is_loopback() {
        let mapped = "[::ffff:127.0.0.1]:5000";
        assert!(check_listen(mapped).unwrap().1);
        assert_eq!(
            is_loopback(mapped.parse().unwrap()),
            daemon::exposure_warning(mapped.parse().unwrap()).is_none()
        );
        assert!(!check_listen("[::ffff:192.168.1.5]:5000").unwrap().1);
    }

    #[test]
    fn the_relay_url_is_connectable() {
        assert_eq!(relay_url("127.0.0.1:5000"), "http://127.0.0.1:5000");
        assert_eq!(relay_url("0.0.0.0:5000"), "http://127.0.0.1:5000");
        assert_eq!(relay_url("[::]:5000"), "http://[::1]:5000");
    }

    #[test]
    fn a_refusal_names_the_running_images() {
        let running = [
            process::ProcessInfo {
                pid: 7,
                name: "codex-x86_64-pc-windows-msvc.exe".into(),
            },
            process::ProcessInfo {
                pid: 9,
                name: "codex-app-server".into(),
            },
        ];
        assert_eq!(
            running_list(&running),
            "codex-x86_64-pc-windows-msvc.exe (pid 7), codex-app-server (pid 9)"
        );
    }

    #[test]
    fn hosts_are_http_origins_without_a_trailing_slash() {
        assert_eq!(
            check_origin("--host", " https://api.business.githubcopilot.com/ ").unwrap(),
            "https://api.business.githubcopilot.com"
        );
        assert_eq!(
            check_origin("--host", "http://127.0.0.1:9").unwrap(),
            "http://127.0.0.1:9"
        );
        let err = check_origin("--hosts", "api.githubcopilot.com").unwrap_err();
        assert!(err.to_string().starts_with("--hosts must be"), "{err}");
    }

    #[test]
    fn the_candidate_hosts_default_to_the_built_in_list() {
        assert_eq!(candidate_hosts(&[]).unwrap(), capi::DEFAULT_HOSTS);
        let given = ["http://a/".to_string(), " ".into(), "https://b".into()];
        assert_eq!(candidate_hosts(&given).unwrap(), ["http://a", "https://b"]);
        assert!(candidate_hosts(&[" ".into()]).is_err());
        assert!(candidate_hosts(&["ftp://x".into()]).is_err());
        for bad in [
            "https://h?x=1",
            "https://h/v1",
            "https://u@h",
            "https://h#f",
        ] {
            assert!(candidate_hosts(&[bad.into()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_capi_fix_follows_the_probe_outcome() {
        let up = "https://api.example.com";
        assert!(capi_fix(Some(401), up).contains("codex-copilot login"));
        assert!(capi_fix(Some(403), up).contains("no CAPI access"));
        let unreachable = capi_fix(None, up);
        assert!(
            unreachable.contains("could not be reached"),
            "{unreachable}"
        );
        assert!(!unreachable.contains("login"), "{unreachable}");
        let other = capi_fix(Some(502), up);
        assert!(
            other.contains("did not answer with a model list"),
            "{other}"
        );
        assert!(!other.contains("login"), "{other}");
    }

    #[test]
    fn explicit_flags_are_trimmed_and_may_not_be_blank() {
        assert_eq!(not_empty("--model", None).unwrap(), None);
        assert_eq!(
            not_empty("--model", Some(" gpt-5.5 ")).unwrap().as_deref(),
            Some("gpt-5.5")
        );
        assert!(not_empty("--model", Some("  ")).is_err());
    }

    #[test]
    fn a_fresh_plan_is_the_rendered_template() {
        let dir = tempfile::tempdir().unwrap();
        let opts = ConfigOptions::default();
        let plan = plan_config(dir.path(), &opts).unwrap();
        assert_eq!(plan.text, config::render(&opts));
        assert_eq!(plan.verb(), "created");
        assert_eq!(plan.root_str("model").as_deref(), Some(DEFAULT_MODEL));
        plan.write().unwrap();
        let again = plan_config(dir.path(), &opts).unwrap();
        assert_eq!(again.verb(), "unchanged");
        let asked = ConfigOptions {
            model: Some("gpt-5.5".into()),
            ..opts
        };
        let pinned = plan_config(dir.path(), &asked).unwrap();
        assert_eq!(pinned.verb(), "updated");
        assert_eq!(pinned.root_str("model").as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn a_relay_that_differs_is_described() {
        let health = Health {
            name: daemon::HEALTH_NAME.into(),
            version: VERSION.into(),
            upstream: "https://a".into(),
            review_model: Some("r".into()),
            pid: 1,
            proxy: None,
        };
        assert_eq!(mismatch(&health, "https://a", "r"), None);
        let other = mismatch(&health, "https://b", "").unwrap();
        assert!(other.contains("forwards to https://a"), "{other}");
        assert!(other.contains("substitutes r"), "{other}");
        let none = Health {
            review_model: None,
            version: "1.0.0".into(),
            ..health
        };
        let old = mismatch(&none, "https://a", "r").unwrap();
        assert!(old.contains("version 1.0.0"), "{old}");
        assert!(old.contains("no review model"), "{old}");
    }

    #[test]
    fn the_proxy_check_compares_the_relay_with_this_shell() {
        let relay = |proxy: Option<&str>| Health {
            name: daemon::HEALTH_NAME.into(),
            version: VERSION.into(),
            upstream: "https://api.example".into(),
            review_model: None,
            pid: 1,
            proxy: proxy.map(str::to_string),
        };
        let warned = |found: Health, shell: Option<&str>| {
            let mut checks = Checks::default();
            check_proxy(&mut checks, &found, shell.map(str::to_string));
            assert_eq!(checks.failed, 0, "a proxy difference is never a FAIL");
            !checks.fixes.is_empty()
        };
        assert!(!warned(relay(None), None));
        let masked = "http://***@proxy.example:3128";
        assert!(!warned(relay(Some(masked)), Some(masked)));
        // Started before the variable was set, or after it was removed.
        assert!(warned(relay(None), Some(masked)));
        assert!(warned(relay(Some(masked)), None));
        assert!(warned(
            relay(Some(masked)),
            Some("http://proxy.example:3128")
        ));
    }

    #[test]
    fn shell_quoting_survives_quotes() {
        assert_eq!(ps_quote(Path::new(r"C:\it's")), r"'C:\it''s'");
    }

    #[test]
    fn posix_words_are_quoted_only_when_needed() {
        assert_eq!(sh_quote("/home/u/.codex-copilot"), "/home/u/.codex-copilot");
        assert_eq!(sh_quote("/home/a b"), "'/home/a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("$HOME"), "'$HOME'");
    }

    fn installed(dir: &Path, listen: &str) -> Ctx {
        let home = dir.join(crate::COPILOT_HOME_NAME);
        fs::create_dir_all(&home).unwrap();
        home::write_state(
            &home,
            &State {
                version: VERSION.into(),
                installed_at: home::now_rfc3339(),
                listen: listen.into(),
                upstream: "https://installed.example".into(),
                review_model: "installed-review".into(),
                yolo: true,
                codex_version: None,
            },
        )
        .unwrap();
        Ctx::new(Some(dir.to_path_buf()), None)
    }

    #[test]
    fn relay_options_fall_back_to_the_install_then_the_defaults() {
        // Nothing installed: the crate defaults.
        let empty = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(Some(empty.path().to_path_buf()), None);
        let (args, note) = resolve_relay(&ctx, &RelayOpts::default()).unwrap();
        assert_eq!(args, RelayArgs::default());
        assert_eq!(note, None);

        // Installed: its values, each overridable on its own.
        let dir = tempfile::tempdir().unwrap();
        let ctx = installed(dir.path(), "127.0.0.1:5000");
        let (args, _) = resolve_relay(&ctx, &RelayOpts::default()).unwrap();
        assert_eq!(
            args,
            RelayArgs {
                listen: "127.0.0.1:5000".into(),
                upstream: "https://installed.example".into(),
                review_model: "installed-review".into(),
            }
        );
        let opts = RelayOpts {
            listen: Some(" 127.0.0.1:6000 ".into()),
            review_model: Some(String::new()),
            ..RelayOpts::default()
        };
        let (args, _) = resolve_relay(&ctx, &opts).unwrap();
        assert_eq!(args.listen, "127.0.0.1:6000");
        assert_eq!(args.upstream, "https://installed.example");
        assert_eq!(args.review_model, "");
    }

    #[test]
    fn an_unreadable_state_is_reported_and_complete_options_skip_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(crate::COPILOT_HOME_NAME);
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(STATE_FILE), "{ broken").unwrap();
        let ctx = Ctx::new(Some(dir.path().to_path_buf()), None);
        let (args, note) = resolve_relay(&ctx, &RelayOpts::default()).unwrap();
        assert_eq!(args, RelayArgs::default());
        let note = note.expect("a warning");
        assert!(note.contains("get the defaults"), "{note}");

        // Two state files: neither can be trusted, an error...
        let codex = dir.path().join(crate::CODEX_HOME_NAME);
        fs::create_dir_all(&codex).unwrap();
        fs::write(codex.join(STATE_FILE), "{}").unwrap();
        assert!(resolve_relay(&ctx, &RelayOpts::default()).is_err());
        // ...unless every option is given (the background relay), when the
        // state is not even looked at.
        let all = RelayOpts {
            listen: Some("127.0.0.1:7".into()),
            upstream: Some("http://a".into()),
            review_model: Some("r".into()),
        };
        let (args, note) = resolve_relay(&ctx, &all).unwrap();
        assert_eq!(args.listen, "127.0.0.1:7");
        assert_eq!(note, None);
    }

    #[test]
    fn only_a_relay_config_marks_a_stateless_home_as_ours() {
        let dir = tempfile::tempdir().unwrap();
        let ours = |text: &str| {
            fs::write(dir.path().join(CONFIG_FILE), text).unwrap();
            looks_like_relay_home(dir.path())
        };
        assert!(!looks_like_relay_home(dir.path()));
        assert!(!ours(
            "[model_providers.copilot]\nbase_url = \"https://api.githubcopilot.com\"\n"
        ));
        // A user's own proxy on loopback, under the same table name.
        assert!(!ours(
            "model_provider = \"copilot\"\n[model_providers.copilot]\nname = \"Copilot \
             proxy\"\nbase_url = \"http://127.0.0.1:8080\"\nwire_api = \"responses\"\n"
        ));

        // What install writes, on any loopback address.
        let fresh = config::render(&Default::default());
        assert!(ours(&fresh));
        let v6 = ConfigOptions {
            listen: "[::1]:5000".into(),
            ..Default::default()
        };
        assert!(ours(&config::render(&v6)));
        // Merged into a file of the user's, too.
        let merged = config::apply("[projects.'/x']\ntrust_level = \"trusted\"\n", &v6).unwrap();
        assert!(ours(&merged));

        // Any fingerprint key off, and it is not ours.
        let base_url = "base_url = \"http://127.0.0.1:12899\"";
        for (from, to) in [
            ("name = \"OpenAI\"", "name = \"Copilot\""),
            ("wire_api = \"responses\"", "wire_api = \"chat\""),
            (
                "env_key = \"COPILOT_GITHUB_TOKEN\"",
                "env_key = \"GITHUB_TOKEN\"",
            ),
            ("supports_websockets = true", "supports_websockets = false"),
            (base_url, "base_url = \"http://192.168.1.5:12899\""),
            (base_url, "base_url = \"http://localhost:12899\""),
            (base_url, "base_url = \"http://127.0.0.1:12899/v1\""),
            (base_url, "base_url = \"https://127.0.0.1:12899\""),
        ] {
            assert!(fresh.contains(from), "the template lacks {from}");
            assert!(!ours(&fresh.replace(from, to)), "{to} still passes");
        }
    }
}
