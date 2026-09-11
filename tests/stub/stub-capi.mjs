#!/usr/bin/env node
// Dependency-free stand-in for the hosts `codex-copilot` talks to:
// the Copilot CAPI gateway (GET /models) and raw.githubusercontent.com
// (GET /models.json, the catalog bundled with a codex release), plus OAuth.
// Binds 127.0.0.1
// and prints `LISTENING <port>` once ready, so the Rust test can read the port.
//
//   node stub-capi.mjs [--port N] [--status 200|401] [--model ID]
//                      [--policy STATE] [--no-ws] [--catalog-status N]
//
// The /models payload mirrors a real Copilot Enterprise response:
// capabilities.limits.max_prompt_tokens plus per-tier
// billing.token_prices.{default,long_context}.max_prompt_tokens, which is what
// catalog calibration reads.

import http from 'node:http';

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith('--')) continue;
    const eq = a.indexOf('=');
    if (eq > 0) out[a.slice(2, eq)] = a.slice(eq + 1);
    else if (i + 1 < argv.length && !argv[i + 1].startsWith('--')) out[a.slice(2)] = argv[++i];
    else out[a.slice(2)] = true;
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));
const PORT = Number(args.port ?? 0);
const STATUS = Number(args.status ?? 200);
const MODEL = String(args.model ?? 'gpt-6-astra');
const POLICY = args.policy === undefined ? 'enabled' : String(args.policy);
const WITH_WS = !args['no-ws'];
const CATALOG_STATUS = Number(args['catalog-status'] ?? 200);

// (id, standard tier ceiling, long-context ceiling or null, hard ceiling) taken
// from a real Enterprise seat: astra 272k/872k, 5.5 272k/922k, and 5.4-mini
// with no long-context tier at all.
const CAPI_MODELS = [
  ['gpt-6-astra', 272000, 872000, 1000000],
  ['gpt-5.5', 272000, 922000, 1050000],
  ['gpt-5.4-mini', 272000, null, 400000],
];

function entry(id, base, long, hardMax, endpoints, policy) {
  const token_prices = { batch_size: 1000000, default: { max_prompt_tokens: base, input_price: 175 } };
  if (long !== null) token_prices.long_context = { max_prompt_tokens: long, input_price: 350 };
  const e = {
    id,
    name: id,
    object: 'model',
    vendor: 'OpenAI',
    supported_endpoints: endpoints,
    capabilities: {
      family: id,
      type: 'chat',
      limits: { max_context_window_tokens: hardMax, max_output_tokens: 128000, max_prompt_tokens: long ?? base },
      supports: { streaming: true, tool_calls: true },
    },
    billing: { restricted_to: ['business', 'enterprise'], token_prices },
  };
  if (policy) e.policy = { state: policy };
  return e;
}

function models() {
  const first = ['/chat/completions', '/responses'];
  if (WITH_WS) first.push('ws:/responses');
  return {
    object: 'list',
    data: CAPI_MODELS.map(([id, base, long, hardMax], i) =>
      // The first entry carries the id/policy/endpoints the test asked for, so
      // --model / --policy / --no-ws keep working.
      i === 0
        ? entry(MODEL, base, long, hardMax, first, POLICY)
        : entry(id, base, long, hardMax, ['/responses', 'ws:/responses'], 'enabled'),
    ),
  };
}

// The models.json bundled with a codex release, trimmed to the fields
// calibration reads plus one it must not disturb. gpt-5.5 is capped at 272k
// here exactly as codex 0.154.0 ships it while the seat allows 922k, and
// gpt-5.2 is not served at all.
function bundledCatalog() {
  const m = (slug, ctx, max, extra = {}) => ({
    slug,
    display_name: slug,
    context_window: ctx,
    max_context_window: max,
    auto_compact_token_limit: null,
    auto_review_model_override: null,
    model_messages: { auto_review: { policy: 'fixture policy: preserve unchanged' } },
    prefer_websockets: true,
    ...extra,
  });
  return {
    models: [
      m('gpt-6-astra', 272000, 872000, { tool_mode: 'code_mode_only' }),
      m('gpt-5.5', 272000, 272000),
      m('gpt-5.4-mini', 272000, 272000),
      m('gpt-5.2', 272000, 272000),
    ],
  };
}

function send(res, status, body) {
  const payload = typeof body === 'string' ? body : JSON.stringify(body);
  res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(payload) });
  res.end(payload);
}

let tokenPolls = 0;
const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://127.0.0.1');
  const auth = req.headers['authorization'] || '';
  process.stderr.write(`stub: ${req.method} ${url.pathname}\n`);

  if (url.pathname === '/login/device/code') {
    tokenPolls = 0;
    return send(res, 200, {
      device_code: 'dummy-device-code',
      user_code: 'ABCD-1234',
      verification_uri: 'http://127.0.0.1/login/device',
      interval: 1,
      expires_in: 30,
    });
  }

  if (url.pathname === '/login/oauth/access_token') {
    if (tokenPolls++ === 0) return send(res, 503, { message: 'Service unavailable' });
    return send(res, 200, { access_token: 'gho_dummy_test_token' });
  }

  if (url.pathname === '/models.json') {
    if (CATALOG_STATUS !== 200) return send(res, CATALOG_STATUS, { message: 'stubbed failure' });
    return send(res, 200, bundledCatalog());
  }

  if (url.pathname === '/models') {
    if (!auth.startsWith('Bearer ')) return send(res, 400, { message: 'Authorization header is badly formatted' });
    const required = ['copilot-integration-id', 'editor-version', 'editor-plugin-version', 'x-github-api-version'];
    const missing = required.filter((h) => !req.headers[h]);
    if (missing.length) return send(res, 400, { message: `missing identity headers: ${missing.join(', ')}` });
    if (STATUS !== 200) return send(res, STATUS, { message: 'stubbed refusal' });
    return send(res, 200, models());
  }

  send(res, 404, { message: 'not found' });
});

server.listen(PORT, '127.0.0.1', () => {
  process.stdout.write(`LISTENING ${server.address().port}\n`);
});

process.on('SIGTERM', () => server.close(() => process.exit(0)));
