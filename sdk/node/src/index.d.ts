/**
 * Type declarations for the Zygo client.
 *
 * Hand-written rather than generated, because the package has no build step:
 * it ships the JavaScript that runs, and a declaration file beside it. That
 * keeps `npm install zygo-sdk` free of a compiler and keeps what you read in the
 * repository identical to what executes.
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
  /** What this request's process wrote, separately from the zygote's output. */
  stdout: string;
  stderr: string;
  metrics: Metrics;
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
}

export interface FunctionHandle<T = unknown> {
  (event?: unknown, options?: CallOptions): Promise<Result<T>>;
  batch(events: unknown[], options?: CallOptions): Promise<Array<Result<T> | ZygoError>>;
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
}

export declare class Client {
  constructor(url?: string, options?: ClientOptions);
  readonly endpoint: { url: string; socketPath?: string; host: string; port: number; tls: boolean; isUnix: boolean };
  token: string | null;
  timeout: number;

  close(): void;

  version(): Promise<{ version: string; api: number; control: number; deploy: boolean }>;
  health(): Promise<{ ok: boolean; uptime_s: number }>;
  functions(): Promise<FunctionStatus[]>;
  stats(name: string): Promise<FunctionStatus>;
  warm(name: string): Promise<Record<string, unknown>>;

  call<T = unknown>(name: string, event?: unknown, options?: CallOptions): Promise<Result<T>>;
  batch<T = unknown>(name: string, events: unknown[], options?: CallOptions): Promise<Array<Result<T> | ZygoError>>;
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

  /** Needs an API started with `--allow-deploy`. */
  serveRuntime(name: string, layer: Layer, options?: { baseDir?: string }): Promise<Record<string, unknown>>;
  runtimes(): Promise<RuntimePool[]>;
  /** Needs an API started with `--allow-deploy`. */
  stopRuntime(name: string): Promise<string[]>;
  runScript<T = unknown>(
    runtime: string,
    script: string,
    event?: unknown,
    options?: { entryPoint?: string; timeout?: number }
  ): Promise<Result<T>>;

  /** Needs an API started with `--allow-deploy`. */
  putScript(source: string): Promise<Script>;
  script(digest: string): Promise<Script>;
  /** Needs an API started with `--allow-deploy`. */
  deleteScript(digest: string): Promise<boolean>;

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
