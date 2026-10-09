# codex-copilot

Runs a small local relay that lets an installed **OpenAI Codex CLI** use the
**GitHub Copilot CAPI** over the stateful `ws:/responses` transport, and gives
Codex a dedicated home, `~/.codex-copilot`, that is configured to go through it.

Codex connects to `ws://127.0.0.1:12899/responses`. The relay opens the real
`wss://api.*.githubcopilot.com/responses`, adds the Copilot identity headers,
and passes every frame through in order. On the way back it changes one thing:
item ids, which Copilot changes on every streamed event and Codex expects to
stay put. Remote compaction v2, `previous_response_id` and incremental input
work as they do against OpenAI. When Codex falls back to HTTP SSE
(`POST /responses`), the relay serves that transport too, with the same
rewrite.

There is one executable, `codex-copilot`, with two halves. The stateless half
(`install`, `override`, `unoverride`, `login`, `status`, `uninstall`) only
reads and writes files and never starts or stops a process (`status` and
`uninstall` only look whether a relay answers). The stateful half (a bare
`codex-copilot`, `start`, `stop`) only starts and stops the relay and never
writes your configuration. **The tool registers nothing to run at login**: after
a reboot you run `codex-copilot start` yourself, or set up one of the recipes in
[Starting the relay at login](#starting-the-relay-at-login).

The dedicated home is a complete, separate Codex identity; plain `codex` keeps
using `~/.codex` until you run `override`, which swaps the two directories.

The bearer is your GitHub OAuth token, read by Codex at request time from the
`COPILOT_GITHUB_TOKEN` environment variable and forwarded by the relay with
each connection. **Nothing here stores it** - no keyring, no registry write, no
file, not even in the relay. `login` prints the token and the one-liner that
sets the variable; you run it.

## Why 2.0 is a proxy

1.x was deliberately **not a proxy**. It wrote a configuration that pointed
Codex straight at `api.*.githubcopilot.com` and got out of the loop, because
a relay in the middle looked like it would have to replay or break the
connection-bound state Codex keeps on the WebSocket.

That configuration connects and runs, but Copilot's Responses backend does not
keep an output item's id stable. Every streamed event of the same item carries
a **different** opaque id: `response.output_item.added`, each
`content_part` and `output_text.delta`, `output_item.done`, and the item's entry
in `response.completed.output[]` all differ. `response.id` changes too, between
`response.created`, `response.in_progress` and `response.completed`. Only
`call_id` and `output_index` are stable. Codex assumes `added` and `done` share
one id, so the TUI, the App Server and clients built on it (the ChatGPT
desktop app) render duplicated or stuck items.

Captured directly from CAPI, with no Codex in between: across eight completed
requests and nine output items, `added` and `done` agreed **zero** times, and
one short message produced ten events with ten different ids (abridged):

```text
output_index  event                       Copilot item id
0             output_item.added           poZfyGL9vc...
0             content_part.added          oQu4nYQ4JB...
0             output_text.delta           FVrVSHQ1Kx...
0             output_text.delta           IM6EH44jqy...
0             output_text.done            HBb46gohOG...
0             output_item.done            Nj7fc2AD8R...
output[0]     response.completed          j0jgZocJuf...
```

It reproduces on the enterprise and the generic host, over WebSocket and over
HTTP SSE, with API version `2025-10-01` as well as `2026-08-01`, and on smaller
models. Turning WebSockets off does not help; the VS Code Chat identity is not
served the target models at all. No configuration fixes it, so 2.0 puts a relay
exactly where 1.x refused to.

The 1.x objection still holds for a relay that terminates or pools
connections. This one does neither: every Codex WebSocket gets its own upstream
WebSocket, opened when Codex connects and closed when it closes, and frames
pass through in order. `previous_response_id`, incremental input and
remote compaction (which rides the same socket) see the connection they would
see without the relay. `response.id` is **never** rewritten, so
`previous_response_id` always names a response Copilot really completed.

What it costs: one background process and one loopback port.

## How the relay works

```text
codex --ws://127.0.0.1:12899/responses--> codex-copilot --wss://api.<plan>.githubcopilot.com/responses--> CAPI
```

**Handshake.** With `supports_websockets = true`, Codex derives the WebSocket
URL from `base_url` itself (`http://` becomes `ws://`). The relay opens the
upstream socket with Codex's headers (bearer included, hop-by-hop headers
dropped) plus the seven Copilot identity headers: `copilot-integration-id`,
`editor-version`, `editor-plugin-version`, `x-github-api-version`,
`openai-intent`, `x-interaction-type` and `x-initiator`. It then copies the
upstream's `101` response headers (minus hop-by-hop and
`sec-websocket-accept`) onto its own `101`, because Codex reads the
reasoning-included, server-model and turn-state headers from there. A refused
upstream handshake (`401`, `403`, `429`, ...) reaches Codex with the same
status (except `426`, see [below](#the-http-sse-fallback)) and the upstream's
end-to-end headers. Its body is read to the end, up to 256 MiB; one that was
cut short is passed on as far as it arrived (a chunked body as far as it
decodes, which may be nothing), and never with its chunk framing.

**Downstream** (CAPI to Codex): item ids are rewritten as described below.
Everything else in each frame is left as it was, key order included.

**Upstream** (Codex to CAPI; each `response.create` frame, and the body of a
`POST /responses`):

- `"model": "codex-auto-review"` becomes the configured review model (see
  [Approval review](#approval-review---no-yolo)).
- Any relay-made `copilot-...` id still present in `input[]` is dropped.
  Codex already strips them; this is a second line of defence.
- Everything else goes out as Codex sent it.

**Plain HTTP** on the same port:

| Request | Answer |
| --- | --- |
| `GET /healthz` | JSON with name, version, upstream, review model, pid and proxy (the one the relay reaches the upstream through, credentials masked, or `null`; see [Proxies](#proxies)). `status`, `start`, `stop`, `uninstall` and a foreground run use it to find a running relay and to tell it from anything else on the port |
| `POST /shutdown` | stops the relay gracefully; this is how `codex-copilot stop` stops it. Refused (`403`) when the request carries an `Origin` header, so a web page cannot send it |
| `POST /responses` | the HTTP SSE transport, relayed with the same rewrites (see [below](#the-http-sse-fallback)) |
| any other request on `/responses` | `501` with a JSON error, unless it is a WebSocket upgrade |
| anything else | proxied to the upstream with the identity headers added, e.g. `POST /alpha/search`, which Codex calls for web search on responses-lite models |

### The HTTP SSE fallback

`supports_websockets = true` makes the WebSocket Codex's transport, but not its
only one. After `stream_max_retries` (default 5) consecutive retryable
failures - a dropped network, a sleeping laptop, the relay's own `502` or
`504`, an upstream reset - Codex switches the session to HTTP SSE for good and
prints "Falling back from WebSockets to HTTPS transport"
(`core/src/responses_retry.rs`, `force_http_fallback` in
[core/src/client.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/client.rs)).
Every later turn of that session is a `POST /responses`, so the relay serves it
as well:

- The request body is a `response.create` without its `type`. It gets the
  upstream rules above and goes to `<upstream>/responses` with the identity
  headers. Each `POST` is a new id namespace.
- The reply's `text/event-stream` is re-framed event by event: each event's
  `data:` JSON gets the same id rewrite as a WebSocket frame and goes out as
  one `data:` line. Codex reads nothing but `data`, so `event:`, `id:`,
  `retry:` and comment lines are dropped; events with nothing to rewrite keep
  their `data:` lines as they were.
- Anything that is not an uncompressed event stream, errors included, is
  mirrored as is.

The relay never answers `426 Upgrade Required` on the WebSocket route, and
turns an upstream `426` into a `502`. At the handshake a `426` makes Codex drop
WebSockets for the whole session at once, where any other failure only does so
after the retry budget is spent.

### Limits and shutdown

- Frames and messages of up to 256 MiB pass on both hops (the WebSocket
  library's defaults are 16 MiB per frame and 64 MiB per message). A
  `POST /responses` body has the same bound: a larger one (or one that cannot
  be read) gets `413`.
- On the SSE transport one event may also be up to 256 MiB. An upstream event
  that grows past that, or a line that never ends, is not passed on with
  Copilot's ids: the relay ends the response body with an error, so Codex sees
  a broken stream and retries. The log says `upstream event stream cut off`.
- `TCP_NODELAY` is set on both hops, so small delta events are not held back.
- The upstream WebSocket handshake gets 12 s, the connection to a proxy and
  its `CONNECT` reply included, less than Codex's own 15 s connect timeout;
  after that Codex gets a `504`. An upstream that cannot be reached is a
  `502`.
- `POST /shutdown` (`codex-copilot stop`), ctrl-c and SIGTERM
  (`systemctl --user stop`, `launchctl bootout`) stop the relay gracefully:
  every open bridge sends a `1001` close to both peers, and the process exits
  0.

Known limits:

- No SOCKS proxies. When the upstream would go through one (`socks5://`,
  ...), the relay refuses to start (see [Proxies](#proxies)).
- Neither hop uses `permessage-deflate`. Codex offers it, the relay does not
  negotiate it, and it does not pass the offer on to CAPI, so both hops are
  uncompressed.

### Proxies

Both transports reach CAPI through one HTTP client (reqwest, the WebSocket
handshake being an HTTP/1.1 upgrade), so they always go the same way, with
the same proxy rules as Codex's own client:

- `HTTPS_PROXY` for an `https://` upstream (the default), `HTTP_PROXY` for an
  `http://` one, and `ALL_PROXY` for either when that variable is not set.
  The upper-case name wins over the lower-case one. A value without a scheme
  (`proxy:3128`) is an `http://` proxy.
- `NO_PROXY` lists the hosts reached directly, comma-separated: `*` for all of
  them, IP addresses and CIDR blocks (`10.0.0.0/8`), and domains, which match
  themselves and their subdomains with or without a leading dot (`example.com`
  and `.example.com` both match `example.com` and `api.example.com`).
- A variable that is not set is filled in from the OS proxy settings, ahead of
  `ALL_PROXY`: Windows Internet Settings (the proxy server and its bypass
  list) or the macOS system proxy.
- An upstream on this machine (`localhost`, a loopback IP) is always reached
  directly, whatever the variables or the system settings say.
- `http://` and `https://` (TLS to the proxy itself) proxies work. Through
  either one, the connection to an `https://` upstream is a `CONNECT` tunnel
  with TLS to CAPI inside it. Credentials in the proxy URL
  (`http://user:password@proxy:3128`, percent-encoded where needed) are sent
  as `Proxy-Authorization: Basic`. Logs, errors and `/healthz` show the proxy
  as `http://***@proxy:3128`.
- SOCKS proxies (`socks5://`, ...) do not: when the upstream would go
  through one, the relay refuses to start, and says so in its log. Only the
  proxy the upstream would use counts, so `HTTPS_PROXY=http://127.0.0.1:7890`
  next to `ALL_PROXY=socks5://127.0.0.1:7891` (a common Clash or v2ray layout)
  works for the default `https://` upstream.
- A proxy that refuses the tunnel (`407`, `403`, ...) or cannot be reached
  gets Codex a `502` of type `upstream_unreachable`. Every `502` or `504` the
  relay sends for an upstream it could not reach says how it tried: through
  which proxy, or with no proxy.
- `/healthz` reports the proxy in use (`null` for none), and
  `codex-copilot status --probe` prints it on its `proxy` line, with a WARN
  when a relay started from the shell running `status` would pick another.

The relay reads the variables and the system settings once, when it starts,
from its own environment:

- `codex-copilot start` and a bare `codex-copilot` pass the environment of the
  shell they run in on to the relay.
- A login-time entry from [Starting the relay at login](#starting-the-relay-at-login)
  gets whatever its starter has. The Windows Run entry has your user
  environment variables. A LaunchAgent / systemd user unit gets launchd's or
  the user manager's environment, which does not include what your shell
  exports. Set the variables there (`launchctl setenv`,
  `systemctl --user set-environment`, or a file in `~/.config/environment.d/`),
  or put them in the unit / plist itself.

After changing them, restart the relay so that it reads them again:
`codex-copilot stop`, then `codex-copilot start`.

### Stable item ids

Each request Codex sends - a `response.create` on the socket, or a
`POST /responses` - gets a fresh UUID. Every item id in the events that answer
it - `item.id` in `output_item.added` / `done`, `item_id` in the delta and part
events, `output[i].id` in the terminal `response.completed`,
`response.incomplete` or `response.failed` - becomes

```text
copilot-<uuid>-<output_index>
```

so all events of one item share one id, and ids never collide across requests.
For the capture above, Codex sees `copilot-01999f6e-5c1a-7b3e-9a41-2f0c7d8e6b15-0`
on every line.

The terminal event's `output[]` carries no `output_index`, so entry `i` gets
the id of index `i`. Codex never reads that array (only the response's `id`
and `usage`); the rewrite just keeps Copilot's ids from showing. An `output[]`
entry Copilot sent without an `id` stays without one.

The UUID changes with every `response.create` on a connection (a
`response.interrupt` does not change it). That is only correct while one
request at a time is in flight per connection, which is how Codex uses the
socket: it holds the socket's stream lock from sending `response.create` until
the terminal event and drops the connection if it gives up earlier
(`codex-api/src/endpoint/responses_websocket.rs`). A request that starts
before the previous one ended is logged at `debug` level; the ids of anything
still streaming for the old request would then land in the new namespace.

The id contains **no underscore**, on purpose. When Codex sends history back
upstream it keeps an item id only if it has a `prefix_suffix` shape (both sides
non-empty) and drops every other id from the request copy
([core/src/client.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/client.rs),
`ResponseItemId::is_prefixed`). Copilot's own base64 ids already fail that
test, so under 1.x Codex never sent them back either. The relay's ids fail it
by design: they never reach Copilot, and upstream requests look exactly as they
did under 1.x.

`response.id` and `call_id` are passed through unchanged.

## Prerequisites

- `codex` on PATH (`npm install -g @openai/codex`), or `--codex-bin <path>`
  (or `CODEX_BIN`). On Windows the npm entry point is a `codex.cmd` /
  `codex.ps1` shim rather than an `.exe`; it is resolved and launched correctly
  without help. `install` only warns when it cannot find Codex: the relay and
  the config do not need it, but Codex has to be there before you run it on
  the dedicated home. Developed against Codex 0.160.
- A GitHub Copilot seat on which the target model is enabled: CAPI
  `GET /models` must list it with `ws:/responses` among its
  `supported_endpoints`. `codex-copilot status --probe` checks.
- Only when building from source: Rust 1.88+ (plus the Visual C++ build tools on
  Windows). The portable executables need neither Rust nor a separate Visual
  C++ Redistributable installation.

## Get the executables

There is one binary, `codex-copilot`. It is the CLI you run and, started
detached by `codex-copilot start`, the relay itself. Put it in a permanent
folder: `start` re-launches the same executable by absolute path, and a login
entry from [Starting the relay at login](#starting-the-relay-at-login) points
at it too.

### Windows portable (no installer)

Download the executable and its `.sha256` file from
[GitHub Releases](https://github.com/ItsAlbertZhang/codex-copilot/releases):
`codex-copilot-<version>-windows-<arch>.exe`. Choose `x64` for Intel/AMD PCs or
`arm64` for Windows on ARM. Verify the download with
`Get-FileHash .\codex-copilot-2.0.0-windows-x64.exe -Algorithm SHA256` and
compare the hash with the one in its `.sha256` file.

Put it in a permanent folder, then rename it, unblock it and run from
PowerShell:

```powershell
Rename-Item .\codex-copilot-2.0.0-windows-x64.exe codex-copilot.exe
Unblock-File .\codex-copilot.exe
.\codex-copilot.exe --help
.\codex-copilot.exe login
# After setting the token as instructed and opening a new terminal:
.\codex-copilot.exe install
.\codex-copilot.exe start
$env:CODEX_HOME = "$HOME\.codex-copilot"; codex
```

`Unblock-File` removes the Mark of the Web the browser put on the download.
The relay is this same executable started with no window to show a security
prompt in, so a still-marked file may be blocked or held at a prompt there.

Optionally add the folder to PATH so `codex-copilot` works from anywhere. The
`install` subcommand configures Codex; it does not install the executable.
Codex CLI and a Copilot seat are still required.

### macOS portable (no installer)

Download the binary and its `.sha256` file from the same releases page.
Choose `macos-arm64` for Apple silicon or `macos-x64` for Intel Macs, then:

```console
$ shasum -a 256 -c codex-copilot-2.0.0-macos-arm64.sha256
$ mv codex-copilot-2.0.0-macos-arm64 codex-copilot
$ chmod +x codex-copilot
$ xattr -d com.apple.quarantine codex-copilot
$ ./codex-copilot --help
```

The binary is unsigned, so Gatekeeper blocks a quarantined first run. Clear
the flag before the first `start`: the relay is the same file launched in the
background, where no Gatekeeper prompt can appear. Put its folder on PATH if
you like.

### Cargo (Windows, macOS, Linux)

Install once from this repository; Cargo places the binary in its bin
directory (normally `~/.cargo/bin`):

```console
cargo install --git https://github.com/ItsAlbertZhang/codex-copilot.git --locked codex-copilot
```

From a local checkout, use `cargo install --path . --locked`. To update, stop
the relay first (`codex-copilot stop`; on Windows a running executable cannot
be overwritten), re-run the git command, then `codex-copilot start` again. This
checkout disables crates.io publishing with `publish = false`; use the git or
local-path command above.

### Build portable Windows executables

From the repository root, using Windows PowerShell or PowerShell 7:

```powershell
# Defaults to the current Rust host architecture:
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\package-windows.ps1
# Or select a target (matching Visual C++ tools must also be installed):
rustup target add x86_64-pc-windows-msvc
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\package-windows.ps1 -Target x86_64-pc-windows-msvc
```

The script links the MSVC runtime statically and writes two files under
`dist/`: `codex-copilot-<version>-windows-<arch>.exe` and a matching
`.exe.sha256` checksum. It checks that the executable was built for the
requested machine. It uses `Cargo.lock` and
keeps its build files under `target/portable/`. For ARM64, use
`aarch64-pc-windows-msvc`. `-ExecutionPolicy Bypass` applies only to this
PowerShell process and does not change the machine's execution policy.

### Build portable macOS binaries

From the repository root on macOS, with the Xcode command line tools and `jq`
installed:

```console
$ ./scripts/package-macos.sh                        # current Rust host target
$ rustup target add x86_64-apple-darwin
$ ./scripts/package-macos.sh x86_64-apple-darwin    # or select a target
```

It writes `dist/codex-copilot-<version>-macos-<arch>` and a matching
`.sha256`, uses `Cargo.lock`, and keeps its build files under `target/portable/`.
The binary is single-architecture; there is no universal build.

The **portable-release** GitHub Actions workflow builds both Windows and both
macOS targets. Run it manually to download the binaries from the workflow's
artifacts, or push a tag matching the Cargo version (for example `v2.0.0`) to
create a draft GitHub Release with every binary and checksum. Review and publish
the draft to make the downloads public.

## Install

```console
$ codex-copilot login

  Open       https://github.com/login/device
  Enter code A1B2-C3D4
  Scope      read:user   (app Ov23ctDVkRmgkPke0Mmm)
  Waiting for approval; Ctrl-C aborts.
  Approved.

GitHub token (GitHub device flow), shown once:

    COPILOT_GITHUB_TOKEN=gho_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

Nothing on disk holds it. Set it as a user environment variable yourself:

    PowerShell   [Environment]::SetEnvironmentVariable("COPILOT_GITHUB_TOKEN", "gho_xxx", "User")
    bash         printf '%s\n' 'export COPILOT_GITHUB_TOKEN=gho_xxx' >> ~/.profile
    zsh          printf '%s\n' 'export COPILOT_GITHUB_TOKEN=gho_xxx' >> ~/.zshrc

Then open a new shell so the variable is in the environment `codex` inherits.
```

A token with characters a shell would interpret (`$`, a quote, a backtick) is
single-quoted in these lines, so pasting them is safe.

Set the variable, open a new shell, then:

```console
$ codex-copilot install          # or: codex-copilot install --token gho_xxx
```

`install` is stateless: it writes files and probes CAPI, and never starts,
stops or probes the relay, nor registers anything for autostart. (A bare
`codex-copilot` is the relay in the foreground, not `install`.) It does, in
order:

1. Resolves the token from `--token`, `--token-stdin`, or
   `COPILOT_GITHUB_TOKEN`. With none of the three it runs the device flow
   inline, exactly as `login` does, unless `--host` was given (step 2). The
   token is used for the gateway probe and not written anywhere. A token given
   with `--token` or `--token-stdin` is not printed either: the set-variable
   one-liners in the next steps carry a placeholder where it goes.
2. Finds your gateway: `api.enterprise` -> `api.business` ->
   `api.individual` -> `api.githubcopilot.com`, first `GET /models` that answers
   200 wins. `--host <url>` names the gateway instead. It is still probed when
   a token is available, but a failed probe only warns; without a token neither
   the device flow nor the probe runs, and the relay forwards whatever token
   Codex sends at runtime. (The hidden `--hosts` option, a comma-separated
   list, or `CODEX_COPILOT_HOSTS` replaces the search list; it exists for
   tests.)
3. Runs `codex --version` against the dedicated home. A missing `codex` is a
   warning, not an error.
4. Creates `~/.codex-copilot` and writes or merges its `config.toml` (managed
   keys only, see below), then writes the state file `codex-copilot.json`.
5. Prints the next steps. Codex cannot connect while the relay is not running:
   `codex-copilot start` starts it, and after changing settings with a later
   `install`, `codex-copilot stop` then `codex-copilot start` makes the running
   relay pick them up. The relay's address, upstream and review model come
   from the state file, so a relay that is already running keeps the old ones
   until restarted. The steps also point at [Starting the relay at
   login](#starting-the-relay-at-login), and when `--listen` changed they name
   the old address: `codex-copilot stop --listen <old>` stops a relay still
   running there.

The arguments are checked before anything is written: `--listen` must be an
IP address with a fixed port (`127.0.0.1:12899`, `[::1]:5000`; a host name such
as `localhost` is refused, because the relay binds one address and Codex's
`base_url` is written from this text) and `--host` (like `--upstream` of the
relay) a bare http(s) origin like `https://api.githubcopilot.com` - a path, a
query, a fragment or credentials are refused, because the relay appends its own
paths to it.

A `codex-copilot.json` that does not parse is not fatal: `install` prints a
WARNING naming the file, goes on, and rewrites it.

Re-running `install` updates everything in place - the managed keys and the
state file - and keeps whatever else is in the home. It does not touch a
running relay. It refuses while an override is active; run `unoverride` first.

| Flag | Default | Effect |
| --- | --- | --- |
| `--token <T>` / `--token-stdin` | `$COPILOT_GITHUB_TOKEN`, else device flow (none with `--host`) | token for the gateway probe; never printed |
| `--host <url>` | probed | use this gateway; still probed when a token is available, and a failure only warns |
| `--listen <IP:PORT>` | `127.0.0.1:12899` | where the relay will listen: an IP address and port, not a host name. Becomes `base_url`, with a wildcard `0.0.0.0` / `[::]` written as `127.0.0.1` / `[::1]` |
| `--model <slug>` | `gpt-6-astra` on a fresh home, else kept | `model`; replaces one already in `config.toml` |
| `--reasoning-effort <level>` | `ultra` on a fresh home, else kept | `model_reasoning_effort`; replaces one already in `config.toml` |
| `--review-model <slug>` | `gpt-6-luna` | what the relay sends instead of `codex-auto-review`; `""` turns the substitution off |
| `--no-yolo` | | keep approvals and the sandbox, enable automatic review (see below) |
| `--dry-run` | | print what would be done, including the full `config.toml` and state file; write nothing |

By default the config sets `approval_policy = "never"` and
`default_permissions = ":danger-full-access"`, equivalent to Codex's `--yolo`:
Codex does not prompt for approvals, and commands run without a sandbox. Pass
`--no-yolo` to leave both to Codex's defaults (with one exception for a
`config.toml` that defines `[permissions]` profiles, see
[below](#the-managed-configtoml)).

## What it writes

| Path | What |
| --- | --- |
| `~/.codex-copilot/config.toml` | the managed Codex config (below); Codex writes to it too. A symlinked `config.toml` stays a link; the file it points at is updated |
| `~/.codex-copilot/codex-copilot.json` | state: tool version, install time, listen address, upstream, review model, yolo, codex version - no secrets |
| relay log | written only by a relay started with `codex-copilot start`: Windows `%LOCALAPPDATA%\codex-copilot\logs\relay.log`, macOS `~/Library/Logs/codex-copilot/relay.log`, Linux `$XDG_STATE_HOME/codex-copilot/logs/relay.log` (`~/.local/state/...` by default) |

The state file marks its directory as the dedicated home and travels with it
through `override`; that is how the tool tells the two directories apart.
`install` never touches `~/.codex`, the credential store, or your environment
variables, and registers nothing with the OS: no Run key, LaunchAgent or
systemd unit (see [Starting the relay at login](#starting-the-relay-at-login)
if you want one).

## The managed config.toml

A fresh install writes exactly this (defaults shown):

```toml
# codex-copilot 2.0.0 - managed keys for running Codex against GitHub
# Copilot CAPI through the local relay. `codex-copilot install` rewrites the
# managed keys and leaves everything else in this file alone (Codex writes
# project trust, hook state and model choices back into it).

model = "gpt-6-astra"
model_reasoning_effort = "ultra"
# Codex clamps this to each model's max_context_window: "use the maximum".
model_context_window = 1000000
model_provider = "copilot"
check_for_update_on_startup = false
# Equivalent to codex --yolo. `install --no-yolo` replaces these two keys with
# approvals_reviewer = "auto_review" (the relay maps the reviewer model).
approval_policy = "never"
default_permissions = ":danger-full-access"

[windows]
sandbox = "unelevated"

[model_providers.copilot]
# "OpenAI" is the name Codex keys remote compaction and web search on. The
# relay, not Codex, is what talks to Copilot.
name = "OpenAI"
base_url = "http://127.0.0.1:12899"
wire_api = "responses"
env_key = "COPILOT_GITHUB_TOKEN"
supports_websockets = true

[analytics]
enabled = false

[otel]
exporter = "none"
trace_exporter = "none"
metrics_exporter = "none"

[feedback]
enabled = false

[shell_environment_policy]
exclude = ["COPILOT_GITHUB_TOKEN"]
```

| Key | Why |
| --- | --- |
| `model`, `model_reasoning_effort` | the defaults, written only when absent; `--model`, `--reasoning-effort` replace them |
| `model_context_window = 1000000` | "the model's maximum": Codex clamps it per model, see [Context window](#context-window). Written only when absent |
| `model_provider = "copilot"` | selects the provider table below |
| `check_for_update_on_startup = false` | no GitHub release check or upgrade prompt at startup in this home; upgrade Codex when you choose to |
| `approval_policy`, `default_permissions` | yolo; replaced by `approvals_reviewer` with `--no-yolo` (`default_permissions` may stay as `":workspace"`, see below) |
| `[windows] sandbox = "unelevated"` | Codex's Windows sandbox in its restricted-token form, which needs no administrator setup. Inert under yolo (nothing is sandboxed); in effect with `--no-yolo`. Ignored on macOS and Linux. Written only when absent |
| `name = "OpenAI"` | Codex keys OpenAI-only behaviour off this literal, remote compaction v2 among it |
| `base_url` | the relay; from `--listen`, with a wildcard `0.0.0.0` / `[::]` replaced by `127.0.0.1` / `[::1]`, which Codex can connect to |
| `wire_api = "responses"`, `supports_websockets = true` | the Responses API over WebSocket, Codex's preferred transport; the relay also serves the [HTTP SSE fallback](#the-http-sse-fallback) |
| `env_key` | Codex reads the bearer from this variable at request time and sends it as `Authorization: Bearer` |
| `[analytics]`, `[otel]`, `[feedback]` | telemetry off. This matters here: with `name = "OpenAI"` an enabled exporter would POST to an openai.com host while the process holds Copilot credentials |
| `[shell_environment_policy] exclude` | keeps the token out of commands Codex runs, unless a project config overrides `shell_environment_policy` (see [Security](#security)) |

A fresh `--no-yolo` install writes `approvals_reviewer = "auto_review"` in
place of `approval_policy` and `default_permissions`. On an existing file
`default_permissions` can survive `--no-yolo`, as the list below explains.

These keys are the **managed** ones. A re-run of `install` treats them like
this:

- `model`, `model_reasoning_effort`, `model_context_window` and
  `[windows] sandbox` are written only when absent and kept across reinstalls,
  so a `/model` choice survives. `--model` and `--reasoning-effort` replace the
  first two when given.
- The other managed keys are rewritten every time: `model_provider`,
  `check_for_update_on_startup`, the approval keys of the current mode,
  `[analytics]`, `[otel]`, `[feedback]`, and `[model_providers.copilot]`,
  which is replaced whole so a stale key (1.x `http_headers`) cannot survive
  next to the relay's `base_url`. Other providers are untouched. A hand edit to
  one of these keys lasts until the next `install`; use the matching flag
  instead.
- Switching between yolo and `--no-yolo` removes `approval_policy`,
  `default_permissions` or `approvals_reviewer` only while it still holds the
  value this tool writes (`"never"`, `":danger-full-access"`,
  `"auto_review"`). A value of your own survives `--no-yolo`, e.g.
  `default_permissions = "dev"` with a `[permissions.dev]` profile.
- One exception: Codex refuses to load a config whose `[permissions]` table
  defines profiles unless `default_permissions` selects one (or a root
  `sandbox_mode` selects the legacy settings). So when `--no-yolo` finds
  `default_permissions = ":danger-full-access"` next to a non-empty
  `[permissions]` table and no `sandbox_mode`, it sets it to `":workspace"`
  instead of removing it, with this comment above it:

  ```toml
  # Codex refuses to load [permissions] profiles unless one is selected here.
  default_permissions = ":workspace"
  ```

  A later install without `--no-yolo` writes `":danger-full-access"` back and
  drops that comment.
- `[shell_environment_policy]`: the token is added to an existing `exclude`
  list, and your entries stay. A table that uses the keyed `filters` form
  instead gets `COPILOT_GITHUB_TOKEN = "exclude"` in `filters`, because Codex
  refuses a table that mixes the two forms. A file that already mixes them is
  refused, and nothing is written.
- `model_catalog_json`, which 1.x wrote, is removed: it names a catalog file
  2.0 does not write.

Everything else stays: keys Codex writes back (trusted projects, MCP servers,
notices), your own additions, and comments, except the comment lines directly
above a key that is removed, which go with it. An unchanged file is not
rewritten, and a merge that would lose a managed key writes nothing.

## Using it

The whole path is three steps. `install` writes the configuration, `start`
brings the relay up, and Codex then runs on the dedicated home:

```console
$ codex-copilot install
$ codex-copilot start
$ CODEX_HOME=~/.codex-copilot codex
```

The relay does not survive a reboot. Run `codex-copilot start` again, or set up
[Starting the relay at login](#starting-the-relay-at-login). `codex-copilot
stop` stops it.

The dedicated home is a **completely separate Codex identity**. Sessions,
`auth.json`, skills, memories, MCP configuration and project trust all live in
whichever directory Codex is pointed at; nothing is shared with `~/.codex` and
nothing is copied over. `codex resume` lists only the active home's sessions.
Copy skills or MCP entries across yourself if you want them in both.

Point a single Codex process at it with `CODEX_HOME`:

```console
$ CODEX_HOME=~/.codex-copilot codex
$ CODEX_HOME=~/.codex-copilot codex exec "..."
```

```powershell
$env:CODEX_HOME = "$HOME\.codex-copilot"   # for the rest of this PowerShell session
codex
```

Clients that cannot be given a home - the desktop app, an App Server started by
something else, editor integrations - need `override`.

`codex-copilot status` re-checks the chain. It starts with a header (the home,
what is installed, and `Relay log <path>`, the file a relay started with
`codex-copilot start` writes), then prints one line per check, each `ok`,
`WARN` or `FAIL`, then what to do about anything that is not ok, and exits
non-zero when a check fails (a `WARN` alone does not). The first check is the
relay, looked for on the installed address, else the default: when it runs, its
version, URL, pid, upstream and proxy; when it does not, `not running on
<addr>` and the fix, `codex-copilot start`:

| Check | FAIL | WARN |
| --- | --- | --- |
| `relay` (first check) | nothing on the relay's port while the tool is installed, or something there that is not a codex-copilot relay (or does not answer) | nothing on the port while the tool is not installed (`state` fails then); the running relay has another version, upstream or review model than installed - `codex-copilot stop`, then `codex-copilot start` |
| `state` | not installed, or `codex-copilot.json` unreadable | written by another codex-copilot version |
| `config` | `config.toml` does not parse, or `model_provider`, `base_url` or `supports_websockets` do not route Codex through the relay | |
| `token` | | `COPILOT_GITHUB_TOKEN` is not visible in this shell; Codex needs it in its own environment |
| `codex` | | not found |
| `proxy` (`--probe`, when the relay runs) | | the running relay uses another proxy (or none) than a relay started from this shell would (its proxy variables, else the system proxy settings). The line says which proxy the relay uses |
| `capi` (`--probe`) | `GET /models` on the upstream fails, or the token is missing | |
| `model` (`--probe`) | the configured model is not served or not enabled | the model lacks `ws:/responses`: it works, but sessions start slower because Codex retries the WebSocket before it falls back to HTTP SSE |
| `review` (`--probe`) | | the review model is not served, not enabled, or lacks `ws:/responses` |

The `token` check is only a warning because the shell you run `status` in is
not where Codex runs: a desktop client or a new terminal may well have the
variable this one lacks. With nothing else wrong, `status` then exits 0.
`status --probe` needs the token to call CAPI, so there a missing token is a
`capi` FAIL.

There are no autostart checks: the tool registers nothing, so it has nothing to
verify there.

A running relay's `relay` line shows the URL Codex connects to, so a relay
listening on `0.0.0.0:<port>` shows as `http://127.0.0.1:<port>`.

`status --probe` costs nothing: `GET /models` is not billed. The real
end-to-end check is to start Codex on the dedicated home and send one short
message.

## Override

`override` makes the dedicated home the **default** one by swapping directory
names:

| | `~/.codex` | `~/.codex-copilot` |
| --- | --- | --- |
| normal | your regular Codex home | the dedicated home |
| overridden | the dedicated home | your regular Codex home |

It takes three renames through a temporary name, or one rename when `~/.codex`
does not exist. `unoverride` swaps back. Nothing inside either directory is
edited, so there is no backup to keep and nothing to merge.

```console
$ codex-copilot override
$ codex-copilot unoverride
```

While overridden, everything that uses the default home - plain `codex`,
`codex app-server`, the desktop app, editor integrations - runs on Copilot
through the relay, with the dedicated home's sessions, trust and settings.
`CODEX_HOME=~/.codex-copilot` now names your **regular** home. `install` and
`uninstall` refuse until you `unoverride`.

After a successful `override` the command prints that every Codex entry point
now goes through the relay, so it should start at login, and points at the
per-platform recipes in [Starting the relay at
login](#starting-the-relay-at-login). Until the relay runs
(`codex-copilot start`), plain `codex`, the desktop app and editor
integrations cannot connect at all.

Both commands **refuse while any codex process is running**. Windows cannot
rename a directory that has open files, and a running Codex (including the App
Server behind the desktop app or an editor) would keep writing into the directory
it opened, which after the swap belongs to the other identity. Close every
client first, then restart them afterward so they pick up the swapped home.
`--force` skips the check; the rename can then still fail on Windows, and on
macOS and Linux a running Codex ends up writing into the wrong directory. If a
rename fails although no codex runs, something else holds a file in one of the
directories (a shell whose current directory is inside it, an editor); close it
and retry.

The check (also run by `uninstall --purge`) looks for processes whose image is
`codex` or `codex.exe`, a release asset name `codex-<triple>[.exe]`, the
standalone app server `codex-app-server[.exe]` or
`codex-app-server-<triple>[.exe]`, or any of those cut to the 15 characters
Linux `ps` shows. Names match case-insensitively on every platform. It never
matches this tool's own binaries (`codex-copilot*`), nor helpers Codex starts
for itself such as `codex-code-mode-host`. The refusal lists what it found by
image name and pid, e.g. `codex-x86_64-pc-windows-msvc.exe (pid 4242)`.

Desktop clients must also inherit `COPILOT_GITHUB_TOKEN`: set it as a user
environment variable, then start the app.

## Running the relay

The relay is `codex-copilot` itself. The commands that run it deal with
processes and the port only; they never write `config.toml` or the state file.
An option given on the command line wins. The others come from the state file
when the tool is installed (also while an `override` is active), and from the
built-in defaults otherwise: `127.0.0.1:12899`,
`https://api.enterprise.githubcopilot.com` and `gpt-6-luna`. A state file that
cannot be read is reported as a warning, and the defaults apply. `--listen` is
`IP:PORT`; a `host:port` name is resolved and the relay binds its first
address, unlike `install --listen`, which insists on an IP literal because
Codex's `base_url` is written from it.

| Command | Does |
| --- | --- |
| `codex-copilot` | runs the relay in the foreground, logging to stderr, and prints `listening on http://<addr>` on stdout once it is bound (`--listen 127.0.0.1:0` picks a free port, and the line shows it). Ctrl-C, SIGTERM or `POST /shutdown` stop it with exit 0. It refuses (non-zero exit) when a relay already answers on the address, and a port held by something else is a bind error. Options `--listen`, `--upstream`, `--review-model` |
| `codex-copilot start [--no-wait]` | needs a fixed port. Refuses (non-zero exit) when a relay already answers on the address, naming its version and pid, or when something that is not a relay holds the port. Otherwise it spawns the same executable detached (no window on Windows; on Unix its own process group, stdio on `/dev/null`), waits up to 10 s until `/healthz` answers, and prints `Relay  <version> running on http://<addr> (pid N)` and `Log  <path>`. If the new process exits first, it fails at once and names the log. Takes the same options. `--no-wait` returns right after spawning (`Relay  starting on ...`); it is meant for a login-time entry |
| `codex-copilot stop [--listen IP:PORT]` | sends `POST /shutdown` to the address (`--listen`, else the state file's, else the default), then waits until a TCP connect to the port is refused (about 10 s), and prints `Relay  stopped <version> (pid N) on <addr>`. When nothing was running it prints `Relay  not running on <addr>` and exits 0. It fails, naming the pid, if the port still accepts connections after the wait, and leaves a listener that is not a relay alone, with an error |

The relay options belong to the bare invocation and to `start` (`stop` takes
`--listen` only). Placed before another subcommand, as in `codex-copilot
--listen 127.0.0.1:5000 status`, they are refused instead of silently ignored.

A relay started with `start` logs to a file instead of a terminal:

| OS | Log |
| --- | --- |
| Windows | `%LOCALAPPDATA%\codex-copilot\logs\relay.log` |
| macOS | `~/Library/Logs/codex-copilot/relay.log` |
| Linux | `$XDG_STATE_HOME/codex-copilot/logs/relay.log`, by default under `~/.local/state` |

`CODEX_COPILOT_LOG_DIR` replaces the directory (a test hook) and
`CODEX_COPILOT_LOG` sets the log level (`error`, `warn`, `info` by default,
`debug`, `trace`). The relay `start` spawns inherits both from the shell that
ran `start`, which also prints the log path from them, and `status` prints it
as `Relay log`. The bare foreground relay honours `CODEX_COPILOT_LOG` too, on
stderr, and ignores `CODEX_COPILOT_LOG_DIR`.

When a background relay starts and `relay.log` is larger than 5 MiB, the file
is moved to `relay.log.1` (replacing an older one) and a fresh one begins. That
happens only when nothing listens on the port yet: a relay that already answers
there is still writing to the file, and on Windows so may be whatever holds the
port without answering (Unix renames the open file in that case anyway).
Nothing rotates while a relay runs.

`start` runs the executable as `codex-copilot --background --listen ...
--upstream ... --review-model ...`: a hidden flag, with every option spelled out
so that this process never reads the state file. If a healthy relay already
answers on that address it logs so and exits 0; any other failure, a bad
command line included, is written to the log and exits non-zero.

- **Nothing supervises the relay.** There is no autostart code and no restart
  after a crash. After a reboot, run `codex-copilot start` or set up one of the
  recipes in [Starting the relay at login](#starting-the-relay-at-login).
- `status` shows whether a relay answers, with its version, pid and proxy;
  `codex-copilot stop` ends it, whichever way it was started.
- The relay holds no token and reads no `config.toml`: its address, upstream
  and review model are the options above, and the ones not given come from the
  state file, so Codex's `base_url` and the relay agree after `install`.
- **Changing settings.** `install` writes files only. After `install --listen`,
  `--host` or `--review-model`, or after changing the proxy variables (see
  [Proxies](#proxies)), run `codex-copilot stop` and then `codex-copilot start`
  so the running relay reads the new values. If your Copilot plan (and so the
  gateway) changes, re-run `install` and restart the same way.
- **Upgrading the binary.** `codex-copilot stop`, replace the file, then
  `codex-copilot start`. On Windows the running executable cannot be
  overwritten, which is why `stop` comes first; `status` shows the version of
  the relay that is running.
- A port held by something that is not a codex-copilot relay is left alone:
  `start` refuses at once, the bare relay cannot bind it, and `stop` fails
  without touching it. Free the port, or pick another with
  `install --listen 127.0.0.1:<port>` and restart.
- Windows: `start` asks for the relay to leave the job object of the terminal
  that ran it, so that it outlives the terminal. Some terminals, IDEs and CI
  runners forbid that; `start` then prints a WARNING that the relay stays in
  that job and may be stopped when the terminal closes. Run `start` from a
  shell outside it (a new window from the Start menu) to keep the relay up.
- To watch the relay, run bare `codex-copilot` in a terminal instead of
  `start`: the same relay in the foreground, logging to stderr. It also
  refuses to run a second relay on an address where one already answers.

## Starting the relay at login

The tool itself registers nothing: no Run key, no LaunchAgent, no systemd unit.
If you want the relay up after you log in, add one of these yourself. Each
points at the **absolute path** of the binary, so replace the path in the
recipe with yours (and redo it if you move the file).

A manual `codex-copilot start` while the login-time relay is running is
harmless: it names the relay that runs, exits non-zero and starts no other.
`codex-copilot stop` stops either kind.

### Windows

The Run entry starts the launcher, which spawns the detached relay and exits.
In `cmd.exe`:

```bat
reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v codex-copilot /t REG_SZ /d "\"C:\path\to\codex-copilot.exe\" start --no-wait" /f
```

The same from PowerShell, which does not mangle the inner quotes:

```powershell
New-ItemProperty -Path HKCU:\Software\Microsoft\Windows\CurrentVersion\Run -Name codex-copilot -Value '"C:\path\to\codex-copilot.exe" start --no-wait' -Force
```

To remove it:

```bat
reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v codex-copilot /f
```

`codex-copilot.exe` is a console program, so its launcher window flashes
briefly at logon. The relay it starts has no window. The entry gets your user
environment variables, so proxy variables set there reach the relay. An entry
turned off in Task Manager > Startup apps stays off until you turn it back on
there.

### macOS

A LaunchAgent that runs the bare, foreground form, so launchd supervises the
relay itself. Save it as `~/Library/LaunchAgents/io.github.codex-copilot.plist`,
replacing `/absolute/path/to/codex-copilot` and `/Users/you`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>io.github.codex-copilot</string>
  <key>ProgramArguments</key>
  <array>
    <string>/absolute/path/to/codex-copilot</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardErrorPath</key>
  <string>/Users/you/Library/Logs/codex-copilot/launchd.log</string>
</dict>
</plist>
```

Load it (and create the log directory first), then to remove it, unload it and
delete the file:

```console
$ mkdir -p ~/Library/Logs/codex-copilot
$ launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.codex-copilot.plist
$ launchctl bootout gui/$(id -u)/io.github.codex-copilot
$ rm ~/Library/LaunchAgents/io.github.codex-copilot.plist
```

launchd starts the agent with its own environment, not your shell's. Proxy
variables go into the plist under `EnvironmentVariables`, or into
`launchctl setenv`. Add `KeepAlive` with `SuccessfulExit` set to `false` if you
want launchd to restart the relay after a crash.

### Linux

A systemd user unit that runs the bare, foreground form. Save it as
`~/.config/systemd/user/codex-copilot.service`, replacing the path:

```ini
[Unit]
Description=codex-copilot relay

[Service]
ExecStart=/absolute/path/to/codex-copilot
Restart=on-failure

[Install]
WantedBy=default.target
```

```console
$ systemctl --user enable --now codex-copilot
$ systemctl --user disable --now codex-copilot
$ rm ~/.config/systemd/user/codex-copilot.service
```

The last two commands remove it. `Restart=on-failure` is
optional; a clean `codex-copilot stop` exits 0, so it is not restarted. The
unit runs in the user manager's environment (`systemctl --user
set-environment`, or `Environment=` lines in the unit). To start it at boot
rather than at your first login, run `loginctl enable-linger`.

## Approval review (`--no-yolo`)

Under the default yolo settings Codex never asks for approval, so the reviewer
is never called. `install --no-yolo` changes two things:

- `approval_policy` and `default_permissions` are omitted, so Codex's defaults
  apply: approval on request, the `:workspace` profile for trusted projects
  (on Windows with the sandbox configured above), `:read-only` otherwise. On a
  `config.toml` with `[permissions]` profiles of your own, the yolo
  `default_permissions` becomes `":workspace"` instead (see
  [The managed config.toml](#the-managed-configtoml)).
- `approvals_reviewer = "auto_review"` hands approval requests to Codex's
  automatic reviewer instead of to you.

Codex picks the reviewer model from the active model's catalog entry
(`auto_review_model_override`) and otherwise uses `codex-auto-review`, which
CAPI does not serve. The review request goes to the same provider, so through
the relay, with `"model": "codex-auto-review"` in its body and an
`x-openai-subagent: guardian` header. The relay rewrites that model to the
review model: `gpt-6-luna` unless `install --review-model <slug>` chose another.
`--review-model ""` turns the rewrite off; under `--no-yolo` reviews then fail,
and `install` warns about it. 1.x did the same by writing
`auto_review_model_override` into every catalog entry; with no catalog, the
relay does it. Reviewer inference consumes additional CAPI usage.

## Context window

Codex sizes a model's window from its **built-in catalog entry**: a
`context_window` (272k for the `gpt-6-*` and `gpt-5.6-*` entries) and a
`max_context_window` (872k for those, 272k for `gpt-5.5`). The global
`model_context_window` key replaces the first and is clamped to the second, so
`1000000` means "the model's maximum":

| model | window | auto-compaction (90%) |
| --- | --- | --- |
| `gpt-6-*`, `gpt-5.6-*` | 872,000 | 784,800 |
| `gpt-5.5` | 272,000 | 244,800 |

Without the key every one of them would get 272k. Auto-compaction fires at 90%
of the window (Codex clamps anything higher); the hard cap on what is offered to
inference is 95% (`effective_context_window_percent`).

1.x calibrated each model to what **this seat's** CAPI actually accepts, by
downloading the `models.json` matching your `codex --version` and rewriting it
into a local catalog. 2.0 drops that on purpose: no per-version catalog
download and no recalibration after every Codex update. The price is whatever CAPI accepts above Codex's built-in maximum - for example
about 178k of window on `gpt-6-astra` on the seat this was developed on.

## Billing

- Prompts above a model's **base tier** (272k) fall into CAPI's `long_context`
  price tier, roughly **2x** per token. A long `gpt-6-astra` session gets there
  before it compacts at 784,800. To stay in the standard tier, add
  `model_auto_compact_token_limit = 244800` to `~/.codex-copilot/config.toml`
  yourself: Codex compacts at the lower of it and 90% of the window, and it is
  not a managed key, so `install` keeps it.
- Codex's startup **prewarm is a real, billed request**. It is also what warms
  the WebSocket the whole session then rides, so shortening
  `websocket_connect_timeout_ms` throws away a request you already paid for.
- Reviewer inference consumes additional CAPI usage.
- `install` and `status` cost nothing: `GET /models` is not a billed endpoint.

## Security

- The default yolo settings allow commands to run without approval prompts or
  a sandbox. Use `install --no-yolo` to keep Codex's approval and sandbox
  defaults.
- The token lives in one place: a **user environment variable you set**. Codex
  reads it at request time and sends it to the relay as `Authorization: Bearer`
  over loopback; the relay forwards it to CAPI over TLS on the same connection
  and keeps nothing. It is never written to disk and never logged, and the
  relay does not need the variable at all. `config.toml` stores only the
  variable's name, and `status` reports only whether its own shell has it.
  `install` does not print a token given with `--token` or `--token-stdin`.
- `[shell_environment_policy] exclude = ["COPILOT_GITHUB_TOKEN"]` keeps the
  token out of commands Codex runs unless a project config overrides
  `shell_environment_policy`. Codex merges config layers by replacing arrays
  wholesale, so a trusted project's `.codex/config.toml` with an `exclude` list
  or a `filters` table of its own drops this entry. `ignore_default_excludes`
  defaults to `true` in Codex, so this entry is the only filter in effect.
- Telemetry is forced off (`[analytics]`, `[otel]`, `[feedback]`), because the
  provider is named `OpenAI`.
- The relay listens on **loopback only** by default and has no authentication
  of its own: whatever reaches the port can use it with its own Copilot token
  (the relay adds identity headers, never credentials), and can stop it with
  `POST /shutdown`. `install` and the relay warn about a non-loopback
  `--listen`, and `install`'s warning names `POST /shutdown` too. Any loopback
  address counts as loopback, IPv4-mapped `[::ffff:127.0.0.1]` included. Keep
  it on `127.0.0.1`.
- `codex-copilot.json` holds no secrets.

## Troubleshooting

- **First question: is the relay running?** Run `codex-copilot status`; its
  `relay` line says. **Codex cannot connect to `127.0.0.1:12899`** (or the
  address in your `base_url`) when it is not running. `codex-copilot start`
  starts it; the log file (`relay.log`, see [Running the relay](#running-the-relay))
  says why it stopped, or run bare `codex-copilot` in a terminal to watch it.
  The relay does not come back by itself after a reboot or a crash; see
  [Starting the relay at login](#starting-the-relay-at-login).
- **`start` says a relay already runs on the address** (and exits non-zero):
  that is the relay you want; the message and `status` show its version and
  pid. After changing settings, `codex-copilot stop` and then
  `codex-copilot start` to pick them up.
- **`start` says the relay did not start**: the new process exited before it
  answered, or never answered within 10 s. The message names its log, which
  says why (a port taken in the meantime, a SOCKS proxy the upstream would go
  through). `start` fails before spawning anything when `--listen` has no fixed
  port or an option does not validate.
- **`stop` fails with a pid**: the port kept accepting connections for the
  whole wait after `POST /shutdown`. Look at the relay's log, or end that
  process yourself.
- **`start` (Windows) warns that the relay stays in the terminal's job
  object**: the terminal forbids breaking away, so the relay may be stopped
  when the terminal closes. Run `codex-copilot start` from a shell outside it
  (a new window from the Start menu); see [Running the
  relay](#running-the-relay).
- **Codex prints "Falling back from WebSockets to HTTPS transport"**: the
  WebSocket failed several times in a row (network drop, relay restart,
  upstream errors), and Codex switched this session to HTTP SSE. The relay
  serves that transport too, with the same id rewrite. Codex goes back to the
  WebSocket when it is restarted.
- **502 "could not open the upstream WebSocket"**: the relay could not reach
  CAPI. The message ends with how it tried: through which proxy, or with no
  proxy (none of its proxy variables or system proxy settings applied to
  CAPI when it started). If your network needs a proxy the relay did not
  use, set the variables where the relay gets its environment, then
  `codex-copilot stop` and `codex-copilot start` (see [Proxies](#proxies));
  `status --probe` shows the proxy the running relay uses. A
  `proxy authorization required` in the message means the proxy wants
  credentials: put them in the proxy URL, `http://user:password@proxy:3128`.
- **The relay does not start and its log says the upstream would go through
  a SOCKS proxy**: the relay supports `http://` and `https://` proxies only.
  Point the variable at the proxy's HTTP address, exclude CAPI with
  `NO_PROXY`, or remove the variable from the relay's environment.
- **401 from upstream**: Codex did not have `COPILOT_GITHUB_TOKEN`, or the token
  expired. Set it as a user variable, open a new shell (restart desktop
  clients), or get a new one with `login`.
- **The `/model` picker shows a red "OpenAI base URL is overridden" line**:
  cosmetic. Codex prints it for any provider named `OpenAI` with a custom
  `base_url`.
- **Duplicated or stuck messages**: the relay is not in the path. Check which
  home Codex is using (`CODEX_HOME`, `override` state in `status`) and that its
  `config.toml` has `base_url = "http://127.0.0.1:12899"` - a leftover 1.x
  configuration points straight at `api.*.githubcopilot.com`.
- **`override` refuses**: a Codex process is running; the message lists each
  one by image name and pid (see [Override](#override) for the names it
  looks for). Close the desktop app, editor integrations and terminals
  running Codex, then retry.
- **Every command says the two homes are the same directory**: `~/.codex` is a
  symlink or junction to `~/.codex-copilot`, or the reverse. Every command but
  `login` checks this first and refuses, even before anything is installed.
  Replace the link with a real directory; `override` is how the dedicated home
  becomes the default.
- **`install` warns that `codex-copilot.json` is corrupt**: it goes on and
  rewrites the file. `install` never touches the relay, so a relay that is
  still running keeps its old address. A plain `codex-copilot stop` looks on
  the address now in the state file, so stop that relay with
  `codex-copilot stop --listen <its address>` (or `POST
  http://<its address>/shutdown`), then `start`.
- **Port already in use**: something that is not a codex-copilot relay holds
  the port; `start` says so and starts nothing, and the bare relay cannot bind
  it. Pick another with `install --listen 127.0.0.1:<port>`, then
  `codex-copilot start`.
- **`install --listen localhost:<port>` is refused**: `install --listen` takes
  an IP address, so use `127.0.0.1:<port>` or `[::1]:<port>`.

## Upgrading from 1.x

1.x wrote a profile overlay into `~/.codex`; 2.0 never reads or writes
`~/.codex/config.toml`. Clean up with the **1.x** binary before replacing it:

```console
$ codex-copilot unoverride       # 1.x, if an override is active
$ codex-copilot uninstall        # 1.x
```

That restores `~/.codex/config.toml` and removes `copilot.config.toml` and
`copilot_config_toml/` (the calibrated `models-catalog.json` and `state.json`).
If the 1.x binary is already gone, those files are inert and can be deleted by
hand. An active 1.x override is not: its `config.toml` still points straight at
CAPI. Restore it from `~/.codex/codex-copilot.override-backup.json` (the
`original` field is the file it replaced, `null` if there was none), or run
`unoverride` with the 1.x release once.

`--profile copilot` no longer exists. Sessions started under the 1.x profile
live in `~/.codex` and stay there. 2.0 needs nothing from `~/.codex`, so a
fresh Codex install is fine.

Within 2.x, upgrade the binary with `codex-copilot stop`, replace the file,
then `codex-copilot start`. Nothing needs re-registering, because the tool
registers nothing; only an entry you added yourself (see [Starting the relay at
login](#starting-the-relay-at-login)) needs a new path if the binary moved.

## Uninstall

```console
$ codex-copilot unoverride       # first, if an override is active
$ codex-copilot uninstall
```

Removes files only: the state file, and with `--purge` the whole directory. It
prints the one-liner to clear `COPILOT_GITHUB_TOKEN` yourself. It does not stop
the relay: when one still answers on the installed address (else the default)
it prints that it runs, with its version and pid, and the command that stops
it: `codex-copilot stop --listen <its address>`, spelled out because once the
state file is gone a plain `stop` only looks at the default
`127.0.0.1:12899`. Remove any login-time entry you added
yourself (see [Starting the relay at login](#starting-the-relay-at-login)).
`~/.codex-copilot` and everything Codex keeps in it stay; its `config.toml` then points at a relay that may no longer
run. `uninstall --purge` deletes `~/.codex-copilot` as well, sessions included;
like `override`, it refuses while a codex process runs (`--force` skips the
check). Both refuse while an override is active. The token variable is yours;
clear it only when you no longer need it. To stop the relay for a while without
uninstalling, use `codex-copilot stop`.

`uninstall` also cleans up after a partial or broken install. Without a state
file, `--purge` deletes the directory only if its
`config.toml` has the whole provider table `install` writes:
`[model_providers.copilot]` with `name = "OpenAI"`, `wire_api = "responses"`,
`env_key = "COPILOT_GITHUB_TOKEN"`, `supports_websockets = true` and a
`base_url` of `http://<loopback IP>:<port>`. Otherwise it prints "Not deleting
..." and leaves the directory alone, so a directory that merely has the name,
or a provider table of your own pointing at a proxy on `127.0.0.1`, is never
deleted.

## Platform support

Developed on Windows 11. macOS and Linux are supported by the code
(`~/.profile` or `~/.zshrc` instead of the registry for the variable, its own
process group instead of a hidden window for the background relay) but are
built and tested only in CI - the matrix runs `fmt`, `clippy -D warnings`,
`build --release` and the full test suite on `ubuntu-latest`, `macos-latest`
and `windows-latest`. The login-time recipes are untested snippets. The relay's
tests run it against an in-process fake Copilot gateway that rotates ids the
way CAPI does, over both transports; the check against the real CAPI is the one
under [Using it](#using-it).

## Commands

Stateless (files only; never starts or stops a process):

| Command | Does |
| --- | --- |
| `install` | resolves the token, probes the gateway, writes `config.toml` and the state file in the dedicated home, detects the codex version. Does not start, stop or probe the relay, and registers no autostart. `--listen` takes an `IP:PORT`, never a host name |
| `login` | device flow, prints the token and the set-variable one-liners; never stores it |
| `status [--probe]` | a header with the `Relay log` path, then the relay first (running: version, URL, pid, upstream, proxy; else `codex-copilot start`), then state, config, token variable and codex version, each `ok` / `WARN` / `FAIL`; exits non-zero on a FAIL. `--probe` also prints the proxy the running relay uses, and checks CAPI `GET /models` and the configured models |
| `override [--force]` | swaps `~/.codex` and `~/.codex-copilot` (refused while Codex runs; the refusal names the processes) and reminds you that every Codex entry point now needs the relay |
| `unoverride [--force]` | swaps them back |
| `uninstall [--purge [--force]]` | removes the state file; `--purge` deletes `~/.codex-copilot` (refused while codex runs, unless `--force`). Refused while overridden. Prints a hint to run `codex-copilot stop` when a relay still answers on the installed (else the default) address; does not stop it |

Stateful (processes and the port only; defaults come from the state file):

| Command | Does |
| --- | --- |
| `codex-copilot` (no subcommand) | the relay in the foreground, logging to stderr; prints `listening on http://<addr>` on stdout; Ctrl-C / SIGTERM stops it. `--listen`, `--upstream`, `--review-model`; refuses if a relay already answers |
| `start [--no-wait]` | starts the relay detached and waits for `/healthz`; refuses (non-zero exit) if one already answers or the port is taken. Same options as above; `--no-wait` returns right after spawning, for a login-time entry |
| `stop [--listen IP:PORT]` | `POST /shutdown`, then waits until the port refuses connections (about 10 s); exits 0 when nothing was running |

Global: `--codex-bin <path>` (or `CODEX_BIN`). The relay options (`--listen`,
`--upstream`, `--review-model`) go with the bare invocation and `start`, never
before another subcommand.

| Variable | Effect |
| --- | --- |
| `COPILOT_GITHUB_TOKEN` | the bearer Codex sends; you set it. `install` and `status` read it (`status` only warns when its shell lacks it) |
| `CODEX_BIN` | same as `--codex-bin` |
| `CODEX_COPILOT_LOG` | log level of the relay, foreground or started by `start`: `error`, `warn`, `info` (default), `debug`, `trace` |
| `CODEX_COPILOT_LOG_DIR` | directory for `relay.log` (and `status`'s `Relay log` line) instead of the per-user one; a test hook, inherited by a relay `start` spawns; the foreground relay logs to stderr and ignores it |
| `CODEX_COPILOT_HOSTS` | same as the hidden `install --hosts`: a comma-separated gateway search list (testing) |
| `CODEX_COPILOT_HOME_DIR` | same as the hidden `--home-dir`: the directory holding `.codex` and `.codex-copilot`, instead of your home directory (testing) |
