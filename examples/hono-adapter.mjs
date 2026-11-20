// examples/hono-adapter.mjs — thin node:http <-> Web Standard (Request/Response) adapter for
// Hono on BOTH `cz-llrt` (native Web Request/Response/Headers) and standalone `cz-js`.
//
// On cz-js (QuickJS realm with no Web Fetch classes) a minimal fallback polyfill is installed on
// globalThis so `hono` can run unmodified:
//   * Headers  — lowercase key/value list with get/set/append/delete/has/forEach + iterator.
//   * Request  — url/method/headers + body text; `json()` calls JSON.parse(bodyText) directly, so
//     the cz-json transparent Lazy Proxy activates automatically on >= 4 KiB payloads!
//   * Response — status/headers/body text with `text()`; Hono constructs these via `new Response`.
//
// `serve(app, { port, hostname, onListen })` returns the `node:http` Server instance.

import http from "node:http";

const hasWebStd =
  typeof globalThis.Request !== "undefined" &&
  typeof globalThis.Response !== "undefined" &&
  typeof globalThis.Headers !== "undefined";

function chunkToText(chunk) {
  if (typeof chunk === "string") return chunk;
  if (typeof TextDecoder !== "undefined") {
    try {
      return new TextDecoder("utf-8").decode(chunk);
    } catch (_e) { /* fall through */ }
  }
  // Buffer-like values stringify to their utf8 content; Uint8Array falls back to String().
  return String(chunk);
}

export function installWebFallback() {
  if (hasWebStd) return false;

  class HeadersPolyfill {
    constructor(init) {
      this._h = []; // array of [lowerName, value] pairs preserves insertion order
      if (init == null) return;
      if (typeof init.forEach === "function") {
        init.forEach((value, key) => this.append(key, value));
        return;
      }
      if (typeof init[Symbol.iterator] === "function") {
        for (const pair of init) this.append(pair[0], pair[1]);
        return;
      }
      for (const key of Object.keys(init)) this.append(key, init[key]);
    }
    get(name) {
      const n = String(name).toLowerCase();
      for (const e of this._h) if (e[0] === n) return e[1];
      return null;
    }
    set(name, value) {
      const n = String(name).toLowerCase();
      for (const e of this._h) {
        if (e[0] === n) { e[1] = String(value); return; }
      }
      this._h.push([n, String(value)]);
    }
    append(name, value) {
      const got = this.get(name);
      if (got !== null) this.set(name, got + ", " + String(value));
      else this._h.push([String(name).toLowerCase(), String(value)]);
    }
    delete(name) {
      const n = String(name).toLowerCase();
      this._h = this._h.filter((e) => e[0] !== n);
    }
    has(name) { return this.get(name) !== null; }
    forEach(fn) {
      for (const e of this._h) fn(e[1], e[0], this);
    }
    [Symbol.iterator]() {
      let i = 0;
      const h = this._h;
      return {
        next() {
          if (i < h.length) return { value: [h[i][0], h[i][1]], done: false };
          return { value: undefined, done: true };
        },
      };
    }
  }

  class RequestPolyfill {
    constructor(url, init = {}) {
      this.url = String(url);
      this.method = String(init.method || "GET").toUpperCase();
      this.headers = init.headers instanceof HeadersPolyfill
        ? init.headers
        : new HeadersPolyfill(init.headers);
      // Body is held as TEXT so req.json() -> JSON.parse(bodyText) hits the cz-json engine and
      // its transparent Lazy Proxy activates on >= 4 KiB documents.
      this._bodyText =
        init.body === undefined || init.body === null
          ? ""
          : typeof init.body === "string"
            ? init.body
            : chunkToText(init.body);
      this.bodyUsed = false;
    }
    async text() { return this._bodyText; }
    async json() { return JSON.parse(this._bodyText); }
    async arrayBuffer() {
      // Minimal: expose utf8 bytes via a plain ArrayBuffer-shaped Uint8Array.
      const b = new Uint8Array(this._bodyText.length);
      for (let i = 0; i < this._bodyText.length; i++) b[i] = this._bodyText.charCodeAt(i) & 0xff;
      return b.buffer !== undefined ? b : b;
    }
    clone() {
      return new RequestPolyfill(this.url, {
        method: this.method,
        headers: this.headers,
        body: this._bodyText,
      });
    }
  }

  class ResponsePolyfill {
    constructor(body, init = {}) {
      this._body = body === undefined || body === null ? null : String(body);
      this.status = init.status === undefined ? 200 : Number(init.status);
      this.statusText = init.statusText === undefined ? "" : String(init.statusText);
      this.headers = init.headers instanceof HeadersPolyfill
        ? init.headers
        : new HeadersPolyfill(init.headers);
      this.ok = this.status >= 200 && this.status < 300;
      this.bodyUsed = false;
    }
    async text() { this.bodyUsed = true; return this._body === null ? "" : this._body; }
    async json() { return JSON.parse(await this.text()); }
    clone() {
      return new ResponsePolyfill(this._body, {
        status: this.status,
        statusText: this.statusText,
        headers: this.headers,
      });
    }
  }

  globalThis.Headers = HeadersPolyfill;
  globalThis.Request = RequestPolyfill;
  globalThis.Response = ResponsePolyfill;
  return true;
}

installWebFallback(); // side-effect on import: hono sees Web globals regardless of realm order

function buildUrl(req, serverPort) {
  const raw = String(req.url || "/");
  if (/^https?:/i.test(raw)) return raw;
  const host = (req.headers && req.headers.host) || "127.0.0.1:" + serverPort;
  return "http://" + host + raw;
}

/**
 * Serve a Hono app through node:http. Returns the created Server instance.
 * @param {{fetch(request: Request): Response | Promise<Response>}} app
 * @param {{port?: number, hostname?: string, onListen?: (port:number)=>void}} opts
 */
export function serve(app, opts = {}) {
  const port = opts.port === undefined ? 0 : Number(opts.port) || 0;
  const hostname = opts.hostname || "127.0.0.1";

  const server = http.createServer((req, res) => {
    const chunks = [];
    req.on("data", (d) => chunks.push(d));
    req.on("end", () => {
      const bodyText = chunks.map(chunkToText).join("");
      const method = String(req.method || "GET").toUpperCase();
      const reqInit = { method, headers: req.headers };
      if (method !== "GET" && method !== "HEAD" && bodyText.length > 0) {
        reqInit.body = bodyText;
      }
      const webReq = new globalThis.Request(buildUrl(req, server.address() ? server.address().port : 0), reqInit);
      Promise.resolve(app.fetch(webReq))
        .then((webRes) => webRes.text().then((txt) => {
          res.statusCode = webRes.status;
          if (webRes.statusText) res.statusMessage = webRes.statusText;
          // Node's res.setHeader overwrites repeated names; coalesce multi-values (set-cookie
          // style duplication is rare in Hono JSON responses — join keeps every value visible).
          const seen = new Map();
          webRes.headers.forEach((value, name) => {
            const key = String(name).toLowerCase();
            if (key === "content-length") return; // node/http engine sets the exact length
            const prev = seen.get(key);
            seen.set(key, prev === undefined ? String(value) : prev + ", " + String(value));
          });
          for (const [name, value] of seen) res.setHeader(name, value);
          res.end(txt);
        }))
        .catch((e) => {
          try {
            res.statusCode = 500;
            res.setHeader("content-type", "application/json");
            res.end(JSON.stringify({ error: String((e && e.message) || e) }));
          } catch (_e2) { /* socket already gone */ }
        });
    });
    req.on("error", () => {
      try { res.destroy ? res.destroy() : res.end(); } catch (_e) {}
    });
  });

  server.listen(port, hostname, () => {
    if (typeof opts.onListen === "function") {
      opts.onListen(server.address().port);
    }
  });
  return server;
}
