// SPDX-License-Identifier: Apache-2.0
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
      // Not every body is JSON: `PUT /scripts` sends the script as itself, so
      // parsing by content type rather than by hope.
      const isJson = (request.headers['content-type'] ?? '').startsWith('application/json');
      this.requests.push({
        method: request.method,
        path: request.url,
        headers: request.headers,
        body: raw && isJson ? JSON.parse(raw) : null,
        raw,
      });

      if (this.delay) await new Promise((r) => setTimeout(r, this.delay));

      const key = `${request.method} ${request.url.split('?')[0]}`;
      const answer = this.nextAnswer(key) ?? { status: 404, body: { error: `no route ${request.url}` } };

      // A stream is written a line at a time, like the real API's: a test
      // that received one whole buffer could not tell a client that yields as
      // lines arrive from one that waits for the last.
      if (answer.lines) {
        response.writeHead(answer.status, { 'content-type': 'application/x-ndjson' });
        for (const [i, item] of answer.lines.entries()) {
          if (i && answer.gap) await new Promise((r) => setTimeout(r, answer.gap));
          response.write(JSON.stringify(item) + '\n');
        }
        response.end();
        return;
      }

      const payload = Buffer.from(JSON.stringify(answer.body));
      const headers = { 'content-type': 'application/json', 'content-length': String(payload.length) };
      // What the real API sends: a second on backpressure, five while a
      // dependency set builds, and nothing on any other refusal.
      let retryAfter = answer.retryAfter;
      if (retryAfter === undefined && answer.status === 429) retryAfter = 3;
      if (retryAfter === undefined && answer.body?.code === 'deps_building') retryAfter = 5;
      if (retryAfter !== undefined) headers['retry-after'] = String(retryAfter);
      response.writeHead(answer.status, headers);
      response.end(payload);
    });
    this.server.on('connection', () => {
      this.connections += 1;
    });
  }

  /**
   * Answer this route the same way every time. `retryAfter` sets the header;
   * left out, it is what the real API sends.
   */
  answer(method, path, status, body, retryAfter = undefined) {
    this.answers.set(`${method} ${path}`, { status, body, retryAfter });
  }

  /**
   * Answer this route with each of `answers` in turn, then keep giving the
   * last one — a host that refuses twice and then accepts.
   */
  answerThen(method, path, answers, retryAfter = undefined) {
    this.answers.set(
      `${method} ${path}`,
      answers.map(([status, body]) => ({ status, body, retryAfter }))
    );
  }

  nextAnswer(key) {
    const planned = this.answers.get(key);
    if (!Array.isArray(planned)) return planned;
    return planned.length > 1 ? planned.shift() : planned[0];
  }

  /// Answer this route with NDJSON, one object per line. `gap` is the pause
  /// between them, so a test can tell early delivery from buffering.
  stream(method, path, lines, gap = 0) {
    this.answers.set(`${method} ${path}`, { status: 200, lines, gap });
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
