//! End-to-end tests of the `codex-copilot` binary against a local stub of
//! CAPI's `GET /models` and github.com's device flow (tests/stub/stub-capi.mjs,
//! run with node).
//!
//! Hermetic: every run gets a temp `--home-dir`, a fake `codex` that only
//! prints a version, a dummy token, and a child environment without
//! CODEX_HOME / CODEX_BIN / COPILOT_GITHUB_TOKEN / CODEX_COPILOT_HOME_DIR /
//! CODEX_COPILOT_HOSTS and without HTTP(S)_PROXY / ALL_PROXY (NO_PROXY covers
//! loopback), so a proxy configured on the machine cannot swallow the stub.
//! CODEX_COPILOT_LOG_DIR points the background relay's log into the temp
//! home. Nothing registers anything with the OS any more: `install` only
//! writes files, and the relays these tests start (`start`, or the bare
//! foreground invocation) run on a reserved loopback port (never the default
//! one, where a real relay may be running) and are always shut down. The
//! tests that start relays or need a port nothing listens on hold
//! [`port_lock`], so they never race each other for ports. On Unix HOME
//! points into the temp dir too. The reads of the default address are
//! `status` on an empty home and `uninstall` without an install, which look
//! for a relay there; their assertions hold whether or not one runs.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_codex-copilot");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const TOKEN: &str = "test-token";
const CODEX_VERSION: &str = "0.160.0";
const DEFAULT_LISTEN: &str = "127.0.0.1:12899";
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:12899";
/// The README section `install` and `override` point at.
const LOGIN_RECIPES: &str = "Starting the relay at login";
const PROXY_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

// ---------------------------------------------------------------------------
// Harness

/// No proxy from the developer's or CI's environment, and loopback exempt
/// from one anyway.
fn without_proxies(cmd: &mut Command) -> &mut Command {
    for var in PROXY_VARS {
        cmd.env_remove(var);
    }
    cmd.env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
}

/// The node stub, killed when the test ends.
struct Stub {
    child: Child,
    port: u16,
}

impl Stub {
    fn start(extra: &[&str]) -> Stub {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/stub/stub-capi.mjs");
        let mut child = without_proxies(Command::new("node").arg(&script).args(extra))
            .stdin(Stdio::null())
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
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A child process killed when the test ends, pass or fail.
struct Guarded(Child);

impl Drop for Guarded {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Stops whatever relay a test started on `listen`, pass or fail, so a
/// failing test never leaks a background process.
struct ShutdownOnDrop(String);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        let _ = http().post(format!("http://{}/shutdown", self.0)).send();
    }
}

/// Serialises the tests that start relays or need a port nothing listens on.
static PORTS: Mutex<()> = Mutex::new(());

fn port_lock() -> MutexGuard<'static, ()> {
    // A test that failed while holding it must not fail every later one.
    PORTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A loopback port held open until [`Reserved::release`], so no other
/// process (a stub, another test's relay) can be handed it in between.
struct Reserved {
    listener: TcpListener,
}

fn reserve() -> Reserved {
    Reserved {
        listener: TcpListener::bind("127.0.0.1:0").unwrap(),
    }
}

impl Reserved {
    fn port(&self) -> u16 {
        self.listener.local_addr().unwrap().port()
    }
    /// Frees the port for the command about to use it; `127.0.0.1:<port>`.
    fn release(self) -> String {
        format!("127.0.0.1:{}", self.port())
    }
}

/// A temp directory standing in for the user's home directory, plus a fake
/// codex binary.
struct Home {
    dir: TempDir,
    codex: PathBuf,
}

impl Home {
    fn new() -> Home {
        let dir = tempfile::tempdir().unwrap();
        let codex = fake_codex(&dir.path().join("bin"));
        Home { dir, codex }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
    /// `~/.codex-copilot`.
    fn copilot(&self) -> PathBuf {
        self.root().join(".codex-copilot")
    }
    /// `~/.codex`.
    fn codex_home(&self) -> PathBuf {
        self.root().join(".codex")
    }
    /// Where the background relays these tests start write their log.
    fn logs(&self) -> PathBuf {
        self.root().join("logs")
    }
    fn relay_log(&self) -> PathBuf {
        self.logs().join("relay.log")
    }

    /// The binary on this home, without `--codex-bin`.
    fn bare_command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.arg("--home-dir")
            .arg(self.root())
            .env_remove("COPILOT_GITHUB_TOKEN")
            .env_remove("CODEX_HOME")
            .env_remove("CODEX_BIN")
            .env_remove("CODEX_COPILOT_HOME_DIR")
            .env_remove("CODEX_COPILOT_HOSTS")
            .env("CODEX_COPILOT_LOG_DIR", self.logs())
            .stdin(Stdio::null());
        without_proxies(&mut cmd);
        if cfg!(unix) {
            // Log directories hang off these, so nothing reaches the real
            // home.
            cmd.env("HOME", self.root())
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_STATE_HOME")
                .env_remove("XDG_DATA_HOME");
        }
        cmd.args(args);
        cmd
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = self.bare_command(&["--codex-bin"]);
        cmd.arg(&self.codex).args(args);
        cmd
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut cmd = self.command(args);
        cmd.envs(env.iter().copied());
        Run::from(cmd.output().expect("the binary must run"))
    }

    fn run(&self, args: &[&str]) -> Run {
        self.run_env(args, &[])
    }

    /// `install` against the stub with the test token.
    fn install(&self, stub: &Stub, extra: &[&str]) -> Run {
        let host = stub.host();
        let mut args = vec!["install", "--token", TOKEN, "--host", &host];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn config_text(&self) -> String {
        fs::read_to_string(self.copilot().join("config.toml")).unwrap()
    }
    fn config(&self) -> toml::Value {
        toml::from_str(&self.config_text()).unwrap()
    }
    fn state_path(&self) -> PathBuf {
        self.copilot().join("codex-copilot.json")
    }
    fn state(&self) -> Value {
        read_json(&self.state_path())
    }
    /// Points the installed state at another gateway.
    fn set_upstream(&self, upstream: &str) {
        let mut state = self.state();
        state["upstream"] = upstream.into();
        fs::write(self.state_path(), state.to_string()).unwrap();
    }
}

/// A `codex` that answers `--version` and nothing else.
fn fake_codex(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    if cfg!(windows) {
        let path = dir.join("codex.cmd");
        fs::write(&path, format!("@echo codex-cli {CODEX_VERSION}\r\n")).unwrap();
        path
    } else {
        let path = dir.join("codex");
        fs::write(
            &path,
            format!("#!/bin/sh\necho \"codex-cli {CODEX_VERSION}\"\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }
}

struct Run {
    out: Output,
    stdout: String,
    stderr: String,
}

impl From<Output> for Run {
    fn from(out: Output) -> Run {
        Run {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            out,
        }
    }
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
            "expected failure\nstdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr
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
    fn lacks(&self, needle: &str) -> &Self {
        assert!(
            !self.stdout.contains(needle) && !self.stderr.contains(needle),
            "did not expect {needle:?} in output\nstdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr
        );
        self
    }
    /// The `status` check lines, `(label, verdict)`, in order.
    fn checks(&self) -> Vec<(&str, &str)> {
        self.stdout
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let (label, verdict) = (words.next()?, words.next()?);
                matches!(verdict, "ok" | "WARN" | "FAIL").then_some((label, verdict))
            })
            .collect()
    }
    /// A `status` line: `<label> <verdict> ...`.
    fn check(&self, label: &str, verdict: &str) -> &Self {
        assert!(
            self.checks().contains(&(label, verdict)),
            "expected a `{label} {verdict}` line\nstdout:\n{}",
            self.stdout
        );
        self
    }
    /// The relay is the first thing `status` checks.
    fn relay_first(&self) -> &Self {
        assert_eq!(
            self.checks().first().map(|check| check.0),
            Some("relay"),
            "the first check is not the relay\nstdout:\n{}",
            self.stdout
        );
        self
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(0)
        .build()
        .unwrap()
}

/// `GET /healthz`, or `None` when nothing listens.
fn healthz(listen: &str) -> Option<Value> {
    match http().get(format!("http://{listen}/healthz")).send() {
        Ok(response) => Some(response.json().expect("healthz returns JSON")),
        Err(err) if err.is_connect() => None,
        Err(err) => panic!("GET http://{listen}/healthz: {err}"),
    }
}

/// Whether anything accepts a TCP connection on `listen`.
fn accepts(listen: &str) -> bool {
    TcpStream::connect_timeout(&listen.parse().unwrap(), Duration::from_millis(500)).is_ok()
}

/// Polls `/healthz` until the relay answers.
fn wait_for(listen: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(health) = healthz(listen) {
            return health;
        }
        assert!(Instant::now() < deadline, "no relay came up on {listen}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Runs the bare (foreground) relay with `args` until it announces its
/// address, checks that the relay answering there is this process, stops it
/// with `POST /shutdown` and waits for a clean exit. Returns its `/healthz`
/// and everything it logged.
fn foreground_and_shut_down(home: &Home, args: &[&str]) -> (Value, String) {
    let mut child = home
        .command(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    // Drained alongside, so a chatty log can never block the relay.
    let log = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let mut guard = Guarded(child);

    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let addr = line
        .trim()
        .strip_prefix("listening on http://")
        .unwrap_or_else(|| panic!("unexpected first line: {line:?}"))
        .to_string();
    let health = healthz(&addr).expect("the relay answers");
    assert_eq!(health["name"], "codex-copilot");
    assert_eq!(health["pid"], guard.0.id());

    let response = http()
        .post(format!("http://{addr}/shutdown"))
        .send()
        .unwrap();
    assert!(response.status().is_success());
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = guard.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the relay did not exit");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "the relay exited with {status}");
    (health, log.join().unwrap())
}

// ---------------------------------------------------------------------------
// install

#[test]
fn a_fresh_install_writes_the_config_and_the_state() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    let run = home.install(&stub, &[]);
    run.ok()
        .has(&format!("Upstream     {}  (4 models)", stub.host()))
        .has(&format!("Codex        {CODEX_VERSION}"))
        .has("(created)")
        .has("Next steps")
        // Nothing runs yet, and the steps say how to start it.
        .has("Codex cannot connect while it is not running")
        .has("         codex-copilot start\n")
        .has("`codex-copilot stop`,\n     then `codex-copilot start`")
        .has(LOGIN_RECIPES)
        .has("CODEX_HOME=")
        .has("$env:CODEX_HOME")
        .has("codex-copilot override")
        // The variable is not set in the child, so the one-liners are shown,
        // without the token that was passed.
        .has("SetEnvironmentVariable(\"COPILOT_GITHUB_TOKEN\", \"<your token>\", \"User\")")
        .lacks(TOKEN)
        .lacks("Autostart")
        .lacks(&format!("{:<12} ", "Relay"))
        .lacks("WARNING");

    let config = home.config();
    assert_eq!(config["model"].as_str(), Some("gpt-6-astra"));
    assert_eq!(config["model_reasoning_effort"].as_str(), Some("ultra"));
    assert_eq!(config["model_context_window"].as_integer(), Some(1_000_000));
    assert_eq!(config["model_provider"].as_str(), Some("copilot"));
    assert_eq!(config["approval_policy"].as_str(), Some("never"));
    assert_eq!(
        config["default_permissions"].as_str(),
        Some(":danger-full-access")
    );
    assert!(config.get("approvals_reviewer").is_none());
    let provider = &config["model_providers"]["copilot"];
    assert_eq!(provider["base_url"].as_str(), Some(DEFAULT_BASE_URL));
    assert_eq!(provider["supports_websockets"].as_bool(), Some(true));
    assert_eq!(provider["wire_api"].as_str(), Some("responses"));
    assert_eq!(provider["env_key"].as_str(), Some("COPILOT_GITHUB_TOKEN"));
    let exclude = config["shell_environment_policy"]["exclude"]
        .as_array()
        .unwrap();
    assert!(exclude
        .iter()
        .any(|v| v.as_str() == Some("COPILOT_GITHUB_TOKEN")));
    // The bearer is never written anywhere.
    assert!(!home.config_text().contains(TOKEN));

    let state = home.state();
    assert_eq!(state["version"], VERSION);
    assert_eq!(state["listen"], DEFAULT_LISTEN);
    assert_eq!(state["upstream"], stub.host());
    assert_eq!(state["review_model"], "gpt-6-luna");
    assert_eq!(state["yolo"], true);
    assert_eq!(state["codex_version"], CODEX_VERSION);
    assert!(state["installed_at"].as_str().unwrap().ends_with('Z'));
    assert!(!fs::read_to_string(home.state_path())
        .unwrap()
        .contains(TOKEN));
    // ~/.codex is never touched, and no relay ever started (it would have
    // created its log).
    assert!(!home.codex_home().exists());
    assert!(!home.logs().exists());
}

/// `install` only writes files: no relay process, nothing on the port.
#[test]
fn install_starts_nothing() {
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let home = Home::new();
    let listen = port.release();
    home.install(&stub, &["--listen", &listen]).ok();
    assert_eq!(home.state()["listen"], listen);
    // Give a stray process time to bind, then look.
    std::thread::sleep(Duration::from_millis(500));
    assert!(!accepts(&listen), "something listens on {listen}");
    assert!(healthz(&listen).is_none());
    assert!(!home.logs().exists(), "a relay started and logged");
}

/// `--token-stdin` keeps the token out of argv; it must not then turn up in
/// the output (a CI log, a pipe).
#[test]
fn a_token_from_stdin_is_never_echoed() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    let secret = "gho_secret_from_stdin_0123";
    let host = stub.host();
    let mut child = home
        .command(&["install", "--token-stdin", "--host", &host])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Dropping stdin closes it: the token is the whole input.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{secret}\n").as_bytes())
        .unwrap();
    Run::from(child.wait_with_output().unwrap())
        .ok()
        .has("from --token-stdin")
        .has(&format!("Upstream     {host}  (4 models)"))
        .has("<your token>")
        .lacks(secret);
}

#[test]
fn a_reinstall_keeps_user_edits_and_explicit_flags_win() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    home.install(&stub, &[]).ok();

    let path = home.copilot().join("config.toml");
    let edited = home
        .config_text()
        .replace("model = \"gpt-6-astra\"", "model = \"gpt-6-luna\"")
        + "\n[projects.'d:\\dev']\ntrust_level = \"trusted\"\n";
    fs::write(&path, edited).unwrap();

    home.install(&stub, &[]).ok();
    let config = home.config();
    assert_eq!(config["model"].as_str(), Some("gpt-6-luna"));
    assert_eq!(
        config["projects"]["d:\\dev"]["trust_level"].as_str(),
        Some("trusted")
    );
    assert_eq!(config["model_provider"].as_str(), Some("copilot"));
    assert_eq!(config["approval_policy"].as_str(), Some("never"));
    assert_eq!(
        config["model_providers"]["copilot"]["base_url"].as_str(),
        Some(DEFAULT_BASE_URL)
    );

    // A third run with the same flags changes nothing at all.
    let before = home.config_text();
    home.install(&stub, &[])
        .ok()
        .has("(unchanged)")
        .lacks("--listen 127.0.0.1");
    assert_eq!(home.config_text(), before);

    // An explicit --model replaces the kept one; the rest stays. The moved
    // address is named, with how to stop a relay still running on the old one.
    home.install(&stub, &["--model", "gpt-5.5", "--listen", "127.0.0.1:5000"])
        .ok()
        .has("(updated)")
        .has(&format!("moved from {DEFAULT_LISTEN}"))
        .has(&format!("codex-copilot stop --listen {DEFAULT_LISTEN}"));
    let config = home.config();
    assert_eq!(config["model"].as_str(), Some("gpt-5.5"));
    assert_eq!(
        config["projects"]["d:\\dev"]["trust_level"].as_str(),
        Some("trusted")
    );
    assert_eq!(
        config["model_providers"]["copilot"]["base_url"].as_str(),
        Some("http://127.0.0.1:5000")
    );
    assert_eq!(home.state()["listen"], "127.0.0.1:5000");
}

/// Whether some line of `text` sets `key` (comments may mention it).
fn sets(text: &str, key: &str) -> bool {
    text.lines()
        .any(|line| line.trim_start().starts_with(&format!("{key} =")))
}

#[test]
fn yolo_and_no_yolo_switch_cleanly_both_ways() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    home.install(&stub, &[]).ok();
    let fresh = home.config_text();
    assert!(sets(&fresh, "approval_policy") && sets(&fresh, "default_permissions"));
    assert!(!sets(&fresh, "approvals_reviewer"));

    home.install(&stub, &["--no-yolo"])
        .ok()
        .has("approvals_reviewer = \"auto_review\"")
        .has("the relay sends gpt-6-luna in place of codex-auto-review");
    let config = home.config();
    assert_eq!(config["approvals_reviewer"].as_str(), Some("auto_review"));
    assert!(config.get("approval_policy").is_none());
    assert!(config.get("default_permissions").is_none());
    let text = home.config_text();
    assert!(!sets(&text, "approval_policy") && !sets(&text, "default_permissions"));
    assert_eq!(home.state()["yolo"], false);

    // Back to yolo: no leftover key or comment, the file is the fresh one.
    home.install(&stub, &[]).ok();
    let config = home.config();
    assert_eq!(config["approval_policy"].as_str(), Some("never"));
    assert_eq!(
        config["default_permissions"].as_str(),
        Some(":danger-full-access")
    );
    assert!(config.get("approvals_reviewer").is_none());
    assert_eq!(home.config_text(), fresh);
    assert_eq!(home.state()["yolo"], true);
}

#[test]
fn a_dry_run_prints_the_config_and_writes_nothing() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    home.install(&stub, &["--dry-run"])
        .ok()
        .has("Dry run, nothing written")
        .has("model_provider = \"copilot\"")
        .has(&format!("base_url = \"{DEFAULT_BASE_URL}\""))
        .has("supports_websockets = true")
        .has("approval_policy = \"never\"")
        .has("\"review_model\": \"gpt-6-luna\"")
        .lacks("autostart")
        .lacks("codex-copilot start")
        .lacks("Wrote");
    assert!(!home.copilot().exists());
    assert!(!home.codex_home().exists());
}

#[test]
fn a_host_without_a_token_skips_the_probe() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    let host = stub.host();
    home.run(&["install", "--host", &host])
        .ok()
        .has("probe skipped")
        .has("codex-copilot login");
    assert_eq!(home.state()["upstream"], host);
}

#[test]
fn a_host_that_refuses_the_token_is_only_a_warning() {
    let stub = Stub::start(&["--status", "401"]);
    let home = Home::new();
    home.install(&stub, &[])
        .ok()
        .has("WARNING  GET")
        .has("401")
        .has("Continuing because --host was given");
    assert_eq!(home.state()["upstream"], stub.host());
}

/// Without --host the gateways are probed in order (here the hidden --hosts
/// list instead of the real ones) and the first that answers 200 wins.
#[test]
fn without_a_host_the_first_gateway_that_accepts_the_token_wins() {
    let refuses = Stub::start(&["--status", "401"]);
    let accepts = Stub::start(&[]);
    let never_asked = Stub::start(&[]);
    let home = Home::new();
    let hosts = format!(
        "{},{}/,{}",
        refuses.host(),
        accepts.host(),
        never_asked.host()
    );
    home.run(&["install", "--token", TOKEN, "--hosts", &hosts])
        .ok()
        .has(&format!("Upstream     {}  (4 models)", accepts.host()))
        .has(&format!("skipped    {}  401", refuses.host()))
        .lacks(&format!("{}  ", never_asked.host()))
        .lacks("WARNING");
    assert_eq!(home.state()["upstream"], accepts.host());
}

#[test]
fn install_fails_when_no_gateway_accepts_the_token() {
    let refuses = Stub::start(&["--status", "401"]);
    let forbids = Stub::start(&["--status", "403"]);
    let home = Home::new();
    let hosts = format!("{},{}", refuses.host(), forbids.host());
    // The environment variable works like the flag.
    home.run_env(
        &["install", "--token", TOKEN],
        &[("CODEX_COPILOT_HOSTS", &hosts)],
    )
    .failed()
    .has("no Copilot CAPI gateway accepted this token")
    .has(&format!("{}  401", refuses.host()))
    .has(&format!("{}  403", forbids.host()))
    .has("a 403 means the seat has no CAPI access");
    assert!(!home.copilot().exists());
}

/// What the gateway says about the configured model and the review model
/// turns into warnings with a remedy; install still goes ahead.
#[test]
fn unusable_models_are_warned_about() {
    let no_ws = Stub::start(&["--no-ws"]);
    let home = Home::new();
    home.install(&no_ws, &[])
        .ok()
        .has("WARNING  gpt-6-astra does not advertise ws:/responses");

    let disabled = Stub::start(&["--policy", "disabled"]);
    home.install(&disabled, &[])
        .ok()
        .has("gpt-6-astra has policy state `disabled`");

    let other = Stub::start(&["--model", "gpt-7"]);
    home.install(&other, &[])
        .ok()
        .has(&format!("gpt-6-astra is not in {}/models", other.host()));

    home.install(&other, &["--model", "gpt-7", "--review-model", "nope"])
        .ok()
        .has(&format!(
            "review model nope is not in {}/models",
            other.host()
        ))
        .lacks("gpt-7 is not in");
}

/// A relay bound to every interface is reached on loopback: Windows refuses
/// a connect to 0.0.0.0, so that is what Codex's base_url says, and what
/// `start` and `stop` talk to.
#[test]
fn a_wildcard_listen_address_is_reached_on_loopback() {
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let home = Home::new();
    let loopback = port.release();
    let wildcard = loopback.replace("127.0.0.1", "0.0.0.0");
    let _shutdown = ShutdownOnDrop(loopback.clone());
    home.install(&stub, &["--listen", &wildcard])
        .ok()
        .has("is not a loopback address")
        .has("stop it with POST /shutdown");
    assert_eq!(
        home.config()["model_providers"]["copilot"]["base_url"].as_str(),
        Some(format!("http://{loopback}").as_str())
    );
    assert_eq!(home.state()["listen"], wildcard);
    home.run(&["status"])
        .failed()
        .relay_first()
        .check("relay", "FAIL")
        .check("config", "ok")
        .has(&format!("not running on {wildcard}"));

    home.run(&["start"])
        .ok()
        .has(&format!("running on http://{loopback}"))
        .has("not a loopback address")
        .lacks(&format!("http://{wildcard}"));
    assert!(healthz(&loopback).is_some());
    home.run(&["stop"]).ok().has(&format!("stopped {VERSION}"));
    assert!(!accepts(&loopback));
}

/// A state file that does not parse is named in a warning and replaced.
#[test]
fn install_warns_about_a_corrupt_state_file_and_rewrites_it() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    fs::create_dir_all(home.copilot()).unwrap();
    fs::write(home.state_path(), "{ broken").unwrap();
    home.install(&stub, &[])
        .ok()
        .has("WARNING")
        .has("codex-copilot.json is corrupt")
        .has("Install goes on and rewrites it")
        .has("codex-copilot stop --listen <its address>");
    assert_eq!(home.state()["version"], VERSION);
    assert_eq!(home.state()["upstream"], stub.host());
    // The rewritten file is fine again.
    home.install(&stub, &[]).ok().lacks("WARNING");
}

#[test]
fn install_refuses_while_overridden() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    fs::create_dir_all(home.codex_home()).unwrap();
    fs::write(home.codex_home().join("codex-copilot.json"), "{}").unwrap();
    home.install(&stub, &[])
        .failed()
        .has("an override is active")
        .has("codex-copilot unoverride");
    assert!(!home.copilot().exists());
}

#[test]
fn bad_arguments_are_refused_before_anything_is_written() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    home.install(&stub, &["--listen", "127.0.0.1:0"])
        .failed()
        .has("fixed port");
    home.install(&stub, &["--listen", "nonsense"]).failed();
    // A name: the relay would bind one of its addresses while Codex's
    // base_url kept the name.
    home.install(&stub, &["--listen", "localhost:12899"])
        .failed()
        .has("use an IP literal such as 127.0.0.1:12899");
    home.run(&["install", "--host", "api.githubcopilot.com"])
        .failed()
        .has("http(s) origin");
    home.run(&["install", "--hosts", "http://a,b.example"])
        .failed()
        .has("--hosts must be an http(s) origin");
    assert!(!home.copilot().exists());
}

// ---------------------------------------------------------------------------
// override / unoverride

fn marker(dir: &Path) -> String {
    fs::read_to_string(dir.join("marker")).unwrap()
}

#[test]
fn override_and_unoverride_swap_the_two_homes() {
    // `status` below probes the installed address: not the default one.
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let home = Home::new();
    fs::create_dir_all(home.codex_home()).unwrap();
    fs::write(home.codex_home().join("marker"), "regular").unwrap();
    home.install(&stub, &["--listen", &port.release()]).ok();
    fs::write(home.copilot().join("marker"), "dedicated").unwrap();

    home.run(&["override", "--force"])
        .ok()
        .has("renamed")
        .has("the codex-copilot home")
        .has("your regular Codex home")
        .has("Every Codex entry point now goes through the relay")
        .has(LOGIN_RECIPES);
    assert_eq!(marker(&home.codex_home()), "dedicated");
    assert_eq!(marker(&home.copilot()), "regular");
    assert!(home.codex_home().join("codex-copilot.json").exists());
    assert!(!home.copilot().join("codex-copilot.json").exists());

    // While overridden, install and uninstall refuse and a second override
    // says why.
    home.install(&stub, &[]).failed().has("unoverride");
    home.run(&["uninstall"]).failed().has("unoverride");
    home.run(&["override", "--force"])
        .failed()
        .has("already overridden");
    home.run(&["status"])
        .has("override active")
        .check("config", "ok");

    home.run(&["unoverride", "--force"])
        .ok()
        .has("renamed")
        .lacks(LOGIN_RECIPES);
    assert_eq!(marker(&home.codex_home()), "regular");
    assert_eq!(marker(&home.copilot()), "dedicated");
    assert!(home.copilot().join("codex-copilot.json").exists());
    home.run(&["unoverride", "--force"])
        .failed()
        .has("not overridden");
}

#[test]
fn override_without_a_regular_home_is_a_single_rename() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    home.install(&stub, &[]).ok();
    home.run(&["override", "--force"])
        .ok()
        .has("absent: there was no regular Codex home");
    assert!(home.codex_home().join("codex-copilot.json").exists());
    assert!(!home.copilot().exists());

    home.run(&["unoverride", "--force"]).ok();
    assert!(home.copilot().join("codex-copilot.json").exists());
    assert!(!home.codex_home().exists());
}

#[test]
fn override_needs_an_install() {
    let home = Home::new();
    home.run(&["override", "--force"])
        .failed()
        .has("not installed");
    home.run(&["override"]).failed().has("not installed");
    home.run(&["unoverride"]).failed().has("not installed");
}

/// A process whose image is named `codex` blocks the swap and a purge. It is
/// a copy of node idling, so nothing Codex-like runs.
#[test]
fn override_and_purge_refuse_while_codex_runs() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    home.install(&stub, &[]).ok();

    let node = Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .expect("node must be on PATH");
    let node = PathBuf::from(String::from_utf8(node.stdout).unwrap().trim());
    // The refusal names the image the process lister reports.
    let image = format!("codex{}", std::env::consts::EXE_SUFFIX);
    let fake = home.root().join("running").join(&image);
    fs::create_dir_all(fake.parent().unwrap()).unwrap();
    fs::copy(&node, &fake).unwrap();
    let mut child = Command::new(&fake)
        .args(["-e", "console.log('up'); setTimeout(() => {}, 60000)"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let stdout = child.stdout.take().unwrap();
    let _guard = Guarded(child);
    // It prints once it is up, so the process list has it by now.
    let mut ready = String::new();
    BufReader::new(stdout).read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), "up");

    home.run(&["override"])
        .failed()
        .has(&format!("{image} (pid {pid})"))
        .has("--force skips");
    // Nothing moved.
    assert!(home.state_path().exists());
    assert!(!home.codex_home().exists());

    home.run(&["uninstall", "--purge"])
        .failed()
        .has(&format!("{image} (pid {pid})"))
        .has("half-deleted")
        .lacks("Removed");
    // Refused before anything was removed.
    assert!(home.state_path().exists());
    assert!(home.copilot().join("config.toml").exists());
}

// ---------------------------------------------------------------------------
// uninstall

#[test]
fn uninstall_removes_the_state_and_purge_the_home() {
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let home = Home::new();
    // Not the default port: whether a relay runs there decides the hint.
    let listen = port.release();
    home.install(&stub, &["--listen", &listen]).ok();

    home.run(&["uninstall"])
        .ok()
        .has("Removed")
        .has("Kept")
        .has("SetEnvironmentVariable(\"COPILOT_GITHUB_TOKEN\", $null, \"User\")")
        .lacks("still runs")
        .lacks("Autostart");
    assert!(!home.state_path().exists());
    assert!(home.copilot().join("config.toml").exists());
    home.run(&["status"]).failed().has("not installed");

    home.install(&stub, &["--listen", &listen]).ok();
    // --force: a codex running elsewhere on this machine (or the fake one of
    // the test above) must not fail this test.
    home.run(&["uninstall", "--purge", "--force"])
        .ok()
        .has("Deleted")
        .has("$null");
    assert!(!home.copilot().exists());
}

/// `uninstall` only removes files: a running relay keeps running, and the
/// output says so and how to stop it.
#[test]
fn uninstall_leaves_a_running_relay_alone() {
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let home = Home::new();
    let listen = port.release();
    let _shutdown = ShutdownOnDrop(listen.clone());
    home.install(&stub, &["--listen", &listen]).ok();
    home.run(&["start"]).ok();
    let running = healthz(&listen).expect("the relay answers after start");

    home.run(&["uninstall", "--purge", "--force"])
        .ok()
        .has("Deleted")
        .has(&format!(
            "still runs on {listen} ({VERSION}, pid {})",
            running["pid"]
        ))
        .has(&format!("`codex-copilot stop --listen {listen}` stops it"));
    let after = healthz(&listen).expect("uninstall stopped the relay");
    assert_eq!(after["pid"], running["pid"]);

    // Nothing installed any more: `stop` needs the address.
    home.run(&["stop", "--listen", &listen])
        .ok()
        .has(&format!("stopped {VERSION}"));
    assert!(!accepts(&listen));
}

// ---------------------------------------------------------------------------
// the relay: start / stop / status / foreground

#[test]
fn start_runs_a_background_relay_that_stop_stops() {
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let home = Home::new();
    let listen = port.release();
    let _shutdown = ShutdownOnDrop(listen.clone());
    home.install(&stub, &["--listen", &listen]).ok();

    // Installed, not started.
    home.run(&["status"])
        .failed()
        .relay_first()
        .check("relay", "FAIL")
        .has(&format!("not running on {listen}"))
        .has("`codex-copilot start` starts the relay");

    home.run(&["start"])
        .ok()
        .has(&format!("{VERSION} running on http://{listen}"))
        .has(&format!("{:<12} {}", "Log", home.relay_log().display()));
    let first = healthz(&listen).expect("the relay answers after start");
    assert_eq!(first["name"], "codex-copilot");
    assert_eq!(first["version"], VERSION);
    assert_eq!(first["upstream"], stub.host());
    assert_eq!(first["review_model"], "gpt-6-luna");
    // It logs into the temp home (CODEX_COPILOT_LOG_DIR), never the real one.
    let log = fs::read_to_string(home.relay_log()).expect("the relay's log");
    assert!(log.contains(&listen), "{log}");

    home.run_env(&["status"], &[("COPILOT_GITHUB_TOKEN", TOKEN)])
        .ok()
        .relay_first()
        .check("relay", "ok")
        .check("config", "ok")
        .check("token", "ok")
        .check("codex", "ok")
        .has(&format!(
            "running on http://{listen} (pid {})",
            first["pid"]
        ))
        .has("proxy none")
        .has(&home.relay_log().display().to_string())
        .lacks("autostart");
    // Without the variable the token check only warns: this shell is not
    // where Codex runs, and status still passes.
    home.run(&["status"])
        .ok()
        .check("token", "WARN")
        .has("not visible in this shell; Codex needs it in its own environment");
    // --probe does need it. Its proxy line comes from /healthz and does not.
    home.run(&["status", "--probe"])
        .failed()
        .check("token", "WARN")
        .check("capi", "FAIL")
        .check("proxy", "ok")
        .has(&format!("the relay reaches {} directly", stub.host()))
        .has("--probe needs COPILOT_GITHUB_TOKEN")
        .has("1 check failed");

    // One relay per address: `start` and the foreground relay refuse, and
    // name the one that runs.
    home.run(&["start"])
        .failed()
        .has(&format!("already runs on http://{listen}"))
        .has(&format!("pid {}", first["pid"]))
        .has("codex-copilot stop");
    home.run(&[])
        .failed()
        .has(&format!("already runs on {listen}"))
        .has("codex-copilot stop")
        .lacks("listening on");
    assert_eq!(healthz(&listen).unwrap()["pid"], first["pid"]);

    home.run(&["stop"])
        .ok()
        .has(&format!(
            "stopped {VERSION} (pid {}) on {listen}",
            first["pid"]
        ))
        .has("codex-copilot start");
    // Gone from the port at once, not just from /healthz.
    assert!(!accepts(&listen), "the port still accepts connections");
    assert!(home.state_path().exists(), "stop removed the state file");
    assert!(home.copilot().join("config.toml").exists());

    // Stopping again is fine, and status is back to "not running".
    home.run(&["stop"])
        .ok()
        .has(&format!("not running on {listen}"));
    home.run(&["status"]).failed().check("relay", "FAIL");
}

/// `start --no-wait` returns right after spawning (what a login entry runs);
/// the relay comes up on its own. Options on the command line win over the
/// installed ones, and a relay that differs from the install is flagged.
#[test]
fn start_no_wait_returns_at_once_and_options_override_the_install() {
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let home = Home::new();
    let listen = port.release();
    let _shutdown = ShutdownOnDrop(listen.clone());
    home.install(&stub, &["--listen", &listen]).ok();

    home.run(&["start", "--no-wait", "--review-model", "other"])
        .ok()
        .has(&format!("starting on http://{listen} (pid "))
        .has("Log ");
    let health = wait_for(&listen);
    assert_eq!(health["review_model"], "other");
    assert_eq!(health["upstream"], stub.host());
    home.run(&["status"])
        .ok()
        .check("relay", "WARN")
        .has("substitutes other")
        .has("`codex-copilot stop`, then `codex-copilot start`");

    home.run(&["stop", "--listen", &listen]).ok();
    assert!(!accepts(&listen));
}

/// `stop` with nothing to stop succeeds, installed or not.
#[test]
fn stop_with_nothing_running_exits_0() {
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let home = Home::new();
    let listen = port.release();
    home.run(&["stop", "--listen", &listen])
        .ok()
        .has(&format!("not running on {listen}"));
    home.install(&stub, &["--listen", &listen]).ok();
    home.run(&["stop"])
        .ok()
        .has(&format!("not running on {listen}"));
}

/// Something on the port that is not a relay: `start` refuses at once with
/// what it found, and `stop` leaves it alone.
#[test]
fn start_and_stop_leave_a_foreign_listener_alone() {
    // Accepts connections and never answers.
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = held.local_addr().unwrap().to_string();
    let home = Home::new();
    home.run(&["start", "--listen", &listen])
        .failed()
        .has("is taken by something that is not a codex-copilot relay");
    home.run(&["stop", "--listen", &listen])
        .failed()
        .has(&format!("left whatever listens on {listen} alone"));
    assert!(!home.logs().exists(), "a relay was started");
    drop(held);
}

#[test]
fn status_fails_when_the_relay_is_down() {
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let home = Home::new();
    home.install(&stub, &["--listen", &port.release()]).ok();
    home.run_env(&["status", "--probe"], &[("COPILOT_GITHUB_TOKEN", TOKEN)])
        .failed()
        .relay_first()
        .check("relay", "FAIL")
        .check("config", "ok")
        .check("capi", "ok")
        .check("model", "ok")
        .check("review", "ok")
        .has("1 check failed");
}

/// `status --probe` blames the token only when the gateway refused it.
#[test]
fn status_probe_advice_follows_the_failure() {
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let refuses = Stub::start(&["--status", "401"]);
    let broken = Stub::start(&["--status", "500"]);
    let home = Home::new();
    home.install(&stub, &["--listen", &port.release()]).ok();
    let token = [("COPILOT_GITHUB_TOKEN", TOKEN)];

    home.set_upstream(&refuses.host());
    home.run_env(&["status", "--probe"], &token)
        .failed()
        .check("capi", "FAIL")
        .has("refused the token (401)")
        .has("codex-copilot login");

    home.set_upstream("http://127.0.0.1:1");
    home.run_env(&["status", "--probe"], &token)
        .failed()
        .check("capi", "FAIL")
        .has("http://127.0.0.1:1 could not be reached")
        .lacks("codex-copilot login");

    home.set_upstream(&broken.host());
    home.run_env(&["status", "--probe"], &token)
        .failed()
        .check("capi", "FAIL")
        .has("did not answer with a model list")
        .lacks("codex-copilot login");
}

#[test]
fn the_bare_invocation_runs_the_relay_in_the_foreground() {
    let home = Home::new();
    let (health, log) = foreground_and_shut_down(
        &home,
        &[
            "--listen",
            "127.0.0.1:0",
            "--upstream",
            "http://127.0.0.1:1",
        ],
    );
    assert_eq!(health["upstream"], "http://127.0.0.1:1");
    assert_eq!(health["review_model"], "gpt-6-luna");
    // It logs to stderr, not to a file.
    assert!(log.contains("relay listening"), "{log}");
    assert!(!home.logs().exists());
}

/// Unset options come from the install, like `start`'s.
#[test]
fn the_foreground_relay_runs_the_installed_settings() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    home.install(&stub, &["--review-model", "installed"]).ok();
    let (health, _) = foreground_and_shut_down(&home, &["--listen", "127.0.0.1:0"]);
    assert_eq!(health["upstream"], stub.host());
    assert_eq!(health["review_model"], "installed");
}

/// A state file that does not parse is named in a warning, not silently
/// replaced by the defaults; two state files are an error.
#[test]
fn the_foreground_relay_reports_a_state_file_it_cannot_use() {
    let home = Home::new();
    fs::create_dir_all(home.copilot()).unwrap();
    fs::write(home.state_path(), "{ broken").unwrap();
    let (health, log) = foreground_and_shut_down(
        &home,
        &[
            "--listen",
            "127.0.0.1:0",
            "--upstream",
            "http://127.0.0.1:1",
        ],
    );
    assert_eq!(health["upstream"], "http://127.0.0.1:1");
    assert!(log.contains("WARN"), "{log}");
    assert!(log.contains("codex-copilot.json is corrupt"), "{log}");
    assert!(log.contains("get the defaults"), "{log}");

    fs::create_dir_all(home.codex_home()).unwrap();
    fs::write(home.codex_home().join("codex-copilot.json"), "{}").unwrap();
    home.run(&["--listen", "127.0.0.1:0"])
        .failed()
        .has("both")
        .lacks("listening on");
}

// ---------------------------------------------------------------------------
// the rest

#[test]
fn old_flags_and_commands_are_rejected() {
    let home = Home::new();
    home.run(&["install", "--profile", "x"])
        .failed()
        .has("unexpected argument '--profile'");
    home.run(&["--codex-home", "y", "status"])
        .failed()
        .has("unexpected argument '--codex-home'");
    home.run(&["install", "--catalog", "models.json"]).failed();
    home.run(&["install", "--auto-review"]).failed();
    home.run(&["install", "--no-start"])
        .failed()
        .has("unexpected argument '--no-start'");
    home.run(&["install", "--no-autostart"]).failed();
    home.run(&["uninstall", "--no-autostart"]).failed();
    home.run(&["serve"]).failed();
    // The relay options of the bare invocation do not go with a subcommand.
    home.run(&["--listen", "127.0.0.1:5", "status"])
        .failed()
        .has("only applies to the relay run without one");
    assert!(!home.copilot().exists());
}

/// `status` on an empty home also looks for a relay on the default address
/// (it may find the developer's own), so only the order and the "not
/// installed" verdict are asserted.
#[test]
fn status_on_an_empty_home_says_not_installed() {
    let home = Home::new();
    home.run(&["status"])
        .failed()
        .relay_first()
        .check("state", "FAIL")
        .has("not installed")
        .has(DEFAULT_LISTEN);
}

/// `export CODEX_BIN=` (set, but empty) means "not set", not "a value is
/// required".
#[test]
fn empty_environment_variables_count_as_unset() {
    let _ports = port_lock();
    let port = reserve();
    let stub = Stub::start(&[]);
    let home = Home::new();
    home.install(&stub, &["--listen", &port.release()]).ok();

    let codex = home.codex.to_str().unwrap();
    let run = |env: &[(&str, &str)]| {
        let mut cmd = home.bare_command(&["status"]);
        cmd.envs(env.iter().copied());
        Run::from(cmd.output().unwrap())
    };
    // CODEX_BIN names the codex binary like --codex-bin.
    run(&[("CODEX_BIN", codex)])
        .check("codex", "ok")
        .has(CODEX_VERSION);
    // Empty: codex is looked up on PATH instead (found or not), and the
    // command runs.
    run(&[
        ("CODEX_BIN", ""),
        ("CODEX_COPILOT_HOME_DIR", ""),
        ("CODEX_COPILOT_HOSTS", ""),
    ])
    .check("config", "ok")
    .lacks("a value is required");
}

#[test]
fn login_runs_the_device_flow_and_prints_the_token() {
    let stub = Stub::start(&[]);
    let home = Home::new();
    // An existing variable is ignored on purpose: login issues a new token.
    home.run_env(
        &["login", "--github-oauth", &stub.host()],
        &[("COPILOT_GITHUB_TOKEN", "old-token")],
    )
    .ok()
    .has("ABCD-1234")
    .has("COPILOT_GITHUB_TOKEN=gho_dummy_test_token")
    .has("SetEnvironmentVariable")
    .lacks("old-token");
}

#[test]
fn version_is_printed() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("codex-copilot {VERSION}")
    );
}

#[test]
fn help_names_the_new_default_port() {
    let out = Command::new(BIN)
        .args(["start", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains(DEFAULT_LISTEN), "{help}");
    assert!(help.contains("--no-wait"), "{help}");
    assert!(!help.contains("41337"), "{help}");
}
