#!/usr/bin/env node
// zygo-agent — the reference runtime agent for Node (design doc §3.4.1).
//
// Node has no `fork()` in the Unix sense. `child_process.fork` starts a whole
// new process, and starting one costs tens of milliseconds — which is the cost
// the warm protocol exists to take off the request path. So this agent keeps a
// *pool* of workers: each is a Node process that has already loaded the
// handler and is parked, a request is handed to an idle one, and the worker
// runs exactly that request and exits. Its replacement is started as it
// finishes, off the request path.
//
// That keeps every requirement in `spec/protocol.md` §3: one process per
// request, a pid announced before any work is done, nothing done until `GO`,
// and a fresh process every time, so request *n* cannot observe anything
// request *n-1* did.
//
// Usage:
//     zygo_agent.js --fd <n> <handler.js> [mode]   the supervisor's form
//     zygo_agent.js <handler.js> [mode]            control socket on fd 3
//     zygo_agent.js --worker <handler.js> [mode]   internal: one pooled worker
//
// Handler contract: `module.exports = function handler(event) {…}` — or
// `module.exports.handler` — returning the result, or a promise of it.
// Anything it writes to stdout or stderr is captured and returned separately.
//
// Pure standard library, on purpose: it is loaded into every Node sandbox and
// its own start-up cost is paid by every tenant.

'use strict';

const fs = require('fs');
const net = require('net');
const os = require('os');
const path = require('path');
const { fork } = require('child_process');

const PROTOCOL_VERSION = 1;

/// The control socket, as the launcher leaves it (`zygo_core::pool::AGENT_FD`).
const AGENT_FD = 3;

/// Must match `zygo_core::protocol::frame`.
const MAX_FRAME_BYTES = 32 * 1024 * 1024;

/// Per-request stdout/stderr capture, truncated past this (design doc §3.12).
const RING_BUFFER_BYTES = 256 * 1024;

/// Workers kept loaded and parked.
///
/// Two, not one: a replacement takes tens of milliseconds to start, and back
/// to back requests would otherwise find the pool empty every other time.
const POOL_SIZE = 2;

/// `EXEC`s that may queue while every worker is busy.
///
/// Past this the honest answer is to refuse: a queue longer than this is a
/// supervisor that has already over-admitted, and `overloaded` is a conforming
/// reply.
const MAX_WAITING = 64;

const CHILD_SECCOMP_ENV = 'ZYGO_CHILD_SECCOMP';

/// A shared object whose constructor installs `ZYGO_CHILD_SECCOMP`.
///
/// See `childFilterPlan` — this is the only way a Node process can reach
/// `prctl`, and it is optional.
const CHILD_SECCOMP_HELPER_ENV = 'ZYGO_CHILD_SECCOMP_HELPER';

/// How the worker was told to restrict itself, passed down in its argv.
const CHILD_FILTER_ARG = '--child-filter';

// ---------------------------------------------------------------------------
// The child's tightening
// ---------------------------------------------------------------------------

// `ZYGO_CHILD_SECCOMP` is base64 of a raw seccomp program the supervisor built
// from *this host's* syscall numbers, to be installed with one `prctl` after
// `GO` and before any handler code. Under `strict` it is how the child loses
// `execve` and process creation while the agent keeps them.
//
// A Node process cannot call `prctl`. There is no FFI in the standard library,
// and the filter has to be installed *after* the last `execve` — so the usual
// escape, a launcher that sets the filter and then execs, cannot work either:
// the filter it installs denies the very `execve` it needs next.
//
// So there are two ways down, and the agent says in `READY` which one it took:
//
//   `seccomp`          a helper shared object is available, and its
//                      constructor installed the supervisor's program in the
//                      worker. This is the real thing: a kernel filter that
//                      survives arbitrary code execution inside the worker.
//                      `agents/node/zygo_child_seccomp.c` is forty lines and
//                      builds with one `cc` command; point
//                      `ZYGO_CHILD_SECCOMP_HELPER` at the result, or drop it
//                      next to this file as `zygo_child_seccomp.so`.
//
//   `node-permission`  no helper, so the worker runs under Node's own
//                      permission model instead: no child processes, no
//                      native addons, no WASI. Worker *threads* stay allowed,
//                      because the supervisor's filter refuses a `clone` only
//                      when `CLONE_THREAD` is clear — a fallback stricter
//                      than the filter it stands in for is its own bug, and
//                      the seccomp compatibility matrix caught this one.
//                      What it removes is what `strict` asks for: new
//                      programs and new processes. But it is enforced by Node
//                      rather than by the kernel, so a V8 escape gets past it
//                      where it would not get past a filter. The sandbox's
//                      own seccomp profile is still installed underneath
//                      either way.
//
// If neither is available the agent refuses every request. Running a `strict`
// request with neither restriction is the one outcome that is not allowed.

/// The permission-model flag this Node understands, or `null` if it has none.
function permissionFlag() {
  const major = Number(process.versions.node.split('.')[0]);
  if (major >= 22) return '--permission';
  if (major >= 20) return '--experimental-permission';
  return null;
}

/// How many seccomp filters are installed on this process.
///
/// `/proc/self/status` has reported `Seccomp_filters` since 5.9. It is how the
/// worker checks that the helper actually did something, rather than trusting
/// a `dlopen` that returned.
function seccompFilterCount() {
  let status;
  try {
    status = fs.readFileSync('/proc/self/status', 'utf8');
  } catch {
    return null;
  }
  const found = /^Seccomp_filters:\s*(\d+)$/m.exec(status);
  return found ? Number(found[1]) : null;
}

/// Where a helper shared object might be, most explicit first.
function helperCandidates() {
  const explicit = process.env[CHILD_SECCOMP_HELPER_ENV];
  const beside = path.join(__dirname, 'zygo_child_seccomp.so');
  return explicit ? [explicit, beside] : [beside];
}

/// Decide how a worker will restrict itself, without doing it.
///
/// Runs in the agent, once, so that a misconfiguration is a start-up failure
/// the supervisor sees rather than a per-request one — and so no worker pays
/// for the decision.
///
/// Returns `null` when the supervisor asked for nothing.
function childFilterPlan() {
  const encoded = process.env[CHILD_SECCOMP_ENV];
  if (!encoded) return null;

  // Validated here even though this process never installs it: a value that
  // is not a whole number of 8-byte BPF instructions is a supervisor bug, and
  // finding it now beats finding it in every child. `Buffer.from(…,
  // 'base64')` is lenient — it drops what it cannot read — so the shape of
  // the string is checked before its length is trusted.
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(encoded)) {
    throw new Error(`${CHILD_SECCOMP_ENV} is not base64`);
  }
  const raw = Buffer.from(encoded, 'base64');
  if (raw.length === 0 || raw.length % 8 !== 0) {
    throw new Error(
      `${CHILD_SECCOMP_ENV} is ${raw.length} bytes, not a whole number of ` +
        `8-byte BPF instructions`
    );
  }

  for (const candidate of helperCandidates()) {
    if (candidate && fs.existsSync(candidate)) {
      return { how: 'seccomp', helper: candidate, instructions: raw.length / 8 };
    }
  }

  const flag = permissionFlag();
  if (flag) return { how: 'node-permission', flag, instructions: raw.length / 8 };

  throw new Error(
    `${CHILD_SECCOMP_ENV} is set and this Node (${process.versions.node}) can honour it ` +
      `in neither way: no helper object at ${helperCandidates().join(' or ')}, and no ` +
      `permission model before Node 20. Build agents/node/zygo_child_seccomp.c into the ` +
      `image, or use seccomp = "default"`
  );
}

/// Install the supervisor's program in *this* process. Irreversible, by design.
///
/// `process.dlopen` is the only door Node leaves open: it runs the object's
/// constructors before it looks for an addon entry point, so the filter is in
/// force by the time the "did not self-register" error is thrown.
function installChildFilter(helper) {
  const before = seccompFilterCount();
  try {
    process.dlopen({ exports: {} }, helper, os.constants.dlopen.RTLD_NOW);
  } catch (e) {
    // Expected: the helper is not a Node addon and says so *after* its
    // constructor has run. Anything else is a real failure.
    if (!/did not self-register/i.test(String((e && e.message) || e))) throw e;
  }
  const after = seccompFilterCount();
  if (before === null || after === null) {
    throw new Error('cannot read Seccomp_filters from /proc/self/status');
  }
  if (after <= before) {
    throw new Error(`${helper} loaded but installed no filter (still ${after})`);
  }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Length-prefixed JSON over a byte stream.
class Framing {
  constructor(socket, onMessage, onClose) {
    this._socket = socket;
    this._pending = Buffer.alloc(0);
    socket.on('data', (chunk) => {
      this._pending = Buffer.concat([this._pending, chunk]);
      for (;;) {
        if (this._pending.length < 4) return;
        const size = this._pending.readUInt32BE(0);
        if (size > MAX_FRAME_BYTES) {
          // Nothing was consumed and nothing can be resynchronised past a
          // length like this: closing is the only correct answer.
          onClose(`peer announced a ${size} byte frame`);
          return;
        }
        if (this._pending.length < 4 + size) return;
        const body = this._pending.subarray(4, 4 + size);
        this._pending = this._pending.subarray(4 + size);
        let message;
        try {
          message = JSON.parse(body.toString('utf8'));
        } catch (e) {
          // The frame arrived whole, so the stream is still aligned: this is
          // reportable rather than fatal, as `spec/protocol.md` §3 requires.
          onMessage(null, `body is not valid JSON: ${e.message}`);
          continue;
        }
        if (message === null || typeof message !== 'object' || Array.isArray(message)) {
          onMessage(null, `body is ${Array.isArray(message) ? 'an array' : typeof message}, not a JSON object`);
          continue;
        }
        onMessage(message, null);
      }
    });
    socket.on('end', () => onClose(null));
    socket.on('error', () => onClose(null));
  }

  send(message) {
    const body = Buffer.from(JSON.stringify(message), 'utf8');
    if (body.length > MAX_FRAME_BYTES) {
      throw new FrameTooLarge(`frame of ${body.length} bytes exceeds the limit`);
    }
    const header = Buffer.alloc(4);
    header.writeUInt32BE(body.length, 0);
    this._socket.write(Buffer.concat([header, body]));
  }
}

class FrameTooLarge extends Error {}

/// Capture that keeps the first `limit` bytes and counts the rest.
class Ring {
  constructor(limit = RING_BUFFER_BYTES) {
    this._parts = [];
    this._size = 0;
    this._dropped = 0;
    this._limit = limit;
  }

  write(chunk) {
    const room = this._limit - this._size;
    if (room > 0) {
      const piece = chunk.subarray(0, room);
      this._parts.push(piece);
      this._size += piece.length;
      this._dropped += chunk.length - piece.length;
    } else {
      this._dropped += chunk.length;
    }
  }

  value() {
    let text = Buffer.concat(this._parts).toString('utf8');
    if (this._dropped) text += `\n… ${this._dropped} bytes truncated`;
    return text;
  }
}

// ---------------------------------------------------------------------------
// Handler loading
// ---------------------------------------------------------------------------

/// Load the script an `EXEC` carried, and return its entry point.
///
/// **This runs in the worker, not the agent.** A worker serves one request and
/// exits, so a script loaded here cannot be seen by the next one — which is
/// the whole point of a runtime pool: one warm Node per image and dependency
/// set, anonymous, serving ten thousand tenants' scripts without being a place
/// any of them can leave something for another.
///
/// `Module.prototype._compile` rather than `vm.runInNewContext`, because the
/// script needs the ordinary Node surface — `require`, `module`, `__dirname` —
/// and a fresh context has none of it. The module is deliberately *not* put in
/// `require.cache`: nothing here should outlive the request.
function loadRequestScript(script) {
  const Module = require('module');
  const filename = script.path || '/zygo/request-script.js';
  const entryPoint = script.entry_point || 'handler';

  let source = script.source;
  if (source === undefined || source === null) {
    if (!script.path) {
      throw new Error("the request's script carries neither `source` nor `path`");
    }
    source = fs.readFileSync(script.path, 'utf8');
  }

  const loaded = new Module(filename, null);
  loaded.filename = filename;
  loaded.paths = Module._nodeModulePaths(path.dirname(filename));
  loaded._compile(source, filename);

  const exported = loaded.exports;
  const handler =
    typeof exported === 'function' ? exported : exported && exported[entryPoint];
  if (typeof handler !== 'function') {
    throw new Error(
      `${filename} exports no \`${entryPoint}\`; expected ` +
        `\`module.exports = function ${entryPoint}(event)\``
    );
  }
  return handler;
}

function loadHandler(handlerPath, mode) {
  const loaded = require(path.resolve(handlerPath));
  if (mode === 'stdin') {
    // Windmill-style scripts: the module body is the program, and re-running
    // it per request is the contract.
    return (event) => runStdinScript(path.resolve(handlerPath), event);
  }
  const handler = typeof loaded === 'function' ? loaded : loaded && loaded.handler;
  if (typeof handler !== 'function') {
    throw new Error(
      `${handlerPath} exports no handler; expected \`module.exports = function handler(event)\``
    );
  }
  return handler;
}

function runStdinScript(scriptPath, event) {
  const { execFileSync } = require('child_process');
  const out = execFileSync(process.execPath, [scriptPath], {
    input: JSON.stringify(event),
    encoding: 'utf8',
  });
  const trimmed = out.trim();
  return trimmed ? JSON.parse(trimmed) : null;
}

// ---------------------------------------------------------------------------
// Worker: one request
// ---------------------------------------------------------------------------

/// A worker that could not restrict itself as the supervisor asked.
///
/// Distinct from any exit code a handler can produce, so the agent's answer
/// says which happened.
const EXIT_NO_CHILD_FILTER = 121;

function worker(argv) {
  const filterArg = argv.indexOf(CHILD_FILTER_ARG);
  if (filterArg >= 0) {
    // Before the handler is loaded, not merely before it is called: the
    // handler's own import-time code is request code too.
    try {
      installChildFilter(argv[filterArg + 1]);
    } catch (e) {
      process.stderr.write(`zygo: ${(e && e.message) || e}\n`);
      process.exit(EXIT_NO_CHILD_FILTER);
    }
    argv.splice(filterArg, 2);
  }

  const handlerPath = argv[0];
  const mode = argv[1] || 'function';
  // No handler is the *runtime pool* shape: this worker holds an interpreter
  // and its dependencies and no tenant code, and every request brings its own
  // script. See `loadRequestScript`.
  const handler = handlerPath ? loadHandler(handlerPath, mode) : null;

  // Loaded, and now parked: nothing runs until the agent says `run`, which it
  // only says once the supervisor's `GO` has arrived.
  process.on('message', async (message) => {
    if (message.type !== 'run') return;

    process.env.ZYGO_REQUEST_ID = message.id || '';
    process.env.ZYGO_DEADLINE_MS = String(message.timeout_ms || 0);
    for (const [key, value] of Object.entries(message.env_overrides || {})) {
      process.env[key] = value;
    }

    const started = process.hrtime.bigint();
    const cpuBefore = process.cpuUsage();
    let reply;
    try {
      // The script, if this request brought one. Loaded here rather than at
      // worker start-up so that the permission model or the seccomp filter —
      // whichever is standing in for `ZYGO_CHILD_SECCOMP` — is already in
      // force while it loads.
      let run = handler;
      if (message.script) {
        run = loadRequestScript(message.script);
      } else if (run === null) {
        throw new Error(
          'this agent was started without a handler, so every request must ' +
            'carry a `script`'
        );
      }
      const result = await run(message.event);
      // A value that cannot cross the IPC channel is the request's failure,
      // not the worker's: found here rather than as a silent `undefined`.
      JSON.stringify(result === undefined ? null : result);
      reply = { type: 'result', ok: true, result: result === undefined ? null : result };
    } catch (e) {
      reply = { type: 'result', ok: false, error: (e && e.stack) || String(e) };
    }
    const cpu = process.cpuUsage(cpuBefore);
    reply.wall_ms = Number(process.hrtime.bigint() - started) / 1e6;
    reply.cpu_ms = (cpu.user + cpu.system) / 1000;
    // `maxRSS` is kilobytes on Linux and bytes on macOS, the same split
    // `getrusage` has.
    const maxRss = process.resourceUsage().maxRSS;
    reply.peak_rss_kb = process.platform === 'darwin' ? Math.round(maxRss / 1024) : maxRss;

    // Let stdout and stderr drain before the exit code is decided.
    process.send(reply, () => process.exit(reply.ok ? 0 : 1));
  });
  process.send({ type: 'ready' });
}

// ---------------------------------------------------------------------------
// Zygote
// ---------------------------------------------------------------------------

class Agent {
  constructor(socket, { handlerPath, mode, plan }) {
    this._handlerPath = handlerPath;
    this._mode = mode;
    this._plan = plan;
    this._idle = [];
    this._waiting = [];
    this._inflight = new Map();
    this._shuttingDown = false;
    this._wire = new Framing(
      socket,
      (message, bad) => (bad === null ? this._onMessage(message) : this._error(null, 'bad_message', bad)),
      () => this._stop()
    );
  }

  send(message) {
    this._wire.send(message);
  }

  _error(id, code, message) {
    this.send({ type: 'ERROR', id: id === undefined ? null : id, code, message });
  }

  /// Whether a request could be dispatched right now.
  hasIdleWorker() {
    return this._idle.length > 0;
  }

  /// Start a worker: a Node process with the handler loaded and parked.
  spawnWorker() {
    const execArgv = [];
    if (this._plan && this._plan.how === 'node-permission') {
      // `strict` removes new programs and new processes from the child. It
      // does not remove the filesystem, so reads and writes stay as they
      // were, and it does not remove *threads*: the supervisor's filter
      // refuses `clone` only when `CLONE_THREAD` is clear, and a worker
      // thread is a `clone` with it. Denying `--allow-worker` here made the
      // fallback stricter than the filter it stands in for, and the seccomp
      // compatibility matrix caught it — a `worker_threads` handler that
      // works under the kernel filter must not fail under this one.
      execArgv.push(
        this._plan.flag,
        '--allow-fs-read=*',
        '--allow-fs-write=*',
        '--allow-worker'
      );
    }
    const args = ['--worker'];
    if (this._plan && this._plan.how === 'seccomp') {
      args.push(CHILD_FILTER_ARG, this._plan.helper);
    }
    if (this._handlerPath) args.push(this._handlerPath, this._mode);

    const child = fork(__filename, args, {
      // Explicitly empty rather than inherited: whatever flags started the
      // agent are not what a request worker should run under.
      execArgv,
      stdio: ['ignore', 'pipe', 'pipe', 'ipc'],
    });

    const w = { child, stdout: new Ring(), stderr: new Ring(), request: null, started: false, result: null };
    child.stdout.on('data', (d) => w.stdout.write(d));
    child.stderr.on('data', (d) => w.stderr.write(d));
    child.on('message', (message) => {
      if (message.type === 'ready') {
        const next = this._waiting.shift();
        if (next) this._dispatch(next, w);
        else this._idle.push(w);
      } else if (message.type === 'result' && w.request) {
        w.result = message;
      }
    });
    // `close`, not `exit`, for the request's answer. `exit` fires when the
    // process ends, and the worker's stdout and stderr can still have
    // buffered data arriving after it — a handler whose last line raced the
    // exit had that line dropped, intermittently (B-26). `close` fires once
    // every stdio stream is done.
    child.on('close', (code, signal) => {
      if (w.request) this._finish(w, code, signal);
    });
    // The replacement starts on `exit`, which is as soon as the slot is free:
    // there is no reason to wait for the dead worker's pipes to drain first.
    child.on('exit', () => {
      const at = this._idle.indexOf(w);
      if (at >= 0) this._idle.splice(at, 1);
      if (!this._shuttingDown) this.spawnWorker();
    });
    return w;
  }

  _dispatch(request, w) {
    w.request = request;
    this._inflight.set(request.id, w);
    // The process exists and has loaded the handler, but has not been given
    // the event: it does nothing until `GO`.
    this.send({ type: 'FORKED', id: request.id, pid: w.child.pid });
  }

  _finish(w, code, signal) {
    const id = w.request.id;
    this._inflight.delete(id);
    const result = w.result;
    const done = {
      type: 'DONE',
      id,
      exit_code: code === null ? 128 + signalNumber(signal) : code,
      result: result && result.ok ? result.result : null,
      stdout: w.stdout.value(),
      stderr: w.stderr.value(),
      peak_rss_kb: result ? result.peak_rss_kb : 0,
      wall_ms: result ? result.wall_ms : 0,
      cpu_ms: result ? result.cpu_ms : 0,
    };
    if (result && !result.ok) done.error = result.error;
    // A worker that died without a result — killed by its deadline, out of
    // memory, or unable to install the filter it was told to — is still
    // answered. Silence is not an acceptable outcome.
    if (!result) {
      done.error =
        done.exit_code === EXIT_NO_CHILD_FILTER
          ? `the request process could not install ${CHILD_SECCOMP_ENV} and refused to run unfiltered`
          : `the request process exited with ${signal || code} before producing a result`;
    }
    this._sendResult(done);
    if (this._shuttingDown && this._inflight.size === 0) process.exit(0);
  }

  /// Send a `DONE`, or a `DONE` saying why the real one could not go.
  ///
  /// A handler that returns more than the frame limit allows must not take
  /// the agent with it: the request is what failed, not the agent.
  _sendResult(done) {
    try {
      this.send(done);
      return;
    } catch (e) {
      if (!(e instanceof FrameTooLarge)) throw e;
      this.send({
        type: 'DONE',
        id: done.id,
        exit_code: 1,
        error:
          `the handler's result does not fit in one frame (${e.message}); return a ` +
          `reference to it — a path in a writable mount, an object key — rather than the bytes`,
        stdout: '',
        stderr: '',
        peak_rss_kb: done.peak_rss_kb,
        wall_ms: done.wall_ms,
        cpu_ms: done.cpu_ms,
      });
    }
  }

  _onExec(message) {
    const request = {
      id: message.id,
      event: message.event,
      timeout_ms: message.timeout_ms,
      env_overrides: message.env_overrides,
      script: message.script,
    };
    const w = this._idle.shift();
    if (w) return this._dispatch(request, w);
    if (this._waiting.length >= MAX_WAITING) {
      return this._error(
        message.id,
        'overloaded',
        `all ${POOL_SIZE} workers are busy and ${MAX_WAITING} requests are waiting`
      );
    }
    this._waiting.push(request);
  }

  _onGo(message) {
    const w = this._inflight.get(message.id);
    if (!w || w.started) return; // a `GO` for nothing: not worth an error
    w.started = true;
    w.child.send({
      type: 'run',
      id: w.request.id,
      event: w.request.event,
      timeout_ms: w.request.timeout_ms,
      env_overrides: w.request.env_overrides,
      script: w.request.script,
    });
  }

  _onMessage(message) {
    switch (message.type) {
      case 'EXEC':
        return this._onExec(message);
      case 'GO':
        return this._onGo(message);
      case 'PING':
        return this.send({ type: 'PONG', seq: message.seq === undefined ? 0 : message.seq });
      case 'SHUTDOWN':
        return this._stop();
      default:
        return this._error(message.id, 'bad_message', `unexpected message \`${message.type}\``);
    }
  }

  /// Stop taking work, but finish what is already dispatched.
  _stop() {
    this._shuttingDown = true;
    for (const w of this._idle) w.child.kill();
    this._idle.length = 0;
    // Nothing left to wait for, and the queue can never drain now.
    for (const queued of this._waiting) {
      this._error(queued.id, 'internal', 'the agent is shutting down');
    }
    this._waiting.length = 0;
    if (this._inflight.size === 0) process.exit(0);
  }
}

/// A signal's number, for the shell's `128 + n`.
///
/// Node reports the *name*; this used to assume `SIGKILL` and answer 137
/// whatever had happened, so a handler killed by SIGSEGV and one killed by its
/// deadline were indistinguishable (B-26).
function signalNumber(signal) {
  return (signal && os.constants.signals[signal]) || 9;
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

function usage() {
  process.stderr.write(
    'usage: zygo_agent.js (--fd <n> | --worker) [handler.js] [mode]\n' +
      '  with no handler, every request must carry its own `script`\n'
  );
  return 2;
}

function main(argv) {
  if (argv[0] === '--worker') {
    worker(argv.slice(1));
    return 0;
  }

  // Two ways to reach the supervisor, matching the Python agent: an
  // already-connected socket inherited across the clone, or descriptor 3 by
  // convention. The launcher uses `--fd`, so nothing has to exist in the
  // sandbox's filesystem.
  let fd = AGENT_FD;
  let rest = argv;
  if (argv[0] === '--fd') {
    if (argv.length < 3) return usage();
    fd = Number(argv[1]);
    rest = argv.slice(2);
  }
  // A handler is optional: without one this is a runtime pool, and every
  // request carries its own script.
  const handlerPath = rest[0] || null;
  const mode = rest[1] || 'function';

  const socket = new net.Socket({ fd, readable: true, writable: true });

  const warming = process.hrtime.bigint();
  let plan;
  try {
    // Decided here, once: a misconfiguration is then a start-up failure the
    // supervisor sees rather than one every request rediscovers.
    plan = childFilterPlan();
    // Loaded in the agent too, so an import error is reported once, as
    // `handler_load`, rather than by every worker in turn. A pool has
    // nothing to load and nothing to report.
    if (handlerPath) loadHandler(handlerPath, mode);
  } catch (e) {
    // Written straight to the socket: there is no agent yet, and one that
    // cannot load its handler is never going to serve.
    const body = Buffer.from(
      JSON.stringify({
        type: 'ERROR',
        id: null,
        code: 'handler_load',
        message: (e && e.stack) || String(e),
      }),
      'utf8'
    );
    const header = Buffer.alloc(4);
    header.writeUInt32BE(body.length, 0);
    socket.write(Buffer.concat([header, body]));
    return 1;
  }
  const importsMs = Number(process.hrtime.bigint() - warming) / 1e6;

  const agent = new Agent(socket, {
    handlerPath: handlerPath ? path.resolve(handlerPath) : null,
    mode,
    plan,
  });
  for (let i = 0; i < POOL_SIZE; i++) agent.spawnWorker();

  // `READY` once the first worker is parked: until then a request would have
  // nowhere to go.
  const parked = setInterval(() => {
    if (!agent.hasIdleWorker()) return;
    clearInterval(parked);
    agent.send({
      type: 'READY',
      proto: PROTOCOL_VERSION,
      pid: process.pid,
      imports_ms: importsMs,
      rss_kb: Math.round(process.memoryUsage().rss / 1024),
      runtime: `node/${process.versions.node}`,
      // Optional, and ignored by a supervisor that does not know it: which of
      // the two ways this agent is honouring `ZYGO_CHILD_SECCOMP`.
      child_filter: plan ? plan.how : 'none',
    });
  }, 5);
  return 0;
}

if (require.main === module) {
  const code = main(process.argv.slice(2));
  if (code) process.exit(code);
}

module.exports = {
  Framing,
  Ring,
  loadRequestScript,
  childFilterPlan,
  loadHandler,
  permissionFlag,
  seccompFilterCount,
  signalNumber,
  MAX_FRAME_BYTES,
  PROTOCOL_VERSION,
  RING_BUFFER_BYTES,
};
