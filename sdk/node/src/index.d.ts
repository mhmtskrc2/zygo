// SPDX-License-Identifier: Apache-2.0
/**
 * Type declarations for the Zygo client.
 *
 * Hand-written rather than generated, because the package has no build step:
 * it ships the JavaScript that runs, and a declaration file beside it. That
 * keeps `npm install zygo-sdk` free of a compiler and keeps what you read in the
 * repository identical to what executes. A test (`test/types.test.js`) holds
 * this file to the JavaScript: every export and every client method has to be
 * declared here, so the two cannot drift apart unnoticed.
 */

export declare const DEFAULT_URL: string;

export declare class ZygoError extends Error {}
/** The API could not be reached, or answered with something unreadable. */
export declare class TransportError extends ZygoError {}
/** The token was missing, wrong, or not permitted to do this. */
export declare class AuthError extends ZygoError {}
/** No function under that name. */
export declare class NotFound extends ZygoError {}
/** The sandbox as described could not be resolved. */
export declare class SpecError extends ZygoError {}

/** Backpressure: the request never ran, and retrying is the right answer. */
export declare class Busy extends ZygoError {
  inFlight: number;
  queued: number;
  limit: number;
  /** Seconds, from the server's `Retry-After`. */
  retryAfter: number;
}

/**
 * The host cannot do this *yet*: a 503, and nothing about the request needs
 * changing. A pool named against a dependency set still building
 * (`code === 'deps_building'`), a zygote that failed to warm
 * (`'warm_failed'`), or an API that is draining. The request never ran, so
 * sending it again is safe; `retries` on the client does that for you.
 */
export declare class Unavailable extends ZygoError {
  code: string;
  /** Seconds, from the server's `Retry-After`; 1 when it sent none. */
  retryAfter: number;
}

/**
 * Somebody stopped this request — usually the caller.
 *
 * Deliberately not a {@link Timeout}: "too slow, raise the limit" is the wrong
 * advice for a request somebody stopped on purpose.
 */
export declare class Cancelled extends ZygoError {
  requestId: string;
  stdout: string;
  stderr: string;
  metrics: Metrics;
}

/**
 * The sandbox stopped reporting this request, and it was killed.
 *
 * Not a {@link Timeout}: the work had budget left, so the thing to look at is
 * the function rather than its `timeout`.
 */
export declare class Stuck extends ZygoError {
  requestId: string;
  stdout: string;
  stderr: string;
  metrics: Metrics;
}

/** The request exceeded the function's timeout and was killed. */
export declare class Timeout extends ZygoError {
  stderr: string;
  metrics: Metrics;
}

/** The handler threw. */
export declare class HandlerError extends ZygoError {
  stdout: string;
  stderr: string;
  exitCode: number;
  metrics: Metrics;
}

export interface Metrics {
  wallMs: number;
  cpuMs: number;
  peakRssKb: number;
}

export interface Result<T = unknown> {
  /** The handler's own return value. */
  result: T;
  /** The server's id for this request, which {@link Client.cancel} accepts. */
  requestId: string;
  /** The sandbox's `/out` as a tar, when the call asked for it with `out`; otherwise `null`. */
  workspace: Buffer | null;
  /** What this request's process wrote, separately from the zygote's output. */
  stdout: string;
  stderr: string;
  metrics: Metrics;
}

/**
 * One line of a streaming call: a piece of output while the request runs,
 * then exactly one `result` carrying what {@link Client.call} would have
 * returned.
 */
export type StreamEvent<T = unknown> =
  | { kind: 'stdout' | 'stderr' | 'progress'; data: string }
  | { kind: 'result'; result: Result<T>; status: number };

/** What `health()` answers. `stopping` arrives as an {@link Unavailable} instead. */
export interface Health {
  ok: boolean;
  status: 'ok' | 'degraded' | string;
  uptime_s: number;
  /** Pools below their `min_warm`, when `status` is `degraded`. */
  below_min_warm?: string[];
}

/**
 * A dependency set: a lockfile built inside an image, once, and mounted into
 * every pool that names its id. `ready` and `building` are `state` as
 * booleans, so a typo in the string is not a condition that is never true.
 */
export interface Deps {
  id: string;
  state: 'building' | 'ready' | 'failed' | string;
  ready: boolean;
  building: boolean;
  /** `python` or `node`: which lockfile it was. */
  kind: string;
  image: string;
  /** Why the build failed, when it did. */
  error: string | null;
  /** The build's own output, for a `failed` set. */
  log: string;
  /** The files that were sent, by name. */
  files: Record<string, unknown>;
  /** Tenants that sent these files; empty is the operator's own. */
  tenants: string[];
}

export interface FunctionStatus {
  name: string;
  state: string;
  image: string;
  runtime: string;
  rssKb: number;
  importsMs: number;
  requests: number;
  failures: number;
  /** The answer as it arrived, so a field a newer Zygo added is reachable. */
  raw: Record<string, unknown>;
}

export interface Served {
  name: string;
  /** `started`, `replaced` or `unchanged`. */
  change: string;
  runtime: string;
  rssKb: number;
  importsMs: number;
  warmMs: number;
  warnings: string[];
}

export interface RunResult {
  exitCode: number;
  stdout: string;
  stderr: string;
  /** The launcher's deadline killed it. Distinct from `oomKilled`: both are exit 137. */
  timedOut: boolean;
  /** The kernel killed something in the sandbox for running out of memory. */
  oomKilled: boolean;
  /** Peak resident memory of the sandbox, from its cgroup. */
  peakRssKb: number;
  wallMs: number;
  /**
   * Whether the program ran at all. `false` is Zygo failing to build the
   * sandbox — unavailable, not the program's failure; `phase` says how far
   * it got (`plan`, `start`, or `run` when it ran).
   */
  started: boolean;
  phase: 'plan' | 'start' | 'run' | string;
  readonly ok: boolean;
}

/**
 * One runtime pool: several anonymous zygotes any script can run in.
 *
 * `warm` and `paused` are zygotes that exist; `cold` is the room left between
 * them and `maxWarm`. A pool has no single state — four zygotes of which two
 * are frozen is working and idle at once — so the counts are what is reported.
 */
export interface RuntimePool {
  name: string;
  image: string;
  /** What the agent announced, e.g. `python/3.12.4`. */
  runtime: string;
  warm: number;
  paused: number;
  cold: number;
  min_warm: number;
  max_warm: number;
  in_flight: number;
  queued: number;
  requests: number;
  failures: number;
  rss_kb: number;
  uptime_s: number;
}

/**
 * What a tenant may not exceed.
 *
 * Every key only ever narrows: applied as the minimum of itself and whatever
 * the function or pool was declared with.
 */
export interface TenantLimits {
  mem?: string;
  cpu?: number;
  pids?: number;
  timeout?: string;
  scratch?: string;
  network?: 'none' | 'egress' | 'full' | 'host';
  allow?: string[];
}

/** One customer of whoever embedded Zygo. */
export interface Tenant {
  id: string;
  created_ms: number;
  /** Digests of the scripts registered for this tenant. */
  scripts: string[];
  /** What this tenant may not exceed. Absent when nothing was set. */
  limits?: TenantLimits;
}

/**
 * One API token, as the server holds it — never the secret.
 *
 * `tenant` is absent on an operator token, which may create tenants and pools
 * and mint more tokens; a tenant token registers scripts and calls, for its own
 * tenant only.
 */
export interface Token {
  id: string;
  /** Absent for an operator token. */
  tenant?: string;
  created_ms: number;
  /** When it was revoked, if it was. A revoked token stops resolving. */
  revoked_ms?: number;
}

/** A script the host holds, named by the SHA-256 of its bytes. */
export interface Script {
  /** `sha256:…`, which is the script's name everywhere else. */
  sha256: string;
  size: number;
  /**
   * The store already had exactly these bytes. What deduplication looks like
   * from outside: two tenants registering the same script get one file.
   */
  existed: boolean;
}

export interface LogEntry {
  seq: number;
  at_ms: number;
  text: string;
  [key: string]: unknown;
}

export interface LogPage {
  name: string;
  entries: LogEntry[];
  /** Pass as `after` to continue from here. */
  next: number;
}

/**
 * A sandbox as `sandbox.toml` would describe it. Keys and value syntax are the
 * spec's own, so `mem: '512M'` and `timeout: '30s'` are written as they would
 * be in the file.
 */
export interface Layer {
  image?: string;
  cmd?: string[];
  entry?: string;
  requirements?: string;
  system?: string[];
  mem?: string;
  cpu?: number;
  pids?: number;
  timeout?: string;
  scratch?: string;
  nofile?: number;
  network?: 'none' | 'egress' | 'full' | 'host';
  allow?: string[];
  mounts?: string[];
  env?: Record<string, string>;
  secrets?: string[];
  isolation?: 'ns' | 'gvisor' | 'vm';
  seccomp?: string;
  concurrency?: number;
  idle_timeout?: string;
  cold_after?: string;
  workdir?: string;
  user?: string;
  /** `[runtime.<name>]` only: which agent to warm, and how many zygotes. */
  agent?: string | { agent: string };
  min_warm?: number;
  max_warm?: number;
  [key: string]: unknown;
}

export interface CallOptions {
  /** Seconds this caller will wait. The function's own timeout still wins. */
  timeout?: number;
  /**
   * A name *you* choose for this request, so that something else can stop it
   * with {@link Client.cancel} before it answers. Reusing a key is allowed and
   * means one cancel stops every call under it.
   */
  key?: string;
  /** Aborting it sends the cancel for you. A key is generated if none was given. */
  signal?: AbortSignal;
  /** A blob digest from {@link Client.putBlob} to unpack into the sandbox. */
  workspace?: string;
  /** Ask for the sandbox's `/out` back, as `workspace` on the result. */
  out?: boolean;
}

/** What `batch` accepts: only the timeout, since one key cannot name many requests. */
export interface BatchOptions {
  timeout?: number;
}

export interface RunScriptOptions {
  /** The function in the script to call. The agent's default when absent. */
  entryPoint?: string;
  timeout?: number;
  key?: string;
  signal?: AbortSignal;
  /**
   * Files to put in the sandbox before the script runs: a blob digest, or an
   * inline description, as the API's `workspace` body field takes it.
   */
  workspace?: string | Record<string, unknown>;
  out?: boolean;
}

export interface FunctionHandle<T = unknown> {
  (event?: unknown, options?: CallOptions): Promise<Result<T>>;
  /** The function's name; `name` is taken by every JavaScript function. */
  readonly name_: string;
  batch(events: unknown[], options?: BatchOptions): Promise<Array<Result<T> | ZygoError>>;
  stats(): Promise<FunctionStatus>;
  logs(options?: { after?: number; limit?: number; failed?: boolean }): Promise<LogPage>;
  warm(): Promise<Record<string, unknown>>;
  stop(): Promise<string[]>;
}

export interface ClientOptions {
  /** Defaults to `ZYGO_API_TOKEN`. Pass `null` for an API with no auth. */
  token?: string | null;
  /** Milliseconds one HTTP exchange may take. Defaults to five minutes. */
  timeout?: number;
  /**
   * How many times a *refused* request — {@link Busy} or {@link Unavailable},
   * both meaning it never ran — is sent again before its error reaches you.
   * Nothing else is retried. Defaults to 0.
   */
  retries?: number;
  /**
   * Seconds. Each wait is the longer of the server's `Retry-After` and this,
   * doubled per attempt. Defaults to 1.
   */
  backoff?: number;
  /** Act for one tenant on every call; what {@link Client.forTenant} sets. */
  tenant?: string | null;
}

export declare class Client {
  constructor(url?: string, options?: ClientOptions);
  readonly endpoint: { url: string; socketPath?: string; host: string; port: number; tls: boolean; isUnix: boolean };
  token: string | null;
  timeout: number;
  retries: number;
  backoff: number;

  close(): void;

  version(): Promise<{ version: string; api: number; control: number; deploy: boolean }>;
  /** Needs no token. Throws {@link Unavailable} once the API is stopping. */
  health(): Promise<Health>;
  /**
   * Stop admitting, let what is running finish, then exit. `inFlight` is what
   * was still running when the grace (seconds) ran out. Operator-only, needs
   * deploy rights.
   */
  drain(grace?: number): Promise<{ drained: boolean; inFlight: number }>;
  functions(): Promise<FunctionStatus[]>;
  stats(name: string): Promise<FunctionStatus>;
  warm(name: string): Promise<Record<string, unknown>>;

  call<T = unknown>(name: string, event?: unknown, options?: CallOptions): Promise<Result<T>>;
  /**
   * Call a function and yield its output as it is produced. Breaking out of
   * the loop closes the connection but does not cancel the request; pass a
   * `key` or a `signal` for that.
   */
  stream<T = unknown>(name: string, event?: unknown, options?: CallOptions): AsyncIterable<StreamEvent<T>>;
  /** Run a script in a pool, yielding its output. See {@link stream}. */
  streamScript<T = unknown>(
    runtime: string,
    script: string,
    event?: unknown,
    options?: RunScriptOptions
  ): AsyncIterable<StreamEvent<T>>;
  /**
   * Stop a request that is running, by the server's id or the `key` it was
   * called with. `started` says whether the handler had begun. Throws
   * {@link NotFound} when nothing is running under that name.
   */
  cancel(requestId: string): Promise<{ cancelled: boolean; started: boolean; [key: string]: unknown }>;
  batch<T = unknown>(name: string, events: unknown[], options?: BatchOptions): Promise<Array<Result<T> | ZygoError>>;
  logs(name: string, options?: { after?: number; limit?: number; failed?: boolean }): Promise<LogPage>;

  /** Needs an API started with `--allow-deploy`. */
  serve(
    name: string,
    layer: Layer,
    options?: { baseDir?: string; secrets?: Record<string, string>; ifChanged?: boolean }
  ): Promise<Served>;
  /** Needs an API started with `--allow-deploy`. */
  stop(name: string): Promise<string[]>;
  /** Needs an API started with `--allow-deploy`. */
  run(image: string, cmd?: string[] | null, options?: Layer & { stdin?: string }): Promise<RunResult>;

  /** Operator-only. */
  createTenant(id: string): Promise<Tenant>;
  /** Operator-only. */
  tenants(): Promise<Tenant[]>;
  /** Operator-only. */
  tenant(id: string): Promise<Tenant>;
  /** Operator-only, and needs an API started with `--allow-deploy`. */
  deleteTenant(id: string): Promise<{ deleted: boolean; removed_scripts: string[]; stopped: string[] }>;
  /** A view of this client that acts for one tenant. */
  forTenant(id: string): Client;

  /**
   * What a tenant may not exceed. These only ever narrow; a value above every
   * ceiling the tenant has is a 422. Operator-only, needs deploy rights.
   */
  setLimits(tenant: string, limits: TenantLimits): Promise<Tenant>;

  /** Store one of a tenant's secrets. Operator-only, needs deploy rights. */
  putSecret(tenant: string, name: string, value: string): Promise<string[]>;
  /** The **names** a tenant has. A value cannot be read back. */
  secrets(tenant: string): Promise<string[]>;
  /** Forget one. Operator-only, needs deploy rights. */
  deleteSecret(tenant: string, name: string): Promise<string[]>;

  /**
   * Mint a token and get its secret, once. Operator-only, needs deploy rights.
   *
   * Pass a tenant id for a tenant token, or nothing for an operator token.
   */
  mintToken(tenant?: string | null): Promise<{ token: Token; secret: string }>;
  /** Every token, revoked ones included. Never a secret. Operator-only. */
  tokens(): Promise<Token[]>;
  /** Revoke one, from the next request onwards. Operator-only. */
  revokeToken(id: string): Promise<{ revoked: boolean; token: Token | null }>;

  /**
   * Register a runtime pool. `deps` is an id from {@link putDeps}; a pool
   * named against one still building throws {@link Unavailable}. `secrets`
   * names the secrets each call may receive, from the *calling* tenant's
   * store. Needs an API started with `--allow-deploy`.
   */
  serveRuntime(
    name: string,
    layer: Layer,
    options?: { baseDir?: string; deps?: string; secrets?: string[] }
  ): Promise<Record<string, unknown>>;
  runtimes(): Promise<RuntimePool[]>;
  /** Needs an API started with `--allow-deploy`. */
  stopRuntime(name: string): Promise<string[]>;
  runScript<T = unknown>(
    runtime: string,
    script: string,
    event?: unknown,
    options?: RunScriptOptions
  ): Promise<Result<T>>;

  /** Needs an API started with `--allow-deploy`. */
  putScript(source: string): Promise<Script>;
  script(digest: string): Promise<Script>;
  /** Needs an API started with `--allow-deploy`. */
  deleteScript(digest: string): Promise<boolean>;

  /**
   * Store a tar this host will hold under its digest, to name as `workspace`
   * on calls. The body is the tar itself.
   */
  putBlob(tar: Uint8Array | ArrayBuffer | string): Promise<Script>;
  /** Whether this host holds a blob, and how big it is. */
  blob(digest: string): Promise<Script>;
  /** Forget a blob. Operator-only: the store is shared by digest. */
  deleteBlob(digest: string): Promise<boolean>;

  /**
   * Build a dependency set from a lockfile, inside `image`. Answers before
   * the build finishes, with `building` true; poll {@link deps} or name the
   * id on {@link serveRuntime} and retry the {@link Unavailable}.
   */
  putDeps(image: string, files: Record<string, string | Uint8Array>): Promise<Deps>;
  /** One dependency set with its build log, or every one you can see. */
  deps(id: string): Promise<Deps>;
  deps(): Promise<Deps[]>;
  /** Forget a dependency set. Refused while a pool is built on it. */
  deleteDeps(id: string): Promise<boolean>;

  fn<T = unknown>(name: string): FunctionHandle<T>;
}

export declare function connect(url?: string, options?: ClientOptions): Client;

export declare function parseEndpoint(text: string): {
  url: string;
  socketPath?: string;
  host: string;
  port: number;
  tls: boolean;
  isUnix: boolean;
};
