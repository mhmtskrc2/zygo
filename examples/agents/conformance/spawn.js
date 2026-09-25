// SPDX-License-Identifier: Apache-2.0
// The script `zygo agent test --script-spawn` sends (proto 1.1).
//
// It starts a program in its **module body**, which is the whole point: a
// script's load-time code is request code, and under `strict` the child filter
// has to be installed before it runs. An agent that loads the script first and
// tightens afterwards passes every other check in the suite and leaves a
// pool's scripts able to start programs.
//
// The suite runs this twice — once on an unfiltered agent, where the program
// must run, and once under `ZYGO_CHILD_SECCOMP`, where it must not. A refusal
// is an outcome rather than an error, so it is caught and reported rather than
// thrown: the request failing is also a conforming answer.
'use strict';

// `zygo-child-spawned`, the word the suite greps for in stdout. Written by a
// *program*, not by this process, because a program is what the filter takes
// away.
try {
  const { execFileSync } = require('child_process');
  process.stdout.write(execFileSync('/bin/echo', ['zygo-child-spawned'], { encoding: 'utf8' }));
} catch (e) {
  process.stderr.write(`spawn refused at load: ${(e && e.message) || e}\n`);
}

module.exports = function handler(event) {
  return { from: 'the-request' };
};
