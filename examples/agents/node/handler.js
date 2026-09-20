// The handler `zygo agent test` expects, for the Node agent: return the event
// unchanged, and honour `stdout` and `stderr` when they are strings.
module.exports = async function handler(event) {
  if (event && typeof event === 'object') {
    if (typeof event.stdout === 'string') process.stdout.write(event.stdout + '\n');
    if (typeof event.stderr === 'string') process.stderr.write(event.stderr + '\n');
  }
  return event;
};
