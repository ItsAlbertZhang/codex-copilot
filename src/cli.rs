//! The command line: clap definitions and dispatch to [`crate::commands`].
//!
//! Two halves that never reach into each other. `install`, `login`,
//! `status`, `override`, `unoverride` and `uninstall` only touch files in the
//! dedicated home and never start or stop a process. The relay itself
//! (no subcommand: in the foreground), `start` and `stop` only touch
//! processes and the port; they read the install state for their defaults.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::commands::{self, Ctx};
use crate::{
    CODEX_AUTO_REVIEW, DEFAULT_LISTEN, DEFAULT_MODEL, DEFAULT_REASONING_EFFORT,
    DEFAULT_REVIEW_MODEL, DEFAULT_UPSTREAM, TOKEN_ENV,
};

/// Names the codex binary, like `--codex-bin`.
pub const CODEX_BIN_ENV: &str = "CODEX_BIN";
/// Sets the hidden `--home-dir` (tests).
pub const HOME_DIR_ENV: &str = "CODEX_COPILOT_HOME_DIR";
/// Sets the hidden `install --hosts` (tests).
pub const HOSTS_ENV: &str = "CODEX_COPILOT_HOSTS";

#[derive(Parser, Debug)]
#[command(
    name = "codex-copilot",
    version,
    about = "Run an installed OpenAI Codex CLI on GitHub Copilot CAPI through a local \
             Responses WebSocket relay.",
    long_about = format!(
        "Runs a small local relay between Codex and GitHub Copilot CAPI that keeps streamed \
         item ids stable, and gives Codex a dedicated home, ~/.codex-copilot, configured to go \
         through it. Plain `codex` keeps using ~/.codex until you run `override`.\n\n\
         `install` only writes files; Codex cannot connect until the relay runs. Without a \
         subcommand, codex-copilot runs the relay in the foreground of this terminal (Ctrl-C \
         stops it); `start` runs it in the background and `stop` stops it.\n\n\
         The bearer is a GitHub OAuth token, read by Codex at request time from the \
         {TOKEN_ENV} environment variable and forwarded by the relay. `login` prints one; \
         setting the variable is left to you."
    ),
    disable_help_subcommand = true
)]
pub struct Cli {
    // The environment variables behind these two are read in `run`, not by
    // clap: clap takes an exported-but-empty variable for an empty value and
    // fails every command with "a value is required".
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = format!("Path to the codex binary, if it is not on PATH [env: {CODEX_BIN_ENV}]")
    )]
    pub codex_bin: Option<PathBuf>,

    #[arg(
        long,
        global = true,
        hide = true,
        value_name = "DIR",
        help = format!(
            "Directory holding .codex and .codex-copilot (default: your home directory). \
             Testing only [env: {HOME_DIR_ENV}]"
        )
    )]
    pub home_dir: Option<PathBuf>,

    /// Options of the relay run in the foreground (no subcommand).
    #[command(flatten)]
    pub relay: RelayOpts,

    /// Run as the background relay `start` spawns: log to a file, exit at
    /// once when a relay already answers on the address.
    #[arg(long, hide = true)]
    pub background: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Probe the gateway and write the dedicated home's config and state.
    /// Starts nothing: run `codex-copilot start` afterwards.
    Install(Box<InstallArgs>),
    /// Get a GitHub token with the device flow and print it. Stores nothing.
    Login(AuthArgs),
    /// Check the relay, the dedicated home, the token variable and codex.
    Status(StatusArgs),
    /// Make the dedicated home the default one: swap ~/.codex and
    /// ~/.codex-copilot.
    Override(SwapArgs),
    /// Swap ~/.codex and ~/.codex-copilot back.
    Unoverride(SwapArgs),
    /// Start the relay in the background and wait until it answers.
    Start(StartArgs),
    /// Stop the relay running on the installed (or given) address.
    Stop(StopArgs),
    /// Remove the state file (and with --purge the dedicated home). Leaves a
    /// running relay alone.
    Uninstall(UninstallArgs),
}

/// How a token is obtained. Shared by `login` and `install`.
#[derive(Args, Debug, Default, Clone, PartialEq, Eq)]
pub struct AuthArgs {
    #[arg(
        long,
        value_name = "TOKEN",
        help = format!("Use this GitHub token instead of ${TOKEN_ENV} or the device flow")
    )]
    pub token: Option<String>,

    /// Read the GitHub token from stdin.
    #[arg(long, conflicts_with = "token")]
    pub token_stdin: bool,

    /// github.com base for the device flow. Testing only.
    #[arg(long, hide = true, value_name = "URL")]
    pub github_oauth: Option<String>,

    /// OAuth app client id for the device flow. Testing only.
    #[arg(long, hide = true, value_name = "ID")]
    pub client_id: Option<String>,
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct InstallArgs {
    #[command(flatten)]
    pub auth: AuthArgs,

    /// Use this Copilot gateway (http(s) origin) instead of probing for it.
    #[arg(long, value_name = "URL")]
    pub host: Option<String>,

    #[arg(
        long,
        hide = true,
        value_name = "URL,...",
        value_delimiter = ',',
        help = format!(
            "Gateways probed in order when --host is not given, instead of the built-in list. \
             Testing only [env: {HOSTS_ENV}]"
        )
    )]
    pub hosts: Vec<String>,

    /// Where the relay listens, as an IP address and port (not a host name);
    /// Codex's base_url points at it.
    #[arg(long, value_name = "IP:PORT", default_value = DEFAULT_LISTEN)]
    pub listen: String,

    #[arg(
        long,
        value_name = "SLUG",
        help = format!(
            "`model` [default: {DEFAULT_MODEL}]. A model already in config.toml (a `/model` \
             choice) is kept unless this flag is given"
        )
    )]
    pub model: Option<String>,

    #[arg(
        long,
        value_name = "LEVEL",
        help = format!(
            "`model_reasoning_effort` [default: {DEFAULT_REASONING_EFFORT}]. Kept like --model"
        )
    )]
    pub reasoning_effort: Option<String>,

    #[arg(
        long,
        value_name = "SLUG",
        default_value = DEFAULT_REVIEW_MODEL,
        help = format!("Model the relay sends instead of `{CODEX_AUTO_REVIEW}` (empty: none)")
    )]
    pub review_model: String,

    /// Keep Codex's approvals and sandbox; let the automatic reviewer answer
    /// approval requests.
    #[arg(long)]
    pub no_yolo: bool,

    /// Print what would be written; change nothing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct StatusArgs {
    #[arg(
        long,
        help = format!("Also call CAPI `GET /models` with ${TOKEN_ENV}")
    )]
    pub probe: bool,
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct SwapArgs {
    /// Skip the check for running codex processes.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct UninstallArgs {
    /// Also delete the dedicated home (sessions, config, everything in it).
    #[arg(long)]
    pub purge: bool,

    /// With --purge: skip the check for running codex processes.
    #[arg(long, requires = "purge")]
    pub force: bool,
}

/// What a relay runs with. Unset options come from the install state, else
/// the built-in defaults.
#[derive(Args, Debug, Default, Clone, PartialEq, Eq)]
pub struct RelayOpts {
    #[arg(
        long,
        value_name = "IP:PORT",
        help = format!("Address to listen on [default: the installed one, else {DEFAULT_LISTEN}]")
    )]
    pub listen: Option<String>,

    #[arg(
        long,
        value_name = "URL",
        help = format!(
            "Copilot gateway origin [default: the installed one, else {DEFAULT_UPSTREAM}]"
        )
    )]
    pub upstream: Option<String>,

    #[arg(
        long,
        value_name = "SLUG",
        help = format!(
            "Model substituted for `{CODEX_AUTO_REVIEW}`; empty disables the swap [default: the \
             installed one, else {DEFAULT_REVIEW_MODEL}]"
        )
    )]
    pub review_model: Option<String>,
}

#[derive(Args, Debug, Default, Clone, PartialEq, Eq)]
pub struct StartArgs {
    #[command(flatten)]
    pub relay: RelayOpts,

    /// Return right after spawning the relay instead of waiting until it
    /// answers (for a login-time entry).
    #[arg(long)]
    pub no_wait: bool,
}

#[derive(Args, Debug, Default, Clone, PartialEq, Eq)]
pub struct StopArgs {
    #[arg(
        long,
        value_name = "IP:PORT",
        help = format!(
            "Address of the relay to stop [default: the installed one, else {DEFAULT_LISTEN}]"
        )
    )]
    pub listen: Option<String>,
}

/// Entry point of the `codex-copilot` binary.
pub fn main() -> Result<()> {
    let cli = match parse_from(std::env::args_os()) {
        Ok(cli) => cli,
        Err(err) => {
            // A background relay has no console: its log is the only place
            // a broken command line (a login entry gone stale) shows up.
            if err.use_stderr() && std::env::args_os().any(|arg| arg == "--background") {
                crate::daemon::log_background_error(&err.to_string().trim_end());
            }
            err.exit()
        }
    };
    run(cli)
}

/// Parses a command line (`argv` starts with the program name).
///
/// The top-level relay options and `--background` belong to the bare
/// invocation: next to a subcommand they would be silently ignored, so they
/// are refused. Done here rather than with clap's
/// `args_conflicts_with_subcommands`, which refuses the global `--codex-bin`
/// / `--home-dir` before a subcommand too.
pub fn parse_from<I, T>(argv: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    use clap::CommandFactory;
    let cli = Cli::try_parse_from(argv)?;
    if let Some(command) = &cli.command {
        let given = [
            ("--listen", cli.relay.listen.is_some()),
            ("--upstream", cli.relay.upstream.is_some()),
            ("--review-model", cli.relay.review_model.is_some()),
            ("--background", cli.background),
        ];
        if let Some((flag, _)) = given.iter().find(|(_, given)| *given) {
            let name = command_name(command);
            let hint = if name == "start" && *flag != "--background" {
                format!("; `codex-copilot start {flag} ...` passes it to `start`")
            } else {
                String::new()
            };
            return Err(Cli::command().error(
                clap::error::ErrorKind::ArgumentConflict,
                format!(
                    "{flag} before a subcommand only applies to the relay run without one, not \
                     to `{name}`{hint}"
                ),
            ));
        }
    }
    Ok(cli)
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Install(_) => "install",
        Command::Login(_) => "login",
        Command::Status(_) => "status",
        Command::Override(_) => "override",
        Command::Unoverride(_) => "unoverride",
        Command::Start(_) => "start",
        Command::Stop(_) => "stop",
        Command::Uninstall(_) => "uninstall",
    }
}

/// Dispatches a parsed command line.
pub fn run(cli: Cli) -> Result<()> {
    let home_dir = cli
        .home_dir
        .or_else(|| env_value(HOME_DIR_ENV).map(PathBuf::from));
    let codex_bin = cli
        .codex_bin
        .or_else(|| env_value(CODEX_BIN_ENV).map(PathBuf::from));
    let ctx = Ctx::new(home_dir, codex_bin);
    match cli.command {
        None => commands::relay(&ctx, &cli.relay, cli.background),
        Some(Command::Install(args)) => commands::install(&ctx, &with_env_hosts(*args)),
        Some(Command::Login(args)) => commands::login(&args),
        Some(Command::Status(args)) => commands::status(&ctx, args.probe),
        Some(Command::Override(args)) => commands::override_home(&ctx, args.force),
        Some(Command::Unoverride(args)) => commands::unoverride_home(&ctx, args.force),
        Some(Command::Start(args)) => commands::start(&ctx, &args),
        Some(Command::Stop(args)) => commands::stop(&ctx, &args),
        Some(Command::Uninstall(args)) => commands::uninstall(&ctx, &args),
    }
}

/// `--hosts` from the environment when the flag was not given.
fn with_env_hosts(mut args: InstallArgs) -> InstallArgs {
    if args.hosts.is_empty() {
        if let Some(hosts) = env_value(HOSTS_ENV) {
            args.hosts = split_hosts(&hosts.to_string_lossy());
        }
    }
    args
}

fn split_hosts(list: &str) -> Vec<String> {
    list.split(',').map(str::to_string).collect()
}

/// An environment variable, unless it is unset or blank.
fn env_value(name: &str) -> Option<OsString> {
    not_blank(std::env::var_os(name))
}

/// `None` for an empty or all-whitespace value: an exported-but-empty
/// variable (`export CODEX_BIN=`) means "not set".
fn not_blank(value: Option<OsString>) -> Option<OsString> {
    value.filter(|v| !v.to_str().is_some_and(|s| s.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{self, RelayArgs};
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        parse_from(std::iter::once("codex-copilot").chain(args.iter().copied()))
    }

    fn install(args: &[&str]) -> InstallArgs {
        let mut argv = vec!["install"];
        argv.extend_from_slice(args);
        match parse(&argv).unwrap().command {
            Some(Command::Install(args)) => *args,
            other => panic!("expected install, got {other:?}"),
        }
    }

    fn start(args: &[&str]) -> StartArgs {
        let mut argv = vec!["start"];
        argv.extend_from_slice(args);
        match parse(&argv).unwrap().command {
            Some(Command::Start(args)) => args,
            other => panic!("expected start, got {other:?}"),
        }
    }

    fn help(subcommand: &str) -> String {
        Cli::command()
            .find_subcommand_mut(subcommand)
            .unwrap_or_else(|| panic!("no {subcommand} subcommand"))
            .render_long_help()
            .to_string()
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_bare_invocation_is_the_foreground_relay() {
        let cli = parse(&[]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.background);
        assert_eq!(cli.relay, RelayOpts::default());

        let cli = parse(&[
            "--listen",
            "127.0.0.1:0",
            "--upstream",
            "http://127.0.0.1:1",
            "--review-model",
            "",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.relay.listen.as_deref(), Some("127.0.0.1:0"));
        assert_eq!(cli.relay.upstream.as_deref(), Some("http://127.0.0.1:1"));
        assert_eq!(cli.relay.review_model.as_deref(), Some(""));
        // Global flags go with it on either side.
        let cli = parse(&[
            "--home-dir",
            "h",
            "--listen",
            "127.0.0.1:5",
            "--codex-bin",
            "c",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.home_dir, Some(PathBuf::from("h")));
        assert_eq!(cli.codex_bin, Some(PathBuf::from("c")));
    }

    /// The relay options of the bare invocation do not mix with a
    /// subcommand, where they would be ignored.
    #[test]
    fn relay_options_do_not_combine_with_a_subcommand() {
        for argv in [
            &["--listen", "127.0.0.1:5", "status"][..],
            &["--upstream", "http://a", "install"],
            &["--background", "stop"],
            &["--review-model", "x", "start"],
        ] {
            assert!(parse(argv).is_err(), "{argv:?} parses");
        }
        // `stop` has its own --listen, and only that one.
        let cli = parse(&["stop", "--listen", "127.0.0.1:5"]).unwrap();
        let Some(Command::Stop(args)) = cli.command else {
            panic!("expected stop")
        };
        assert_eq!(args.listen.as_deref(), Some("127.0.0.1:5"));
        assert!(parse(&["stop", "--upstream", "http://a"]).is_err());
        assert!(parse(&["stop", "--purge"]).is_err());
    }

    /// What `start` spawns is read back by this parser as the same options.
    #[test]
    fn the_background_command_line_round_trips() {
        let args = RelayArgs {
            listen: "127.0.0.1:5000".into(),
            upstream: "https://api.githubcopilot.com".into(),
            review_model: String::new(),
        };
        let mut argv = vec!["codex-copilot".to_string()];
        argv.extend(daemon::background_argv(&args));
        let cli = parse_from(argv).unwrap();
        assert!(cli.background && cli.command.is_none());
        assert_eq!(cli.relay.listen.as_deref(), Some("127.0.0.1:5000"));
        assert_eq!(
            cli.relay.upstream.as_deref(),
            Some("https://api.githubcopilot.com")
        );
        assert_eq!(cli.relay.review_model.as_deref(), Some(""));
        assert_eq!(daemon::BACKGROUND_FLAG, "--background");
    }

    #[test]
    fn start_takes_the_relay_options_and_no_wait() {
        assert_eq!(start(&[]), StartArgs::default());
        let a = start(&[
            "--listen",
            "127.0.0.1:5000",
            "--upstream",
            "http://127.0.0.1:9",
            "--review-model",
            "",
            "--no-wait",
        ]);
        assert_eq!(a.relay.listen.as_deref(), Some("127.0.0.1:5000"));
        assert_eq!(a.relay.upstream.as_deref(), Some("http://127.0.0.1:9"));
        assert_eq!(a.relay.review_model.as_deref(), Some(""));
        assert!(a.no_wait);
        assert!(parse(&["start", "--background"]).is_err());
    }

    #[test]
    fn install_flags_parse_with_their_defaults() {
        let defaults = install(&[]);
        assert_eq!(defaults.listen, DEFAULT_LISTEN);
        assert_eq!(defaults.review_model, DEFAULT_REVIEW_MODEL);
        // Unset, so a model already in config.toml is kept.
        assert!(defaults.model.is_none() && defaults.reasoning_effort.is_none());
        assert!(!defaults.no_yolo && !defaults.dry_run);
        assert!(defaults.host.is_none() && defaults.hosts.is_empty());
        assert_eq!(defaults.auth, AuthArgs::default());

        let a = install(&[
            "--token",
            "t",
            "--host",
            "http://127.0.0.1:9",
            "--listen",
            "127.0.0.1:5000",
            "--model",
            "gpt-5.5",
            "--reasoning-effort",
            "high",
            "--review-model",
            "",
            "--no-yolo",
            "--dry-run",
        ]);
        assert_eq!(a.auth.token.as_deref(), Some("t"));
        assert_eq!(a.host.as_deref(), Some("http://127.0.0.1:9"));
        assert_eq!(a.listen, "127.0.0.1:5000");
        assert_eq!(a.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(a.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(a.review_model, "");
        assert!(a.no_yolo && a.dry_run);
    }

    #[test]
    fn hosts_is_a_comma_separated_list() {
        let a = install(&["--hosts", "http://a,http://b", "--hosts", "http://c"]);
        assert_eq!(a.hosts, ["http://a", "http://b", "http://c"]);
        assert_eq!(split_hosts("http://a,http://b"), ["http://a", "http://b"]);
    }

    #[test]
    fn removed_flags_are_really_gone() {
        for flags in [
            &["install", "--profile", "x"][..],
            &["--profile", "x", "status"],
            &["install", "--codex-home", "y"],
            &["--codex-home", "y", "status"],
            &["install", "--catalog", "models.json"],
            &["install", "--model-window", "gpt-5.5=400000"],
            &["install", "--context-window", "max"],
            &["install", "--auto-review"],
            &["install", "--codex-version", "0.160.0"],
            &["install", "--no-autostart"],
            &["install", "--no-start"],
            &["uninstall", "--no-autostart"],
            &["status", "--dry-run"],
            &["serve"],
            &["doctor"],
        ] {
            assert!(parse(flags).is_err(), "{flags:?} still parses");
        }
    }

    #[test]
    fn token_and_token_stdin_are_exclusive() {
        assert!(parse(&["login", "--token", "x", "--token-stdin"]).is_err());
        assert!(parse(&["install", "--token", "x", "--token-stdin"]).is_err());
    }

    #[test]
    fn global_flags_work_on_either_side_of_the_subcommand() {
        let cli = parse(&["status", "--home-dir", "h", "--codex-bin", "c"]).unwrap();
        assert_eq!(cli.home_dir, Some(PathBuf::from("h")));
        assert_eq!(cli.codex_bin, Some(PathBuf::from("c")));
        let cli = parse(&[
            "--home-dir",
            "h",
            "--codex-bin",
            "c",
            "uninstall",
            "--purge",
        ])
        .unwrap();
        assert_eq!(cli.home_dir, Some(PathBuf::from("h")));
        assert!(matches!(
            cli.command,
            Some(Command::Uninstall(UninstallArgs { purge: true, .. }))
        ));
        let cli = parse(&["--home-dir", "h", "start", "--no-wait"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Start(StartArgs { no_wait: true, .. }))
        ));
    }

    #[test]
    fn uninstall_force_only_goes_with_purge() {
        let cli = parse(&["uninstall", "--purge", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Uninstall(UninstallArgs {
                purge: true,
                force: true,
            }))
        ));
        assert!(parse(&["uninstall", "--force"]).is_err());
    }

    #[test]
    fn help_shows_the_defaults_the_crate_uses() {
        let install = help("install");
        for default in [
            DEFAULT_MODEL,
            DEFAULT_REASONING_EFFORT,
            DEFAULT_REVIEW_MODEL,
            DEFAULT_LISTEN,
            CODEX_AUTO_REVIEW,
            TOKEN_ENV,
        ] {
            assert!(install.contains(default), "install help lacks {default}");
        }
        assert!(!install.contains("--hosts"), "--hosts is hidden");
        let start = help("start");
        for default in [DEFAULT_LISTEN, DEFAULT_UPSTREAM, DEFAULT_REVIEW_MODEL] {
            assert!(start.contains(default), "start help lacks {default}");
        }
        assert!(start.contains("--no-wait"));
        assert!(help("stop").contains(DEFAULT_LISTEN));
        assert!(help("status").contains(TOKEN_ENV));
        let top = Cli::command().render_long_help().to_string();
        assert!(top.contains(CODEX_BIN_ENV) && top.contains(TOKEN_ENV));
        for default in [DEFAULT_LISTEN, DEFAULT_UPSTREAM, DEFAULT_REVIEW_MODEL] {
            assert!(top.contains(default), "top-level help lacks {default}");
        }
        assert!(top.contains("`start` runs it in the background"));
        assert!(!top.contains(HOME_DIR_ENV), "--home-dir is hidden");
        assert!(!top.contains("--background"), "--background is hidden");
    }

    #[test]
    fn blank_environment_values_count_as_unset() {
        assert_eq!(not_blank(None), None);
        assert_eq!(not_blank(Some("".into())), None);
        assert_eq!(not_blank(Some("  ".into())), None);
        assert_eq!(not_blank(Some("codex".into())), Some("codex".into()));
    }
}
