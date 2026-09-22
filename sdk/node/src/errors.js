/**
 * What can go wrong, as classes a caller can branch on.
 *
 * The distinctions are the ones that change what to do next, and no others. A
 * handler that threw is not a sandbox that ran out of time, which is not a
 * pool that is full — the first is a bug in the function, the second is a
 * limit doing its job, and the third is worth retrying in a moment. One error
 * class with a message is how a caller ends up parsing English to decide.
 */

export class ZygoError extends Error {
  constructor(message) {
    super(message);
    this.name = new.target.name;
  }
}

/**
 * The API could not be reached, or answered with something unreadable.
 * Never raised because the sandbox failed — that always arrives below, with
 * the sandbox's own output attached.
 */
export class TransportError extends ZygoError {}

/**
 * The token was missing, wrong, or not permitted to do this. A 403 usually
 * means the API was started without `--allow-deploy` and the call was one that
 * creates or destroys a sandbox.
 */
export class AuthError extends ZygoError {}

/** No function under that name. */
export class NotFound extends ZygoError {}

/** The sandbox as described could not be resolved. Always the request. */
export class SpecError extends ZygoError {}

/**
 * The function is at its concurrency limit and refused the request.
 *
 * Not a failure of the call: it is backpressure, and the request never ran.
 * Retrying after `retryAfter` seconds is the intended response.
 */
export class Busy extends ZygoError {
  constructor(message, { inFlight = 0, queued = 0, limit = 0, retryAfter = 1 } = {}) {
    super(message);
    this.inFlight = inFlight;
    this.queued = queued;
    this.limit = limit;
    this.retryAfter = retryAfter;
  }
}

/**
 * The request exceeded the function's timeout and was killed.
 *
 * Zygo's supervisor records that *it* killed the request rather than inferring
 * it: a deadline kill and an out-of-memory kill both surface as exit 137, and
 * only the side that enforced the deadline can tell them apart. So this is
 * never a guess.
 */
export class Timeout extends ZygoError {
  constructor(message, { stderr = '', metrics = {} } = {}) {
    super(message);
    this.stderr = stderr;
    this.metrics = metrics;
  }
}

/**
 * Somebody stopped this request — usually the caller.
 *
 * The third reading of exit 137. A cancel kill, a deadline kill and an
 * out-of-memory kill are one signal and three different things to tell a
 * caller, and only the side that sent the signal knows which it was.
 *
 * Distinct from {@link Timeout} on purpose: a timeout says the work is too slow
 * or the limit is too tight, and this says the answer stopped being wanted.
 */
export class Cancelled extends ZygoError {
  constructor(message, { requestId = '', stdout = '', stderr = '', metrics = {} } = {}) {
    super(message);
    this.requestId = requestId;
    this.stdout = stdout;
    this.stderr = stderr;
    this.metrics = metrics;
  }
}

/** The handler threw. Its message and both streams are attached. */
export class HandlerError extends ZygoError {
  constructor(message, { stdout = '', stderr = '', exitCode = 1, metrics = {} } = {}) {
    super(message);
    this.stdout = stdout;
    this.stderr = stderr;
    this.exitCode = exitCode;
    this.metrics = metrics;
  }
}

/**
 * Map one HTTP answer onto the error a caller should see.
 *
 * Status first, then the body where the status is ambiguous. The fallback
 * carries the status, because an unmapped code is a version skew worth
 * reporting rather than swallowing.
 *
 * @param {number} status
 * @param {Record<string, any>} body
 * @param {number} retryAfter
 * @returns {ZygoError}
 */
export function fromResponse(status, body, retryAfter = 1) {
  const message = String(body?.error ?? body?.message ?? `HTTP ${status}`);
  if (status === 401 || status === 403) return new AuthError(message);
  if (status === 404) return new NotFound(message);
  if (status === 400) return new SpecError(message);
  if (status === 408) {
    return new Timeout(message, { stderr: String(body?.stderr ?? ''), metrics: body?.metrics ?? {} });
  }
  // 499 is nginx's for a client that went away, and the nearest thing to a
  // registered code for a request its caller stopped.
  if (status === 499 || body?.cancelled === true) {
    return new Cancelled(message, {
      requestId: String(body?.request_id ?? ''),
      stdout: String(body?.stdout ?? ''),
      stderr: String(body?.stderr ?? ''),
      metrics: body?.metrics ?? {},
    });
  }
  if (status === 429) {
    return new Busy(message, {
      inFlight: Number(body?.in_flight ?? 0),
      queued: Number(body?.queued ?? 0),
      limit: Number(body?.limit ?? 0),
      retryAfter,
    });
  }
  if (status === 500 && body && 'exit_code' in body) {
    return new HandlerError(message, {
      stdout: String(body.stdout ?? ''),
      stderr: String(body.stderr ?? ''),
      exitCode: Number(body.exit_code ?? 1),
      metrics: body.metrics ?? {},
    });
  }
  return new ZygoError(`${message} (HTTP ${status})`);
}
