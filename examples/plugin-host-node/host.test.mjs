// SPDX-License-Identifier: Apache-2.0
/**
 * The host's routing, against a stand-in for `zygo api`.
 *
 * The same pattern as `sdk/node/test/fake-api.js`: an `http.Server` that
 * answers the routes the SDK calls and records what it received. Nothing here
 * runs a sandbox, so this runs anywhere `node` does — `verify.mjs` is the
 * same routes against a real Zygo.
 *
 *     node --test host.test.mjs
 */

import assert from 'node:assert/strict';
import http from 'node:http';
import { test } from 'node:test';

import { connect } from '../../sdk/node/src/index.js';
import { PluginHost, createServer } from './host.mjs';

const DIGEST = 'sha256:' + 'a'.repeat(64);

/** A fake `zygo api`: `answers` by "METHOD /path", the last one repeated. */
async function fakeZygo() {
  const requests = [];
  const answers = new Map();
  const server = http.createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const raw = Buffer.concat(chunks).toString('utf8');
    const isJson = (request.headers['content-type'] ?? '').startsWith('application/json');
    requests.push({ method: request.method, path: request.url, headers: request.headers, body: isJson && raw ? JSON.parse(raw) : raw });
    const planned = answers.get(`${request.method} ${request.url.split('?')[0]}`) ?? [[404, { error: 'no route' }]];
    const [status, body, lines] = planned.length > 1 ? planned.shift() : planned[0];
    if (lines) {
      response.writeHead(200, { 'content-type': 'application/x-ndjson' });
      for (const line of lines) response.write(JSON.stringify(line) + '\n');
      return response.end();
    }
    const headers = { 'content-type': 'application/json' };
    // Zero is a value the SDK honours ("now"), which keeps this test quick.
    if (status === 429 || status === 503) headers['retry-after'] = '0';
    response.writeHead(status, headers);
    response.end(JSON.stringify(body));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    requests,
    /** Answer a route with each of `replies` in turn, then keep the last. */
    answer: (method, path, ...replies) => answers.set(`${method} ${path}`, replies),
    close: () => {
      server.closeAllConnections?.();
      return new Promise((resolve) => server.close(resolve));
    },
  };
}

/** A host in front of the fake, with one customer already onboarded. */
async function hostUp() {
  const zygo = await fakeZygo();
  zygo.answer('POST', '/tenants', [201, { tenant: { id: 'acme' } }]);
  zygo.answer('PATCH', '/tenants/acme/limits', [200, { tenant: { id: 'acme' } }]);
  zygo.answer('POST', '/tenants/acme/tokens', [201, { token: { id: 'tok_1', tenant: 'acme' }, secret: 'zygo_acme' }]);
  zygo.answer('PUT', '/scripts', [201, { sha256: DIGEST, size: 40, existed: false }]);
  const operator = connect(zygo.url, { token: 'zygo_operator', retries: 2, backoff: 0.01 });
  const host = new PluginHost(operator);
  const server = createServer(host);
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  const call = (method, path, token, body, headers = {}) =>
    fetch(base + path, { method, body, headers: { authorization: `Bearer ${token}`, ...headers } });
  const onboarded = await (await call('POST', '/customers', 'zygo_operator', JSON.stringify({ id: 'acme', mem: '64M' }))).json();
  const installed = await (await call('PUT', '/plugins?language=javascript', onboarded.token, 'module.exports = () => 1')).json();
  const close = async () => {
    operator.close();
    server.closeAllConnections?.();
    await new Promise((resolve) => server.close(resolve));
    await zygo.close();
  };
  return { zygo, call, token: onboarded.token, digest: installed.digest, close };
}

test('onboarding mints a token, and only that token opens the customer routes', async () => {
  const h = await hostUp();
  try {
    assert.equal(h.token, 'zygo_acme');
    assert.equal(h.zygo.requests.find((r) => r.path === '/tenants/acme/limits').body.mem, '64M');
    assert.equal((await h.call('PUT', '/plugins', 'nobody', 'x')).status, 401);
    assert.equal((await h.call('POST', '/customers', h.token, '{"id":"globex"}')).status, 403);
    assert.equal((await h.call('PUT', '/plugins', 'zygo_operator', 'x')).status, 403);
    assert.equal((await h.call('GET', '/nothing', h.token)).status, 404);
  } finally {
    await h.close();
  }
});

test('a plugin is registered and run as its customer, in the pool for its language', async () => {
  const h = await hostUp();
  h.zygo.answer('POST', '/runtimes/plugins-javascript/call', [200, { result: { hello: 'world' }, request_id: 'r1', metrics: { wall_ms: 2 } }]);
  try {
    const response = await h.call('POST', `/plugins/${h.digest}/run`, h.token, '{"name":"world"}', { 'x-run-key': 'job-1' });
    assert.equal(response.status, 200);
    assert.deepEqual((await response.json()).result, { hello: 'world' });
    const put = h.zygo.requests.find((r) => r.path === '/scripts');
    assert.equal(put.headers['x-zygo-tenant'], 'acme');
    const run = h.zygo.requests.find((r) => r.path.startsWith('/runtimes/'));
    assert.equal(run.headers['x-zygo-tenant'], 'acme');
    assert.equal(run.headers['x-zygo-request-key'], 'job-1');
    assert.equal(run.body.script, h.digest);
    assert.equal((await h.call('POST', `/plugins/sha256:${'b'.repeat(64)}/run`, h.token, '{}')).status, 404);
  } finally {
    await h.close();
  }
});

test('what Zygo says becomes the host\'s own status code', async () => {
  const h = await hostUp();
  const path = '/runtimes/plugins-javascript/call';
  try {
    h.zygo.answer('POST', path, [500, { error: 'boom', stderr: 'Error: boom\n', exit_code: 1 }]);
    let response = await h.call('POST', `/plugins/${h.digest}/run`, h.token, '{}');
    assert.equal(response.status, 500);
    const body = await response.json();
    assert.equal(body.detail, 'boom');
    assert.equal(body.stderr, 'Error: boom\n');

    h.zygo.answer('POST', path, [408, { error: 'too slow', stderr: '' }]);
    assert.equal((await h.call('POST', `/plugins/${h.digest}/run`, h.token, '{}')).status, 504);

    h.zygo.answer('POST', path, [499, { error: 'stopped', cancelled: true }]);
    assert.equal((await h.call('POST', `/plugins/${h.digest}/run`, h.token, '{}')).status, 499);

    h.zygo.answer('POST', path, [404, { error: 'no such script' }]);
    assert.equal((await h.call('POST', `/plugins/${h.digest}/run`, h.token, '{}')).status, 404);

    assert.equal((await h.call('POST', `/plugins/${h.digest}/run`, h.token, 'not json')).status, 400);
  } finally {
    await h.close();
  }
});

test('a refused request is retried, and one refused for good is a 503 with Retry-After', async () => {
  const h = await hostUp();
  const path = '/runtimes/plugins-javascript/call';
  const busy = [429, { error: 'at its limit', in_flight: 8, queued: 0, limit: 8 }];
  try {
    h.zygo.answer('POST', path, busy, busy, [200, { result: 'ran', request_id: 'r2', metrics: {} }]);
    const before = h.zygo.requests.length;
    const response = await h.call('POST', `/plugins/${h.digest}/run`, h.token, '{}');
    assert.equal(response.status, 200);
    assert.equal(h.zygo.requests.length - before, 3);

    h.zygo.answer('POST', path, busy);
    const refused = await h.call('POST', `/plugins/${h.digest}/run`, h.token, '{}');
    assert.equal(refused.status, 503);
    assert.equal(refused.headers.get('retry-after'), '0');

    h.zygo.answer('POST', path, [503, { error: 'warming', code: 'warm_failed' }]);
    assert.equal((await h.call('POST', `/plugins/${h.digest}/run`, h.token, '{}')).status, 503);
  } finally {
    await h.close();
  }
});

test('a stream is passed on a line at a time, and a cancel names the key', async () => {
  const h = await hostUp();
  h.zygo.answer('POST', '/runtimes/plugins-javascript/call', [200, null, [
    { stream: 'stdout', data: 'step 0\n' },
    { stream: 'progress', data: '1 of 1' },
    { status: 200, result: { done: true }, request_id: 'r3', metrics: { wall_ms: 5 } },
  ]]);
  h.zygo.answer('DELETE', '/requests/job-2', [200, { cancelled: true, started: true }]);
  try {
    const response = await h.call('POST', `/plugins/${h.digest}/stream`, h.token, '{}');
    assert.equal(response.headers.get('content-type'), 'application/x-ndjson');
    const lines = (await response.text()).trim().split('\n').map((line) => JSON.parse(line));
    assert.deepEqual(lines.map((l) => l.stream ?? 'result'), ['stdout', 'progress', 'result']);
    assert.deepEqual(lines[2].result, { done: true });

    const cancelled = await h.call('DELETE', '/runs/job-2', h.token);
    assert.equal((await cancelled.json()).cancelled, true);
    assert.equal(h.zygo.requests.at(-1).path, '/requests/job-2');
    assert.equal(h.zygo.requests.at(-1).headers['x-zygo-tenant'], 'acme');
  } finally {
    await h.close();
  }
});
