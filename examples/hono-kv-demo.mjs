// examples/hono-kv-demo.mjs — real-world Hono demo on CardputerZero proving:
//   * `cardputer:kv` row serving: c.json(sessions.row(id)) DIRECT and NESTED
//     c.json({ ok: true, session: sessions.row(id) }) — both serialize straight from Rust
//     EntryPayload::Record (zero-copy Proxy handles).
//   * Transparent Lazy JSON: POST >= 4 KiB JSON bodies -> await c.req.json() yields a handle with
//     czJson.isLazy(body) === true (the cz-json engine swaps globalThis.JSON.parse in cz-js/llrt).
//
// Runs on cz-llrt AND standalone cz-js via ./hono-adapter.mjs (Web Request/Response polyfill on
// realms without them). Import ORDER matters: the adapter installs Web globals as an import
// side-effect, so it is imported BEFORE hono below.

import { serve } from "./hono-adapter.mjs";
import { Hono } from "./node_modules/hono/index.mjs";
import * as cardputerKv from "cardputer:kv";
import * as czJson from "cardputer:json";

const sessions = cardputerKv.open("cz_demo_sessions");
// Pre-populated session s1 per spec.
sessions.setField("s1", "user", "jimmy");
sessions.setField("s1", "role", "admin");
sessions.setField("s1", "count", 7);

const app = new Hono();

app.get("/health", (c) => c.json({ ok: true, runtime: "cardputer-zero-llrt" }));

app.get("/session/:id", (c) => c.json(sessions.row(c.req.param("id"))));

app.get("/session-wrap/:id", (c) => c.json({ ok: true, session: sessions.row(c.req.param("id")) }));

app.post("/big-json", async (c) => {
  const body = await c.req.json();
  return c.json({ lazy: czJson.isLazy(body), item42: body.items[42].title });
});

let server = null;
app.post("/shutdown", (c) => {
  // Respond first, then close the listener cleanly so the event loop drains and exits 0.
  setTimeout(() => {
    if (server) server.close();
  }, 50);
  return c.json({ bye: true });
});

// Port selection safe on BOTH realms: `process` only exists in Node/LLRT; guard it.
const envPort =
  typeof globalThis.process !== "undefined" && globalThis.process && globalThis.process.env
    ? Number(globalThis.process.env.PORT || globalThis.process.env.CZ_DEMO_PORT || 0)
    : 0;
const wantedPort = Number(envPort || globalThis.__PORT || 0);

server = serve(app, {
  port: wantedPort,
  hostname: "127.0.0.1",
  onListen: (port) => console.log("HONO_READY " + port),
});
