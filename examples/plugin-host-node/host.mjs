// SPDX-License-Identifier: Apache-2.0
/**
 * A plugin host in Node, built on the Zygo API alone.
 *
 * The Node twin of `examples/plugin-host`: software with customers who supply
 * code, where each customer's code runs under their own limits and cannot
 * reach anybody else's. No `sandbox.toml`, no file on the Zygo machine, no
 * shelling out to `zygo` — one operator token and the SDK.
 *
 * Where the Python example is a class its demo calls, this one is also an HTTP
 * server, because that is what a plugin host is to its customers:
 *
 *     a customer            this host (node:http)                zygo api
 *     ──────────            ─────────────────────                ────────
 *     PUT  /plugins    ──►  forTenant(c).putScript   ──►  PUT  /scripts
 *     POST /plugins/…/run   forTenant(c).runScript   ──►  POST /runtimes/<r>/call
 *     POST /plugins/…/stream    …streamScript        ──►  …?stream=1
 *     DELETE /runs/<key>        …cancel              ──►  DELETE /requests/<key>
 *
 *     the operator
 *     ────────────
 *     POST   /customers ──►  createTenant, setLimits, mintToken
 *     DELETE /customers/<id> ──►  deleteTenant
 *
 * Run it against a Zygo whose API is up:
 *
 *     ZYGO_API_URL=unix:///run/zygo/api.sock ZYGO_API_TOKEN=... node host.mjs
 */

import http from 'node:http';
import { pathToFileURL } from 'node:url';

// From npm this line is `from 'zygo-sdk'`; the repository's own copy is used
// here so the example runs from a checkout with nothing installed.
import { Busy, Cancelled, HandlerError, NotFound, Timeout, Unavailable, ZygoError, connect } from '../../sdk/node/src/index.js';

/**
 * One runtime pool per language, and no more than that. A pool holds an
 * interpreter and a dependency set and **no code at all**: the script arrives
 * with the request and is loaded in the forked child, after its seccomp
 * filter. That is what makes it safe for many customers to share one, and why
 * ten thousand plugins are not ten thousand warm processes.
 */
export const RUNTIMES = {
  python: { image: process.env.ZYGO_IMAGE ?? 'python:3.12-slim', agent: 'python' },
  javascript: { image: process.env.ZYGO_NODE_IMAGE ?? 'node:22-slim', agent: 'node' },
};

/** The pool a plugin in this language runs in. */
export function runtimeOf(language) {
  if (!(language in RUNTIMES)) throw new NotFound(`no runtime for ${language}; there is ${Object.keys(RUNTIMES).join(', ')}`);
  return `plugins-${language}`;
}

/** Everything this host knows how to do. */
export class PluginHost {
  /** @param {import('../../sdk/node/src/index.js').Client} operator */
  constructor(operator) {
    this.operator = operator;
    /** A customer's token, as this host recognises it: secret → customer id. */
    this.customers = new Map();
    /** Which pool a digest belongs in — the host's own record, not Zygo's. */
    this.languages = new Map();
  }

  // ---- the operator's side --------------------------------------------

  /**
   * Declare one pool per language. The limits are the pool's ceiling and are
   * generous on purpose: what holds a plugin down is its customer's own
   * limits, which narrow these on every request.
   */
  async start() {
    for (const [language, runtime] of Object.entries(RUNTIMES)) {
      await this.operator.serveRuntime(runtimeOf(language), {
        ...runtime,
        min_warm: 1,
        max_warm: 8,
        timeout: '120s',
        seccomp: 'strict',
      });
    }
  }

  /**
   * A new customer: their tenant, their limits, their token. The token is the
   * only thing that leaves; everything their code can reach follows from it.
   */
  async onboard(customer, { mem = '128M' } = {}) {
    await this.operator.createTenant(customer);
    await this.operator.setLimits(customer, { mem, pids: 64, timeout: '60s' });
    const minted = await this.operator.mintToken(customer);
    this.customers.set(minted.secret, customer);
    return minted.secret;
  }

  /** Everything of theirs goes: code, tokens, secrets, running work. */
  async offboard(customer) {
    for (const [secret, owner] of this.customers) if (owner === customer) this.customers.delete(secret);
    return this.operator.deleteTenant(customer);
  }

  /** Drop every pool. The reverse of `start`. */
  async stop() {
    for (const language of Object.keys(RUNTIMES)) await this.operator.stopRuntime(runtimeOf(language));
  }

  // ---- a customer's side ------------------------------------------------

  /**
   * The operator's client, acting for one customer. A header on the same
   * connection rather than a second client, so a thousand customers are one
   * connection pool — and scripts registered through it are theirs, and pool
   * calls may only name their own.
   */
  as(customer) {
    return this.operator.forTenant(customer);
  }

  /** Register a plugin's code against its customer, by digest. Sent once. */
  async install(customer, source, language = 'python') {
    const runtime = runtimeOf(language);
    const { sha256 } = await this.as(customer).putScript(source);
    this.languages.set(sha256, language);
    return { digest: sha256, language, runtime };
  }

  /** The pool this digest was installed into. Unknown to this host: 404. */
  poolOf(digest) {
    const language = this.languages.get(digest);
    if (!language) throw new NotFound('no such plugin');
    return runtimeOf(language);
  }

  /** Call a plugin, under a key this host can stop it by. */
  run(customer, digest, event, key) {
    return this.as(customer).runScript(this.poolOf(digest), digest, event, { key });
  }

  /** Call a plugin and yield its output as it is produced. */
  watch(customer, digest, event) {
    return this.as(customer).streamScript(this.poolOf(digest), digest, event);
  }

  cancel(customer, key) {
    return this.as(customer).cancel(key);
  }
}

// ---- the HTTP surface ---------------------------------------------------

/**
 * What a Zygo error is to a customer of this host. The host's status codes
 * are its own: a customer sees "your plugin failed", never Zygo's answer
 * passed through raw. `Busy` and `Unavailable` arrive here only after the
 * client's retries gave up, and the server's `Retry-After` is passed on.
 */
export function statusOf(error) {
  // `detail` is the exception's own text; `stderr` is what else it printed.
  if (error instanceof HandlerError) return [500, { error: 'the plugin failed', detail: error.message, stderr: error.stderr, exitCode: error.exitCode }];
  if (error instanceof Timeout) return [504, { error: 'the plugin ran out of time', detail: error.message, stderr: error.stderr }];
  if (error instanceof Cancelled) return [499, { error: 'the plugin was stopped' }];
  if (error instanceof NotFound) return [404, { error: 'no such plugin' }];
  if (error instanceof Busy || error instanceof Unavailable) {
    return [503, { error: 'try again shortly' }, { 'retry-after': String(error.retryAfter) }];
  }
  if (error instanceof SyntaxError) return [400, { error: 'the body is not JSON' }];
  if (error instanceof ZygoError) return [502, { error: 'the sandbox host refused the request' }];
  return [500, { error: 'internal error' }];
}

async function readBody(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  return Buffer.concat(chunks).toString('utf8');
}

function send(response, status, body, headers = {}) {
  const payload = JSON.stringify(body);
  response.writeHead(status, http.STATUS_CODES[status] ?? 'Client Closed Request', {
    'content-type': 'application/json',
    'content-length': String(Buffer.byteLength(payload)),
    ...headers,
  });
  response.end(payload);
}

/**
 * The routes, each with who may call it: the operator, or a customer whose
 * token this host minted. Every route needs a bearer token.
 */
const ROUTES = [
  ['POST', /^\/customers$/, 'operator', async (host, { body }) => {
    const { id, mem } = JSON.parse(body);
    return [201, { id, token: await host.onboard(id, { mem }) }];
  }],
  ['DELETE', /^\/customers\/([^/]+)$/, 'operator', async (host, { m }) => [200, await host.offboard(m[1])]],
  ['PUT', /^\/plugins$/, 'customer', async (host, { customer, body, url }) => [
    201,
    await host.install(customer, body, url.searchParams.get('language') ?? 'python'),
  ]],
  ['POST', /^\/plugins\/([^/]+)\/run$/, 'customer', async (host, { customer, m, body, request }) => {
    const out = await host.run(customer, m[1], JSON.parse(body || 'null'), request.headers['x-run-key']);
    return [200, { result: out.result, metrics: out.metrics }];
  }],
  ['POST', /^\/plugins\/([^/]+)\/stream$/, 'customer', async (host, { customer, m, body, response }) => {
    response.writeHead(200, { 'content-type': 'application/x-ndjson' });
    try {
      for await (const item of host.watch(customer, m[1], JSON.parse(body || 'null'))) {
        const line = item.kind === 'result' ? { result: item.result.result, metrics: item.result.metrics } : { stream: item.kind, data: item.data };
        response.write(JSON.stringify(line) + '\n');
      }
    } catch (error) {
      // The headers are gone, so the failure travels as the last line — the
      // way Zygo's own stream carries its status.
      const [status, failure] = statusOf(error);
      response.write(JSON.stringify({ status, ...failure }) + '\n');
    }
    response.end();
    return null;
  }],
  ['DELETE', /^\/runs\/([^/]+)$/, 'customer', async (host, { customer, m }) => [200, await host.cancel(customer, m[1])]],
];

/** An `http.Server` fronting the host. */
export function createServer(host) {
  return http.createServer(async (request, response) => {
    const url = new URL(request.url, 'http://host');
    const bearer = (request.headers.authorization ?? '').replace(/^Bearer /, '');
    const role = bearer !== '' && bearer === host.operator.token ? 'operator' : host.customers.has(bearer) ? 'customer' : null;
    if (!role) return send(response, 401, { error: 'no such token' });

    let m = null;
    const route = ROUTES.find(([method, pattern]) => request.method === method && (m = url.pathname.match(pattern)));
    if (!route) return send(response, 404, { error: `no route ${request.method} ${url.pathname}` });
    if (route[2] !== role) return send(response, 403, { error: `${route[2] === 'operator' ? "the operator's" : "a customer's"} token is needed here` });

    try {
      const body = await readBody(request);
      const answer = await route[3](host, { m, body, url, request, response, customer: host.customers.get(bearer) });
      if (answer) send(response, answer[0], answer[1]);
    } catch (error) {
      const [status, body, headers] = statusOf(error);
      send(response, status, body, headers);
    }
  });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const operator = connect(process.env.ZYGO_API_URL, { retries: 3 });
  const host = new PluginHost(operator);
  await host.start();
  const server = createServer(host).listen(Number(process.env.PORT ?? 8080), () => {
    console.log(`plugin host on :${server.address().port}, pools: ${Object.keys(RUNTIMES).map(runtimeOf).join(', ')}`);
  });
  process.on('SIGINT', async () => {
    server.close();
    await host.stop();
    operator.close();
  });
}
