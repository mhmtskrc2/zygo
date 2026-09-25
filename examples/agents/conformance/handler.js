// SPDX-License-Identifier: Apache-2.0
// The handler `zygo agent test` expects, for the Node agent.
//
// The conformance suite cannot assert anything about an agent's answers unless
// it knows what the handler was supposed to do, so the contract is four lines:
// return the event unchanged, honour `stdout` and `stderr` when they are
// strings, start a program when `spawn` is one, and sleep for `sleep_ms` when
// it is a number. Every agent ships one of these.
'use strict';

const { execFileSync } = require('child_process');

function sleep(ms) {
  // Blocking on purpose: the point is a handler that is still running when a
  // cancel arrives, and an `await` would hand control back to the loop.
  const until = Date.now() + ms;
  while (Date.now() < until) {
    try {
      execFileSync('/bin/sleep', ['0.01']);
    } catch {
      // No `/bin/sleep`, or the child filter took it away. Spin instead: a
      // busy wait is a worse citizen and a fine subject for a cancel.
      for (let i = 0; i < 1e6; i += 1);
    }
  }
}

module.exports = function handler(event) {
  if (event && typeof event === 'object') {
    if (typeof event.stdout === 'string') process.stdout.write(event.stdout + '\n');
    if (typeof event.stderr === 'string') process.stderr.write(event.stderr + '\n');
    if (typeof event.sleep_ms === 'number') sleep(event.sleep_ms);
    if (typeof event.spawn === 'string') {
      // A *program*, not a function call: this is what the `strict` child
      // filter removes, so it is what the suite has to be able to attempt.
      // A refusal is an outcome, not an error — the suite reads stdout.
      try {
        process.stdout.write(execFileSync('/bin/echo', [event.spawn], { encoding: 'utf8' }));
      } catch (e) {
        process.stderr.write(`spawn refused: ${(e && e.message) || e}\n`);
      }
    }
  }
  return event;
};
