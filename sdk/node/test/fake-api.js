/**
 * A stand-in for `zygo api`, so the client can be tested without a kernel.
 *
 * It answers the routes the client calls, records what it received, and can be
 * told to answer with a particular status. Nothing here runs a sandbox: a test
 * that needs one belongs in the Rust suites, against a real kernel.
 *
 * Both transports are covered, because they are the ones the client implements
 * differently — a unix socket and a TCP port — and a bug in either is invisible
 * from the other.
 */

import http from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export class FakeApi {
  constructor() {
    this.requests = [];
    this.connections = 0;
    this.answers = new Map();
    this.delay = 0;

    this.server = http.createServer(async (request, response) => {
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const raw = Buffer.concat(chunks).toString('utf8');
      this.requests.push({
        method: request.method,
        path: request.url,
        headers: request.headers,
        body: raw ? JSON.parse(raw) : null,
      });

      if (this.delay) await new Promise((r) => setTimeout(r, this.delay));

      const key = `${request.method} ${request.url.split('?')[0]}`;
      const answer = this.answers.get(key) ?? { status: 404, body: { error: `no route ${request.url}` } };
      const payload = Buffer.from(JSON.stringify(answer.body));
      const headers = { 'content-type': 'application/json', 'content-length': String(payload.length) };
      if (answer.status === 429) headers['retry-after'] = '3';
      response.writeHead(answer.status, headers);
      response.end(payload);
    });
    this.server.on('connection', () => {
      this.connections += 1;
    });
  }

  answer(method, path, status, body) {
    this.answers.set(`${method} ${path}`, { status, body });
  }

  /** @param {{unix?: boolean}} options */
  static async start({ unix = false } = {}) {
    const api = new FakeApi();
    if (unix) {
      api.dir = mkdtempSync(join(tmpdir(), 'zygo-fake-'));
      api.socketPath = join(api.dir, 'api.sock');
      await new Promise((resolve) => api.server.listen(api.socketPath, resolve));
      api.url = `unix://${api.socketPath}`;
    } else {
      await new Promise((resolve) => api.server.listen(0, '127.0.0.1', resolve));
      api.url = `http://127.0.0.1:${api.server.address().port}`;
    }
    return api;
  }

  async close() {
    // Keep-alive sockets would hold the server open past the test.
    this.server.closeAllConnections?.();
    await new Promise((resolve) => this.server.close(resolve));
    if (this.dir) rmSync(this.dir, { recursive: true, force: true });
  }
}
