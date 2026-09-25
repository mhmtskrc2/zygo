// SPDX-License-Identifier: Apache-2.0
// The script `zygo agent test --script` sends inside an `EXEC` (proto 1.1).
//
// A runtime pool holds an interpreter and no tenant code; the script arrives
// with the request and the *worker* loads it. This is the smallest script that
// can prove that happened: it returns a value the suite's own handler never
// would, so "the script ran" and "the agent ignored it and ran its handler"
// are told apart rather than guessed at.
'use strict';

module.exports = function handler(event) {
  return { from: 'the-request' };
};
