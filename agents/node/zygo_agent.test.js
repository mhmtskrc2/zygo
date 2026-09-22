// Unit tests for the Node reference agent.
//
// What is under test here is the parts that are decisions rather than
// conversation: framing, output truncation, and how the agent answers the
// supervisor's `ZYGO_CHILD_SECCOMP`. The conversation itself is covered by
// `zygo agent test`, which drives a real agent over a real socket — there is
// no point in mocking a protocol that has a conformance suite.
//
//     node --test agents/node/*.test.js

'use strict';

const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { test } = require('node:test');
const { PassThrough } = require('node:stream');

const agent = require('./zygo_agent.js');

const CHILD_SECCOMP_ENV = 'ZYGO_CHILD_SECCOMP';
const HELPER_ENV = 'ZYGO_CHILD_SECCOMP_HELPER';

/// Run `body` with the environment `env`, and put the environment back.
function withEnv(env, body) {
  const saved = new Map();
  for (const [key, value] of Object.entries(env)) {
    saved.set(key, process.env[key]);
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
  try {
    return body();
  } finally {
    for (const [key, value] of saved) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  }
}

/// A plausible filter: `n` instructions of eight bytes each.
function filterOf(instructions) {
  return Buffer.alloc(instructions * 8, 7).toString('base64');
}

test('a frame is a big-endian length and that many bytes of JSON', () => {
  // A stub rather than a `PassThrough`: the agent's own reader would consume
  // what this test is trying to look at.
  const sent = [];
  const socket = { on() {}, write: (chunk) => sent.push(chunk) };
  const wire = new agent.Framing(socket, () => {}, () => {});
  wire.send({ type: 'PONG', seq: 7 });

  const written = Buffer.concat(sent);
  assert.strictEqual(written.readUInt32BE(0), written.length - 4);
  assert.deepStrictEqual(JSON.parse(written.subarray(4).toString('utf8')), {
    type: 'PONG',
    seq: 7,
  });
});

test('a frame split across chunks is one message, and two in one chunk are two', () => {
  const socket = new PassThrough();
  const seen = [];
  new agent.Framing(socket, (message, bad) => seen.push(bad === null ? message.type : bad), () => {});

  const frame = (body) => {
    const bytes = Buffer.from(JSON.stringify(body), 'utf8');
    const header = Buffer.alloc(4);
    header.writeUInt32BE(bytes.length, 0);
    return Buffer.concat([header, bytes]);
  };

  const first = frame({ type: 'PING' });
  socket.write(first.subarray(0, 3));
  assert.deepStrictEqual(seen, [], 'half a header is not a message');
  socket.write(first.subarray(3));
  assert.deepStrictEqual(seen, ['PING']);

  socket.write(Buffer.concat([frame({ type: 'GO' }), frame({ type: 'SHUTDOWN' })]));
  assert.deepStrictEqual(seen, ['PING', 'GO', 'SHUTDOWN']);
});

test('a frame that is not a message is reported, and the stream stays aligned', () => {
  const socket = new PassThrough();
  const seen = [];
  new agent.Framing(socket, (message, bad) => seen.push(bad === null ? message.type : `bad: ${bad}`), () => {});

  const body = Buffer.from('{ this is not json', 'utf8');
  const header = Buffer.alloc(4);
  header.writeUInt32BE(body.length, 0);
  socket.write(Buffer.concat([header, body]));

  assert.strictEqual(seen.length, 1);
  assert.match(seen[0], /^bad: body is not valid JSON/);

  // Still aligned: the next frame is read normally.
  const next = Buffer.from(JSON.stringify({ type: 'PING' }), 'utf8');
  const nextHeader = Buffer.alloc(4);
  nextHeader.writeUInt32BE(next.length, 0);
  socket.write(Buffer.concat([nextHeader, next]));
  assert.deepStrictEqual(seen[1], 'PING');
});

test('an announced length past the cap closes the connection rather than allocating', () => {
  const socket = new PassThrough();
  let closedBecause = 'still open';
  new agent.Framing(socket, () => {}, (why) => (closedBecause = why));

  const header = Buffer.alloc(4);
  header.writeUInt32BE(agent.MAX_FRAME_BYTES + 1, 0);
  socket.write(header);

  assert.match(closedBecause, /announced a 33554433 byte frame/);
});

test('captured output keeps the first bytes and counts the rest', () => {
  const ring = new agent.Ring(16);
  ring.write(Buffer.from('0123456789'));
  ring.write(Buffer.from('abcdefghij'));
  ring.write(Buffer.from('!!!'));

  const value = ring.value();
  assert.ok(value.startsWith('0123456789abcdef'), value);
  assert.match(value, /… 7 bytes truncated$/);
});

test('output that fits is returned untouched', () => {
  const ring = new agent.Ring(agent.RING_BUFFER_BYTES);
  ring.write(Buffer.from('to-stdout\n'));
  assert.strictEqual(ring.value(), 'to-stdout\n');
});

test('no ZYGO_CHILD_SECCOMP is no plan at all', () => {
  withEnv({ [CHILD_SECCOMP_ENV]: undefined, [HELPER_ENV]: undefined }, () => {
    assert.strictEqual(agent.childFilterPlan(), null);
  });
});

test('a malformed filter is a start-up failure, not a silent skip', () => {
  withEnv({ [CHILD_SECCOMP_ENV]: 'not base64!!', [HELPER_ENV]: undefined }, () => {
    assert.throws(() => agent.childFilterPlan(), /is not base64/);
  });
  // Base64 of something that is not a whole number of BPF instructions.
  withEnv({ [CHILD_SECCOMP_ENV]: Buffer.alloc(9).toString('base64'), [HELPER_ENV]: undefined }, () => {
    assert.throws(() => agent.childFilterPlan(), /8-byte BPF instructions/);
  });
});

test('a helper object, when there is one, is preferred to the permission model', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'zygo-agent-'));
  const helper = path.join(dir, 'pretend.so');
  fs.writeFileSync(helper, '');
  try {
    withEnv({ [CHILD_SECCOMP_ENV]: filterOf(3), [HELPER_ENV]: helper }, () => {
      const plan = agent.childFilterPlan();
      assert.strictEqual(plan.how, 'seccomp');
      assert.strictEqual(plan.helper, helper);
      assert.strictEqual(plan.instructions, 3);
    });
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('without a helper the worker runs under Node\'s own permission model', (t) => {
  if (agent.permissionFlag() === null) {
    t.skip(`Node ${process.versions.node} has no permission model`);
    return;
  }
  withEnv({ [CHILD_SECCOMP_ENV]: filterOf(12), [HELPER_ENV]: '/nonexistent/helper.so' }, () => {
    const plan = agent.childFilterPlan();
    assert.strictEqual(plan.how, 'node-permission');
    assert.match(plan.flag, /^--(experimental-)?permission$/);
  });
});

test('a handler that exports nothing callable is named, not silently absent', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'zygo-agent-'));
  const handler = path.join(dir, 'handler.js');
  fs.writeFileSync(handler, 'module.exports = { notAHandler: 1 };\n');
  try {
    assert.throws(() => agent.loadHandler(handler, 'function'), /exports no handler/);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a handler may export the function directly or as `handler`', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'zygo-agent-'));
  const direct = path.join(dir, 'direct.js');
  const named = path.join(dir, 'named.js');
  fs.writeFileSync(direct, 'module.exports = (event) => event;\n');
  fs.writeFileSync(named, 'module.exports.handler = (event) => event;\n');
  try {
    assert.strictEqual(agent.loadHandler(direct, 'function')({ n: 1 }).n, 1);
    assert.strictEqual(agent.loadHandler(named, 'function')({ n: 2 }).n, 2);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/// What the supervisor puts in `script.digest`, computed here rather than
/// taken from the agent — the two have to agree without sharing code.
function digestOf(source) {
  return 'sha256:' + require('node:crypto').createHash('sha256').update(source).digest('hex');
}

test('a script whose bytes do not match its digest is refused before it runs', () => {
  // The check that makes `path` delivery safe on a shared uid: the worker
  // about to load `/run/script/<hash>` can unlink it and write its own there,
  // for itself or for another tenant's request on the same pool zygote. The
  // digest arrives on the supervisor's connection, which it cannot reach.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'zygo-agent-'));
  const script = path.join(dir, 'swapped.js');
  fs.writeFileSync(script, 'module.exports = () => "mine";\n');
  try {
    assert.throws(
      () => agent.loadRequestScript({ path: script, digest: digestOf('what was asked for') }),
      (e) => e instanceof agent.ScriptDigestMismatch && /refusing to run it/.test(e.message)
    );
    // The positive path, without which the above proves nothing.
    const honest = fs.readFileSync(script);
    assert.strictEqual(
      agent.loadRequestScript({ path: script, digest: digestOf(honest) })(),
      'mine'
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('an inline script is checked against its digest too', () => {
  const source = 'module.exports = () => "inline";\n';
  assert.throws(
    () => agent.loadRequestScript({ source, digest: digestOf('something else') }),
    agent.ScriptDigestMismatch
  );
  assert.strictEqual(agent.loadRequestScript({ source, digest: digestOf(source) })(), 'inline');
  // The field is optional: an `EXEC` without one still works.
  assert.strictEqual(agent.loadRequestScript({ source })(), 'inline');
});

test('a script with types and an enum runs, and its digest is over what was uploaded', () => {
  // Types are the easy half: Node strips those itself. An `enum` is the half
  // that needs the transform, and it is in here because a tenant who writes
  // TypeScript writes TypeScript, not the subset that happens to blank out.
  const source = [
    'enum Size { Small = 1, Large = 2 }',
    'interface Order { item: string; size: Size }',
    'export function handler(event: Order): { item: string; units: number } {',
    '  const units: number = event.size === Size.Large ? 2 : 1;',
    '  return { item: event.item, units };',
    '}',
    '',
  ].join('\n');

  // Over the source as uploaded, not over the JavaScript this agent makes of
  // it: the supervisor hashes what the tenant sent, and a digest over anything
  // else would fail on every request.
  const handler = agent.loadRequestScript({ source, digest: digestOf(source) });
  assert.deepStrictEqual(handler({ item: 'desk', size: 2 }), { item: 'desk', units: 2 });
});

test('a handler file may be TypeScript too', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'zygo-agent-'));
  const handler = path.join(dir, 'handler.ts');
  fs.writeFileSync(
    handler,
    'enum Mode { One = 1 }\n' +
      'export function handler(event: { n: number }): number {\n' +
      '  return event.n + Mode.One;\n' +
      '}\n'
  );
  try {
    assert.strictEqual(agent.loadHandler(handler, 'function')({ n: 41 }), 42);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('JavaScript that is simply broken is reported as JavaScript', () => {
  // The language is decided by what the source is, not by a file extension
  // there is none of — so the danger is a JavaScript syntax error being
  // reported as a TypeScript one, or worse, stripped into something that runs.
  // Stripping a file with no types in it returns it unchanged, which is what
  // this leans on.
  assert.throws(
    () => agent.loadRequestScript({ source: 'module.exports = function ( {\n' }),
    (e) => e instanceof SyntaxError && !/TypeScript/i.test(e.message)
  );
});

test('a module whose body throws is not compiled a second time', () => {
  // A `SyntaxError` raised *by the body* — `JSON.parse` of bad input is the
  // everyday one — must not send the loader back for a second `_compile`,
  // which would run every side effect above it twice.
  const source = [
    'globalThis.__zygoRanTwice = (globalThis.__zygoRanTwice || 0) + 1;',
    "JSON.parse('{');",
    '',
  ].join('\n');
  try {
    assert.throws(() => agent.loadRequestScript({ source }), SyntaxError);
    assert.strictEqual(globalThis.__zygoRanTwice, 1);
  } finally {
    delete globalThis.__zygoRanTwice;
  }
});

test('a signal is reported by its own number, not as SIGKILL whatever happened', () => {
  if (process.platform === 'win32') return;
  assert.strictEqual(agent.signalNumber('SIGKILL'), os.constants.signals.SIGKILL);
  assert.strictEqual(agent.signalNumber('SIGSEGV'), os.constants.signals.SIGSEGV);
  assert.notStrictEqual(agent.signalNumber('SIGSEGV'), agent.signalNumber('SIGKILL'));
  // A name this platform does not have still produces something usable.
  assert.strictEqual(agent.signalNumber('SIGNOTREAL'), 9);
  assert.strictEqual(agent.signalNumber(null), 9);
});

test('the seccomp filter count is read from /proc where there is one', () => {
  const count = agent.seccompFilterCount();
  if (process.platform !== 'linux') {
    assert.strictEqual(count, null, 'there is no /proc/self/status here');
    return;
  }
  assert.ok(count === null || Number.isInteger(count), `got ${count}`);
});
