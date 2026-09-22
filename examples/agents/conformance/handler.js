// The handler `zygo agent test` expects, for the Node agent.
//
// The conformance suite cannot assert anything about an agent's answers unless
// it knows what the handler was supposed to do, so the contract is four lines:
// return the event unchanged, honour `stdout` and `stderr` when they are
// strings, and start a program when `spawn` is one. Every agent ships one of
// these.
'use strict';

const { execFileSync } = require('child_process');

module.exports = function handler(event) {
  if (event && typeof event === 'object') {
    if (typeof event.stdout === 'string') process.stdout.write(event.stdout + '\n');
    if (typeof event.stderr === 'string') process.stderr.write(event.stderr + '\n');
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
