/**
 * Where the API is, and how it was decided.
 *
 * One function, so nothing else has to reimplement the discovery order. Two
 * orders that drift is a support question nobody can answer from the stack
 * trace.
 */

/** What `zygo api` listens on when nothing says otherwise. */
export const DEFAULT_URL = 'http://127.0.0.1:7700';

/**
 * Work out which API to talk to.
 *
 * In order: the argument, then `ZYGO_API_URL`, then loopback on the port
 * `zygo api` uses by default. Accepts `unix:///path/to.sock`,
 * `http://host:port`, `https://host:port` and a bare `host:port`.
 *
 * @param {string} [url]
 * @returns {{url: string, socketPath?: string, host: string, port: number, tls: boolean, isUnix: boolean}}
 */
export function resolve(url) {
  return parse(url || process.env.ZYGO_API_URL || DEFAULT_URL);
}

/** @param {string} text */
export function parse(text) {
  const trimmed = String(text).trim();

  if (trimmed.startsWith('unix://')) {
    const socketPath = trimmed.slice('unix://'.length);
    if (!socketPath) {
      throw new TypeError('unix:// needs a path, e.g. unix:///run/user/1000/zygo/api.sock');
    }
    return { url: trimmed, socketPath, host: 'localhost', port: 0, tls: false, isUnix: true };
  }

  let tls = false;
  let rest = trimmed;
  if (trimmed.startsWith('https://')) {
    tls = true;
    rest = trimmed.slice('https://'.length);
  } else if (trimmed.startsWith('http://')) {
    rest = trimmed.slice('http://'.length);
  } else if (trimmed.includes('://')) {
    const scheme = trimmed.split('://', 1)[0];
    throw new TypeError(`\`${scheme}://\` is not an address Zygo serves; use http://, https:// or unix://`);
  }

  // A trailing path is dropped rather than honoured: every route this client
  // calls is rooted, and silently prefixing them would turn a typo in the
  // address into a 404 on every call.
  rest = rest.split('/')[0];
  const colon = rest.lastIndexOf(':');
  const host = colon === -1 ? rest : rest.slice(0, colon);
  const portText = colon === -1 ? (tls ? '443' : '7700') : rest.slice(colon + 1);
  const port = Number(portText);
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    throw new TypeError(`\`${portText}\` is not a port number, in \`${text}\``);
  }
  if (!host) throw new TypeError(`\`${text}\` names no host`);

  return {
    url: `${tls ? 'https' : 'http'}://${host}:${port}`,
    host,
    port,
    tls,
    isUnix: false,
  };
}
