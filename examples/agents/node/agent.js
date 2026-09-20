#!/usr/bin/env node
// A Zygo warm agent for Node. No dependencies.
//
// Node has no `fork()` in the Unix sense — `child_process.fork` starts a whole
// new process, and starting one costs tens of milliseconds, which is the cost
// the warm protocol exists to take off the request path. So this agent keeps a
// *pool* of workers: each is a Node process that has already loaded the
// handler and is waiting, and a request is handed to an idle one. The worker
// runs exactly one request and exits; a replacement is started as it finishes,
// off the request path. That keeps every requirement in `spec/protocol.md` §3:
// one process per request, a pid announced before any work is done, nothing
// done until `GO`, and a fresh process every time so request *n* cannot see
// what request *n-1* did.
//
//     zygo agent test node -- examples/agents/node/agent.js examples/agents/node/handler.js
//
// Usage: agent.js <handler.js>   with the control socket on descriptor 3.
//        agent.js --worker <handler.js>   (internal: one pooled worker)
//
// Handler contract: `module.exports = async function handler(event) {...}`;
// its return value is the result. Anything it writes to stdout or stderr is
// captured and returned in separate fields.

'use strict';

const net = require('net');
const { fork } = require('child_process');
const path = require('path');

const PROTO = 1;
const WIRE_FD = 3;
const POOL_SIZE = 2;
const MAX_FRAME = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------
// worker
// ---------------------------------------------------------------------------

if (process.argv[2] === '--worker') {
  const handler = require(path.resolve(process.argv[3]));
  // Loaded, and now parked: nothing runs until the agent says `run`, which it
  // only says after the supervisor's `GO`.
  process.on('message', async (msg) => {
    if (msg.type !== 'run') return;
    const started = process.hrtime.bigint();
    let reply;
    try {
      const result = await handler(msg.event);
      reply = { type: 'result', ok: true, result: result === undefined ? null : result };
    } catch (e) {
      reply = { type: 'result', ok: false, error: (e && e.stack) || String(e) };
    }
    reply.wall_ms = Number(process.hrtime.bigint() - started) / 1e6;
    // Let stdout/stderr drain before the exit code is decided by `exit`.
    process.send(reply, () => process.exit(reply.ok ? 0 : 1));
  });
  process.send({ type: 'ready' });
  return;
}

// ---------------------------------------------------------------------------
// agent
// ---------------------------------------------------------------------------

const handlerPath = process.argv[2];
if (!handlerPath) {
  process.stderr.write('usage: agent.js <handler.js>\n');
  process.exit(2);
}

const wire = new net.Socket({ fd: WIRE_FD, readable: true, writable: true });

function send(message) {
  const body = Buffer.from(JSON.stringify(message), 'utf8');
  const header = Buffer.alloc(4);
  header.writeUInt32BE(body.length, 0);
  wire.write(Buffer.concat([header, body]));
}

function error(id, code, message) {
  send({ type: 'ERROR', id: id === undefined ? null : id, code, message });
}

// --- the pool ---------------------------------------------------------------

const idle = []; // workers loaded and waiting
const waiting = []; // EXECs that arrived while every worker was busy
const inflight = new Map(); // request id -> { worker, started }
// A replacement worker takes tens of milliseconds to start, and two requests
// back to back would otherwise see "overloaded" for the second. A short queue
// absorbs that; past it the honest answer is to refuse.
const MAX_WAITING = 64;
let shuttingDown = false;

function spawnWorker() {
  const child = fork(__filename, ['--worker', handlerPath], {
    stdio: ['ignore', 'pipe', 'pipe', 'ipc'],
  });
  const worker = { child, stdout: [], stderr: [], request: null, ready: false };
  child.stdout.on('data', (d) => worker.stdout.push(d));
  child.stderr.on('data', (d) => worker.stderr.push(d));
  child.on('message', (msg) => {
    if (msg.type === 'ready') {
      worker.ready = true;
      // A request that was waiting for a worker gets this one straight away.
      const next = waiting.shift();
      if (next) dispatch(next, worker);
      else idle.push(worker);
    } else if (msg.type === 'result' && worker.request) {
      worker.result = msg;
    }
  });
  child.on('exit', (code, signal) => {
    if (!worker.request) return; // a spare that died: replaced below
    finish(worker, code, signal);
  });
  child.on('exit', () => {
    const at = idle.indexOf(worker);
    if (at >= 0) idle.splice(at, 1);
    if (!shuttingDown) spawnWorker();
  });
  return worker;
}

function finish(worker, code, signal) {
  const { id } = worker.request;
  inflight.delete(id);
  const stdout = Buffer.concat(worker.stdout).toString('utf8');
  const stderr = Buffer.concat(worker.stderr).toString('utf8');
  const r = worker.result;
  const done = {
    type: 'DONE',
    id,
    exit_code: code === null ? 128 + 9 : code,
    result: r && r.ok ? r.result : null,
    stdout,
    stderr,
    wall_ms: r ? r.wall_ms : 0,
    cpu_ms: 0,
    peak_rss_kb: 0,
  };
  if (r && !r.ok) done.error = r.error;
  // A worker that died without a result — killed by its deadline, out of
  // memory — is still answered: silence is not an acceptable outcome.
  if (!r) done.error = `the request process exited with ${signal || code} before producing a result`;
  send(done);
  if (shuttingDown && inflight.size === 0) process.exit(0);
}

// --- messages ---------------------------------------------------------------

function dispatch(msg, worker) {
  worker.request = { id: msg.id, event: msg.event };
  inflight.set(msg.id, worker);
  // The process exists and has loaded the handler, but has not been given
  // the event: it does nothing until GO.
  send({ type: 'FORKED', id: msg.id, pid: worker.child.pid });
}

function onExec(msg) {
  const worker = idle.shift();
  if (worker) return dispatch(msg, worker);
  if (waiting.length >= MAX_WAITING) {
    error(msg.id, 'overloaded', `all ${POOL_SIZE} workers are busy and ${MAX_WAITING} requests are waiting`);
    return;
  }
  waiting.push(msg);
}

function onGo(msg) {
  const worker = inflight.get(msg.id);
  if (!worker || worker.started) return; // a GO for nothing: not an error
  worker.started = true;
  worker.child.send({ type: 'run', event: worker.request.event });
}

function onMessage(msg) {
  switch (msg.type) {
    case 'EXEC':
      return onExec(msg);
    case 'GO':
      return onGo(msg);
    case 'PING':
      return send({ type: 'PONG', seq: msg.seq === undefined ? 0 : msg.seq });
    case 'SHUTDOWN':
      shuttingDown = true;
      for (const w of idle) w.child.kill();
      if (inflight.size === 0) process.exit(0);
      return;
    default:
      return error(msg.id, 'bad_message', `unexpected message \`${msg.type}\``);
  }
}

// --- framing ----------------------------------------------------------------

let pending = Buffer.alloc(0);
wire.on('data', (chunk) => {
  pending = Buffer.concat([pending, chunk]);
  for (;;) {
    if (pending.length < 4) return;
    const len = pending.readUInt32BE(0);
    if (len > MAX_FRAME) {
      // Nothing can be resynchronised past a length like this.
      process.exit(1);
    }
    if (pending.length < 4 + len) return;
    const body = pending.subarray(4, 4 + len);
    pending = pending.subarray(4 + len);
    let msg;
    try {
      msg = JSON.parse(body.toString('utf8'));
    } catch (e) {
      // The frame arrived whole, so the stream is still aligned: report and
      // carry on, as `spec/protocol.md` §3 requires.
      error(null, 'bad_message', `body is not valid JSON: ${e.message}`);
      continue;
    }
    if (!msg || typeof msg !== 'object' || Array.isArray(msg)) {
      error(null, 'bad_message', 'body is not a JSON object');
      continue;
    }
    onMessage(msg);
  }
});
wire.on('end', () => process.exit(0));
wire.on('error', () => process.exit(0));

// --- warm-up ----------------------------------------------------------------

const warming = process.hrtime.bigint();
// The handler is loaded here too, so an import error is reported once, as the
// protocol's `handler_load`, rather than by every worker in turn.
try {
  require(path.resolve(handlerPath));
} catch (e) {
  error(null, 'handler_load', (e && e.stack) || String(e));
  process.exit(1);
}
for (let i = 0; i < POOL_SIZE; i++) spawnWorker();

// READY once the first worker is parked: until then a request would have
// nowhere to go.
const waitReady = setInterval(() => {
  if (idle.length === 0) return;
  clearInterval(waitReady);
  send({
    type: 'READY',
    proto: PROTO,
    pid: process.pid,
    imports_ms: Number(process.hrtime.bigint() - warming) / 1e6,
    rss_kb: Math.round(process.memoryUsage().rss / 1024),
    runtime: `node/${process.versions.node}`,
  });
}, 5);
