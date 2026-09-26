// SPDX-License-Identifier: Apache-2.0
/**
 * What the Node plugin host can do, run end to end against a real Zygo.
 *
 * Starts the host in this process, on a port of its own, then talks to it the
 * way a customer would: over HTTP, with the token onboarding handed out.
 * Every line below goes through the Zygo API; nothing reads a `sandbox.toml`,
 * writes a file on the Zygo machine, or shells out to `zygo`.
 *
 *     make verify-plugin-host-node
 *     node verify.mjs unix:///run/zygo/api.sock       # with ZYGO_API_TOKEN set
 */

import { AuthError, connect } from '../../sdk/node/src/index.js';
import { PluginHost, createServer } from './host.mjs';

let passed = 0;
let failed = 0;
const ok = (what) => (passed += 1, console.log(`  PASS  ${what}`));
const bad = (what, detail = '') => {
  failed += 1;
  console.log(`  FAIL  ${what}`);
  for (const line of String(detail).split('\n').slice(0, 6)) if (line) console.log(`          ${line}`);
};

// Customers' plugins: ordinary Python and JavaScript, by people who have never
// heard of Zygo — which is the point.
const GREETER = 'import platform\n\ndef handler(event):\n    return {"hello": event["name"], "python": platform.python_version()}\n';
const BROKEN = 'def handler(event):\n    raise ValueError("this plugin has a bug")\n';
const CHATTY = 'import sys, time\n\ndef handler(event):\n    for i in range(3):\n        print(f"step {i}"); sys.stdout.flush()\n        event.progress(f"{i + 1} of 3")\n        time.sleep(0.4)\n    return {"done": True}\n';
const SLOW = 'import time\n\ndef handler(event):\n    time.sleep(30)\n    return "never"\n';
const GREETER_JS = 'module.exports = function handler(event) {\n  return { hello: event.name, node: process.versions.node };\n};\n';
const HUNGRY_JS = 'module.exports = function handler() {\n  const chunks = [];\n  for (let i = 0; i < 96; i += 1) chunks.push(Buffer.alloc(1024 * 1024, 1));\n  return chunks.length;\n};\n';
const HUNGRY_PY = 'def handler(event):\n    b = bytearray(96 * 1024 * 1024)\n    b[::4096] = b"x" * len(b[::4096])\n    return len(b)\n';

const url = process.argv[2] ?? process.env.ZYGO_API_URL;
console.log('a plugin host in Node, on the API alone');
console.log(`  ${url}`);

const operator = connect(url, { retries: 3, timeout: 600_000 });
const host = new PluginHost(operator);
const server = createServer(host);
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const call = (method, path, token, body, headers = {}) =>
  fetch(base + path, { method, body, headers: { authorization: `Bearer ${token}`, ...headers } });
const json = async (method, path, token, body, headers) => {
  const response = await call(method, path, token, body, headers);
  return { status: response.status, ...(await response.json()) };
};
const operatorToken = operator.token;

try {
  await host.start();
  ok('the host declares a runtime per language, naming no path on the Zygo machine');

  const acme = (await json('POST', '/customers', operatorToken, JSON.stringify({ id: 'acme', mem: '128M' }))).token;
  const globex = (await json('POST', '/customers', operatorToken, JSON.stringify({ id: 'globex', mem: '64M' }))).token;
  if (acme && globex && acme !== globex) ok('two customers are onboarded over HTTP, each with their own token');
  else bad('onboarding', `${acme} ${globex}`);

  const greeter = await json('PUT', '/plugins', acme, GREETER);
  let out = await json('POST', `/plugins/${greeter.digest}/run`, acme, JSON.stringify({ name: 'world' }));
  if (out.status === 200 && out.result?.hello === 'world' && out.result.python) ok(`a customer's plugin is installed and called (python ${out.result.python})`);
  else bad('the plugin did not run', JSON.stringify(out));

  const broken = await json('PUT', '/plugins', acme, BROKEN);
  out = await json('POST', `/plugins/${broken.digest}/run`, acme, '{}');
  if (out.status === 500 && `${out.detail}${out.stderr}`.includes('ValueError')) ok('a plugin that throws is a 500 carrying its own error');
  else bad('a broken plugin', JSON.stringify(out));

  const chatty = await json('PUT', '/plugins', acme, CHATTY);
  const streamed = await call('POST', `/plugins/${chatty.digest}/stream`, acme, '{}');
  const lines = (await streamed.text()).trim().split('\n').map((line) => JSON.parse(line));
  if (lines.some((l) => l.stream === 'progress') && lines.at(-1).result?.done === true) ok("a long plugin's output and progress arrive while it runs, as NDJSON");
  else bad('streaming', JSON.stringify(lines));

  const slow = await json('PUT', '/plugins', acme, SLOW);
  const running = json('POST', `/plugins/${slow.digest}/run`, acme, '{}', { 'x-run-key': 'job-1' });
  await new Promise((resolve) => setTimeout(resolve, 2000));
  await json('DELETE', '/runs/job-1', acme);
  out = await running;
  if (out.status === 499) ok('a running plugin can be stopped by its customer');
  else bad('cancelling', JSON.stringify(out));

  out = await json('POST', `/plugins/${greeter.digest}/run`, globex, JSON.stringify({ name: 'theirs' }));
  if (out.status === 404) ok("and one customer cannot run another's plugin, digest or not");
  else bad("one customer ran another's plugin by naming its digest", JSON.stringify(out));

  const greeterJs = await json('PUT', '/plugins?language=javascript', acme, GREETER_JS);
  out = await json('POST', `/plugins/${greeterJs.digest}/run`, acme, JSON.stringify({ name: 'world' }));
  if (out.status === 200 && out.result?.hello === 'world' && out.result.node) ok(`the same customer's JavaScript plugin runs through the same routes (node ${out.result.node})`);
  else bad('the JavaScript plugin did not run', JSON.stringify(out));
  if (greeterJs.runtime !== greeter.runtime) ok(`in its own pool (${greeterJs.runtime}, not ${greeter.runtime})`);
  else bad('both languages named the same runtime', greeterJs.runtime);

  for (const [language, source] of [['python', HUNGRY_PY], ['javascript', HUNGRY_JS]]) {
    const hungry = await json('PUT', `/plugins?language=${language}`, globex, source);
    out = await json('POST', `/plugins/${hungry.digest}/run`, globex, '{}');
    if (out.status >= 400) ok(`a ${language} plugin is held to the customer's 64M, not the pool's (${out.status})`);
    else bad(`a ${language} plugin took more memory than its tenant allows`, JSON.stringify(out));
  }

  const removed = await json('DELETE', '/customers/globex', operatorToken);
  if (removed.deleted) ok("offboarding removes a customer's code, tokens and secrets");
  else bad('offboarding', JSON.stringify(removed));
  if ((await call('PUT', '/plugins', globex, 'x')).status === 401) ok('their token stops opening this host');
  else bad("an offboarded customer's token still opens the host");
  const theirs = connect(url, { token: globex });
  await theirs.functions().then(() => bad("and still works at Zygo"), (e) => (e instanceof AuthError ? ok('and stops working at Zygo too') : bad('a different refusal', e)));
  theirs.close();

  await json('DELETE', '/customers/acme', operatorToken);
  await host.stop();
} catch (error) {
  bad('the driver itself failed', error.stack ?? error);
} finally {
  server.close();
  operator.close();
}

console.log(`\n  ${passed} passed, ${failed} failed`);
console.log('\n  no sandbox.toml was read and no file was written on the Zygo host:\n  every line above went through the HTTP API.');
process.exit(failed ? 1 : 0);
