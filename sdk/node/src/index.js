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

import { randomBytes } from 'node:crypto';
import http from 'node:http';
import https from 'node:https';

import { DEFAULT_URL, parse, resolve } from './endpoint.js';
import {
  AuthError,
  Busy,
  Cancelled,
  HandlerError,
  NotFound,
  SpecError,
  Stuck,
  Timeout,
  TransportError,
  ZygoError,
  fromResponse,
} from './errors.js';

export {
  AuthError,
  Busy,
  Cancelled,
  DEFAULT_URL,
  HandlerError,
  NotFound,
  SpecError,
  Stuck,
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

  /**
   * `GET /healthz`, which needs no token.
   *
   * `status` is `ok`, `degraded` — a pool below its `min_warm`, so requests
   * work but the first of them pay a cold start — or `stopping`, which is the
   * only one that is not a 200.
   */
  health() {
    return this.#request('GET', '/healthz', { authenticated: false });
  }

  /**
   * Stop admitting, let what is running finish, then exit.
   *
   * Answers before the process leaves: `inFlight` is what was still running
   * when the grace ran out, so `0` is a clean drain. `SIGTERM` does the same.
   * Operator-only, and needs deploy rights.
   */
  async drain(grace = 30) {
    const body = await this.#request('POST', `/drain?grace_ms=${Math.round(grace * 1000)}`);
    return { drained: Boolean(body.drained), inFlight: Number(body.in_flight ?? 0) };
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
   * `key` is a name *you* choose for this request, so that something else can
   * stop it with {@link cancel} before it answers — the server's own id only
   * arrives with the answer, which is too late to cancel the call it belongs
   * to. `signal` does the same thing through an `AbortController`: aborting it
   * sends the cancel for you.
   *
   * @param {string} name
   * @param {unknown} [event]
   * @param {{timeout?: number, key?: string, signal?: AbortSignal}} [options]
   *   `timeout` in seconds.
   */
  async call(name, event = null, options = {}) {
    const key = options.key || (options.signal ? requestKey() : undefined);
    const headers = timeoutHeader(options.timeout);
    if (key) headers['x-zygo-request-key'] = key;
    const stop = this.#onAbort(options.signal, key);
    try {
      const path = `/fn/${esc(name)}${workspaceQuery(options.workspace, options.out)}`;
      const body = await this.#request('POST', path, { body: event, headers });
      return parseResult(body);
    } finally {
      stop();
    }
  }

  /**
   * Call a function and yield its output as it is produced.
   *
   * An async iterator. Each item is either a piece of the request's output —
   * `{ kind: 'stdout' | 'stderr' | 'progress', data }` — or, exactly once and
   * last, `{ kind: 'result', result, status }` carrying what {@link call}
   * would have returned. If the request failed, iterating past the result
   * throws what {@link call} would have thrown.
   *
   * ```js
   * for await (const event of client.stream('render', { pages: 400 })) {
   *   if (event.kind === 'result') console.log(event.result.result);
   *   else process.stdout.write(event.data);
   * }
   * ```
   *
   * The connection is held for the whole request and is not pooled: it is in
   * use for as long as the handler runs. Breaking out of the loop closes it,
   * which does **not** cancel the request — pass a `key` or a `signal` and use
   * {@link cancel} for that.
   */
  stream(name, event = null, options = {}) {
    return this.#streamed(`/fn/${esc(name)}?stream=1`, event, options);
  }

  /** Run a script in a pool, yielding its output. See {@link stream}. */
  streamScript(runtime, script, event = null, options = {}) {
    const body = {
      script: script.startsWith('sha256:') ? script : { source: script },
      event,
    };
    if (options.entryPoint !== undefined) body.entry_point = options.entryPoint;
    return this.#streamed(`/runtimes/${esc(runtime)}/call?stream=1`, body, options);
  }

  async *#streamed(path, body, options) {
    const key = options.key || (options.signal ? requestKey() : undefined);
    const headers = timeoutHeader(options.timeout);
    headers.accept = 'application/x-ndjson';
    if (key) headers['x-zygo-request-key'] = key;
    const stop = this.#onAbort(options.signal, key);
    try {
      const response = await this.#open('POST', path, body, headers);
      if ((response.statusCode ?? 0) >= 400) {
        throw fromResponse(response.statusCode ?? 0, await readJson(response), 1);
      }
      for await (const line of lines(response)) {
        const raw = JSON.parse(line);
        const event = parseEvent(raw);
        yield event;
        if (event.kind === 'result') {
          if (!(event.status >= 200 && event.status < 300)) {
            throw fromResponse(event.status, raw, 1);
          }
          return;
        }
      }
    } finally {
      stop();
    }
  }

  /**
   * Store a tar this host will hold under its digest.
   *
   * For the case an embedder actually has: the same fixture across a thousand
   * calls. Sent once, named with `workspace` on every call after — the bargain
   * {@link putScript} makes for code. The body is the tar itself.
   */
  async putBlob(tar) {
    const body = await this.#request('PUT', '/blobs', {
      rawBody: Buffer.from(tar),
      binary: true,
    });
    return { sha256: String(body.sha256 ?? ''), size: Number(body.size ?? 0), existed: Boolean(body.existed) };
  }

  /** Whether this host holds a blob, and how big it is. */
  async blob(digest) {
    const body = await this.#request('GET', `/blobs/${escDigest(digest)}`);
    return { sha256: String(body.sha256 ?? ''), size: Number(body.size ?? 0), existed: true };
  }

  /** Forget a blob. Operator-only: the store is shared by digest. */
  async deleteBlob(digest) {
    const body = await this.#request('DELETE', `/blobs/${escDigest(digest)}`);
    return Boolean(body.deleted);
  }

  /**
   * Stop a request that is running.
   *
   * `requestId` is either the server's own id — from `X-Zygo-Request-Id`, or
   * from a result that has already come back — or the `key` the call was made
   * with, which is the only name a caller has for a request that has not
   * answered yet.
   *
   * Answers as soon as the kill has been sent, not when the request has
   * stopped: whoever is waiting on that request gets the outcome, as a
   * {@link Cancelled}. `started` says whether the handler had begun — `false`
   * is the better outcome, because no handler code ran at all.
   *
   * Throws {@link NotFound} when nothing is running under that name, which
   * includes a request that finished a moment ago and one belonging to another
   * tenant.
   */
  async cancel(requestId) {
    return this.#request('DELETE', `/requests/${esc(requestId)}`);
  }

  /**
   * Send a cancel when `signal` aborts, and return a function that unsubscribes.
   *
   * The cancel is fire-and-forget: the caller is already unwinding with their
   * own abort error, and a request that had finished — or a connection that has
   * gone — is "nothing left to stop" rather than something to report over the
   * top of it.
   */
  #onAbort(signal, key) {
    if (!signal || !key) return () => {};
    const send = () => {
      this.cancel(key).catch(() => {});
    };
    if (signal.aborted) {
      send();
      return () => {};
    }
    signal.addEventListener('abort', send, { once: true });
    return () => signal.removeEventListener('abort', send);
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
   * What a tenant may not exceed: `mem`, `cpu`, `pids`, `timeout`, `scratch`,
   * `network`, `allow`.
   *
   * They only ever **narrow**: applied as the minimum of themselves and
   * whatever the function or pool was declared with, so the worst a wrong
   * value can do is give a customer less than they were promised.
   *
   * A value above every ceiling the tenant currently has is refused with 422,
   * naming the key — it could not take effect, and storing it would leave you
   * believing you had tightened something you had not.
   *
   * Partial: the keys you pass are set and the rest are left alone.
   * Operator-only, and needs deploy rights.
   */
  async setLimits(tenant, limits) {
    const body = await this.#request('PATCH', `/tenants/${esc(tenant)}/limits`, {
      body: limits,
    });
    return body.tenant ?? {};
  }

  /**
   * Store one of a tenant's secrets, and get back their names.
   *
   * The body is the value itself, and the host encrypts it the moment it
   * lands. Operator-only, and needs deploy rights.
   */
  async putSecret(tenant, name, value) {
    const body = await this.#request('PUT', `/tenants/${esc(tenant)}/secrets/${esc(name)}`, {
      rawBody: Buffer.from(String(value)),
    });
    return body.secrets ?? [];
  }

  /**
   * The **names** a tenant has. There is no way to read a value back.
   *
   * A tenant may read its own; anything else is the operator's.
   */
  async secrets(tenant) {
    const body = await this.#request('GET', `/tenants/${esc(tenant)}/secrets`);
    return body.secrets ?? [];
  }

  /** Forget one. Operator-only, and needs deploy rights. */
  async deleteSecret(tenant, name) {
    const body = await this.#request('DELETE', `/tenants/${esc(tenant)}/secrets/${esc(name)}`);
    return body.secrets ?? [];
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
   * `deps` is an id from {@link putDeps}. A pool named against one that is
   * still building **throws** rather than starting: a zygote warmed without
   * the dependencies it was promised serves requests that fail at import. The
   * right reaction is to wait and send the same call again.
   *
   * Needs an API started with `--allow-deploy`.
   */
  async serveRuntime(name, layer, { baseDir, deps } = {}) {
    const body = { name, layer: { ...layer } };
    if (baseDir !== undefined) {
      const { resolve: resolvePath } = await import('node:path');
      body.base_dir = resolvePath(baseDir);
    }
    if (deps !== undefined) body.deps = deps;
    return this.#request('POST', '/runtimes', { body });
  }

  /**
   * Build a dependency set from a lockfile, inside `image`.
   *
   * The API-native half of the spec's `requirements`, which names a file on
   * the Zygo host — the one thing an embedder has none of. Send the files
   * themselves: `{ 'package.json': …, 'package-lock.json': … }` for Node, or
   * `{ 'requirements.txt': … }` for Python.
   *
   * **Answers before the build finishes.** An `npm ci` is minutes, so this
   * returns as soon as the files are on disk, with `state === 'building'`.
   * Idempotent by content: the same files against the same image are the same
   * id, however many callers send them.
   *
   * The build runs in a sandbox that can reach the package registries and
   * nothing else, because installing a package runs that package's code.
   */
  async putDeps(image, files) {
    const encoded = {};
    for (const [name, content] of Object.entries(files)) {
      encoded[name] = Buffer.from(content).toString('base64');
    }
    return depsOf(await this.#request('POST', '/deps', { body: { image, files: encoded } }));
  }

  /** One dependency set with its build log, or every one you can see. */
  async deps(id) {
    if (id === undefined) {
      const body = await this.#request('GET', '/deps');
      return (body.deps ?? []).map(depsOf);
    }
    return depsOf(await this.#request('GET', `/deps/${esc(id)}`));
  }

  /**
   * Forget a dependency set.
   *
   * Refused while a pool is built on it: the pool holds a read-only mount of
   * the directory, and removing it under a warm zygote would leave the pool
   * serving requests whose imports fail one at a time. Stop the pool first.
   */
  async deleteDeps(id) {
    const body = await this.#request('DELETE', `/deps/${esc(id)}`);
    return Boolean(body.deleted ?? false);
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
  async runScript(runtime, script, event = null, { entryPoint, timeout, key, signal, workspace, out } = {}) {
    const body = {
      script: script.startsWith('sha256:') ? script : { source: script },
      event,
    };
    if (entryPoint !== undefined) body.entry_point = entryPoint;
    if (workspace !== undefined) body.workspace = workspace;
    const name = key || (signal ? requestKey() : undefined);
    const headers = timeoutHeader(timeout);
    if (name) headers['x-zygo-request-key'] = name;
    const stop = this.#onAbort(signal, name);
    try {
      const path = `/runtimes/${esc(runtime)}/call${out ? '?out=1' : ''}`;
      const answer = await this.#request('POST', path, { body, headers });
      return parseResult(answer);
    } finally {
      stop();
    }
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

  /**
   * Send a request and resolve with the *response object*, unread.
   *
   * What {@link stream} needs and {@link #request} does not: a stream's body
   * has no end to wait for, so the caller reads it a line at a time. A fresh
   * agent rather than the pooled one, because this connection is in use for
   * as long as the handler runs and a pooled connection is one that is
   * finished with.
   */
  #open(method, path, body, headers) {
    const payload = Buffer.from(JSON.stringify(body === undefined ? null : body));
    const sent = {
      'content-type': 'application/json',
      'content-length': String(payload.length),
      ...headers,
    };
    if (this.token) sent.authorization = `Bearer ${this.token}`;
    if (this._tenantId) sent['x-zygo-tenant'] = this._tenantId;

    const options = { method, path, headers: sent, agent: false, timeout: this.timeout };
    if (this.endpoint.isUnix) options.socketPath = this.endpoint.socketPath;
    else {
      options.host = this.endpoint.host;
      options.port = this.endpoint.port;
    }

    return new Promise((resolvePromise, reject) => {
      const request = this._transport.request(options, resolvePromise);
      request.on('error', (e) =>
        reject(new TransportError(`${method} ${path} failed against ${this.endpoint.url}: ${e.message}`))
      );
      request.end(payload);
    });
  }

  #request(method, path, { body = undefined, headers = {}, authenticated = true, rawBody = undefined, binary = false } = {}) {
    // `rawBody` is for the one route whose body is not JSON: a script is a
    // file, and wrapping its bytes in a JSON string to unwrap them again is a
    // transformation with no reader.
    const payload = rawBody !== undefined ? rawBody : body === undefined ? null : Buffer.from(JSON.stringify(body));
    const sent = { accept: 'application/json', ...headers };
    if (payload !== null) {
      sent['content-type'] =
        rawBody === undefined
          ? 'application/json'
          : binary
            ? 'application/octet-stream'
            : 'text/plain; charset=utf-8';
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

/**
 * A name for one request, unique enough that a cancel finds only it.
 *
 * 128 bits from the crypto source. The server checks ownership as well, so this
 * is the second lock rather than the only one.
 */
function requestKey() {
  return 'k-' + randomBytes(16).toString('hex');
}

/// One line of a streaming call. See {@link Client#stream}.
function parseEvent(raw) {
  if (typeof raw.stream === 'string') {
    return { kind: raw.stream, data: String(raw.data ?? '') };
  }
  return { kind: 'result', result: parseResult(raw), status: Number(raw.status ?? 200) };
}

/// Newline-delimited JSON off a response, a line at a time as it arrives.
async function* lines(response) {
  let buffer = '';
  for await (const chunk of response) {
    buffer += chunk.toString('utf8');
    let at;
    while ((at = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, at).trim();
      buffer = buffer.slice(at + 1);
      if (line) yield line;
    }
  }
  const last = buffer.trim();
  if (last) yield last;
}

/// The whole of a response as JSON. For the refusal that is not a stream.
async function readJson(response) {
  const parts = [];
  for await (const chunk of response) parts.push(chunk);
  try {
    return JSON.parse(Buffer.concat(parts).toString('utf8') || '{}');
  } catch {
    return {};
  }
}

/// `?workspace=…&out=1`, for the route whose body is the event itself.
///
/// Only a *blob* can be named here: an inline tar in a URL would be a megabyte
/// of base64 in a request line, which every proxy in between has an opinion
/// about. Use a pool's body for that.
function workspaceQuery(blob, out) {
  const parts = [];
  if (blob !== undefined && blob !== null) {
    if (typeof blob !== 'string' || !blob.startsWith('sha256:')) {
      throw new SpecError(`\`${blob}\` is not a blob digest; store one with putBlob()`);
    }
    parts.push(`workspace=${blob}`);
  }
  if (out) parts.push('out=1');
  return parts.length ? `?${parts.join('&')}` : '';
}

function parseResult(raw) {
  return {
    result: raw.result ?? null,
    requestId: String(raw.request_id ?? ''),
    // Already decoded: a caller wanting files back should get files, not an
    // encoding to undo.
    workspace: typeof raw.workspace === 'string' ? Buffer.from(raw.workspace, 'base64') : null,
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
 * One dependency set as a caller reads it.
 *
 * `ready` and `building` as booleans beside the string, because every caller
 * writes one of those two comparisons and a typo in `'buidling'` is a
 * condition that is silently never true.
 */
function depsOf(raw) {
  const state = String(raw.state ?? 'building');
  return {
    id: String(raw.id ?? ''),
    state,
    ready: state === 'ready',
    building: state === 'building',
    kind: String(raw.kind ?? ''),
    image: String(raw.image ?? ''),
    error: raw.error ?? null,
    log: String(raw.log ?? ''),
    files: raw.files ?? {},
    // A dependency set is shared by content, so two customers who send the
    // same lockfile share one build and both are listed. Empty is the
    // operator's own.
    tenants: raw.tenants ?? [],
  };
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
