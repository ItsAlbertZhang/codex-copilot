# codex-copilot

Points an installed **OpenAI Codex CLI** at the **GitHub Copilot CAPI** over the
stateful `ws:/responses` transport, with native remote compaction v2, and
recalibrates the bundled model catalog so every model on your seat budgets
against its own real CAPI context window.

It is deliberately **not a proxy**. Nothing sits between Codex and CAPI at
runtime: Codex opens the WebSocket to `api.*.githubcopilot.com` itself, sends
the bearer itself, and keeps the connection-bound state (`previous_response_id`,
incremental input, server-side compaction) that a relay would have to replay or
break. What this tool does is one-shot: it computes the configuration that makes
the native path work and writes it down. After that it is out of the loop, and
`codex` runs with no extra process, port, or certificate.

It writes one overlay file plus one directory under `$CODEX_HOME`. Your
`config.toml` is never touched, and nothing is active until you pass
`--profile copilot`.

The bearer is your GitHub OAuth token, read at request time from the
`COPILOT_GITHUB_TOKEN` environment variable. **This tool never stores it** - no
keyring, no registry write, no file. `login` prints the token and the one-liner
that sets the variable; you run it.

## Prerequisites

- `codex` on PATH (`npm install -g @openai/codex`), or `--codex-bin <path>`.
  On Windows the npm entry point is a `codex.cmd` / `codex.ps1` shim rather than
  an `.exe`; it is resolved and launched correctly without help.
- A GitHub Copilot seat on which the target model is enabled: CAPI `GET /models`
  must list it with `policy.state == enabled` and `ws:/responses` among its
  `supported_endpoints`. `install` warns (with the remedy) rather than aborting
  if it is not, so the rest of the setup still lands.
- Build from source: Rust 1.88+, `cargo build --release`.

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
    bash         echo 'export COPILOT_GITHUB_TOKEN=gho_xxx' >> ~/.profile
    zsh          echo 'export COPILOT_GITHUB_TOKEN=gho_xxx' >> ~/.zshrc
```

Set the variable, open a new shell, then:

```console
$ codex-copilot install          # or: codex-copilot install --token gho_xxx
```

`install` reads the token from `--token`, `--token-stdin`, or
`COPILOT_GITHUB_TOKEN`; with none of the three it runs the device flow inline,
exactly as `login` does. Then it probes for your gateway
(`api.enterprise` -> `api.business` -> `api.individual` -> `api.githubcopilot.com`,
first `GET /models` that answers 200 wins), downloads the `models.json` bundled
with your `codex --version`, calibrates it, and writes the profile. Add
`--dry-run` to see all of it without writing anything.

## What it writes

| Path | What |
| --- | --- |
| `$CODEX_HOME/copilot.config.toml` | the overlay Codex layers on `config.toml` when `--profile copilot` is passed |
| `$CODEX_HOME/copilot_config_toml/models-catalog.json` | the calibrated catalog (`model_catalog_json`) |
| `$CODEX_HOME/copilot_config_toml/state.json` | host, model, reviewer, codex version, calibration table, timestamp - no secrets |

Nothing else on the machine changes: not `config.toml`, not the credential
store, not your environment variables.

## Usage

```console
$ codex --profile copilot
$ codex --profile copilot exec "..."
$ codex --profile copilot resume --last      # resume with the same profile
```

Plain `codex` keeps using your untouched base config. Sessions started under the
profile must be resumed under it too, or they fall back to your default
provider.

`codex-copilot status` re-checks the whole chain (token present, codex version
vs catalog version, `GET /models`, the model's policy and `ws:/responses`,
overlay and catalog parse). `codex doctor` is not profile-aware, so the real
end-to-end check is to start `codex --profile copilot` and send one short
message.

## Approval review

`install` points Codex's automatic approval review at a real CAPI model, and
never prompts: `gpt-5.6-luna` by default, `--auto-review-model <ID>` for
another, `--no-auto-review` to configure none and inherit `config.toml`'s.

It writes `approvals_reviewer = "auto_review"` in the overlay and
`"auto_review_model_override": "<ID>"` into every CAPI-served catalog entry, so
switching conversation models keeps the reviewer - that is
[Codex's own model selection](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/guardian/review.rs#L872),
not an alias or a proxy. Slugs, Guardian messages and approval settings stay.

A reviewer missing from CAPI or the catalog, disabled, or without
`ws:/responses` warns and is skipped; the rest of the install still lands.
`status` checks the saved override and the reviewer's own policy.

## Per-model calibration

Codex resolves a model's window from its **catalog entry**, and the global
`model_context_window` key is clamped to that entry's `max_context_window` -
so raising the window means rewriting the entry. That is what calibration does,
per model, from this seat's own `GET /models`:

```
  model                    window    compact   capi max  base tier  ws   policy
  gpt-6-astra              872000     784800     872000     272000  yes  enabled  (2x tier)
  gpt-5.6-sol              922000     829800     922000     272000  yes  enabled  (2x tier)
  gpt-5.5                  922000     829800     922000     272000  yes  enabled  (2x tier)
  gpt-5.4                  922000     829800     922000     272000  yes  enabled  (2x tier)
  gpt-5.4-mini             272000     244800     272000     272000  yes  enabled
```

- `window` = CAPI's `long_context` tier ceiling, else the default tier, else
  `capabilities.limits.max_prompt_tokens`. `--context-window base` budgets
  against the standard-price tier instead; `--context-window <N>` clamps to a
  count you choose.
- `compact` = `floor(0.9 x window)`. The ratio is fixed at 0.9 because that is
  exactly Codex's own auto-compaction ceiling (it clamps anything higher);
  the hard cap on what is offered to inference is 95%
  (`effective_context_window_percent`).
- `--model-window <slug>=<window>[:<compact>]` overrides one model. A window
  above what CAPI accepts is refused rather than written.
- Models in the bundled catalog that this seat does not serve are left exactly
  as Codex shipped them.

A local `model_catalog_json` has a second effect: Codex then stops polling
`/models` itself, whose CAPI-shaped answer it cannot parse (one error log every
~4m30s otherwise).

If the download fails (offline, proxy), `install` fails with the raw URL - fetch
it by hand and pass `--catalog <path>`.

## Billing

- Prompts above a model's **base tier** (272k on the seat above) fall into
  CAPI's `long_context` price tier, roughly **2x** per token. `--context-window
  max` is what puts you there; `--context-window base` keeps every model in the
  standard tier.
- Codex's startup **prewarm is a real, billed request** (~14 AIU per session
  with the stock responses-lite catalog). It is also what warms the WebSocket
  the whole session then rides, so shortening
  `websocket_connect_timeout_ms` throws away a request you already paid for.
- Reviewer inference consumes additional CAPI usage.
- `install` and `status` cost nothing: `GET /models` is not a billed endpoint.

## Security

- The token lives in one place: a **user environment variable you set**. This
  tool prints it once and forgets it; the overlay stores only the variable's
  name (`env_key = "COPILOT_GITHUB_TOKEN"`), so the value is read fresh from the
  environment at request time.
- The overlay sets
  `[shell_environment_policy] exclude = ["COPILOT_GITHUB_TOKEN"]`, which keeps
  the token out of every subprocess Codex spawns for a shell tool call.
  `ignore_default_excludes` defaults to `true` in Codex, so this list is the
  only filter in effect. Being an array, it **replaces** any `exclude` list in
  your base `config.toml` - merge yours in if you have one.
- Telemetry is forced off in the overlay (`[analytics]`, `[otel]`,
  `[feedback]`). This matters here: the provider is named `OpenAI`, so an
  enabled exporter would POST to an openai.com host while the process holds
  Copilot credentials.
- `state.json` holds no secrets, and `status` reports the token as a prefix and
  a length only.

## Rollback

```console
$ codex-copilot uninstall
```

Removes the overlay, the catalog and `state.json` (and the profile directory, if
empty), then prints the one-liner to clear `COPILOT_GITHUB_TOKEN` yourself. It
does not touch your environment. Deleting
`$CODEX_HOME/copilot.config.toml` by hand is equally valid: with the overlay
gone, `--profile copilot` is a silent no-op.

## Platform support

Verified on Windows 11. macOS and Linux are supported by the code (the same
paths, `~/.profile` for bash or `~/.zshrc` for zsh instead of the registry) but are
built and tested only in CI - the matrix runs `fmt`, `clippy -D warnings`,
`build --release` and the full test suite on `ubuntu-latest`, `macos-latest` and
`windows-latest`.

## Commands

| Command | Does |
| --- | --- |
| `login` | device flow, prints the token and the set-variable one-liner |
| `install` (default) | probes the host, calibrates the catalog, writes the overlay |
| `status` | summarises the install and checks every link |
| `uninstall` | removes what install wrote, prints the unset one-liner |

Global: `--profile <name>` (default `copilot`), `--codex-home <dir>`,
`--codex-bin <path>`, `--dry-run`.
