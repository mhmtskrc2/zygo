/**
 * Type declarations for the Zygo client.
 *
 * Hand-written rather than generated, because the package has no build step:
 * it ships the JavaScript that runs, and a declaration file beside it. That
 * keeps `npm install zygo` free of a compiler and keeps what you read in the
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
  readonly ok: boolean;
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
