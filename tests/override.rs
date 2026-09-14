//! Exercise `override` / `unoverride` through the CLI without Codex,
//! credentials, or a network service.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::json;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_codex-copilot");
const BACKUP: &str = "codex-copilot.override-backup.json";
const DISCARDED: &str = "config.toml.unoverride-discarded";

/// A personal config.toml: comments, an exclude list of the user's own, and a
/// provider table carrying a credential the profile must not leave behind.
const BASE: &str = r#"# Personal configuration: retain this comment.
model = "gpt-6-astra" # This model was already selected.
model_reasoning_effort = "high"

[shell_environment_policy]
exclude = ["PRIVATE_*", "SECRET_*"]

[model_providers.copilot]
experimental_bearer_token = "ghu_personal_leftover"

[model_providers.copilot.http_headers]
authorization = "Bearer leftover"

[personal]
keep = "original" # Keep this inline comment, too.
"#;

struct Run(Output);

impl Run {
    fn output(&self) -> String {
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&self.0.stdout),
            String::from_utf8_lossy(&self.0.stderr)
        )
    }

    fn ok(&self) -> &Self {
        assert!(self.0.status.success(), "{}", self.output());
        self
    }

    fn failed(&self) -> &Self {
        assert!(!self.0.status.success(), "{}", self.output());
        self
    }

    fn has(&self, text: &str) -> &Self {
        assert!(self.output().contains(text), "{}", self.output());
        self
    }

    fn lacks(&self, text: &str) -> &Self {
        assert!(!self.output().contains(text), "{}", self.output());
        self
    }
}

fn run(home: &Path, args: &[&str]) -> Run {
    Run(Command::new(BIN)
        .arg("--codex-home")
        .arg(home)
        .args(["--codex-version", "0.154.0"])
        .args(args)
        .env_remove("CODEX_HOME")
        .env_remove("CODEX_BIN")
        .env_remove("COPILOT_GITHUB_TOKEN")
        .stdin(Stdio::null())
        .output()
        .expect("the CLI must run"))
}

/// Installation metadata is deliberately local and contains no credentials.
fn installed(home: &Path, profile: &str) {
    let directory = home.join(format!("{profile}_config_toml"));
    fs::create_dir_all(&directory).unwrap();
    let catalog_path = directory.join("models-catalog.json");
    fs::write(&catalog_path, r#"{"models":[{"slug":"gpt-6-astra"}]}"#).unwrap();
    fs::write(
        directory.join("state.json"),
        serde_json::to_string(&json!({
            "schema": 2,
            "installed_at": "2026-09-14T00:00:00Z",
            "codex_version": "0.154.0",
            "host": "http://127.0.0.1:9",
            "model": "gpt-6-astra",
            "context_window": "max",
            "models": []
        }))
        .unwrap(),
    )
    .unwrap();
    let catalog_literal = serde_json::to_string(&catalog_path.to_str().unwrap()).unwrap();
    // The five-line header is what must not travel into config.toml.
    fs::write(
        home.join(format!("{profile}.config.toml")),
        format!(
            r#"# codex-copilot 1.0.0 - managed file, rewritten by `install`.
# Use `codex --profile {profile}`; install leaves config.toml unchanged.
model = "gpt-6-astra"
model_provider = "copilot"
model_reasoning_effort = "ultra"
model_catalog_json = {catalog_literal}

[model_providers.copilot]
name = "OpenAI"
base_url = "http://127.0.0.1:9"
wire_api = "responses"
supports_websockets = true
# The bearer is read from the environment at request time.
env_key = "COPILOT_GITHUB_TOKEN"

[model_providers.copilot.http_headers]
"x-github-api-version" = "2026-08-01"

[features]
apps = false

[analytics]
enabled = false

[shell_environment_policy]
exclude = ["COPILOT_GITHUB_TOKEN"]

[tui]
theme = "monokai-extended"
"#
        ),
    )
    .unwrap();
}

fn configured() -> TempDir {
    let home = TempDir::new().unwrap();
    installed(home.path(), "copilot");
    fs::write(home.path().join("config.toml"), BASE).unwrap();
    home
}

fn config(home: &Path) -> toml::Value {
    toml::from_str(&text(home)).unwrap()
}

fn text(home: &Path) -> String {
    fs::read_to_string(home.join("config.toml")).unwrap()
}

fn backup(home: &Path) -> PathBuf {
    home.join(BACKUP)
}

fn discarded(home: &Path) -> PathBuf {
    home.join(DISCARDED)
}

/// Rewrites one line of the installed profile, for the cases only a
/// hand-edited overlay can produce.
fn edit_overlay(home: &Path, from: &str, to: &str) {
    let path = home.join("copilot.config.toml");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(from), "{from} is not in the test overlay");
    fs::write(&path, text.replace(from, to)).unwrap();
}

#[test]
fn a_dry_run_writes_nothing() {
    let home = configured();
    let path = home.path();
    run(path, &["override", "--dry-run"])
        .ok()
        .has("add model_provider")
        .has("Dry run");
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());
    run(path, &["status"]).ok().has("override     inactive");

    run(path, &["override"]).ok();
    let active = fs::read(path.join("config.toml")).unwrap();
    let saved = fs::read(backup(path)).unwrap();
    run(path, &["status"]).ok().has(&format!(
        "override     active (profile `copilot`, backup {})",
        backup(path).display()
    ));
    run(path, &["unoverride", "--dry-run"]).ok().has("Dry run");
    assert_eq!(fs::read(path.join("config.toml")).unwrap(), active);
    assert_eq!(fs::read(backup(path)).unwrap(), saved);
}

#[test]
fn unoverride_restores_the_previous_file_byte_for_byte() {
    let home = configured();
    let path = home.path();
    // CRLF and a blank trailing line: the backup is the file, so both survive.
    let original = BASE.replace('\n', "\r\n") + "\r\n";
    fs::write(path.join("config.toml"), &original).unwrap();

    run(path, &["override"]).ok();
    assert_ne!(text(path), original);
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(backup(path)).unwrap()).unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["profile"], "copilot");
    assert_eq!(saved["original"].as_str(), Some(original.as_str()));
    assert_eq!(saved["applied"].as_str(), Some(text(path).as_str()));

    run(path, &["unoverride"]).ok().lacks("WARNING");
    assert_eq!(text(path), original);
    assert!(!backup(path).exists());
    run(path, &["unoverride"])
        .ok()
        .has("No override backup; nothing to restore.");
    assert_eq!(text(path), original);
}

#[test]
fn the_users_exclude_list_is_unioned_not_replaced() {
    let home = configured();
    let path = home.path();
    run(path, &["override"])
        .ok()
        .has("extend shell_environment_policy.exclude (+1)");
    assert_eq!(
        config(path)["shell_environment_policy"]["exclude"],
        toml::Value::try_from(["PRIVATE_*", "SECRET_*", "COPILOT_GITHUB_TOKEN"]).unwrap()
    );
    // A restore followed by a second override lands on the same list.
    run(path, &["unoverride"]).ok();
    run(path, &["override"]).ok();
    assert_eq!(
        config(path)["shell_environment_policy"]["exclude"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );
}

#[test]
fn a_user_provider_table_is_replaced_whole_and_the_dropped_keys_are_named() {
    let home = configured();
    let path = home.path();
    run(path, &["override"])
        .ok()
        .has("replace model_providers.copilot")
        .has("WARNING  model_providers.copilot replaced whole; dropped your keys: experimental_bearer_token, http_headers.authorization");
    let provider = &config(path)["model_providers"]["copilot"];
    assert_eq!(provider["env_key"].as_str(), Some("COPILOT_GITHUB_TOKEN"));
    assert!(provider.get("experimental_bearer_token").is_none());
    assert!(provider["http_headers"].get("authorization").is_none());
    assert!(!text(path).contains("ghu_personal_leftover"));
    assert!(!text(path).contains("Bearer leftover"));
}

#[test]
fn the_profiles_own_comments_stay_out_of_config_toml() {
    let home = configured();
    let path = home.path();
    run(path, &["override"]).ok();
    let merged = text(path);
    assert!(!merged.contains("codex-copilot 1.0.0 - managed file"));
    assert!(!merged.contains("install leaves config.toml unchanged"));
    assert!(!merged.contains("The bearer is read from the environment"));
    // The user's own comments are all still there.
    assert!(merged.contains("# Personal configuration: retain this comment."));
    assert!(merged.contains("# Keep this inline comment, too."));
    assert_eq!(config(path)["personal"]["keep"].as_str(), Some("original"));
}

#[test]
fn a_second_override_is_refused_by_name() {
    let home = configured();
    let path = home.path();
    run(path, &["override"]).ok();
    let active = fs::read(path.join("config.toml")).unwrap();
    let saved = fs::read(backup(path)).unwrap();
    run(path, &["override"])
        .failed()
        .has(&backup(path).display().to_string())
        .has("run unoverride")
        .has("delete that file to abandon the override");
    // The profile that owns config.toml may not be reinstalled or removed.
    // Empty stdin would reject the token if install reached authentication.
    for args in [vec!["install", "--token-stdin"], vec!["uninstall"]] {
        run(path, &args)
            .failed()
            .has("profile `copilot` is overridden; run unoverride first");
    }
    assert_eq!(fs::read(path.join("config.toml")).unwrap(), active);
    assert_eq!(fs::read(backup(path)).unwrap(), saved);
    assert!(path.join("copilot.config.toml").exists());
}

#[test]
fn another_profiles_override_is_left_alone() {
    let home = configured();
    let path = home.path();
    installed(path, "other");
    run(path, &["--profile", "other", "override"]).ok();
    let active = fs::read(path.join("config.toml")).unwrap();

    run(path, &["unoverride"])
        .failed()
        .has("override belongs to profile `other`; run --profile other unoverride");
    run(path, &["override"]).failed();
    run(path, &["status"])
        .ok()
        .has("override     active for profile `other` (not this profile)");
    // A different profile may still be installed or removed.
    run(path, &["uninstall"]).ok();
    assert_eq!(fs::read(path.join("config.toml")).unwrap(), active);
    assert!(backup(path).exists());

    run(path, &["--profile", "other", "unoverride"]).ok();
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());
}

#[test]
fn a_corrupt_backup_stops_every_write_but_not_status() {
    let home = configured();
    let path = home.path();
    run(path, &["override"]).ok();
    let active = fs::read(path.join("config.toml")).unwrap();
    let corrupt = b"{ not JSON";
    fs::write(backup(path), corrupt).unwrap();
    let named = backup(path).display().to_string();

    run(path, &["status"])
        .ok()
        .has(&format!("override     backup unreadable ({named})"));
    for args in [
        vec!["unoverride"],
        vec!["uninstall"],
        vec!["install", "--token-stdin"],
    ] {
        run(path, &args)
            .failed()
            .has(&named)
            .has("delete it to abandon the override");
    }
    assert_eq!(fs::read(backup(path)).unwrap(), corrupt);
    assert_eq!(fs::read(path.join("config.toml")).unwrap(), active);
    assert!(path.join("copilot.config.toml").exists());
}

#[test]
fn an_unreadable_model_catalog_is_reported_by_path() {
    let home = configured();
    let path = home.path();
    let catalog = path.join("copilot_config_toml").join("models-catalog.json");
    fs::remove_file(&catalog).unwrap();
    run(path, &["override"])
        .failed()
        .has("could not read the model catalog")
        .has(&catalog.display().to_string());
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());

    fs::write(&catalog, r#"{"models":[]}"#).unwrap();
    run(path, &["override"])
        .failed()
        .has(&catalog.display().to_string());
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());
}

#[test]
fn unoverride_deletes_a_config_that_did_not_exist_before() {
    let home = TempDir::new().unwrap();
    let path = home.path();
    installed(path, "copilot");
    run(path, &["override", "--dry-run"]).ok();
    assert!(!path.join("config.toml").exists());

    run(path, &["override"]).ok();
    assert_eq!(config(path)["model_provider"].as_str(), Some("copilot"));
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(backup(path)).unwrap()).unwrap();
    assert!(saved["original"].is_null());

    run(path, &["unoverride"]).ok();
    assert!(!path.join("config.toml").exists());
    assert!(!backup(path).exists());
}

#[test]
fn later_edits_are_saved_to_the_discarded_file() {
    let home = configured();
    let path = home.path();
    run(path, &["override"]).ok();
    let edited = text(path) + "\n[user_notes]\nvalue = \"added after the override\"\n";
    fs::write(path.join("config.toml"), &edited).unwrap();

    let warning = |verb: &str| {
        format!(
            "WARNING  config.toml changed since override; the edited file {verb} {}; merge it \
             back by hand",
            discarded(path).display()
        )
    };
    // A dry run says what it *would* do, and still writes nothing.
    run(path, &["unoverride", "--dry-run"])
        .ok()
        .has(&warning("would be saved to"));
    assert_eq!(text(path), edited);
    assert!(!discarded(path).exists());

    run(path, &["unoverride"])
        .ok()
        .has(&warning("was saved to"));
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());
    // Byte-for-byte what config.toml held just before the restore.
    assert_eq!(fs::read(discarded(path)).unwrap(), edited.as_bytes());

    // And that file now blocks a fresh override until it is dealt with.
    run(path, &["override"]).failed().has(&format!(
        "{} exists from an earlier unoverride; merge it into config.toml by hand, then delete \
             it",
        discarded(path).display()
    ));
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());
    run(path, &["status"]).ok().has(&format!(
        "discarded    {} (merge into config.toml, then delete)",
        discarded(path).display()
    ));

    fs::remove_file(discarded(path)).unwrap();
    run(path, &["status"]).ok().lacks("discarded    ");
    run(path, &["override"]).ok();
}

#[test]
fn a_missing_config_leaves_no_discarded_file() {
    let home = configured();
    let path = home.path();
    run(path, &["override"]).ok();
    fs::remove_file(path.join("config.toml")).unwrap();
    run(path, &["unoverride"])
        .ok()
        .has("WARNING  config.toml is gone since the override; there is nothing to save, restoring the backup");
    assert!(!discarded(path).exists());
    assert_eq!(text(path), BASE);
}

/// Codex mixes `exclude` with `include_only` happily; only `filters` next to
/// either of them is refused, and the file to fix is the one adding `filters`.
#[test]
fn a_merged_policy_codex_would_reject_is_refused_and_names_the_file_to_fix() {
    // config.toml brings `filters`, the profile brings `exclude`.
    let home = configured();
    let path = home.path();
    let base = BASE.replace(
        "[shell_environment_policy]\n",
        "[shell_environment_policy]\nfilters = [\"PATH\"]\n",
    );
    fs::write(path.join("config.toml"), &base).unwrap();
    // The dry run refuses too: it would otherwise promise a merge that
    // leaves codex unable to read its own config.
    for args in [vec!["override", "--dry-run"], vec!["override"]] {
        run(path, &args)
            .failed()
            .has("shell_environment_policy.filters next to shell_environment_policy.exclude")
            .has("cannot mix filters with legacy exclude or include_only")
            .has(&format!(
                "Reconcile shell_environment_policy in {} by hand, then run override again",
                path.join("config.toml").display()
            ));
    }
    assert_eq!(text(path), base);
    assert!(!backup(path).exists());

    // The other direction: a hand-edited profile brings `filters`, and that
    // profile is the file the user is sent to.
    let home = configured();
    let path = home.path();
    edit_overlay(
        path,
        r#"exclude = ["COPILOT_GITHUB_TOKEN"]"#,
        r#"filters = ["PATH"]"#,
    );
    for args in [vec!["override", "--dry-run"], vec!["override"]] {
        run(path, &args)
            .failed()
            .has("shell_environment_policy.filters next to shell_environment_policy.exclude")
            .has(&format!(
                "Reconcile shell_environment_policy in {} by hand, then run override again",
                path.join("copilot.config.toml").display()
            ));
    }
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());
}

#[test]
fn include_only_next_to_the_profiles_exclude_is_merged_not_refused() {
    let home = configured();
    let path = home.path();
    let base = BASE.replace(
        "[shell_environment_policy]\n",
        "[shell_environment_policy]\ninclude_only = [\"PATH\"]\n",
    );
    fs::write(path.join("config.toml"), &base).unwrap();
    run(path, &["override"]).ok();
    let policy = &config(path)["shell_environment_policy"];
    assert_eq!(
        policy["include_only"],
        toml::Value::try_from(["PATH"]).unwrap()
    );
    assert_eq!(
        policy["exclude"],
        toml::Value::try_from(["PRIVATE_*", "SECRET_*", "COPILOT_GITHUB_TOKEN"]).unwrap()
    );
}

#[test]
fn an_incomplete_install_is_refused_before_anything_is_merged() {
    // No state.json: install never finished, or only half of it is left.
    let home = configured();
    let path = home.path();
    let state = path.join("copilot_config_toml").join("state.json");
    fs::remove_file(&state).unwrap();
    run(path, &["override"])
        .failed()
        .has(&state.display().to_string())
        .has("run install first");

    // Anything `status` will not read as an install is refused here too.
    for content in ["{ not JSON", "{}"] {
        fs::write(&state, content).unwrap();
        run(path, &["override"])
            .failed()
            .has("is not a codex-copilot state.json")
            .has("run install first");
        run(path, &["status"])
            .ok()
            .has("Not installed for profile `copilot`");
    }

    // A profile that names no catalog is not one install wrote.
    let home = configured();
    let path = home.path();
    edit_overlay(path, "model_catalog_json = ", "# model_catalog_json = ");
    run(path, &["override"])
        .failed()
        .has("does not set model_catalog_json; run install first");
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());
}

#[test]
fn a_backup_without_an_original_field_is_unreadable_not_empty() {
    let home = configured();
    let path = home.path();
    run(path, &["override"]).ok();
    let active = fs::read(path.join("config.toml")).unwrap();
    let mut saved: serde_json::Value =
        serde_json::from_slice(&fs::read(backup(path)).unwrap()).unwrap();
    saved.as_object_mut().unwrap().remove("original");
    fs::write(backup(path), serde_json::to_vec(&saved).unwrap()).unwrap();

    run(path, &["status"]).ok().has(&format!(
        "override     backup unreadable ({})",
        backup(path).display()
    ));
    run(path, &["unoverride"])
        .failed()
        .has("delete it to abandon the override")
        .has("missing field `original`");
    // config.toml was neither restored nor deleted.
    assert_eq!(fs::read(path.join("config.toml")).unwrap(), active);
}

#[test]
fn an_exclude_entry_is_not_duplicated_by_its_quoting_style() {
    let home = configured();
    let path = home.path();
    fs::write(
        path.join("config.toml"),
        "[shell_environment_policy]\nexclude = ['AWS_*', 'COPILOT_GITHUB_TOKEN']\n",
    )
    .unwrap();
    edit_overlay(
        path,
        r#"exclude = ["COPILOT_GITHUB_TOKEN"]"#,
        r#"exclude = ["AWS_*", "COPILOT_GITHUB_TOKEN"]"#,
    );
    run(path, &["override"]).ok().lacks("extend");
    assert_eq!(
        config(path)["shell_environment_policy"]["exclude"],
        toml::Value::try_from(["AWS_*", "COPILOT_GITHUB_TOKEN"]).unwrap()
    );
}

#[test]
fn an_inline_table_in_config_toml_keeps_every_key() {
    let home = configured();
    let path = home.path();
    fs::write(
        path.join("config.toml"),
        "analytics = { enabled = true, keep = \"mine\" }\n\
         model_providers = { copilot = { name = \"mine\" }, other = { name = \"theirs\" } }\n",
    )
    .unwrap();
    run(path, &["override"]).ok();

    let merged = config(path);
    assert_eq!(merged["analytics"]["enabled"].as_bool(), Some(false));
    assert_eq!(merged["analytics"]["keep"].as_str(), Some("mine"));
    assert_eq!(
        merged["model_providers"]["copilot"]["env_key"].as_str(),
        Some("COPILOT_GITHUB_TOKEN")
    );
    assert_eq!(
        merged["model_providers"]["copilot"]["http_headers"]["x-github-api-version"].as_str(),
        Some("2026-08-01")
    );
    assert_eq!(
        merged["model_providers"]["other"]["name"].as_str(),
        Some("theirs")
    );
    assert_eq!(merged["model_provider"].as_str(), Some("copilot"));
}

#[test]
fn a_deleted_config_is_recreated_from_the_backup() {
    let home = configured();
    let path = home.path();
    run(path, &["override"]).ok();
    fs::remove_file(path.join("config.toml")).unwrap();
    run(path, &["unoverride"]).ok().has("WARNING");
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());
}

/// An unoverride interrupted after it wrote the original back, before it could
/// remove the backup: the retry must finish the job, not discard the restored
/// file over the edits the first run saved.
#[test]
fn a_retried_unoverride_keeps_the_edits_the_first_run_saved() {
    let home = configured();
    let path = home.path();
    run(path, &["override"]).ok();
    let edited = text(path) + "\n[user_notes]\nvalue = \"added after the override\"\n";
    fs::write(path.join("config.toml"), &edited).unwrap();
    let marker = fs::read(backup(path)).unwrap();

    run(path, &["unoverride"]).ok().has("WARNING");
    assert_eq!(text(path), BASE);
    // Put the marker back: that is all the interruption left behind.
    fs::write(backup(path), &marker).unwrap();

    run(path, &["unoverride"])
        .ok()
        .lacks("WARNING")
        .has(&format!("Restored {}.", path.join("config.toml").display()));
    assert_eq!(fs::read(discarded(path)).unwrap(), edited.as_bytes());
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());
}

#[test]
fn a_discarded_file_from_an_earlier_unoverride_is_never_overwritten() {
    let home = configured();
    let path = home.path();
    run(path, &["override"]).ok();
    let edited = text(path) + "\n[user_notes]\nvalue = \"round two\"\n";
    fs::write(path.join("config.toml"), &edited).unwrap();
    let earlier = "earlier = \"work\"\n";
    fs::write(discarded(path), earlier).unwrap();
    let marker = fs::read(backup(path)).unwrap();

    for args in [vec!["unoverride", "--dry-run"], vec!["unoverride"]] {
        run(path, &args).failed().has(&format!(
            "{} already exists from an earlier unoverride; merge it into config.toml by hand and \
             delete it, then run unoverride again",
            discarded(path).display()
        ));
    }
    assert_eq!(fs::read_to_string(discarded(path)).unwrap(), earlier);
    assert_eq!(text(path), edited);
    assert_eq!(fs::read(backup(path)).unwrap(), marker);

    // The same file already holding exactly these edits is left as it is.
    fs::write(discarded(path), &edited).unwrap();
    run(path, &["unoverride"]).ok().has("WARNING");
    assert_eq!(fs::read(discarded(path)).unwrap(), edited.as_bytes());
    assert_eq!(text(path), BASE);
    assert!(!backup(path).exists());
}
