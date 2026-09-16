//! `codex-copilot` - point an installed Codex CLI at GitHub Copilot's CAPI.
//!
//! Two decoupled halves, exposed as two commands:
//!
//!   * [`auth`] obtains a GitHub OAuth token and prints it. It knows nothing
//!     about Codex (`codex-copilot login`).
//!   * [`capi`] / [`catalog`] / [`overlay`] probe the seat, calibrate the model
//!     catalog and write the profile overlay. They know nothing about how the
//!     token was obtained (`codex-copilot install`).
//!
//! [`commands`] is the only place the two meet.

mod auth;
mod capi;
mod catalog;
mod codex;
mod commands;
mod config_override;
mod defaults;
mod overlay;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::defaults::{DEFAULT_AUTO_REVIEW, DEFAULT_MODEL};

/// The variable Codex reads the CAPI bearer from (`env_key` in the overlay).
/// The user owns it: nothing here ever writes it.
pub const TOKEN_ENV: &str = "COPILOT_GITHUB_TOKEN";
/// `model_provider` id, and the `[model_providers.<id>]` table name.
pub const PROVIDER_ID: &str = "copilot";

const DEFAULT_WINDOW: &str = "max";

#[derive(Parser, Debug)]
#[command(
    name = "codex-copilot",
    version,
    about = "Wire an installed OpenAI Codex CLI to GitHub Copilot CAPI (stateful ws:/responses).",
    long_about = "Writes one file, $CODEX_HOME/<profile>.config.toml, which Codex reads only when \
                  you pass `--profile <profile>`, plus a model catalog calibrated to what CAPI \
                  really allows this seat. install leaves config.toml unchanged; override merges \
                  the profile into it, and unoverride restores config.toml to its pre-override \
                  content.\n\n\
                  The bearer is a GitHub OAuth token, read at runtime from the \
                  COPILOT_GITHUB_TOKEN environment variable. `login` prints one; setting the \
                  variable is left to you.",
    disable_help_subcommand = true
)]
struct Cli {
    /// Codex home directory (default: $CODEX_HOME, else ~/.codex).
    #[arg(long, global = true, value_name = "DIR", env = "CODEX_HOME")]
    codex_home: Option<PathBuf>,

    /// Profile name; writes $CODEX_HOME/<name>.config.toml.
    #[arg(long, global = true, value_name = "NAME", default_value = "copilot")]
    profile: String,

    /// Print what would happen and write nothing.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Path to the codex binary, if it is not on PATH.
    #[arg(long, global = true, value_name = "PATH", env = "CODEX_BIN")]
    codex_bin: Option<PathBuf>,

    /// Use this instead of running `codex --version`. Testing only.
    #[arg(long, global = true, hide = true, value_name = "VERSION")]
    codex_version: Option<String>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Probe the seat, calibrate the catalog, write the profile (the default).
    Install(Box<InstallArgs>),
    /// Obtain a GitHub token and print it. Touches nothing else.
    Login(AuthArgs),
    /// Summarise the install and check every link in the chain.
    Status,
    /// Merge the installed profile into config.toml, backing up the whole file.
    Override,
    /// Restore config.toml to its pre-override content, then drop the backup.
    Unoverride,
    /// Remove the overlay and the profile directory.
    Uninstall,
}

/// How a token is obtained. Shared by `login` and `install`.
#[derive(Args, Debug, Default, Clone)]
pub struct AuthArgs {
    /// Use this token instead of running the device flow.
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,

    /// Read the token from stdin instead of running the device flow.
    #[arg(long, conflicts_with = "token")]
    pub token_stdin: bool,

    /// github.com base for the device flow. Testing only.
    #[arg(long, hide = true, value_name = "URL")]
    pub github_oauth: Option<String>,

    /// OAuth app client id for the device flow. Testing only.
    #[arg(long, hide = true, value_name = "ID")]
    pub client_id: Option<String>,
}

#[derive(Args, Debug)]
pub struct InstallArgs {
    #[command(flatten)]
    pub auth: AuthArgs,

    /// Model id to wire up.
    #[arg(long, value_name = "ID", default_value = DEFAULT_MODEL)]
    pub model: String,

    /// Leave approval policy and sandbox mode to Codex instead of configuring --yolo.
    #[arg(long)]
    pub no_yolo: bool,

    /// Enable automatic approval review with an optional model.
    #[arg(
        long,
        value_name = "MODEL",
        num_args = 0..=1,
        default_missing_value = DEFAULT_AUTO_REVIEW,
        help = format!("Enable automatic approval review (no MODEL: {DEFAULT_AUTO_REVIEW}; only useful with --no-yolo)")
    )]
    pub auto_review: Option<String>,

    /// Window every calibrated model is budgeted against: `max` (everything
    /// CAPI accepts, billed ~2x above the base tier), `base` (the
    /// standard-price tier), or a token count.
    #[arg(long, value_name = "max|base|N", default_value = DEFAULT_WINDOW)]
    pub context_window: String,

    /// Override one model after calibration; repeatable.
    /// Example: --model-window gpt-5.5=400000:320000
    #[arg(long, value_name = "SLUG=WINDOW[:COMPACT]")]
    pub model_window: Vec<String>,

    /// Calibrate this local copy of the bundled models.json instead of
    /// downloading it.
    #[arg(long, value_name = "PATH")]
    pub catalog: Option<PathBuf>,

    /// Pin the CAPI origin instead of probing for it.
    #[arg(long, value_name = "URL")]
    pub host: Option<String>,

    /// Replace the probed origin list; repeatable, preference order. Testing only.
    #[arg(long, hide = true, value_name = "URL")]
    pub host_list: Vec<String>,

    /// Where the bundled models.json is fetched from. Testing only.
    #[arg(long, hide = true, value_name = "URL")]
    pub catalog_url: Option<String>,
}

/// Mirrors the clap defaults for the bare `codex-copilot` invocation, which
/// means `install`. Kept honest by `bare_invocation_matches_install_defaults`.
impl Default for InstallArgs {
    fn default() -> Self {
        Self {
            auth: AuthArgs::default(),
            model: DEFAULT_MODEL.to_string(),
            no_yolo: false,
            auto_review: None,
            context_window: DEFAULT_WINDOW.to_string(),
            model_window: Vec::new(),
            catalog: None,
            host: None,
            host_list: Vec::new(),
            catalog_url: None,
        }
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let ctx = commands::Ctx::new(
        cli.codex_home,
        cli.profile,
        cli.dry_run,
        cli.codex_bin,
        cli.codex_version,
    )?;
    match cli.command {
        None => commands::install(&ctx, &InstallArgs::default()),
        Some(Cmd::Install(args)) => commands::install(&ctx, &args),
        Some(Cmd::Login(args)) => commands::login(&args),
        Some(Cmd::Status) => commands::status(&ctx),
        Some(Cmd::Override) => config_override::apply(&ctx),
        Some(Cmd::Unoverride) => config_override::restore(&ctx),
        Some(Cmd::Uninstall) => commands::uninstall(&ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_invocation_matches_install_defaults() {
        let cli = Cli::try_parse_from(["codex-copilot"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.profile, "copilot");
        assert!(!cli.dry_run);

        let parsed = Cli::try_parse_from(["codex-copilot", "install"]).unwrap();
        let Some(Cmd::Install(a)) = parsed.command else {
            panic!("expected install")
        };
        let d = InstallArgs::default();
        assert_eq!(a.model, d.model);
        assert_eq!(a.no_yolo, d.no_yolo);
        assert_eq!(a.auto_review, d.auto_review);
        assert!(!a.no_yolo);
        assert!(a.auto_review.is_none());
        assert_eq!(a.context_window, d.context_window);
        assert_eq!(a.model_window, d.model_window);
        assert_eq!(a.catalog, d.catalog);
        assert_eq!(a.host, d.host);
        assert_eq!(a.auth.token, d.auth.token);
        assert_eq!(a.auth.token_stdin, d.auth.token_stdin);
    }

    #[test]
    fn auto_review_is_opt_in_with_an_optional_model() {
        for (flags, reviewer, no_yolo) in [
            (vec!["--auto-review"], DEFAULT_AUTO_REVIEW, false),
            (vec!["--auto-review", "gpt-5.5"], "gpt-5.5", false),
            (vec!["--auto-review=gpt-5.5"], "gpt-5.5", false),
            (
                vec!["--auto-review", "--no-yolo"],
                DEFAULT_AUTO_REVIEW,
                true,
            ),
            (
                vec!["--no-yolo", "--auto-review", "gpt-5.5"],
                "gpt-5.5",
                true,
            ),
        ] {
            let cli =
                Cli::try_parse_from(["codex-copilot", "install"].into_iter().chain(flags)).unwrap();
            let Some(Cmd::Install(a)) = cli.command else {
                panic!("expected install")
            };
            assert_eq!(a.auto_review.as_deref(), Some(reviewer));
            assert_eq!(a.no_yolo, no_yolo);
        }
    }

    #[test]
    fn no_yolo_does_not_enable_auto_review() {
        let cli = Cli::try_parse_from(["codex-copilot", "install", "--no-yolo"]).unwrap();
        let Some(Cmd::Install(a)) = cli.command else {
            panic!("expected install")
        };
        assert!(a.no_yolo);
        assert!(a.auto_review.is_none());
    }

    #[test]
    fn removed_flags_are_really_gone() {
        for flag in [
            "--no-persist",
            "--forget-token",
            "--env-target",
            "--no-catalog",
            "--compact-ratio",
            "--no-copy",
            "--set-default",
            "--skip-auto-review",
            "--auto-review-model",
            "--no-auto-review",
        ] {
            assert!(
                Cli::try_parse_from(["codex-copilot", "install", flag]).is_err(),
                "{flag} still parses"
            );
        }
        assert!(Cli::try_parse_from([
            "codex-copilot",
            "install",
            "--auto-review-model",
            "gpt-5.5",
        ])
        .is_err());
        assert!(Cli::try_parse_from(["codex-copilot", "doctor"]).is_err());
        assert!(Cli::try_parse_from(["codex-copilot", "token"]).is_err());
    }

    #[test]
    fn token_and_token_stdin_are_exclusive_on_both_commands() {
        assert!(
            Cli::try_parse_from(["codex-copilot", "login", "--token", "x", "--token-stdin"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["codex-copilot", "install", "--token", "x", "--token-stdin"])
                .is_err()
        );
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from([
            "codex-copilot",
            "install",
            "--dry-run",
            "--profile",
            "cp2",
            "--host",
            "http://127.0.0.1:9",
        ])
        .unwrap();
        assert!(cli.dry_run);
        assert_eq!(cli.profile, "cp2");
        let Some(Cmd::Install(a)) = cli.command else {
            panic!("expected install")
        };
        assert_eq!(a.host.as_deref(), Some("http://127.0.0.1:9"));
    }
}
