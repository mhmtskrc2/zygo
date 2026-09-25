// SPDX-License-Identifier: Apache-2.0
// Every operation in the OpenAPI document has a client method.
//
// The same conformance the Python suite has, and for the same reason: the
// roadmap offered "regenerate the SDKs from the document", and what generation
// would give for free is the guarantee that nothing is missing. This is that
// guarantee without the generated client nobody wants to read.
//
// The document comes from the binary. Without one the test skips rather than
// passes: a check that quietly does nothing is worse than one that says so.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { Client } from '../src/index.js';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..', '..', '..');

/// Operation to the method that calls it.
///
/// Written out rather than derived: `POST /fn/{name}` is `call`, not
/// `postFnName`, and that is the whole reason these clients are hand-written.
const METHODS = {
  'GET /healthz': 'health',
  'GET /version': 'version',
  'GET /metrics': null, // Prometheus text; for a scraper, not a client.
  'GET /fn': 'functions',
  'POST /fn/{name}': 'call',
  'PUT /fn/{name}': 'serve',
  'DELETE /fn/{name}': 'stop',
  'POST /fn/{name}/batch': 'batch',
  'GET /fn/{name}/stats': 'stats',
  'GET /fn/{name}/logs': 'logs',
  'POST /fn/{name}/warm': 'warm',
  'GET /runtimes': 'runtimes',
  'POST /runtimes': 'serveRuntime',
  'DELETE /runtimes/{name}': 'stopRuntime',
  'POST /runtimes/{name}/call': 'runScript',
  'POST /deps': 'putDeps',
  'GET /deps': 'deps',
  'GET /deps/{id}': 'deps',
  'DELETE /deps/{id}': 'deleteDeps',
  'PUT /scripts': 'putScript',
  'GET /scripts/{digest}': 'script',
  'DELETE /scripts/{digest}': 'deleteScript',
  'PUT /blobs': 'putBlob',
  'GET /blobs/{digest}': 'blob',
  'DELETE /blobs/{digest}': 'deleteBlob',
  'POST /run': 'run',
  'DELETE /requests/{id}': 'cancel',
  'POST /drain': 'drain',
  'GET /tenants': 'tenants',
  'POST /tenants': 'createTenant',
  'GET /tenants/{id}': 'tenant',
  'DELETE /tenants/{id}': 'deleteTenant',
  'PATCH /tenants/{id}/limits': 'setLimits',
  'GET /tenants/{id}/secrets': 'secrets',
  'PUT /tenants/{id}/secrets/{name}': 'putSecret',
  'DELETE /tenants/{id}/secrets/{name}': 'deleteSecret',
  'POST /tenants/{id}/tokens': 'mintToken',
  'GET /tokens': 'tokens',
  'POST /tokens': 'mintToken',
  'DELETE /tokens/{id}': 'revokeToken',
};

function document() {
  const override = process.env.ZYGO_OPENAPI;
  if (override && existsSync(override)) return JSON.parse(readFileSync(override, 'utf8'));

  for (const binary of [join(root, 'target/release/zygo'), join(root, 'target/debug/zygo')]) {
    if (!existsSync(binary)) continue;
    try {
      return JSON.parse(execFileSync(binary, ['api', '--openapi'], { encoding: 'utf8' }));
    } catch {
      // A binary that cannot print one is the same as not having one.
    }
  }
  return null;
}

const doc = document();

test('every operation in the document has a client method', { skip: !doc && 'no zygo binary printed a document' }, () => {
  const missing = [];
  for (const [path, item] of Object.entries(doc.paths)) {
    for (const method of Object.keys(item)) {
      const key = `${method.toUpperCase()} ${path}`;
      if (!(key in METHODS)) {
        missing.push(`${key} is not in this test's METHODS table`);
        continue;
      }
      const name = METHODS[key];
      if (name === null) continue;
      if (typeof Client.prototype[name] !== 'function') {
        missing.push(`${key} maps to \`${name}\`, which the client lacks`);
      }
    }
  }
  assert.deepEqual(missing, []);
});

test('the table does not name a route that is gone', { skip: !doc && 'no document' }, () => {
  const real = new Set(
    Object.entries(doc.paths).flatMap(([path, item]) =>
      Object.keys(item).map((m) => `${m.toUpperCase()} ${path}`)
    )
  );
  const stale = Object.keys(METHODS).filter((k) => !real.has(k));
  assert.deepEqual(stale, [], 'these are in the table and not in the API');
});
