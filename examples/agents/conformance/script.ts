// The script `zygo agent test --script` sends inside an `EXEC` (proto 1.1),
// in TypeScript.
//
// Same job as `script.js`: return a value the suite's own handler never would,
// so "the script ran" and "the agent ignored it and ran its handler" are told
// apart rather than guessed at. What it adds is the language — an agent that
// serves TypeScript has to do it here, in the child, after the fork, with no
// build step anywhere and the digest still over the source as uploaded.
//
// The `enum` is deliberate. Types alone blank out, and a runtime that only
// blanks them passes on most TypeScript and fails on a tenant's; an enum is
// code, so it needs the transform and is the honest check.

enum Origin {
  Handler = 'the-handler',
  Request = 'the-request',
}

interface Answer {
  from: Origin;
}

export function handler(event: unknown): Answer {
  return { from: Origin.Request };
}
