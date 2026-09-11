//! End-to-end tests against a local stub of the two hosts install talks to.
//!
//! Nothing here touches the real CODEX_HOME, the real CAPI, or the user's
//! environment: every run gets a temp `--codex-home`, a dummy token, and a
//! child environment with COPILOT_GITHUB_TOKEN/CODEX_HOME/CODEX_BIN removed.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_codex-copilot");
const TOKEN: &str = "gho_dummy_test_token";
const CODEX_VERSION: &str = "0.154.0";

/// The node stub, killed when the test ends.
struct Stub {
    child: Child,
    port: u16,
}

impl Stub {
    fn start(extra: &[&str]) -> Stub {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/stub/stub-capi.mjs");
        let mut child = Command::new("node")
            .arg(&script)
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("node must be on PATH to run these tests");
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .expect("stub must announce its port");
        let port = line
            .trim()
            .strip_prefix("LISTENING ")
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("unexpected stub greeting: {line:?}"));
        Stub { child, port }
    }
    fn host(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    fn catalog_url(&self) -> String {
        format!("http://127.0.0.1:{}/models.json", self.port)
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Run {
    out: Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn ok(&self) -> &Self {
        assert!(
            self.out.status.success(),
            "expected success\nstdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr
        );
        self
    }
    fn failed(&self) -> &Self {
        assert!(
            !self.out.status.success(),
            "expected failure\n{}",
            self.stdout
        );
        self
    }
    fn has(&self, needle: &str) -> &Self {
        assert!(
            self.stdout.contains(needle) || self.stderr.contains(needle),
            "expected {needle:?} in output\nstdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr
        );
        self
    }
}

/// Runs the binary with a clean environment; `token` seeds COPILOT_GITHUB_TOKEN.
fn run_with(home: &Path, args: &[&str], token: Option<&str>) -> Run {
    let mut cmd = Command::new(BIN);
    cmd.args(["--codex-home"])
        .arg(home)
        .args(["--codex-version", CODEX_VERSION])
        .args(args)
        .env_remove("COPILOT_GITHUB_TOKEN")
        .env_remove("CODEX_HOME")
        .env_remove("CODEX_BIN")
        .stdin(Stdio::null());
    if let Some(t) = token {
        cmd.env("COPILOT_GITHUB_TOKEN", t);
    }
    let out = cmd.output().expect("binary must run");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        out,
    }
}

fn run(home: &Path, args: &[&str]) -> Run {
    run_with(home, args, None)
}

/// A full install against the stub, which most tests start from.
fn install(home: &Path, stub: &Stub, extra: &[&str]) -> Run {
    let (host, catalog_url) = (stub.host(), stub.catalog_url());
    let mut args = vec![
        "install",
        "--token",
        TOKEN,
        "--host",
        &host,
        "--catalog-url",
        &catalog_url,
    ];
    args.extend_from_slice(extra);
    run(home, &args)
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn overlay(home: &Path) -> String {
    std::fs::read_to_string(home.join("copilot.config.toml")).unwrap()
}

fn catalog(home: &Path) -> Value {
    read_json(&home.join("copilot_config_toml/models-catalog.json"))
}

#[test]
fn dry_run_prints_the_overlay_and_writes_nothing() {
    let home = TempDir::new().unwrap();
    let stub = Stub::start(&[]);
    let r = install(home.path(), &stub, &["--dry-run"]);
    r.ok()
        .has("Dry run, nothing written")
        .has("model_provider = \"copilot\"")
        .has("env_key = \"COPILOT_GITHUB_TOKEN\"")
        .has("exclude = [\"COPILOT_GITHUB_TOKEN\"]")
        .has("wire_api = \"responses\"")
        .has("supports_websockets = true");
    assert!(!home.path().join("copilot.config.toml").exists());
    assert!(!home.path().join("copilot_config_toml").exists());
}

#[test]
fn install_with_relative_home_writes_the_overlay_the_catalog_and_the_state() {
    let cwd = std::env::current_dir().unwrap();
    let home = TempDir::new_in(&cwd).unwrap();
    let stub = Stub::start(&[]);
    let r = install(home.path().strip_prefix(&cwd).unwrap(), &stub, &[]);
    r.ok()
        // The token is echoed once, with the one-liner the user runs.
        .has(&format!("COPILOT_GITHUB_TOKEN={TOKEN}"))
        .has("SetEnvironmentVariable")
        .has("codex --profile copilot");

    let text = overlay(home.path());
    assert!(text.contains(&format!("base_url = \"{}\"", stub.host())));
    assert!(text.contains("name = \"OpenAI\""));
    assert!(text.contains("remote_compaction_v2 = true"));
    // The bearer is never written anywhere.
    assert!(!text.contains(TOKEN));

    // Calibration: the seat allows astra 872k, the bundled catalog said 272k.
    let config: toml::Value = toml::from_str(&text).unwrap();
    let catalog_path = Path::new(config["model_catalog_json"].as_str().unwrap());
    assert!(catalog_path.is_absolute());
    let cat = read_json(catalog_path);
    let astra = &cat["models"][0];
    assert_eq!(astra["slug"], "gpt-6-astra");
    assert_eq!(astra["context_window"], 872_000);
    assert_eq!(astra["max_context_window"], 872_000);
    assert_eq!(astra["auto_compact_token_limit"], 784_800);
    assert_eq!(astra["tool_mode"], "code_mode_only");
    // gpt-5.2 is not on this seat, so it keeps what codex shipped.
    assert_eq!(cat["models"][3]["slug"], "gpt-5.2");
    assert_eq!(cat["models"][3]["context_window"], 272_000);

    let state = read_json(&home.path().join("copilot_config_toml/state.json"));
    assert_eq!(state["host"], stub.host());
    assert_eq!(state["model"], "gpt-6-astra");
    assert_eq!(state["codex_version"], CODEX_VERSION);
    assert_eq!(state["models"][0]["capi_max"], 872_000);
    assert_eq!(state["models"][0]["tier_base"], 272_000);
    // No secret ever reaches state.json.
    let raw = std::fs::read_to_string(home.path().join("copilot_config_toml/state.json")).unwrap();
    assert!(!raw.contains(TOKEN) && !raw.contains("login"));
}

#[test]
fn the_probe_walks_the_host_list_in_preference_order() {
    let home = TempDir::new().unwrap();
    let refused = Stub::start(&["--status", "401"]);
    let good = Stub::start(&[]);
    let r = run(
        home.path(),
        &[
            "install",
            "--token",
            TOKEN,
            "--host-list",
            &refused.host(),
            "--host-list",
            &good.host(),
            "--catalog-url",
            &good.catalog_url(),
        ],
    );
    r.ok()
        .has(&format!("CAPI host    {}", good.host()))
        .has(&format!("skipped    {}  401", refused.host()));
    assert!(overlay(home.path()).contains(&format!("base_url = \"{}\"", good.host())));

    // When no host answers, the error names every attempt.
    let home2 = TempDir::new().unwrap();
    run(
        home2.path(),
        &["install", "--token", TOKEN, "--host", &refused.host()],
    )
    .failed()
    .has("no Copilot CAPI gateway accepted this token")
    .has("401");
}

#[test]
fn a_catalog_that_cannot_be_fetched_fails_with_the_url_and_the_flag() {
    let home = TempDir::new().unwrap();
    let stub = Stub::start(&["--catalog-status", "500"]);
    install(home.path(), &stub, &[])
        .failed()
        .has("could not fetch the model catalog")
        .has("--catalog <path>")
        .has("/models.json");
    assert!(!home.path().join("copilot.config.toml").exists());
}

#[test]
fn a_local_catalog_plus_window_overrides_are_honoured() {
    let home = TempDir::new().unwrap();
    let stub = Stub::start(&[]);
    // Grab the bundled catalog through one install, then feed it back as a file.
    install(home.path(), &stub, &[]).ok();
    let local = home.path().join("bundled.json");
    std::fs::write(
        &local,
        serde_json::to_string(&serde_json::json!({
            "models": [
                {"slug": "gpt-6-astra", "context_window": 272000, "max_context_window": 872000},
                {"slug": "gpt-5.5", "context_window": 272000, "max_context_window": 272000}
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let home2 = TempDir::new().unwrap();
    run(
        home2.path(),
        &[
            "install",
            "--token",
            TOKEN,
            "--host",
            &stub.host(),
            "--catalog",
            local.to_str().unwrap(),
            "--context-window",
            "base",
            "--model-window",
            "gpt-5.5=400000:300000",
        ],
    )
    .ok();
    let cat = catalog(home2.path());
    // `base` keeps every model in the standard price tier ...
    assert_eq!(cat["models"][0]["context_window"], 272_000);
    assert_eq!(cat["models"][0]["auto_compact_token_limit"], 244_800);
    // ... and an explicit --model-window still wins.
    assert_eq!(cat["models"][1]["context_window"], 400_000);
    assert_eq!(cat["models"][1]["auto_compact_token_limit"], 300_000);

    // A window CAPI would refuse is refused here.
    let home3 = TempDir::new().unwrap();
    run(
        home3.path(),
        &[
            "install",
            "--token",
            TOKEN,
            "--host",
            &stub.host(),
            "--catalog",
            local.to_str().unwrap(),
            "--model-window",
            "gpt-6-astra=9000000",
        ],
    )
    .failed()
    .has("CAPI accepts at most 872000");
}

#[test]
fn status_checks_the_chain_and_notices_a_missing_token() {
    let home = TempDir::new().unwrap();
    let stub = Stub::start(&[]);
    run(home.path(), &["status"])
        .ok()
        .has("Not installed for profile `copilot`");

    install(home.path(), &stub, &[]).ok();

    // Without the variable set, status says so and skips the network check.
    run(home.path(), &["status"])
        .ok()
        .has("token        FAIL")
        .has("capi         skipped")
        .has("codex-copilot login");

    // With it set, every check passes and the manual verification is printed.
    run_with(home.path(), &["status"], Some(TOKEN))
        .ok()
        .has("token        ok")
        .has("capi         ok")
        .has("model        ok")
        .has("overlay      ok")
        .has("catalog      ok")
        .has("All checks passed")
        .has("codex --profile copilot");

    // All other checks pass, but a missing Codex must prevent the success summary.
    let out = Command::new(BIN)
        .arg("--codex-home")
        .arg(home.path())
        .arg("--codex-bin")
        .arg(home.path().join("missing-codex"))
        .arg("status")
        .env_remove("CODEX_HOME")
        .env_remove("CODEX_BIN")
        .env("COPILOT_GITHUB_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("codex        FAIL"), "{stdout}");
    assert!(stdout.contains("To fix:"), "{stdout}");
    assert!(!stdout.contains("All checks passed"), "{stdout}");
}

#[test]
fn login_retries_a_json_server_error() {
    let home = TempDir::new().unwrap();
    let stub = Stub::start(&[]);
    run(home.path(), &["login", "--github-oauth", &stub.host()])
        .ok()
        .has(&format!("COPILOT_GITHUB_TOKEN={TOKEN}"));
}

#[test]
fn uninstall_removes_the_files_and_only_prints_the_unset_line() {
    let home = TempDir::new().unwrap();
    let stub = Stub::start(&[]);
    install(home.path(), &stub, &[]).ok();

    run(home.path(), &["uninstall"])
        .ok()
        .has("copilot.config.toml")
        .has("$null")
        .has("this tool never set it");
    assert!(!home.path().join("copilot.config.toml").exists());
    assert!(!home.path().join("copilot_config_toml/state.json").exists());
    assert!(!home.path().join("copilot_config_toml").exists());

    run(home.path(), &["uninstall"])
        .ok()
        .has("Nothing to remove");
}

#[test]
fn an_unusable_model_warns_with_a_remedy() {
    let home = TempDir::new().unwrap();
    let disabled = Stub::start(&["--policy", "disabled", "--no-ws"]);
    install(home.path(), &disabled, &[])
        .ok()
        .has("policy state `disabled`")
        .has("does not advertise ws:/responses");

    let home2 = TempDir::new().unwrap();
    let stub = Stub::start(&[]);
    install(home2.path(), &stub, &["--model", "gpt-9-nope"])
        .ok()
        .has("is not in")
        .has("--model");
}
