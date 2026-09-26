// SPDX-License-Identifier: Apache-2.0
// The declarations keep up with the JavaScript.
//
// `index.d.ts` is written by hand, which is what lets the package ship with no
// build step — and what let it fall ten methods behind the code once. This
// reads the declaration file as text and checks it against the module as it
// runs: every export, every method on `Client`, every method on a function
// handle, and every option a call reads. A method added to the JavaScript
// without a declaration fails here.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import * as sdk from '../src/index.js';
import { Client, connect } from '../src/index.js';

const here = dirname(fileURLToPath(import.meta.url));
const declarations = readFileSync(join(here, '..', 'src', 'index.d.ts'), 'utf8');

/// The body of one `declare class` or `interface` block, by name.
function block(kind, name) {
  const at = declarations.search(new RegExp(`${kind} ${name}\\b[^{]*\\{`));
  assert.notEqual(at, -1, `${kind} ${name} is not declared`);
  let depth = 0;
  for (let i = declarations.indexOf('{', at); i < declarations.length; i += 1) {
    if (declarations[i] === '{') depth += 1;
    if (declarations[i] === '}' && (depth -= 1) === 0) return declarations.slice(at, i);
  }
  throw new Error(`${kind} ${name} never closes`);
}

/// Method names declared in a block: `name(` or `name<T>(` at the start of a line.
function declaredMethods(text) {
  return new Set([...text.matchAll(/^\s+(?:readonly\s+)?([a-zA-Z_]\w*)(?:<[^>]*>)?\(/gm)].map((m) => m[1]));
}

/// Property names declared in an interface: `name?:` or `name:`.
function declaredProperties(text) {
  return new Set([...text.matchAll(/^\s+(?:readonly\s+)?([a-zA-Z_]\w*)\??:/gm)].map((m) => m[1]));
}

test('every export of the module is declared', () => {
  const missing = Object.keys(sdk).filter(
    (name) => !new RegExp(`export declare (?:class|function|const|type|interface) ${name}\\b`).test(declarations)
  );
  assert.deepEqual(missing, [], 'exported by index.js and absent from index.d.ts');
});

test('every method on Client is declared, and no declared method is missing from the code', () => {
  const real = new Set(
    Object.getOwnPropertyNames(Client.prototype).filter(
      (name) => name !== 'constructor' && typeof Client.prototype[name] === 'function'
    )
  );
  const declared = declaredMethods(block('class', 'Client'));
  declared.delete('constructor');
  assert.deepEqual([...real].filter((n) => !declared.has(n)), [], 'methods with no declaration');
  assert.deepEqual([...declared].filter((n) => !real.has(n)), [], 'declarations with no method');
});

test('every method on a function handle is declared', () => {
  const client = connect('http://127.0.0.1:1', { token: null });
  try {
    const handle = client.fn('x');
    const real = Object.keys(handle).filter((name) => typeof handle[name] === 'function');
    const declared = declaredMethods(block('interface', 'FunctionHandle'));
    assert.deepEqual(real.filter((n) => !declared.has(n)), [], 'handle methods with no declaration');
    assert.ok(declaredProperties(block('interface', 'FunctionHandle')).has('name_'));
  } finally {
    client.close();
  }
});

test('every option the code reads is declared', () => {
  // The options `call` and `runScript` destructure or read, as the source
  // spells them; a new one has to appear in the matching interface.
  const source = readFileSync(join(here, '..', 'src', 'index.js'), 'utf8');
  const callOptions = new Set([...source.matchAll(/options\.(\w+)/g)].map((m) => m[1]));
  const declaredCall = declaredProperties(block('interface', 'CallOptions'));
  const declaredRun = declaredProperties(block('interface', 'RunScriptOptions'));
  const declaredClient = declaredProperties(block('interface', 'ClientOptions'));
  for (const name of ['timeout', 'key', 'signal', 'workspace', 'out']) {
    assert.ok(callOptions.has(name), `the code no longer reads options.${name}`);
    assert.ok(declaredCall.has(name), `CallOptions lacks ${name}`);
    assert.ok(declaredRun.has(name), `RunScriptOptions lacks ${name}`);
  }
  assert.ok(declaredRun.has('entryPoint'));
  for (const name of ['token', 'timeout', 'retries', 'backoff', 'tenant']) {
    assert.ok(callOptions.has(name), `the constructor no longer reads options.${name}`);
    assert.ok(declaredClient.has(name), `ClientOptions lacks ${name}`);
  }
  // `agent` is the internal seam `forTenant` uses, and undeclared on purpose.
  assert.ok(!declaredClient.has('agent'));

  // The options the two `serve` methods destructure, as the source spells
  // them, each declared inline on its method. `secrets` is on both and
  // means a different thing on each — values for a function, names for a
  // pool — which is why the declaration has to say `string[]` on the pool.
  const inline = (method) => {
    const found = source.match(new RegExp(`async ${method}\\(name, layer, \\{ (.*?) \\} = \\{\\}\\)`));
    assert.ok(found, `${method} no longer destructures its options`);
    return new Set(found[1].split(',').map((s) => s.trim().split(' ')[0]));
  };
  const declaredInline = (method) => {
    const found = block('class', 'Client').match(new RegExp(`${method}\\([^;]*?options\\?: \\{([^}]*)\\}`));
    assert.ok(found, `${method} declares no options`);
    return declaredProperties(found[1].replace(/;\s*/g, ';\n  '));
  };
  for (const [method, wanted] of [
    ['serve', ['baseDir', 'secrets', 'ifChanged']],
    ['serveRuntime', ['baseDir', 'deps', 'secrets']],
  ]) {
    const read = inline(method);
    const declared = declaredInline(method);
    for (const name of wanted) {
      assert.ok(read.has(name), `${method} no longer reads ${name}`);
      assert.ok(declared.has(name), `${method}'s declared options lack ${name}`);
    }
  }
  assert.match(block('class', 'Client'), /secrets\?: string\[\]/, 'a pool takes names');
});

test('a result carries what the code puts on it', () => {
  const declared = declaredProperties(block('interface', 'Result'));
  for (const name of ['result', 'requestId', 'workspace', 'stdout', 'stderr', 'metrics']) {
    assert.ok(declared.has(name), `Result lacks ${name}`);
  }
});
