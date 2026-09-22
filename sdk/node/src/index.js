/**
 * Zygo — warm sandboxes for function-shaped code, from Node.
 *
 *     import { connect } from 'zygo';
 *
 *     const client = connect();                    // `zygo api` on loopback
 *     const resize = client.fn('resize');          // a function from sandbox.toml
 *     const out = await resize({ url: '…' });      // ~2 ms, a fresh process
 *
 * A warm function costs about a millisecond and gets a clean process per
 * request. A one-shot sandbox costs tens of milliseconds and needs nothing
 * declared in advance:
 *
 *     const r = await client.run('node:22-slim', ['node', '-e', 'console.log(6*7)']);
 *
 * This package talks to `zygo api` over HTTP — on a unix socket when the API
 * is on this machine, which is the usual case and needs no token. It has no
 * dependencies, and `node:http` handles both transports and connection reuse,
 * so there is nothing here that a runtime does not already ship.
 *
 * What it is *not* is a second implementation of Zygo. Every boundary a
 * sandbox has is built by the Zygo binary and enforced by the kernel; nothing
 * here can widen one, and an API started without `--allow-deploy` will not let
 * this package create a sandbox at all.
 */

import http from 'node:http';
import https from 'node:https';

import { DEFAULT_URL, parse, resolve } from './endpoint.js';
import {
  AuthError,
  Busy,
  HandlerError,
  NotFound,
  SpecError,
  Timeout,
  TransportError,
  ZygoError,
  fromResponse,
} from './errors.js';

export {
  AuthError,
  Busy,
  DEFAULT_URL,
  HandlerError,
  NotFound,
  SpecError,
  Timeout,
  TransportError,
  ZygoError,
  parse as parseEndpoint,
};

/**
 * Largest answer read into memory. The API's own request limit is the same
 * order, and an answer past it is a bug rather than a large result.
 */
const MAX_BODY = 64 * 1024 * 1024;

/** A connection to a Zygo API. */
export class Client {
  /**
   * @param {string} [url]
   * @param {{token?: string|null, timeout?: number, tenant?: string|null,
   *          agent?: import('http').Agent}} [options]
   */
  constructor(url, options = {}) {
    this.endpoint = resolve(url);
    // The environment by default, the same variable the server reads, so a
    // shell that can start the API can also talk to it.
    this.token = options.token !== undefined ? options.token : process.env.ZYGO_API_TOKEN ?? null;
    // Bounds one HTTP exchange. Generous on purpose: the request's real limit
    // is the function's own `timeout` and the supervisor enforces it, so a
    // client that gives up first only loses the answer.
    this.timeout = options.timeout ?? 300_000;
    /** Sent as `X-Zygo-Tenant`; set by {@link Client#forTenant}. */
    this._tenantId = options.tenant ?? null;

    // Keep-alive is what keeps this client's overhead off the warm path: a
    // fresh connection per call would cost more than a warm request does. The
    // agent also pools, so concurrent callers become concurrent sandbox
    // requests rather than a queue behind one socket.
    const transport = this.endpoint.tls ? https : http;
    this._transport = transport;
    // `agent` is how `forTenant` shares this pool rather than opening a
    // second one. Undocumented on purpose: it is an internal seam, not a
    // knob, and passing a foreign agent here is a way to lose keep-alive.
    this._agent = options.agent ?? new transport.Agent({ keepAlive: true, maxSockets: 64 });
  }

  /** Close every pooled connection. Calling it twice is harmless. */
  close() {
    this._agent.destroy();
  }

  // ---- the API ------------------------------------------------------

  /**
   * The server's version, and the version of the HTTP surface itself.
   * `api` is what to check against: it is bumped only when a route changes
   * incompatibly, so it stays put across releases that change what happens
   * behind them.
   */
  version() {
    return this.#request('GET', '/version');
  }

  /** `GET /healthz`, which needs no token. */
  health() {
    return this.#request('GET', '/healthz', { authenticated: false });
  }

  /** Every warm function the supervisor holds. */
  async functions() {
    const body = await this.#request('GET', '/fn');
    return (body.functions ?? []).map(parseFunction);
  }

  /** One function's counters. */
  async stats(name) {
    return parseFunction(await this.#request('GET', `/fn/${esc(name)}/stats`));
  }

  /**
   * Bring a registered function up now, without calling it. For the moment
   * after a deploy, so the first real request does not pay for the warm-up.
   */
  warm(name) {
    return this.#request('POST', `/fn/${esc(name)}/warm`);
  }

  /**
   * Call a warm function and return what its handler returned.
   *
   * Throws {@link HandlerError} when the handler threw, {@link Timeout} when
   * the deadline killed the request, and {@link Busy} when the function is at
   * its concurrency limit — the last of which means the request never ran.
   *
   * @param {string} name
   * @param {unknown} [event]
   * @param {{timeout?: number}} [options] `timeout` in seconds.
   */
  async call(name, event = null, options = {}) {
    const body = await this.#request('POST', `/fn/${esc(name)}`, {
      body: event,
      headers: timeoutHeader(options.timeout),
    });
    return parseResult(body);
  }

  /**
   * Call a function with several events at once, answers in order.
   *
   * Each element is a result or the error that element would have thrown —
   * returned rather than thrown, because one event being refused must not hide
   * the answers to the others.
   */
  async batch(name, events, options = {}) {
    const answers = await this.#request('POST', `/fn/${esc(name)}/batch`, {
      body: [...events],
      headers: timeoutHeader(options.timeout),
    });
    return answers.map(batchElement);
  }

  /**
   * A function's recent log. `after` is a sequence number: zero for the last
   * `limit` entries, and a previous page's `next` for everything since.
   */
  async logs(name, { after = 0, limit = 50, failed = false } = {}) {
    const query = `?after=${Number(after)}&limit=${Number(limit)}&failed=${failed ? 'true' : 'false'}`;
    const body = await this.#request('GET', `/fn/${esc(name)}/logs${query}`);
    return {
      name: String(body.name ?? name),
      entries: (body.entries ?? []).map((e) => ({ ...e, seq: Number(e.seq ?? 0) })),
      next: Number(body.next ?? 0),
    };
  }

  /**
   * Warm a function, replacing whatever held the name.
   *
   * `layer` is a `[fn.<name>]` table as an object. `baseDir` is what its
   * relative paths are relative to **on the host the API runs on**; it
   * defaults to this process's working directory, which is right only when the
   * two are the same machine.
   *
   * Needs an API started with `--allow-deploy`; without it this throws
   * {@link AuthError} and says so.
   */
  async serve(name, layer, { baseDir, secrets = {}, ifChanged = false } = {}) {
    const { resolve: resolvePath } = await import('node:path');
    const body = await this.#request('PUT', `/fn/${esc(name)}`, {
      body: {
        layer: { ...layer },
        base_dir: resolvePath(baseDir ?? process.cwd()),
        secrets: { ...secrets },
        if_changed: Boolean(ifChanged),
      },
    });
    return {
      name: String(body.name ?? name),
      change: String(body.change ?? 'started'),
      runtime: String(body.runtime ?? ''),
      rssKb: Number(body.rss_kb ?? 0),
      importsMs: Number(body.imports_ms ?? 0),
      warmMs: Number(body.warm_ms ?? 0),
      warnings: body.warnings ?? [],
    };
  }

  /** Stop one function. Throws {@link NotFound} if it is not there. */
  async stop(name) {
    const body = await this.#request('DELETE', `/fn/${esc(name)}`);
    return body.stopped ?? [];
  }

  /**
   * Run one command in a fresh sandbox and collect its output.
   *
   * `layer` holds spec fields — `mem`, `cpu`, `pids`, `timeout`, `network`,
   * `allow`, `mounts`, `env` — written exactly as they would be in
   * `sandbox.toml`. Mount sources must be absolute, because a relative path in
   * a request body has no directory to be relative to.
   *
   * A non-zero exit code is not an error: the sandbox ran, and this is what it
   * said. Only Zygo failing to run it at all throws.
   *
   * Needs an API started with `--allow-deploy`.
   */
  async run(image, cmd = null, { stdin = '', ...layer } = {}) {
    const described = { ...layer, image };
    if (cmd !== null) described.cmd = [...cmd];
    const body = await this.#request('POST', '/run', { body: { layer: described, stdin } });
    // `timedOut` and `oomKilled` are *why* it ended, which the exit code
    // cannot carry: a deadline kill and an out-of-memory kill are both
    // SIGKILL, so both are 137.
    return {
      exitCode: Number(body.exit_code ?? -1),
      stdout: String(body.stdout ?? ''),
      stderr: String(body.stderr ?? ''),
      timedOut: Boolean(body.timed_out),
      oomKilled: Boolean(body.oom_killed),
      peakRssKb: Number(body.peak_rss_kb ?? 0),
      wallMs: Number(body.wall_ms ?? 0),
      get ok() {
        return this.exitCode === 0 && !this.timedOut && !this.oomKilled;
      },
    };
  }

  /**
   * Register a customer, or find the one already registered.
   *
   * Idempotent, so a deploy that runs twice is not an error. A tenant owns the
   * scripts registered for it, and its functions and pools live in a cgroup of
   * its own — which is what makes {@link deleteTenant} able to stop everything
   * of theirs and nobody else's. Operator-only.
   */
  async createTenant(id) {
    const body = await this.#request('POST', '/tenants', { body: { id } });
    return body.tenant ?? {};
  }

  /** Every tenant this host holds. Operator-only. */
  async tenants() {
    const body = await this.#request('GET', '/tenants');
    return body.tenants ?? [];
  }

  /** One tenant. Throws {@link NotFound} if there is no such id. */
  async tenant(id) {
    const body = await this.#request('GET', `/tenants/${esc(id)}`);
    return body.tenant ?? {};
  }

  /**
   * Forget a tenant: stop its work, then remove the scripts only it had.
   *
   * Answers with what it stopped and what it removed, because neither can be
   * reconstructed afterwards. Operator-only, and needs `--allow-deploy`.
   */
  async deleteTenant(id) {
    return this.#request('DELETE', `/tenants/${esc(id)}`);
  }

  /**
   * Mint an API token, and get its secret — once.
   *
   * Without `tenant` this is an **operator** token: tenants, functions, pools,
   * and more tokens. With one it is that tenant's, and it may register scripts
   * and call, for itself only. Minting for a tenant registers the tenant if it
   * is new.
   *
   * The secret is in the answer and nowhere else. The server keeps a SHA-256
   * of it, so it cannot be fetched again; keep it or revoke it.
   *
   * Operator-only, and needs deploy rights.
   */
  async mintToken(tenant = null) {
    const path = tenant == null ? '/tokens' : `/tenants/${esc(tenant)}/tokens`;
    const body = await this.#request('POST', path);
    return { token: body.token ?? {}, secret: body.secret ?? '' };
  }

  /** Every token this host holds, revoked ones included. Never a secret. */
  async tokens() {
    const body = await this.#request('GET', '/tokens');
    return body.tokens ?? [];
  }

  /**
   * Revoke one, from the next request onwards.
   *
   * The record stays, marked with when it went, so an id in a log line still
   * resolves to something.
   */
  async revokeToken(id) {
    return this.#request('DELETE', `/tokens/${esc(id)}`);
  }

  /**
   * A view of this client that acts for one tenant.
   *
   * Every call through it carries the tenant, so scripts are registered
   * against them and pool calls may only name their own. The connection is
   * shared — this is a header, not a second client.
   *
   * For an **operator** token: the header says which of your customers you are
   * acting for. A **tenant** token already names its tenant and does not need
   * this — and the server refuses a header that disagrees with the token
   * rather than ignoring it.
   */
  forTenant(id) {
    // A real `new Client`, not a copy of this one's properties: the request
    // path is a private class field, which `Object.assign` does not carry, so
    // a view built that way looked like a client and threw `Receiver must be
    // an instance of class Client` on its first call.
    //
    // The connection pool is passed rather than rebuilt, so this stays a
    // header on the same client. Closing either closes both.
    return new Client(this.endpoint.url, {
      token: this.token,
      timeout: this.timeout,
      tenant: id,
      agent: this._agent,
    });
  }

  /**
   * Register a **runtime pool**: an image, a dependency set, an agent.
   *
   * A pool holds no code. Scripts arrive with each call, so one pool serves
   * ten thousand of them where ten thousand functions would be ten thousand
   * warm zygotes. `layer` is a `[runtime.<name>]` table as an object —
   * `image`, `agent`, `min_warm`, `max_warm` and the limits.
   *
   * Needs an API started with `--allow-deploy`.
   */
  async serveRuntime(name, layer, { baseDir } = {}) {
    const body = { name, layer: { ...layer } };
    if (baseDir !== undefined) {
      const { resolve: resolvePath } = await import('node:path');
      body.base_dir = resolvePath(baseDir);
    }
    return this.#request('POST', '/runtimes', { body });
  }

  /** Every runtime pool this host holds. */
  async runtimes() {
    const body = await this.#request('GET', '/runtimes');
    return body.runtimes ?? [];
  }

  /** Stop a pool and drop its zygotes. */
  async stopRuntime(name) {
    const body = await this.#request('DELETE', `/runtimes/${esc(name)}`);
    return body.stopped ?? [];
  }

  /**
   * Run one script in a pool.
   *
   * `script` is either a `sha256:…` digest this host holds — register it once
   * with {@link putScript} — or the source itself. The digest is the shape to
   * build on: the bytes cross the wire once rather than on every call, and the
   * host can put the file in the sandbox instead of sending it through the
   * zygote.
   */
  async runScript(runtime, script, event = null, { entryPoint, timeout } = {}) {
    const body = {
      script: script.startsWith('sha256:') ? script : { source: script },
      event,
    };
    if (entryPoint !== undefined) body.entry_point = entryPoint;
    const answer = await this.#request('POST', `/runtimes/${esc(runtime)}/call`, {
      body,
      headers: timeoutHeader(timeout),
    });
    return parseResult(answer);
  }

  /**
   * Register a script and get back the name the host gave it.
   *
   * The name is the SHA-256 of the bytes, so this is idempotent in the
   * strongest sense: the same script registered twice — or by two tenants — is
   * one file, and `existed` says which call wrote it. Register once and name
   * the digest on every call after that.
   *
   * Needs an API started with `--allow-deploy`.
   */
  async putScript(source) {
    const body = await this.#request('PUT', '/scripts', { rawBody: Buffer.from(source, 'utf8') });
    return { sha256: String(body.sha256 ?? ''), size: Number(body.size ?? 0), existed: Boolean(body.existed) };
  }

  /**
   * Whether this host holds a script, and how big it is.
   *
   * Never the bytes: a digest is not a capability, so a store that answered
   * with the script would make every tenant's code readable by anyone who
   * could guess what it was. Throws {@link NotFound} when the host does not
   * have it.
   */
  async script(digest) {
    const body = await this.#request('GET', `/scripts/${escDigest(digest)}`);
    return { sha256: String(body.sha256 ?? ''), size: Number(body.size ?? 0), existed: true };
  }

  /** Forget a script. Throws {@link NotFound} if it was not there. */
  async deleteScript(digest) {
    const body = await this.#request('DELETE', `/scripts/${escDigest(digest)}`);
    return Boolean(body.deleted);
  }

  /**
   * A callable bound to one function. `client.fn('resize')(event)` reads
   * better than repeating the name at every call site, and it is the shape an
   * embedder wraps as a tool.
   */
  fn(name) {
    const call = (event, options) => this.call(name, event, options);
    call.name_ = name;
    call.batch = (events, options) => this.batch(name, events, options);
    call.stats = () => this.stats(name);
    call.logs = (options) => this.logs(name, options);
    call.warm = () => this.warm(name);
    call.stop = () => this.stop(name);
    return call;
  }

  // ---- transport ----------------------------------------------------

  #request(method, path, { body = undefined, headers = {}, authenticated = true, rawBody = undefined } = {}) {
    // `rawBody` is for the one route whose body is not JSON: a script is a
    // file, and wrapping its bytes in a JSON string to unwrap them again is a
    // transformation with no reader.
    const payload = rawBody !== undefined ? rawBody : body === undefined ? null : Buffer.from(JSON.stringify(body));
    const sent = { accept: 'application/json', ...headers };
    if (payload !== null) {
      sent['content-type'] = rawBody !== undefined ? 'text/plain; charset=utf-8' : 'application/json';
      sent['content-length'] = String(payload.length);
    }
    if (authenticated && this.token) sent.authorization = `Bearer ${this.token}`;
    if (this._tenantId) sent['x-zygo-tenant'] = this._tenantId;

    const options = {
      method,
      path,
      headers: sent,
      agent: this._agent,
      timeout: this.timeout,
    };
    if (this.endpoint.isUnix) {
      options.socketPath = this.endpoint.socketPath;
    } else {
      options.host = this.endpoint.host;
      options.port = this.endpoint.port;
    }

    return new Promise((resolvePromise, reject) => {
      const request = this._transport.request(options, (response) => {
        const chunks = [];
        let size = 0;
        response.on('data', (chunk) => {
          size += chunk.length;
          if (size > MAX_BODY) {
            response.destroy();
            reject(new TransportError(`the API answered with more than ${MAX_BODY} bytes`));
            return;
          }
          chunks.push(chunk);
        });
        response.on('end', () => {
          const raw = Buffer.concat(chunks).toString('utf8');
          const retryAfter = Number(response.headers['retry-after']) || 1;
          try {
            resolvePromise(decode(response.statusCode ?? 0, raw, retryAfter));
          } catch (e) {
            reject(e);
          }
        });
        response.on('error', (e) =>
          reject(new TransportError(`${method} ${path} failed against ${this.endpoint.url}: ${e.message}`))
        );
      });

      request.on('timeout', () => {
        request.destroy();
        reject(new TransportError(`${method} ${path} got no answer within ${this.timeout} ms`));
      });
      request.on('error', (e) => {
        const hint =
          e.code === 'ECONNREFUSED' || e.code === 'ENOENT'
            ? '\n  -> nothing is listening there; start one with `zygo api`'
            : '';
        reject(new TransportError(`${method} ${path} failed against ${this.endpoint.url}: ${e.message}${hint}`));
      });

      if (payload !== null) request.write(payload);
      request.end();
    });
  }
}

/** Open a client. See {@link Client} for what the arguments mean. */
export function connect(url, options = {}) {
  return new Client(url, options);
}

// ---- parsing ----------------------------------------------------------

function decode(status, raw, retryAfter) {
  let body;
  try {
    body = raw ? JSON.parse(raw) : {};
  } catch {
    throw new TransportError(`the API answered HTTP ${status} with something that is not JSON`);
  }
  if (status >= 200 && status < 300) return body;
  throw fromResponse(status, typeof body === 'object' && body !== null ? body : { error: String(body) }, retryAfter);
}

function parseResult(raw) {
  return {
    result: raw.result ?? null,
    stdout: String(raw.stdout ?? ''),
    stderr: String(raw.stderr ?? ''),
    metrics: {
      wallMs: Number(raw.metrics?.wall_ms ?? 0),
      cpuMs: Number(raw.metrics?.cpu_ms ?? 0),
      peakRssKb: Number(raw.metrics?.peak_rss_kb ?? 0),
    },
  };
}

function parseFunction(raw) {
  return {
    name: String(raw.name ?? ''),
    state: String(raw.state ?? ''),
    image: String(raw.image ?? ''),
    runtime: String(raw.runtime ?? ''),
    rssKb: Number(raw.rss_kb ?? 0),
    importsMs: Number(raw.imports_ms ?? 0),
    requests: Number(raw.requests ?? 0),
    failures: Number(raw.failures ?? 0),
    raw,
  };
}

/**
 * One element of a batch: a result, or the error it would have thrown.
 *
 * Returned rather than thrown. A batch exists so that one refused event does
 * not hide the answers to the others, and throwing on the first bad element
 * would undo exactly that.
 */
function batchElement(answer) {
  if (typeof answer !== 'object' || answer === null) {
    return new ZygoError(`unreadable batch element: ${JSON.stringify(answer)}`);
  }
  const status = Number(answer.status ?? 200);
  if (status >= 200 && status < 300) return parseResult(answer);
  return fromResponse(status, answer);
}

function esc(name) {
  return encodeURIComponent(name);
}

/**
 * A digest, checked here so it can go into the path as it stands.
 *
 * `encodeURIComponent` would escape the colon, and the route matches on the
 * segment it was given. Checking the shape instead of escaping it also means
 * `../../etc/passwd` is a mistake this client names, rather than a request
 * somebody's proxy might normalise into a different route.
 */
function escDigest(digest) {
  if (!/^sha256:[0-9a-f]{64}$/.test(String(digest))) {
    throw new SpecError(
      `\`${digest}\` is not a script digest; expected sha256: followed by 64 lowercase hex digits`
    );
  }
  return digest;
}

function timeoutHeader(seconds) {
  if (seconds === undefined || seconds === null) return {};
  return { 'x-zygo-timeout-ms': String(Math.max(1, Math.round(seconds * 1000))) };
}
