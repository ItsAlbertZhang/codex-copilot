//! Run the relay, in the foreground or as a detached background process, and
//! talk to a running one.
//!
//! There is one binary. `codex-copilot` without a subcommand runs the relay in
//! the foreground of the terminal, logging to stderr. `codex-copilot start`
//! spawns the same executable again with the hidden `--background` flag and
//! every option spelled out, detached from the terminal; that instance logs
//! to a file in [`log_dir`]. Nothing here registers anything with the OS:
//! starting the relay at login is left to the user (the README has
//! per-platform recipes that call `codex-copilot start --no-wait`).
//!
//! A running relay is found over plain HTTP on its listen address: `GET
//! /healthz` identifies it, `POST /shutdown` stops it. No process id is ever
//! waited on: the port is what Codex needs, so the port is what is checked.
//!
//! Everything is ordinary portable code, so all of it is type-checked and
//! unit-tested on any host. Only the process-creation flags and the one Win32
//! call around them are `cfg`-gated, behind small functions with a stub
//! elsewhere.

use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::StatusCode;

use crate::proxy::{self, ProxyConfig};

/// `name` a codex-copilot relay reports on `GET /healthz`. Anything else on
/// the port is somebody else's server.
pub const HEALTH_NAME: &str = "codex-copilot";

/// The background relay's log, in [`log_dir`].
pub const LOG_FILE: &str = "relay.log";
/// Checked once when a background relay starts; the relay is long-lived but
/// quiet, so a startup-time rotation keeps the file bounded without a
/// rotating writer.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// Optional log level override for the relay (`debug`, `trace`, ...).
const LOG_LEVEL_ENV: &str = "CODEX_COPILOT_LOG";
/// Overrides [`log_dir`]; a test hook.
const LOG_DIR_ENV: &str = "CODEX_COPILOT_LOG_DIR";
/// The hidden flag that makes `codex-copilot` the background relay.
pub const BACKGROUND_FLAG: &str = "--background";

const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Bounds of one probe while polling, so a single slow request can neither
/// eat a whole wait nor time out instantly.
const MIN_PROBE: Duration = Duration::from_millis(100);
const MAX_PROBE: Duration = Duration::from_secs(1);
/// How long a probe waits for the TCP handshake before deciding nothing runs.
/// A listening loopback socket completes it in the kernel at once, however
/// busy the relay is, while Windows retries a refused loopback connect for
/// about 2 s before reporting it; this cuts that wait short.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// Single-instance probe when a relay starts.
const STARTUP_PROBE: Duration = Duration::from_secs(2);

/// What a relay runs with, resolved: every value given on the command line,
/// read from the install state, or the crate default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayArgs {
    /// Address to listen on (`IP:PORT`; a host name resolves to one address).
    pub listen: String,
    /// Copilot gateway origin.
    pub upstream: String,
    /// Model substituted for `codex-auto-review`; empty disables the swap.
    pub review_model: String,
}

impl Default for RelayArgs {
    /// The crate defaults, i.e. what a relay runs with when nothing is
    /// installed and no option is given.
    fn default() -> Self {
        Self {
            listen: crate::DEFAULT_LISTEN.to_string(),
            upstream: crate::DEFAULT_UPSTREAM.to_string(),
            review_model: crate::DEFAULT_REVIEW_MODEL.to_string(),
        }
    }
}

/// What `GET /healthz` returns.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Health {
    pub name: String,
    pub version: String,
    pub upstream: String,
    pub review_model: Option<String>,
    pub pid: u32,
    /// The proxy the relay reaches its upstream through, credentials
    /// masked; `None` when it connects directly.
    #[serde(default)]
    pub proxy: Option<String>,
}

/// A background relay [`spawn_background`] started.
#[derive(Debug)]
pub struct Started {
    /// The spawned process. Never waited on to the end: the relay outlives
    /// the CLI. [`wait_started`] only polls it, to fail fast when the relay
    /// exits before it answers.
    pub child: Child,
    /// Windows: the job object the caller runs in (a terminal, an IDE, a CI
    /// runner) refused to let the relay break away, so the relay is still in
    /// that job, and a job that kills its processes when it closes takes the
    /// relay with it. Report it with [`job_warning`]. Always `false`
    /// elsewhere.
    pub in_callers_job: bool,
}

/// The arguments (program name excluded) that make this executable the
/// background relay running `args`. Every option is spelled out, so the
/// background instance never has to find the install state itself (and
/// cannot pick a different one than `start` did).
pub fn background_argv(args: &RelayArgs) -> Vec<String> {
    [
        BACKGROUND_FLAG,
        "--listen",
        &args.listen,
        "--upstream",
        &args.upstream,
        "--review-model",
        &args.review_model,
    ]
    .map(str::to_string)
    .to_vec()
}

// ---------------------------------------------------------------------------
// Running the relay

/// The relay in the foreground: logs to stderr (set up by the caller with
/// [`log_to_stderr`]), prints `listening on http://<addr>` on stdout once it
/// is bound (port 0 picks one), and runs until `POST /shutdown`, Ctrl-C or
/// SIGTERM, which all end it with exit code 0. A port that is taken is an
/// error, loudly: there is no supervisor to retry, and a person is watching.
pub fn run_foreground(args: &RelayArgs) -> Result<()> {
    let cfg = proxy_config(args)?;
    // A live relay holds the port; say whose it is instead of a bind error.
    if cfg.listen.port() != 0 {
        if let Ok(Some(found)) = health(&args.listen, STARTUP_PROBE) {
            bail!(
                "a codex-copilot relay already runs on {} ({}, pid {}). `codex-copilot stop` \
                 stops it, or pick another --listen.",
                args.listen,
                found.version,
                found.pid
            );
        }
    }
    log_start(&cfg, "starting codex-copilot relay in the foreground");
    runtime()?.block_on(async {
        // `bind`'s error already names the address.
        let handle = proxy::bind(&cfg).await?;
        // On stdout, apart from the log on stderr: the line scripts wait for.
        let mut out = std::io::stdout();
        writeln!(out, "listening on http://{}", handle.addr)?;
        out.flush()?;
        handle.wait_or_signal().await
    })
}

/// The background relay (`--background`): logs to [`LOG_FILE`] in
/// [`log_dir`], exits 0 at once when a healthy relay already answers on its
/// address (a login entry that runs `start` twice is harmless), and otherwise
/// runs until `POST /shutdown` or a termination signal. Anything that stops
/// it from serving, a taken port included, is logged and ends it with a
/// non-zero exit code. `note` is a warning to log once the log is open (an
/// install state that could not be read).
pub fn run_background(args: &RelayArgs, note: Option<String>) -> Result<()> {
    // Probe before opening the log: a second instance must not rotate the
    // file the first one is still writing to.
    let running = health(&args.listen, STARTUP_PROBE);
    init_logging(&LogTarget::Dir(log_dir()?), may_rotate_log(&running))?;
    if let Some(note) = note {
        tracing::warn!("{note}");
    }
    match running {
        Ok(Some(found)) => {
            tracing::info!(
                pid = found.pid,
                version = %found.version,
                listen = %args.listen,
                "codex-copilot relay already running; exiting"
            );
            return Ok(());
        }
        Ok(None) => {}
        // Binding will fail with a clearer error if the port is really taken.
        Err(err) => tracing::warn!("probing {} failed: {err:#}; starting anyway", args.listen),
    }
    let result = proxy_config(args).and_then(|cfg| {
        log_start(&cfg, "starting codex-copilot relay");
        // Binds (its error names the address), logs "relay listening" /
        // "relay stopped", and stops gracefully on POST /shutdown, Ctrl-C or
        // SIGTERM, returning Ok: exit 0.
        runtime()?.block_on(proxy::run(cfg))
    });
    // A clean stop is logged by the relay itself ("relay stopped").
    if let Err(err) = &result {
        tracing::error!("relay failed: {err:#}");
    }
    result
}

/// Logs an error that ends a background relay before it could set up its
/// log (bad arguments, an unusable install state): without a console the
/// log file is the only place it can show up. Best effort.
pub fn log_background_error(err: &dyn std::fmt::Display) {
    if let Ok(dir) = log_dir() {
        if init_logging(&LogTarget::Dir(dir), false).is_ok() {
            tracing::error!("{err}");
        }
    }
}

/// The startup line and, for an address other machines reach, the exposure
/// warning.
fn log_start(cfg: &ProxyConfig, message: &str) {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        listen = %cfg.listen,
        upstream = %cfg.upstream,
        review_model = cfg.review_model.as_deref().unwrap_or("(none)"),
        "{message}"
    );
    if let Some(warning) = exposure_warning(cfg.listen) {
        tracing::warn!("{warning}");
    }
}

/// Whether a starting background relay may rotate the log, given what the
/// single-instance probe found: when nothing listens on its port. A relay
/// that answers is still writing to the file, and so may whatever holds the
/// port without answering properly (a relay too busy or hung to reply in
/// time); this instance then fails to bind anyway.
///
/// On Unix that last case rotates too: renaming a file another process has
/// open is harmless there (it keeps writing to the renamed file), and the
/// instance that fails to bind logs a line of its own, so a login entry that
/// keeps hitting a port held for good must not grow the log without bound.
/// Windows cannot rename an open file, so it keeps to the stricter rule.
fn may_rotate_log(running: &Result<Option<Health>>) -> bool {
    match running {
        Ok(None) => true,
        Ok(Some(_)) => false,
        Err(_) => cfg!(unix),
    }
}

/// The async runtime a relay runs on.
pub fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("codex-copilot")
        .build()
        .context("could not start the async runtime")
}

/// A warning when `listen` is reachable from other machines (a wildcard or
/// LAN address): the relay has no authentication of its own, so anything
/// that reaches the port can use it with its own Copilot token, and anyone
/// can stop it with `POST /shutdown`. `None` for loopback addresses.
pub fn exposure_warning(listen: SocketAddr) -> Option<String> {
    // `to_canonical`: `::ffff:127.0.0.1` is loopback too.
    (!listen.ip().to_canonical().is_loopback()).then(|| {
        format!(
            "listening on {listen}, which is not a loopback address. The relay has no \
             authentication of its own: anything that reaches the port can use it with its own \
             Copilot token, or stop it."
        )
    })
}

/// Validates the relay arguments into the relay's configuration: `listen`
/// must resolve, `upstream` must be an http(s) origin (a trailing slash is
/// dropped), and an empty `review_model` turns the substitution off. No
/// proxy here: reqwest's policy is applied by [`proxy::bind`] from the
/// environment of the process that starts the relay (the proxy variables,
/// else the OS proxy settings), read once when it starts.
pub fn proxy_config(args: &RelayArgs) -> Result<ProxyConfig> {
    let listen = resolve_listen(args.listen.trim())?;
    let upstream = crate::check_origin("--upstream", &args.upstream)?;
    let review_model = Some(args.review_model.trim())
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    Ok(ProxyConfig {
        listen,
        upstream,
        review_model,
        proxy: None,
    })
}

/// `listen` as a socket address: an `IP:PORT` literal, or a `host:port` name
/// resolved to its first address.
pub fn resolve_listen(listen: &str) -> Result<SocketAddr> {
    if let Ok(addr) = listen.parse() {
        return Ok(addr);
    }
    listen
        .to_socket_addrs()
        .with_context(|| format!("--listen {listen:?} is not a host:port"))?
        .next()
        .ok_or_else(|| anyhow!("--listen {listen:?} resolved to no address"))
}

// ---------------------------------------------------------------------------
// Logging

enum LogTarget {
    Stderr,
    /// Append to [`LOG_FILE`] in this directory.
    Dir(PathBuf),
}

fn init_logging(target: &LogTarget, rotate: bool) -> Result<()> {
    let builder = tracing_subscriber::fmt().with_max_level(log_level());
    // `try_init` only fails when a subscriber is already installed; keep
    // that one.
    match target {
        LogTarget::Stderr => {
            let _ = builder
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal())
                .try_init();
        }
        LogTarget::Dir(dir) => {
            fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
            let path = dir.join(LOG_FILE);
            if rotate {
                // Best effort: a log that cannot be rotated is still worth
                // appending to.
                let _ = rotate_log(&path, MAX_LOG_BYTES);
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("could not open {}", path.display()))?;
            let _ = builder
                .with_writer(Mutex::new(file))
                .with_ansi(false)
                .try_init();
            log_panics();
        }
    }
    Ok(())
}

/// Logs to stderr at the `CODEX_COPILOT_LOG` level (default `info`), with
/// colours on a terminal: the foreground relay. Keeps a subscriber that is
/// already installed.
pub fn log_to_stderr() {
    // Writing to stderr cannot fail to set up.
    let _ = init_logging(&LogTarget::Stderr, false);
}

fn log_level() -> tracing::Level {
    std::env::var(LOG_LEVEL_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(tracing::Level::INFO)
}

/// Without a console a panic message would vanish; put it in the log too.
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        previous(info);
    }));
}

/// Moves `path` to `path.1` (replacing an older one) when it is larger than
/// `max_bytes`. Returns whether it rotated.
fn rotate_log(path: &Path, max_bytes: u64) -> std::io::Result<bool> {
    match fs::metadata(path) {
        Ok(meta) if meta.len() > max_bytes => {
            let mut rotated = path.as_os_str().to_owned();
            rotated.push(".1");
            fs::rename(path, rotated)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// Paths

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    Windows,
    MacOs,
    /// Linux and other Unixes: XDG paths.
    Unix,
}

const PLATFORM: Platform = if cfg!(windows) {
    Platform::Windows
} else if cfg!(target_os = "macos") {
    Platform::MacOs
} else {
    Platform::Unix
};

/// Per-user log directory, created on demand: `%LOCALAPPDATA%\codex-copilot\logs`,
/// `~/Library/Logs/codex-copilot`, or `$XDG_STATE_HOME/codex-copilot/logs`
/// (`~/.local/state/...`).
///
/// A non-empty `CODEX_COPILOT_LOG_DIR` replaces it. That is a test hook, not a
/// user setting: the end-to-end tests point it into their temp home, because
/// on Windows nothing else can redirect `%LOCALAPPDATA%` (it comes from
/// `SHGetKnownFolderPath`), and a test relay must neither write to nor rotate
/// the user's real log. A relay started by `start` inherits it.
pub fn log_dir() -> Result<PathBuf> {
    let dir = log_dir_path()?;
    fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir)
}

/// [`log_dir`] without creating it.
fn log_dir_path() -> Result<PathBuf> {
    match std::env::var_os(LOG_DIR_ENV).filter(|dir| !dir.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => log_dir_for(
            PLATFORM,
            dirs::home_dir(),
            dirs::data_local_dir(),
            dirs::state_dir(),
        )
        .context("could not determine the per-user log directory"),
    }
}

/// Where the background relay logs. Creates nothing: `status` prints it,
/// and the relay creates the directory when it starts.
pub fn log_file() -> Result<PathBuf> {
    Ok(log_dir_path()?.join(LOG_FILE))
}

/// `state` is the XDG state directory (`dirs::state_dir`, already honouring
/// `$XDG_STATE_HOME`); `local_data` is `%LOCALAPPDATA%`.
fn log_dir_for(
    platform: Platform,
    home: Option<PathBuf>,
    local_data: Option<PathBuf>,
    state: Option<PathBuf>,
) -> Option<PathBuf> {
    match platform {
        Platform::Windows => local_data.map(|d| d.join("codex-copilot").join("logs")),
        Platform::MacOs => home.map(|h| h.join("Library").join("Logs").join("codex-copilot")),
        Platform::Unix => state
            .or_else(|| home.map(|h| h.join(".local").join("state")))
            .map(|d| d.join("codex-copilot").join("logs")),
    }
}

// ---------------------------------------------------------------------------
// Starting a background relay

/// Spawns this executable as the background relay running `args`, detached
/// from this process and its terminal, and returns at once (pair with
/// [`wait_started`]). Invalid `args` are an error here, not a relay that
/// exits at once. A relay already running on the address makes the new one
/// exit, so check for one first.
pub fn spawn_background(args: &RelayArgs) -> Result<Started> {
    proxy_config(args).context("invalid relay arguments")?;
    let exe = std::env::current_exe().context("could not locate the running executable")?;
    spawn_detached(&exe, &background_argv(args))
}

/// A warning for a relay that had to stay in the caller's job object
/// ([`Started::in_callers_job`]); `None` when it did not.
pub fn job_warning(started: &Started) -> Option<String> {
    started.in_callers_job.then(|| {
        "Windows did not let the relay leave the job object this terminal runs in, so it stays \
         in that job and may be stopped when the terminal closes. To keep it running, run \
         `codex-copilot start` from a shell outside this terminal (a new window from the Start \
         menu)."
            .to_string()
    })
}

/// Spawns `exe` with no console and no inherited stdio, detached from this
/// process so it outlives the terminal that ran the CLI (see
/// [`Started::in_callers_job`] for when it cannot quite).
fn spawn_detached(exe: &Path, argv: &[String]) -> Result<Started> {
    let spawn = |breakaway: bool| {
        let mut cmd = Command::new(exe);
        cmd.args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Not the caller's cwd: on Windows a process pins its working
        // directory, which would stop the user from deleting it.
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        detach(&mut cmd, breakaway);
        cmd.spawn()
    };
    let (child, in_callers_job) =
        without_inherited_std_handles(|| spawn_breaking_away(PLATFORM == Platform::Windows, spawn))
            .with_context(|| format!("could not start {}", exe.display()))?;
    let pid = child.id();
    if in_callers_job {
        tracing::warn!(
            pid,
            "{} could not break away from the caller's job object; it stays in that job and \
             ends with it if the job kills its processes when it closes",
            exe.display()
        );
    } else {
        tracing::debug!(pid, "started {}", exe.display());
    }
    Ok(Started {
        child,
        in_callers_job,
    })
}

/// Calls `spawn(true)`, which on Windows asks to leave the caller's job
/// object: that keeps the relay alive when a terminal that kills its job on
/// exit closes. A job that forbids breakaway makes CreateProcess fail with
/// ERROR_ACCESS_DENIED; then this retries with `spawn(false)`, inside the
/// job. Returns the child and whether it stayed in the caller's job.
fn spawn_breaking_away<T>(
    windows: bool,
    mut spawn: impl FnMut(bool) -> std::io::Result<T>,
) -> std::io::Result<(T, bool)> {
    match spawn(true) {
        Err(err) if windows && err.kind() == std::io::ErrorKind::PermissionDenied => {
            spawn(false).map(|child| (child, true))
        }
        other => other.map(|child| (child, false)),
    }
}

/// Runs `spawn` with this process's own stdin / stdout / stderr marked
/// non-inheritable. std's `CreateProcessW` passes `bInheritHandles = TRUE`, so
/// the relay would otherwise inherit them next to its null stdio and hold
/// them open for its whole life: a caller reading the CLI through a pipe
/// (`start | tee`, a CI log, a test harness) would never see EOF. The flags
/// are restored afterwards. On Unix the child's fds 0-2 are replaced and
/// everything else std opens is close-on-exec, so nothing leaks.
#[cfg(windows)]
fn without_inherited_std_handles<T>(spawn: impl FnOnce() -> T) -> T {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, SetHandleInformation, HANDLE_FLAG_INHERIT,
    };

    // std maps a missing standard handle (INVALID_HANDLE_VALUE, which would
    // alias the current-process pseudo handle) to null.
    let handles = [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ];
    let mut cleared = Vec::new();
    for handle in handles {
        // stdout and stderr are often the same console or pipe handle.
        if handle.is_null() || cleared.contains(&handle) {
            continue;
        }
        let mut flags = 0u32;
        // SAFETY: plain Win32 calls on this process's standard handles; an
        // invalid one makes them fail, and failure only means "leave it".
        let inherited = unsafe { GetHandleInformation(handle, &mut flags) } != 0
            && flags & HANDLE_FLAG_INHERIT != 0;
        if inherited && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0 {
            cleared.push(handle);
        }
    }
    let result = spawn();
    for handle in cleared {
        // SAFETY: as above; restores the flag cleared a moment ago.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    result
}

#[cfg(not(windows))]
fn without_inherited_std_handles<T>(spawn: impl FnOnce() -> T) -> T {
    spawn()
}

/// No console window (the binary is a console program), no console to
/// share with the caller, its own process group (a Ctrl-C in the terminal
/// must not reach it), and out of the caller's job when allowed.
#[cfg(windows)]
fn detach(cmd: &mut Command, breakaway: bool) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let mut flags = CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    if breakaway {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    cmd.creation_flags(flags);
}

/// Its own process group, outside the terminal's foreground group: a Ctrl-C
/// in the terminal that ran the CLI does not reach it, and neither does the
/// hangup sent to the foreground group when the terminal closes. Its stdio
/// is already /dev/null, so losing the terminal costs it nothing.
#[cfg(unix)]
fn detach(cmd: &mut Command, _breakaway: bool) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(any(windows, unix)))]
fn detach(_cmd: &mut Command, _breakaway: bool) {}

// ---------------------------------------------------------------------------
// Talking to a running relay

/// Asks `listen` whether a codex-copilot relay runs there. `Ok(None)` when
/// nothing accepts the connection; an error when something answers that is
/// not a healthy relay, or accepts and does not answer within `timeout`.
///
/// Blocking; do not call from inside an async runtime.
pub fn health(listen: &str, timeout: Duration) -> Result<Option<Health>> {
    Probe::new(listen, timeout)?.health()
}

/// Polls [`health`] every 200 ms until the relay answers or `timeout` passes.
pub fn wait_healthy(listen: &str, timeout: Duration) -> Result<Health> {
    poll_healthy(listen, timeout, || Ok(()))
}

/// [`wait_healthy`] for a relay [`spawn_background`] just started: also
/// fails as soon as its process has exited without answering (a port taken
/// by something else, an upstream proxy it cannot use), instead of waiting
/// out `timeout`.
pub fn wait_started(listen: &str, child: &mut Child, timeout: Duration) -> Result<Health> {
    poll_healthy(listen, timeout, || {
        match child
            .try_wait()
            .context("could not check on the relay process")?
        {
            Some(status) => bail!("the relay process exited ({status})"),
            None => Ok(()),
        }
    })
}

/// Polls `/healthz` until it answers; between tries `alive` may end the wait
/// with its error.
fn poll_healthy(
    listen: &str,
    timeout: Duration,
    mut alive: impl FnMut() -> Result<()>,
) -> Result<Health> {
    let probe = Probe::new(listen, timeout.clamp(MIN_PROBE, MAX_PROBE))?;
    let deadline = Instant::now() + timeout;
    loop {
        let last = match probe.health() {
            Ok(Some(found)) => return Ok(found),
            Ok(None) => anyhow!("nothing is listening on {listen}"),
            Err(err) => err,
        };
        if let Err(gone) = alive() {
            return Err(gone.context(format!("the relay on {listen} never answered: {last:#}")));
        }
        if Instant::now() >= deadline {
            return Err(last.context(format!(
                "the relay on {listen} did not become healthy within {timeout:?}"
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Stops the relay on `listen` with `POST /shutdown`, then waits until its
/// port refuses connections, which is what the next relay needs to bind it.
/// Returns what the relay's `/healthz` said before it stopped; `Ok(None)`
/// when nothing listened on `listen`. Something on the port that is not a
/// relay is an error and is left alone; so is a relay whose port still
/// accepts connections `timeout` after the request (the error names its pid,
/// for the user to end it).
pub fn stop(listen: &str, timeout: Duration) -> Result<Option<Health>> {
    let probe = Probe::new(listen, timeout.clamp(MIN_PROBE, MAX_PROBE))?;
    let Some(found) = probe.health()? else {
        return Ok(None);
    };
    match probe.post_shutdown() {
        Ok(status) if status.is_success() => {}
        Ok(status) => bail!("POST {}/shutdown returned {status}", probe.base),
        // The relay may drop the connection as it stops; the port decides.
        Err(_) => {}
    }
    if !wait_port_closed(&connect_addr(listen), timeout)? {
        // The pid it reports now, should something have replaced it.
        let pid = match probe.health() {
            Ok(Some(now)) => now.pid,
            _ => found.pid,
        };
        bail!(
            "the relay on {listen} still accepts connections {timeout:?} after POST /shutdown; \
             end its process (pid {pid}) yourself"
        );
    }
    Ok(Some(found))
}

/// Polls a plain TCP connect to `addr` until it fails (refused, or no
/// handshake within [`CONNECT_TIMEOUT`], which on loopback means nothing
/// listens) or `timeout` passes. `true` once nothing accepts connections.
fn wait_port_closed(addr: &str, timeout: Duration) -> Result<bool> {
    let targets: Vec<SocketAddr> = addr
        .to_socket_addrs()
        .with_context(|| format!("{addr:?} is not a host:port"))?
        .collect();
    let deadline = Instant::now() + timeout;
    loop {
        let open = targets
            .iter()
            .any(|target| TcpStream::connect_timeout(target, CONNECT_TIMEOUT).is_ok());
        if !open {
            return Ok(true);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline - now));
    }
}

struct Probe {
    client: reqwest::blocking::Client,
    /// `http://host:port`.
    base: String,
}

impl Probe {
    fn new(listen: &str, timeout: Duration) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            // A loopback probe must never go through HTTP(S)_PROXY.
            .no_proxy()
            // A connect error or timeout means "not running"; a relay that
            // accepts but does not answer within `timeout` is an error.
            .connect_timeout(timeout.min(CONNECT_TIMEOUT))
            .timeout(timeout)
            // A pooled connection to a relay that just shut down would turn
            // "gone" into a transport error.
            .pool_max_idle_per_host(0)
            .build()
            .context("could not build an HTTP client")?;
        Ok(Self {
            client,
            base: format!("http://{}", connect_addr(listen)),
        })
    }

    fn health(&self) -> Result<Option<Health>> {
        let url = format!("{}/healthz", self.base);
        let response = match self.client.get(&url).send() {
            Ok(response) => response,
            Err(err) if err.is_connect() => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("GET {url} failed")),
        };
        let status = response.status();
        if status != StatusCode::OK {
            bail!("GET {url} returned {status}; is something else listening there?");
        }
        let found: Health = response
            .json()
            .with_context(|| format!("GET {url} did not return codex-copilot health JSON"))?;
        if found.name != HEALTH_NAME {
            bail!(
                "{} is served by {:?}, not {HEALTH_NAME}",
                self.base,
                found.name
            );
        }
        Ok(Some(found))
    }

    fn post_shutdown(&self) -> reqwest::Result<StatusCode> {
        let url = format!("{}/shutdown", self.base);
        self.client.post(url).send().map(|r| r.status())
    }
}

/// The address a client on this machine connects to for a relay listening
/// on `listen`. A wildcard bind address is not connectable everywhere
/// (Windows refuses `0.0.0.0`), so it becomes loopback of the same family;
/// anything else is returned as is. The probes use it, and `install` writes
/// it into Codex's `base_url`.
pub fn connect_addr(listen: &str) -> String {
    match listen.parse::<SocketAddr>() {
        Ok(mut addr) if addr.ip().is_unspecified() => {
            let loopback = match addr {
                SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
            };
            addr.set_ip(loopback);
            addr.to_string()
        }
        _ => listen.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    #[test]
    fn relay_args_default_to_the_crate_constants() {
        let args = RelayArgs::default();
        assert_eq!(args.listen, crate::DEFAULT_LISTEN);
        assert_eq!(args.listen, "127.0.0.1:12899");
        assert_eq!(args.upstream, crate::DEFAULT_UPSTREAM);
        assert_eq!(args.review_model, crate::DEFAULT_REVIEW_MODEL);
    }

    #[test]
    fn the_background_argv_spells_every_option_out() {
        let args = RelayArgs {
            listen: "127.0.0.1:5000".into(),
            upstream: "https://api.githubcopilot.com".into(),
            review_model: String::new(),
        };
        assert_eq!(
            background_argv(&args),
            [
                "--background",
                "--listen",
                "127.0.0.1:5000",
                "--upstream",
                "https://api.githubcopilot.com",
                "--review-model",
                ""
            ]
        );
        // Defaults too: the background instance never consults the state.
        assert_eq!(background_argv(&RelayArgs::default()).len(), 7);
    }

    #[test]
    fn proxy_config_validates_and_normalizes() {
        let mut a = RelayArgs {
            upstream: "https://api.githubcopilot.com/".into(),
            ..RelayArgs::default()
        };
        let cfg = proxy_config(&a).unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:12899".parse::<SocketAddr>().unwrap());
        assert_eq!(cfg.upstream, "https://api.githubcopilot.com");
        assert_eq!(cfg.review_model.as_deref(), Some("gpt-6-luna"));
        // The proxy variables are read by the starting relay, not here.
        assert_eq!(cfg.proxy, None);

        a.review_model = "  ".into();
        assert_eq!(proxy_config(&a).unwrap().review_model, None);

        for bad in [
            "https://host?x=1",
            "https://host/v1",
            "https://user@host",
            "https://host#frag",
            "https://",
        ] {
            a.upstream = bad.into();
            let err = proxy_config(&a).unwrap_err().to_string();
            assert!(err.starts_with("--upstream must be"), "{bad}: {err}");
        }

        a.upstream = "api.githubcopilot.com".into();
        assert!(proxy_config(&a)
            .unwrap_err()
            .to_string()
            .contains("--upstream"));

        let b = RelayArgs {
            listen: "not an address".into(),
            ..RelayArgs::default()
        };
        assert!(proxy_config(&b).is_err());
    }

    #[test]
    fn invalid_arguments_are_refused_before_anything_is_spawned() {
        let bad = RelayArgs {
            upstream: "api.githubcopilot.com".into(),
            ..RelayArgs::default()
        };
        let err = spawn_background(&bad).unwrap_err();
        assert!(format!("{err:#}").contains("--upstream"), "{err:#}");
        let bad = RelayArgs {
            listen: "not an address".into(),
            ..RelayArgs::default()
        };
        let err = spawn_background(&bad).unwrap_err();
        assert!(format!("{err:#}").contains("--listen"), "{err:#}");
    }

    #[test]
    fn log_dir_follows_platform_conventions() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            log_dir_for(
                Platform::Windows,
                home.clone(),
                Some(PathBuf::from(r"C:\Users\u\AppData\Local")),
                None
            ),
            Some(
                PathBuf::from(r"C:\Users\u\AppData\Local")
                    .join("codex-copilot")
                    .join("logs")
            )
        );
        assert_eq!(
            log_dir_for(Platform::Windows, home.clone(), None, None),
            None
        );
        assert_eq!(
            log_dir_for(Platform::MacOs, home.clone(), None, None),
            Some(PathBuf::from("/home/u/Library/Logs/codex-copilot"))
        );
        assert_eq!(
            log_dir_for(
                Platform::Unix,
                home.clone(),
                None,
                Some(PathBuf::from("/xdg/state"))
            ),
            Some(PathBuf::from("/xdg/state/codex-copilot/logs"))
        );
        assert_eq!(
            log_dir_for(Platform::Unix, home, None, None),
            Some(PathBuf::from("/home/u/.local/state/codex-copilot/logs"))
        );
    }

    #[test]
    fn large_logs_rotate_and_replace_the_previous_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(LOG_FILE);
        let old = dir.path().join("relay.log.1");

        assert!(!rotate_log(&log, 10).unwrap(), "missing file is a no-op");

        fs::write(&log, "small").unwrap();
        assert!(!rotate_log(&log, 10).unwrap());
        assert!(log.exists());

        fs::write(&old, "older").unwrap();
        fs::write(&log, "more than ten bytes").unwrap();
        assert!(rotate_log(&log, 10).unwrap());
        assert!(!log.exists());
        assert_eq!(fs::read_to_string(&old).unwrap(), "more than ten bytes");
    }

    #[test]
    fn non_loopback_listen_addresses_get_a_warning() {
        let warn = |addr: &str| exposure_warning(addr.parse().unwrap());
        assert_eq!(warn("127.0.0.1:12899"), None);
        assert_eq!(warn("127.0.0.2:12899"), None);
        assert_eq!(warn("[::1]:12899"), None);
        assert_eq!(warn("[::ffff:127.0.0.1]:12899"), None);
        let wildcard = warn("0.0.0.0:12899").unwrap();
        assert!(wildcard.contains("0.0.0.0:12899"), "{wildcard}");
        assert!(wildcard.contains("not a loopback address"), "{wildcard}");
        assert!(warn("[::]:12899").is_some());
        assert!(warn("192.168.1.20:12899").is_some());
    }

    #[test]
    fn a_denied_breakaway_is_retried_inside_the_callers_job() {
        fn denied<T>() -> std::io::Result<T> {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }
        // Windows, job forbids breakaway: retried without it, and reported.
        let mut tries = Vec::new();
        let outcome = spawn_breaking_away(true, |breakaway| {
            tries.push(breakaway);
            if breakaway {
                denied()
            } else {
                Ok("child")
            }
        });
        assert_eq!(outcome.unwrap(), ("child", true));
        assert_eq!(tries, [true, false]);
        // Breakaway allowed: one try, out of the job.
        let mut tries = Vec::new();
        let outcome = spawn_breaking_away(true, |breakaway| {
            tries.push(breakaway);
            Ok("child")
        });
        assert_eq!(outcome.unwrap(), ("child", false));
        assert_eq!(tries, [true]);
        // Off Windows access denied is a real failure (an unexecutable
        // binary), and so is any other error on Windows.
        let mut tries = 0;
        let outcome = spawn_breaking_away(false, |_| {
            tries += 1;
            denied::<&str>()
        });
        assert_eq!(
            outcome.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(tries, 1);
        let outcome = spawn_breaking_away(true, |_| {
            Err::<&str, _>(std::io::ErrorKind::NotFound.into())
        });
        assert_eq!(outcome.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn only_a_relay_left_in_the_callers_job_is_warned_about() {
        let started = |in_callers_job| Started {
            child: exits_with(0),
            in_callers_job,
        };
        let mut free = started(false);
        assert_eq!(job_warning(&free), None);
        let mut stuck = started(true);
        let warning = job_warning(&stuck).unwrap();
        assert!(warning.contains("job object"), "{warning}");
        assert!(warning.contains("when the terminal closes"), "{warning}");
        assert!(warning.contains("`codex-copilot start`"), "{warning}");
        assert!(warning.contains("from a shell outside"), "{warning}");
        let _ = free.child.wait();
        let _ = stuck.child.wait();
    }

    #[test]
    fn the_log_rotates_only_when_nothing_answers_the_startup_probe() {
        assert!(may_rotate_log(&Ok(None)));
        let running: Health = serde_json::from_str(&health_json(HEALTH_NAME)).unwrap();
        assert!(!may_rotate_log(&Ok(Some(running))));
        // Something holds the port without answering as a relay should: its
        // bind failure is logged too, so Unix rotates (renaming is safe there).
        assert_eq!(
            may_rotate_log(&Err(anyhow!("GET /healthz timed out"))),
            cfg!(unix)
        );
    }

    #[test]
    fn wildcard_listen_addresses_are_reached_on_loopback() {
        assert_eq!(connect_addr("0.0.0.0:12899"), "127.0.0.1:12899");
        assert_eq!(connect_addr("[::]:12899"), "[::1]:12899");
        assert_eq!(connect_addr("127.0.0.1:12899"), "127.0.0.1:12899");
        assert_eq!(connect_addr("localhost:12899"), "localhost:12899");
    }

    // --- a minimal stand-in for a running relay -----------------------------

    fn health_json(name: &str) -> String {
        serde_json::to_string(&Health {
            name: name.to_string(),
            version: "2.0.0".into(),
            upstream: crate::DEFAULT_UPSTREAM.into(),
            review_model: Some("gpt-6-luna".into()),
            pid: 4242,
            proxy: None,
        })
        .unwrap()
    }

    /// A process that exits at once with `code`.
    fn exits_with(code: u8) -> Child {
        let mut cmd = if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", &format!("exit {code}")]);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", &format!("exit {code}")]);
            cmd
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Serves `GET /healthz` with `status` / `body` and answers `POST
    /// /shutdown` with 200. With `close_on_shutdown` it then closes the
    /// listener, so later connects are refused, like a relay; without, it
    /// keeps accepting, like a relay that does not stop.
    fn fake_relay(status: u16, body: String, close_on_shutdown: bool) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let head = read_head(&mut stream);
                let shutdown = head.starts_with("POST /shutdown ");
                let (code, text) = if head.starts_with("GET /healthz ") {
                    (status, body.as_str())
                } else if shutdown {
                    (200, "")
                } else {
                    (404, "")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = stream.flush();
                if shutdown && close_on_shutdown {
                    return;
                }
            }
        });
        (addr, handle)
    }

    fn read_head(stream: &mut std::net::TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => break,
            }
        }
        String::from_utf8_lossy(&head).into_owned()
    }

    /// An address nothing listens on (bound, then released).
    fn dead_addr() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().to_string()
    }

    const T: Duration = Duration::from_secs(5);

    #[test]
    fn health_is_none_when_nothing_listens() {
        let started = Instant::now();
        assert!(health(&dead_addr(), T).unwrap().is_none());
        // Not Windows' ~2 s refused-connect retry, and not the full timeout.
        assert!(started.elapsed() < Duration::from_millis(1500));
    }

    #[test]
    fn health_errors_when_a_listener_never_answers() {
        // Never accepted: the kernel still completes the handshake, so this
        // is a hung server, not a missing one.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let err = health(&addr, Duration::from_millis(300)).unwrap_err();
        assert!(format!("{err:#}").contains("/healthz"), "{err:#}");
        drop(listener);
    }

    #[test]
    fn health_reads_a_running_relay() {
        let (addr, _server) = fake_relay(200, health_json(HEALTH_NAME), true);
        let found = health(&addr, T).unwrap().unwrap();
        assert_eq!(found.name, HEALTH_NAME);
        assert_eq!(found.pid, 4242);
        assert_eq!(found.review_model.as_deref(), Some("gpt-6-luna"));
    }

    #[test]
    fn health_rejects_other_servers() {
        let (addr, _server) = fake_relay(503, String::new(), true);
        let err = health(&addr, T).unwrap_err();
        assert!(format!("{err:#}").contains("503"), "{err:#}");

        let (addr, _server) = fake_relay(200, "<html>".into(), true);
        let err = health(&addr, T).unwrap_err();
        assert!(format!("{err:#}").contains("health JSON"), "{err:#}");

        let (addr, _server) = fake_relay(200, health_json("something-else"), true);
        let err = health(&addr, T).unwrap_err();
        assert!(format!("{err:#}").contains("something-else"), "{err:#}");
    }

    #[test]
    fn wait_healthy_returns_once_the_relay_answers() {
        let (addr, _server) = fake_relay(200, health_json(HEALTH_NAME), true);
        assert_eq!(wait_healthy(&addr, T).unwrap().pid, 4242);
    }

    #[test]
    fn wait_healthy_times_out_when_nothing_answers() {
        let started = Instant::now();
        let err = wait_healthy(&dead_addr(), Duration::from_millis(600)).unwrap_err();
        assert!(
            format!("{err:#}").contains("did not become healthy"),
            "{err:#}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A relay process that died at startup is reported at once, not after
    /// the whole wait.
    #[test]
    fn wait_started_gives_up_when_the_relay_process_exits() {
        let mut child = exits_with(3);
        let started = Instant::now();
        let err = wait_started(&dead_addr(), &mut child, Duration::from_secs(30)).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("the relay process exited"), "{text}");
        assert!(text.contains("never answered"), "{text}");
        assert!(started.elapsed() < Duration::from_secs(10), "{text}");
    }

    #[test]
    fn stop_is_none_when_nothing_runs() {
        assert!(stop(&dead_addr(), T).unwrap().is_none());
    }

    #[test]
    fn stop_leaves_something_that_is_not_a_relay_alone() {
        let (addr, _server) = fake_relay(200, health_json("something-else"), true);
        let err = stop(&addr, T).unwrap_err();
        assert!(format!("{err:#}").contains("something-else"), "{err:#}");
        // Still there: no shutdown was posted.
        assert!(TcpStream::connect(&addr).is_ok());
    }

    #[test]
    fn stop_posts_shutdown_and_waits_until_the_port_is_closed() {
        let (addr, server) = fake_relay(200, health_json(HEALTH_NAME), true);
        let found = stop(&addr, T).unwrap().expect("a relay answered");
        assert_eq!(found.pid, 4242);
        server.join().unwrap();
        assert!(TcpStream::connect_timeout(&addr.parse().unwrap(), CONNECT_TIMEOUT).is_err());
        assert!(health(&addr, T).unwrap().is_none());
    }

    #[test]
    fn stop_fails_with_the_pid_when_the_port_stays_open() {
        let (addr, _server) = fake_relay(200, health_json(HEALTH_NAME), false);
        let err = stop(&addr, Duration::from_millis(800)).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("still accepts connections"), "{text}");
        assert!(text.contains("pid 4242"), "{text}");
    }

    /// The polling behind `stop`, against an in-process listener.
    #[test]
    fn the_port_wait_ends_when_the_listener_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        // Held for the whole wait: still open at the deadline.
        let started = Instant::now();
        assert!(!wait_port_closed(&addr, Duration::from_millis(500)).unwrap());
        assert!(started.elapsed() >= Duration::from_millis(500));

        // Closed part-way through: the wait ends soon after, well before
        // its timeout.
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            drop(listener);
        });
        let started = Instant::now();
        assert!(wait_port_closed(&addr, Duration::from_secs(20)).unwrap());
        assert!(started.elapsed() < Duration::from_secs(5));
        closer.join().unwrap();

        // Nothing listening at all: true at once.
        assert!(wait_port_closed(&dead_addr(), Duration::ZERO).unwrap());
        assert!(wait_port_closed("not an address", T).is_err());
    }
}
