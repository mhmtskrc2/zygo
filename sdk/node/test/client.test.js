// SPDX-License-Identifier: Apache-2.0
/**
 * What the Node client promises, checked against a stand-in API.
 *
 * These need no Linux, no kernel and no sandbox: what is under test is the
 * client — the transport, the error mapping, the connection pool — and putting
 * a real supervisor behind it would test the supervisor instead, more slowly
 * and on one platform.
 */

import assert from 'node:assert/strict';
import http from 'node:http';
import test from 'node:test';

import { FakeApi } from './fake-api.js';
import {
  AuthError,
  Busy,
  Cancelled,
  HandlerError,
  NotFound,
  Stuck,
  Timeout,
  TransportError,
  Unavailable,
  ZygoError,
  connect,
  parseEndpoint,
} from '../src/index.js';

const OK_RESULT = {
  result: { size: [80, 60] },
  stdout: 'resized\n',
  stderr: '',
  metrics: { wall_ms: 1.7, cpu_ms: 1.2, peak_rss_kb: 2048 },
};

test('every address form is understood', () => {
  const unix = parseEndpoint('unix:///run/user/1000/zygo/api.sock');
  assert.equal(unix.isUnix, true);
  assert.equal(unix.socketPath, '/run/user/1000/zygo/api.sock');

  const tcp = parseEndpoint('http://10.0.0.4:7700');
  assert.deepEqual([tcp.host, tcp.port, tcp.tls], ['10.0.0.4', 7700, false]);
  assert.equal(parseEndpoint('box:9000').port, 9000);
  assert.equal(parseEndpoint('https://zygo.example.com:8443').tls, true);
});

test('an address that cannot work is refused where the mistake is', () => {
  // Rather than as a connection failure thirty seconds later against a host
  // nobody meant.
  assert.throws(() => parseEndpoint('unix://'), TypeError);
  assert.throws(() => parseEndpoint('ftp://host:21'), TypeError);
  assert.throws(() => parseEndpoint('host:not-a-port'), TypeError);
});

test('a call returns the handler’s own value', async () => {
  const api = await FakeApi.start();
  api.answer('POST', '/fn/resize', 200, OK_RESULT);
  const client = connect(api.url, { token: null });
  try {
    const out = await client.fn('resize')({ url: 'http://example.com/a.png' });
    assert.deepEqual(out.result, { size: [80, 60] });
    assert.equal(out.stdout, 'resized\n');
    assert.equal(out.metrics.wallMs, 1.7);
    assert.deepEqual(api.requests[0].body, { url: 'http://example.com/a.png' });
  } finally {
    client.close();
    await api.close();
  }
});

test('a call works over a unix socket', async () => {
  // The transport the local case actually uses: no port, no token, and
  // permissions the operating system enforces. A client that only ever ran
  // over TCP would fail here and nowhere else.
  const api = await FakeApi.start({ unix: true });
  api.answer('POST', '/fn/resize', 200, OK_RESULT);
  const client = connect(api.url, { token: null });
  try {
    const out = await client.call('resize', {});
    assert.deepEqual(out.result, { size: [80, 60] });
  } finally {
    client.close();
    await api.close();
  }
});

test('the token is sent, and the timeout header with it', async () => {
  const api = await FakeApi.start();
  api.answer('POST', '/fn/f', 200, OK_RESULT);
  const client = connect(api.url, { token: 's3cret' });
  try {
    await client.call('f', {}, { timeout: 2.5 });
    assert.equal(api.requests[0].headers.authorization, 'Bearer s3cret');
    assert.equal(api.requests[0].headers['x-zygo-timeout-ms'], '2500');
  } finally {
    client.close();
    await api.close();
  }
});

test('a name that needs escaping reaches the right route', async () => {
  const api = await FakeApi.start();
  api.answer('POST', '/fn/a%2Fb', 200, OK_RESULT);
  const client = connect(api.url, { token: null });
  try {
    await client.call('a/b', {});
    // Without escaping this is `POST /fn/a/b`, which is a different route.
    assert.equal(api.requests[0].path, '/fn/a%2Fb');
  } finally {
    client.close();
    await api.close();
  }
});

test('a handler that threw carries its output', async () => {
  const api = await FakeApi.start();
  api.answer('POST', '/fn/f', 500, {
    error: 'ZeroDivisionError: division by zero',
    stdout: 'before\n',
    stderr: 'Traceback...\n',
    exit_code: 1,
    metrics: { wall_ms: 2 },
  });
  const client = connect(api.url, { token: null });
  try {
    await assert.rejects(
      () => client.call('f', {}),
      (e) => {
        assert.ok(e instanceof HandlerError);
        assert.equal(e.stderr, 'Traceback...\n');
        assert.equal(e.exitCode, 1);
        return true;
      }
    );
  } finally {
    client.close();
    await api.close();
  }
});

test('backpressure is not a failure of the call', async () => {
  // A 429 means the request never ran, so it has to be distinguishable from a
  // handler that failed: the answer to one is to retry, and the answer to the
  // other is to fix the code.
  const api = await FakeApi.start();
  api.answer('POST', '/fn/f', 429, { error: 'at its limit', in_flight: 4, queued: 16, limit: 4 });
  const client = connect(api.url, { token: null });
  try {
    await assert.rejects(
      () => client.call('f', {}),
      (e) => {
        assert.ok(e instanceof Busy);
        assert.ok(!(e instanceof HandlerError));
        assert.equal(e.limit, 4);
        assert.equal(e.retryAfter, 3);
        return true;
      }
    );
  } finally {
    client.close();
    await api.close();
  }
});

test('a deadline kill is its own type', async () => {
  const api = await FakeApi.start();
  api.answer('POST', '/fn/f', 408, { error: 'the request exceeded its timeout', stderr: 'killed\n' });
  const client = connect(api.url, { token: null });
  try {
    await assert.rejects(
      () => client.call('f', {}),
      (e) => e instanceof Timeout && e.stderr === 'killed\n'
    );
  } finally {
    client.close();
    await api.close();
  }
});

test('a refused deploy says which flag turns it on', async () => {
  const api = await FakeApi.start();
  api.answer('POST', '/run', 403, {
    error: 'this API may only call functions that are already served\n  -> start it with `zygo api --allow-deploy`',
  });
  const client = connect(api.url, { token: null });
  try {
    await assert.rejects(
      () => client.run('alpine:3', ['echo', 'hi']),
      (e) => e instanceof AuthError && /--allow-deploy/.test(e.message)
    );
  } finally {
    client.close();
    await api.close();
  }
});

test('an unreachable API is a transport error, not a sandbox one', async () => {
  // The distinction matters: nothing ran, so nothing about the sandbox can be
  // concluded from it.
  const client = connect('http://127.0.0.1:1', { token: null, timeout: 2000 });
  try {
    await assert.rejects(() => client.functions(), TransportError);
  } finally {
    client.close();
  }
});

test('one refused event does not hide the others', async () => {
  // The whole reason `/batch` answers with a status per element. A client that
  // threw on the first bad one would throw away the good answers.
  const api = await FakeApi.start();
  api.answer('POST', '/fn/f/batch', 200, [
    { ...OK_RESULT, status: 200 },
    { status: 429, error: 'at its limit', limit: 4 },
    { ...OK_RESULT, status: 200 },
  ]);
  const client = connect(api.url, { token: null });
  try {
    const answers = await client.batch('f', [{}, {}, {}]);
    assert.equal(answers.length, 3);
    assert.ok(!(answers[0] instanceof ZygoError));
    assert.ok(answers[1] instanceof Busy);
    assert.ok(!(answers[2] instanceof ZygoError));
  } finally {
    client.close();
    await api.close();
  }
});

test('connections are reused between calls', async () => {
  // Keep-alive is what keeps this client's overhead off the warm path: a fresh
  // connection per call would cost more than a warm request does.
  const api = await FakeApi.start();
  api.answer('POST', '/fn/f', 200, OK_RESULT);
  const client = connect(api.url, { token: null });
  try {
    for (let i = 0; i < 5; i += 1) await client.call('f', {});
    assert.equal(api.connections, 1);
  } finally {
    client.close();
    await api.close();
  }
});

test('a connection the server closed is replaced, not reported', async () => {
  // The server hangs up after answering, without saying so — what `zygo api`
  // does to a connection idle for 30 s. The agent usually notices the close
  // before the next call and opens a new socket; when it does not, the call
  // meets a reset with nothing answered and goes once more on a fresh one.
  for (const unix of [false, true]) {
    const api = await FakeApi.start({ unix });
    api.answer('POST', '/fn/f', 200, OK_RESULT);
    api.hangUp = true;
    const client = connect(api.url, { token: null });
    try {
      for (let i = 0; i < 3; i += 1) await client.call('f', {});
      assert.equal(api.requests.length, 3, `unix=${unix}`);
    } finally {
      client.close();
      await api.close();
    }
  }
});

test('a reused connection the server drops is sent again, once', async () => {
  // The server closes a pooled connection the moment it is reused, without
  // answering. Every call after the first meets that, goes once more on a
  // fresh connection, and is answered there: three calls, three answers,
  // three connections.
  const api = await FakeApi.start();
  api.answer('POST', '/fn/f', 200, OK_RESULT);
  api.dropReused = true;
  const client = connect(api.url, { token: null });
  try {
    for (let i = 0; i < 3; i += 1) await client.call('f', {});
    assert.equal(api.requests.length, 3);
    assert.equal(api.connections, 3);
  } finally {
    client.close();
    await api.close();
  }
});

test('a fresh connection that fails is reported, not retried', async () => {
  // Only a reused socket earns a second attempt. A new one the server drops
  // is a broken server, and one connection is all it gets.
  const api = await FakeApi.start();
  api.answer('POST', '/fn/f', 200, OK_RESULT);
  api.drop = true;
  const client = connect(api.url, { token: null });
  try {
    await assert.rejects(client.call('f', {}), TransportError);
    assert.equal(api.connections, 1);
  } finally {
    client.close();
    await api.close();
  }
});

test('a pooled connection is dropped before the server would close it', async () => {
  // `zygo api` hangs up on a connection idle for 30 s. The agent's `timeout`
  // is what drops a free socket before that; it must stay under the server's
  // figure, and it must not touch a socket with a request in flight. The
  // second half is checked with a short-fused agent through the internal seam.
  const api = await FakeApi.start();
  api.answer('POST', '/fn/f', 200, OK_RESULT);
  const client = connect(api.url, { token: null });
  const quick = connect(api.url, { token: null, agent: new http.Agent({ keepAlive: true, timeout: 100 }) });
  try {
    assert.ok(client._agent.options.timeout <= 25_000, 'the idle limit is not under the server\'s 30 s');

    await quick.call('f', {});
    await new Promise((r) => setTimeout(r, 300));
    await quick.call('f', {});
    assert.equal(api.connections, 2, 'the idle socket was reused past the limit');

    api.delay = 300;
    await quick.call('f', {}); // longer than the idle limit, and still answered
    assert.equal(api.connections, 2, 'a socket with a request in flight was cut');
  } finally {
    quick.close();
    client.close();
    await api.close();
  }
});

test('concurrent callers are concurrent at the socket', async () => {
  // One pooled connection would serialise these, and elapsed time is the only
  // thing that can tell the difference: eight calls against a server that
  // holds each for 200 ms take 1.6 s in a queue and about 200 ms in parallel.
  const api = await FakeApi.start();
  api.answer('POST', '/fn/f', 200, OK_RESULT);
  api.delay = 200;
  const client = connect(api.url, { token: null });
  try {
    const started = Date.now();
    await Promise.all(Array.from({ length: 8 }, () => client.call('f', {})));
    const elapsed = Date.now() - started;
    assert.ok(elapsed < 1000, `the calls were serialised behind one connection (${elapsed} ms)`);
    assert.ok(api.connections >= 2);
  } finally {
    client.close();
    await api.close();
  }
});

test('running out of memory and out of time are different answers', async () => {
  // Both are SIGKILL, so both are exit 137. A caller deciding between "too
  // slow" and "too much memory" needs two values, not two readings of one.
  const api = await FakeApi.start();
  api.answer('POST', '/run', 200, {
    exit_code: 137,
    stdout: '',
    stderr: '',
    timed_out: false,
    oom_killed: true,
    peak_rss_kb: 65536,
    wall_ms: 412.7,
  });
  const client = connect(api.url, { token: null });
  try {
    const starved = await client.run('python:3.12-slim', ['python3', '-c', 'b=bytearray(1<<30)']);
    assert.equal(starved.exitCode, 137);
    assert.equal(starved.oomKilled, true);
    assert.equal(starved.timedOut, false);
    assert.equal(starved.peakRssKb, 65536);
    assert.equal(starved.ok, false);
    assert.equal(starved.started, true, 'an API that omits it only answered once the program ran');
    assert.equal(starved.phase, 'run');
  } finally {
    client.close();
    await api.close();
  }
});

test('a sandbox that never started is unavailable, not the program\'s fault', async () => {
  // The first adoption report had to match on the error's text to tell a
  // start failure from a failing program. `started` says it outright.
  const api = await FakeApi.start();
  api.answer('POST', '/run', 200, {
    exit_code: 125,
    stdout: '',
    stderr: 'error: this host cannot run sandboxes\n',
    timed_out: false,
    oom_killed: false,
    started: false,
    phase: 'start',
  });
  const client = connect(api.url, { token: null });
  try {
    const never = await client.run('alpine:3', ['true']);
    assert.equal(never.started, false);
    assert.equal(never.phase, 'start');
    assert.equal(never.ok, false);
  } finally {
    client.close();
    await api.close();
  }
});

test('an older API that reports no reason still parses', async () => {
  // The fields were added after the route; a client that required them would
  // fail against a Zygo one release behind.
  const api = await FakeApi.start();
  api.answer('POST', '/run', 200, { exit_code: 0, stdout: 'hi\n' });
  const client = connect(api.url, { token: null });
  try {
    const run = await client.run('alpine:3', ['echo', 'hi']);
    assert.equal(run.ok, true);
    assert.equal(run.oomKilled, false);
    assert.equal(run.peakRssKb, 0);
  } finally {
    client.close();
    await api.close();
  }
});

test('serving sends an absolute base directory', async () => {
  // The API refuses a relative one, and it is right to: a path in a request
  // body has no meaning on the host that receives it.
  const api = await FakeApi.start();
  api.answer('PUT', '/fn/resize', 200, { name: 'resize', change: 'started', warm_ms: 40 });
  const client = connect(api.url, { token: null });
  try {
    const served = await client.serve('resize', { entry: './resize.py' }, { baseDir: '/srv/app' });
    assert.equal(served.change, 'started');
    assert.equal(api.requests[0].body.base_dir, '/srv/app');
  } finally {
    client.close();
    await api.close();
  }
});

test('a one-shot run reports a non-zero exit without throwing', async () => {
  // The sandbox ran; this is what it said. Throwing would make it impossible
  // to read the output of a command that failed on purpose.
  const api = await FakeApi.start();
  api.answer('POST', '/run', 200, {
    exit_code: 2,
    stdout: '',
    stderr: 'boom\n',
    timed_out: false,
    wall_ms: 21,
  });
  const client = connect(api.url, { token: null });
  try {
    const run = await client.run('alpine:3', ['false'], { mem: '64M', network: 'none' });
    assert.equal(run.ok, false);
    assert.equal(run.exitCode, 2);
    assert.equal(run.stderr, 'boom\n');
    const sent = api.requests[0].body.layer;
    assert.equal(sent.image, 'alpine:3');
    assert.equal(sent.mem, '64M');
    assert.equal(sent.network, 'none');
    // `stdin` describes the call, not the sandbox, and must not leak into the
    // spec the server writes — `deny_unknown_fields` would refuse it there.
    assert.ok(!('stdin' in sent));
  } finally {
    client.close();
    await api.close();
  }
});

test('a script is sent as itself, and its digest comes back', async () => {
  // Not JSON around the bytes: the API takes the script as the body, and the
  // name it answers with is the SHA-256 of exactly what was sent.
  const api = await FakeApi.start();
  const digest = 'sha256:' + '0'.repeat(64);
  api.answer('PUT', '/scripts', 201, { sha256: digest, size: 37, existed: false });
  api.answer('GET', `/scripts/${digest}`, 200, { sha256: digest, size: 37 });
  api.answer('DELETE', `/scripts/${digest}`, 200, { deleted: true });
  const client = connect(api.url, { token: null });
  try {
    const source = 'def handler(event):\n    return event\n';
    const script = await client.putScript(source);
    assert.equal(script.sha256, digest);
    assert.equal(script.existed, false);
    assert.equal(api.requests[0].raw, source, 'the body is the script itself');
    assert.match(api.requests[0].headers['content-type'], /^text\/plain/);

    assert.equal((await client.script(digest)).size, 37);
    assert.equal(await client.deleteScript(digest), true);
  } finally {
    client.close();
    await api.close();
  }
});

test('a lockfile goes up as base64 and the answer is an id to poll', async () => {
  // Base64 because a lockfile is not always UTF-8, and JSON has no other way
  // to carry bytes. `202` rather than `201`: the build has not happened yet.
  const api = await FakeApi.start();
  const id = 'deps_' + 'a'.repeat(32);
  api.answer('POST', '/deps', 202, {
    id,
    state: 'building',
    kind: 'node',
    image: 'node:22-slim',
    files: { 'package.json': 18, 'package-lock.json': 24 },
  });
  api.answer('GET', `/deps/${id}`, 200, {
    id,
    state: 'failed',
    error: 'npm exited 1',
    log: 'npm error 404 Not Found - GET https://registry.npmjs.org/nosuchpkg',
  });
  const client = connect(api.url, { token: null });
  try {
    const deps = await client.putDeps('node:22-slim', {
      'package.json': '{"name":"x"}',
      'package-lock.json': '{"lockfileVersion":3}',
    });
    assert.equal(deps.id, id);
    assert.equal(deps.building, true);
    assert.equal(deps.ready, false);

    const sent = JSON.parse(api.requests[0].raw);
    assert.equal(sent.image, 'node:22-slim');
    assert.equal(
      Buffer.from(sent.files['package.json'], 'base64').toString(),
      '{"name":"x"}'
    );

    // The reason is on the same object as the state: a caller looking at
    // `failed` wants it, and asking twice is how a client ends up not
    // showing it at all.
    const after = await client.deps(id);
    assert.equal(after.state, 'failed');
    assert.match(after.log, /nosuchpkg/);
  } finally {
    client.close();
    await api.close();
  }
});

test('a pool on a dependency set that is still building is told to retry', async () => {
  // Not queued and not started: a zygote warmed without the dependencies it
  // was promised serves requests that fail at import.
  const api = await FakeApi.start();
  const id = 'deps_' + 'b'.repeat(32);
  api.answer('POST', '/runtimes', 503, {
    error: `${id} is still building`,
    code: 'deps_building',
  });
  const client = connect(api.url, { token: null });
  try {
    // Not a plain `ZygoError`: this one is safe to send again, and it says
    // when. The five seconds are the host's, from `Retry-After`.
    await assert.rejects(
      () => client.serveRuntime('pool', { image: 'node:22-slim' }, { deps: id }),
      (e) => e instanceof Unavailable && /still building/.test(e.message) && e.code === 'deps_building' && e.retryAfter === 5
    );
    assert.equal(JSON.parse(api.requests[0].raw).deps, id);
  } finally {
    client.close();
    await api.close();
  }
});

test('a stopping API is unavailable from health, and says so', async () => {
  const api = await FakeApi.start();
  api.answer('GET', '/healthz', 503, { ok: false, status: 'stopping', uptime_s: 9 });
  const client = connect(api.url, { token: null });
  try {
    await assert.rejects(
      () => client.health(),
      (e) => e instanceof Unavailable && /stopping/.test(e.message) && e.retryAfter === 1
    );
  } finally {
    client.close();
    await api.close();
  }
});

// ---- retries: a refusal is sent again, a failure is not ---------------------
//
// Off by default, because a retry is a decision about the caller's time that
// the client should not make unasked. When it is on, only the two errors that
// mean "the request never ran" qualify — `Busy` and `Unavailable` — and the
// wait honours what the server asked for.

const BUSY = [429, { error: 'busy', in_flight: 2, queued: 0, limit: 2 }];

test('a refused call is sent again, and the answer is the second one', async () => {
  const api = await FakeApi.start();
  api.answerThen('POST', '/fn/f', [BUSY, BUSY, [200, OK_RESULT]], 0);
  const client = connect(api.url, { token: null, retries: 3, backoff: 0.01 });
  try {
    const out = await client.call('f', { n: 1 });
    assert.deepEqual(out.result, { size: [80, 60] });
    assert.equal(api.requests.length, 3, 'two refusals, then the answer');
    // The same request each time: one caller, one event.
    assert.deepEqual(new Set(api.requests.map((r) => r.raw)), new Set(['{"n":1}']));
  } finally {
    client.close();
    await api.close();
  }
});

test('the wait is at least what the server asked for, and grows on repeats', async () => {
  const api = await FakeApi.start();
  api.answerThen('POST', '/fn/f', [BUSY, [200, OK_RESULT]], 0.3);
  api.answerThen('POST', '/fn/g', [BUSY, BUSY, BUSY, [200, OK_RESULT]], 0);
  const client = connect(api.url, { token: null, retries: 3, backoff: 0 });
  try {
    let started = performance.now();
    await client.call('f', {});
    let elapsed = (performance.now() - started) / 1000;
    // `Retry-After: 0.3` and no backoff of its own: the server's number.
    assert.ok(elapsed >= 0.29 && elapsed < 1.5, `waited ${elapsed}s`);

    client.backoff = 0.1;
    started = performance.now();
    await client.call('g', {});
    elapsed = (performance.now() - started) / 1000;
    // 0.1, then 0.2, then 0.4: doubled each time, from the base.
    assert.ok(elapsed >= 0.69 && elapsed < 2, `waited ${elapsed}s`);
  } finally {
    client.close();
    await api.close();
  }
});

test('retries are off unless asked for, and the last refusal is the one thrown', async () => {
  const api = await FakeApi.start();
  api.answerThen('POST', '/fn/f', [BUSY, [200, OK_RESULT]], 0);
  api.answer('POST', '/fn/full', ...BUSY, 0);
  const off = connect(api.url, { token: null });
  const on = connect(api.url, { token: null, retries: 2, backoff: 0 });
  try {
    await assert.rejects(() => off.call('f', {}), Busy);
    assert.equal(api.requests.length, 1);
    await assert.rejects(() => on.call('full', {}), Busy);
    assert.equal(api.requests.length, 4, 'the first try and two retries');
  } finally {
    off.close();
    on.close();
    await api.close();
  }
});

test('a pool waiting on a build is retried, through the tenant view too', async () => {
  const api = await FakeApi.start();
  api.answerThen(
    'POST',
    '/runtimes',
    [
      [503, { error: 'still building', code: 'deps_building' }],
      [200, { name: 'pool', warm: 1, change: 'started' }],
    ],
    0
  );
  const client = connect(api.url, { token: null, retries: 1, backoff: 0.01 });
  try {
    const served = await client.forTenant('acme').serveRuntime('pool', { image: 'x' }, { deps: 'deps_1' });
    assert.equal(served.warm, 1);
    assert.equal(api.requests.length, 2);
    assert.equal(api.requests[1].headers['x-zygo-tenant'], 'acme');
  } finally {
    client.close();
    await api.close();
  }
});

test('a handler that threw is never sent again', async () => {
  // The one that matters. A handler that threw will throw again, and a
  // request the deadline killed *ran*; sending either twice is how a side
  // effect happens twice.
  const api = await FakeApi.start();
  api.answerThen('POST', '/fn/f', [[500, { error: 'boom', exit_code: 1, stdout: '', stderr: 'Trace' }], [200, OK_RESULT]]);
  api.answerThen('POST', '/fn/slow', [[408, { error: 'timed out' }], [200, OK_RESULT]]);
  api.answerThen('POST', '/fn/gone', [[404, { error: 'no such' }], [200, OK_RESULT]]);
  const client = connect(api.url, { token: null, retries: 5, backoff: 0 });
  try {
    await assert.rejects(() => client.call('f', {}), HandlerError);
    await assert.rejects(() => client.call('slow', {}), Timeout);
    await assert.rejects(() => client.call('gone', {}), NotFound);
    assert.equal(api.requests.length, 3, 'each was sent exactly once');
  } finally {
    client.close();
    await api.close();
  }
});

test('a stream refused before its first line is retried', async () => {
  const api = await FakeApi.start();
  api.answerThen('POST', '/fn/f', [BUSY, [200, OK_RESULT]], 0);
  api.stream('POST', '/fn/g', [{ stream: 'stdout', data: 'hi\n' }, { status: 200, ...OK_RESULT }]);
  const client = connect(api.url, { token: null, retries: 1, backoff: 0.01 });
  try {
    // The refusal is a JSON answer; the retry is the same streaming request.
    const events = [];
    for await (const event of client.stream('f', {})) events.push(event.kind);
    assert.deepEqual(events, ['result']);
    assert.equal(api.requests.length, 2);
    assert.equal(api.requests[1].headers.accept, 'application/x-ndjson');
  } finally {
    client.close();
    await api.close();
  }
});

test('a secret arrives once, and the listing never carries one', async () => {
  const api = await FakeApi.start();
  api.answer('POST', '/tenants/acme/tokens', 201, {
    token: { id: 'tok_1a2b3c4d5e6f', tenant: 'acme', created_ms: 1 },
    secret: 'zygo_deadbeef',
  });
  api.answer('GET', '/tokens', 200, {
    tokens: [{ id: 'tok_1a2b3c4d5e6f', tenant: 'acme', created_ms: 1 }],
  });
  api.answer('DELETE', '/tokens/tok_1a2b3c4d5e6f', 200, { revoked: true });
  const client = connect(api.url, { token: null });
  try {
    const minted = await client.mintToken('acme');
    assert.equal(minted.secret, 'zygo_deadbeef');
    assert.equal(minted.token.tenant, 'acme');
    assert.equal(api.requests[0].path, '/tenants/acme/tokens');

    const tokens = await client.tokens();
    assert.deepEqual(tokens.map((t) => t.id), ['tok_1a2b3c4d5e6f']);
    await client.revokeToken('tok_1a2b3c4d5e6f');
  } finally {
    client.close();
    await api.close();
  }
});

test('an operator token is minted on its own route and names nobody', async () => {
  const api = await FakeApi.start();
  api.answer('POST', '/tokens', 201, {
    token: { id: 'tok_000000000000', created_ms: 1 },
    secret: 'zygo_x',
  });
  const client = connect(api.url, { token: null });
  try {
    const minted = await client.mintToken();
    assert.equal(minted.token.tenant, undefined, 'an operator token names nobody');
    assert.equal(api.requests[0].path, '/tokens');
  } finally {
    client.close();
    await api.close();
  }
});

test('acting for a tenant is a header on the same connection', async () => {
  const api = await FakeApi.start();
  api.answer('GET', '/fn', 200, { functions: [] });
  const client = connect(api.url, { token: null });
  try {
    await client.functions();
    await client.forTenant('acme').functions();
    assert.equal(api.requests[0].headers['x-zygo-tenant'], undefined);
    assert.equal(api.requests[1].headers['x-zygo-tenant'], 'acme');
  } finally {
    client.close();
    await api.close();
  }
});

const STREAM_LINES = [
  { stream: 'stdout', data: 'page 1\n' },
  { stream: 'progress', data: 'halfway' },
  { stream: 'stderr', data: 'a warning\n' },
  { status: 200, result: { pages: 2 }, stdout: 'page 1\n', stderr: 'a warning\n' },
];

test('a stream yields its lines, then exactly one result', async () => {
  const api = await FakeApi.start();
  api.stream('POST', '/fn/render', STREAM_LINES);
  const client = connect(api.url, { token: null });
  try {
    const kinds = [];
    let last = null;
    for await (const event of client.stream('render', { pages: 2 })) {
      kinds.push(event.kind);
      last = event;
    }
    assert.deepEqual(kinds, ['stdout', 'progress', 'stderr', 'result']);
    // `progress` is its own kind, not a line of stdout.
    assert.equal(last.result.result.pages, 2);
  } finally {
    client.close();
    await api.close();
  }
});

test('lines are yielded as they arrive, not at the end', async () => {
  // The same lines arrive either way; what a stream promises is *when*.
  const api = await FakeApi.start();
  api.stream('POST', '/fn/render', STREAM_LINES, 300);
  const client = connect(api.url, { token: null });
  try {
    const began = Date.now();
    let first = null;
    for await (const _event of client.stream('render', {})) {
      if (first === null) first = Date.now() - began;
    }
    const whole = Date.now() - began;
    assert.ok(first !== null && first < whole / 2, `first line took ${first}ms of ${whole}ms`);
  } finally {
    client.close();
    await api.close();
  }
});

test('a failed request throws after its output has been seen', async () => {
  const api = await FakeApi.start();
  api.stream('POST', '/fn/render', [
    { stream: 'stdout', data: 'starting\n' },
    { status: 500, error: 'boom', exit_code: 1, stdout: 'starting\n' },
  ]);
  const client = connect(api.url, { token: null });
  try {
    const kinds = [];
    await assert.rejects(async () => {
      for await (const event of client.stream('render', {})) kinds.push(event.kind);
    }, HandlerError);
    assert.deepEqual(kinds, ['stdout', 'result'], 'the output was not delivered first');
  } finally {
    client.close();
    await api.close();
  }
});

test('a stuck request is its own error, not a Timeout', async () => {
  const api = await FakeApi.start();
  api.answer('POST', '/fn/render', 504, {
    error: 'the sandbox stopped reporting this request',
    stuck: true,
    request_id: '00000009',
  });
  const client = connect(api.url, { token: null });
  try {
    await assert.rejects(() => client.call('render', {}), Stuck);
    // "too slow, raise the limit" is the wrong advice for a request that
    // still had budget when the sandbox stopped answering.
    await assert.rejects(() => client.call('render', {}), (e) => !(e instanceof Timeout));
  } finally {
    client.close();
    await api.close();
  }
});

test('a cancelled request is its own error, not a Timeout', async () => {
  const api = await FakeApi.start();
  api.answer('POST', '/fn/slow', 499, {
    error: 'the request was cancelled',
    cancelled: true,
    request_id: '00000007',
    metrics: { wall_ms: 1200 },
  });
  const client = connect(api.url, { token: null });
  try {
    await assert.rejects(() => client.call('slow', {}), Cancelled);
    // "too slow, raise the limit" is the wrong advice for a request somebody
    // stopped on purpose.
    await assert.rejects(() => client.call('slow', {}), (e) => !(e instanceof Timeout));
  } finally {
    client.close();
    await api.close();
  }
});

test('aborting the signal cancels the request on the server', async () => {
  const api = await FakeApi.start();
  // Long enough that the call is still waiting when the signal aborts.
  api.delay = 500;
  api.answer('POST', '/fn/slow', 200, { result: null });
  api.answer('DELETE', '/requests/', 200, { cancelled: true, started: true });
  const client = connect(api.url, { token: null });
  const controller = new AbortController();
  try {
    const call = client.call('slow', {}, { signal: controller.signal });
    await new Promise((r) => setTimeout(r, 50));
    controller.abort();
    await call.catch(() => {});
    // The cancel is fire-and-forget, so give it a moment to land.
    await new Promise((r) => setTimeout(r, 200));

    const key = api.requests[0].headers['x-zygo-request-key'];
    assert.ok(key && key.startsWith('k-'), `no key on the call: ${key}`);
    const cancels = api.requests.filter((r) => r.method === 'DELETE');
    assert.equal(cancels.length, 1, 'the aborted call sent no cancel');
    assert.equal(cancels[0].path, `/requests/${key}`);
  } finally {
    client.close();
    await api.close();
  }
});

test('a script the host does not have is a NotFound', async () => {
  const api = await FakeApi.start();
  const digest = 'sha256:' + 'a'.repeat(64);
  api.answer('GET', `/scripts/${digest}`, 404, { error: `no script ${digest}`, code: 'not_found' });
  const client = connect(api.url, { token: null });
  try {
    await assert.rejects(() => client.script(digest), NotFound);
  } finally {
    client.close();
    await api.close();
  }
});

test('a runtime pool is registered, listed, called and stopped', async () => {
  // The embedder's path end to end: one pool, a script registered once, and a
  // call that names the digest rather than shipping the bytes again.
  const api = await FakeApi.start();
  const digest = 'sha256:' + '1'.repeat(64);
  api.answer('POST', '/runtimes', 200, {
    name: 'py312', runtime: 'python/3.12.4', warm: 2, warm_ms: 410, change: 'started',
  });
  api.answer('GET', '/runtimes', 200, {
    runtimes: [{ name: 'py312', warm: 2, paused: 0, cold: 2, min_warm: 2, max_warm: 4 }],
  });
  api.answer('POST', '/runtimes/py312/call', 200, {
    result: { ok: true }, stdout: '', stderr: '', metrics: { wall_ms: 2.1 },
  });
  api.answer('DELETE', '/runtimes/py312', 200, { stopped: ['py312'] });

  const client = connect(api.url, { token: null });
  try {
    const served = await client.serveRuntime('py312', {
      image: 'python:3.12-slim', agent: 'python', min_warm: 2,
    }, { secrets: ['STRIPE_KEY'] });
    assert.equal(served.warm, 2);
    assert.equal(api.requests[0].body.layer.agent, 'python');
    // Names in the layer, as `[runtime.<name>] secrets = [...]` would put
    // them; the values are the calling tenant's and never travel here.
    assert.deepEqual(api.requests[0].body.layer.secrets, ['STRIPE_KEY']);
    assert.equal('secrets' in api.requests[0].body, false);

    const pools = await client.runtimes();
    assert.equal(pools[0].max_warm, 4);

    const out = await client.runScript('py312', digest, { n: 1 });
    assert.deepEqual(out.result, { ok: true });
    const sent = api.requests[2].body;
    assert.equal(sent.script, digest, 'a digest goes as a string, not as source');
    assert.deepEqual(sent.event, { n: 1 });

    // And a one-off, where there is nothing registered to name.
    await client.runScript('py312', 'def handler(e):\n    return e\n');
    assert.equal(api.requests[3].body.script.source.startsWith('def handler'), true);

    assert.deepEqual(await client.stopRuntime('py312'), ['py312']);
  } finally {
    client.close();
    await api.close();
  }
});
