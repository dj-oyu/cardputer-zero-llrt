// examples/aws-edge/edge_gateway.mjs — standalone cz-js HTTPS edge gateway (Phase 9A).
//
// Accepts telemetry over TLS on 127.0.0.1:0 (ephemeral), printing `READY:<port>` once bound,
// and syncs each accepted event Edge-to-Cloud through aws-emulator:
//   POST /telemetry  { id, device, payload }
//     1) DynamoDB conditional PutItem (`attribute_not_exists(pk`) is the IDEMPOTENCY GUARD —
//        a duplicate id answers 409 Conflict WITHOUT overwriting S3 or KV.
//     2) Payload JSON lands in S3 at `telemetry/<id>.json`.
//     3) A summary is persisted in `cardputer:kv` (persistPath + flush() → durable on device).
//     4) A notification { id, device, s3Key } goes to SQS.
//     5) 201 Created { ok, id, s3Key, messageId }.
//   GET  /local/:id (or /local?id=...) reads the persisted KV entry back (200 JSON / 404).
//   POST /shutdown replies 200 then closes the server so cz-js exits cleanly with status 0.
//
// Config via env: GATEWAY_CERT, GATEWAY_KEY (PEM paths), GATEWAY_KV_PATH (persist file),
// AWS_BUCKET, AWS_TABLE, AWS_QUEUE_URL (plus AWS_* endpoint/creds from aws_client.mjs).

import https from "node:https";
import fs from "node:fs";
import kv from "cardputer:kv";
import { ddbPutItem, s3PutObject, sqsSendMessage } from "./aws_client.mjs";

const env = (typeof globalThis.process !== "undefined" && process.env) || {};
const certPath = env.GATEWAY_CERT;
const keyPath = env.GATEWAY_KEY;
if (!certPath || !keyPath) throw new Error("GATEWAY_CERT and GATEWAY_KEY must point to PEM files");

const bucket = env.AWS_BUCKET;
const table = env.AWS_TABLE;
const queueUrl = env.AWS_QUEUE_URL;
if (!bucket || !table || !queueUrl) {
  throw new Error("AWS_BUCKET, AWS_TABLE and AWS_QUEUE_URL are required for the edge gateway");
}

const store = kv.open("cz-edge-gateway", { persistPath: env.GATEWAY_KV_PATH || "" });

function jsonReply(res, status, obj) {
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(JSON.stringify(obj));
}

async function readBody(req) {
  // cz-js delivers the FULL Content-Length body in one data event; late listeners replay too.
  let body = "";
  req.on("data", (c) => { body += typeof c === "string" ? c : String(c); });
  await new Promise((resolve, reject) => {
    req.on("end", resolve);
    req.on("error", reject);
    setTimeout(() => resolve(), 2000); // safety valve if a client never ends (buffered already)
  });
  return body;
}

async function handleTelemetry(req, res) {
  let parsed;
  try {
    parsed = JSON.parse(await readBody(req) || "");
  } catch (_e) {
    return jsonReply(res, 400, { error: "invalid json" });
  }
  const id = parsed.id;
  const device = parsed.device;
  const payload = parsed.payload;
  if (typeof id !== "string" || !id || typeof device !== "string" || !device) {
    return jsonReply(res, 400, { error: "id and device are required" });
  }
  const s3Key = `telemetry/${id}.json`;

  // 1) Idempotency guard: DynamoDB conditional write rejects replays without side effects.
  const put = await ddbPutItem(table, { pk: { S: id }, device: { S: device }, s3Key: { S: s3Key } }, "attribute_not_exists(pk)");
  if (!put.ok) {
    if (put.conditionalFailed) return jsonReply(res, 409, { error: "duplicate", id });
    return jsonReply(res, 502, { error: "ddb put failed" });
  }

  // 2) S3 object with the full telemetry document.
  await s3PutObject(bucket, s3Key, JSON.stringify({ id, device, payload }), "application/json");

  // 3) Local durable summary (persistPath + flush → survives process restarts).
  //    cardputer:kv values are primitive-typed, so the summary rides as explicit fields
  //    (the same setField/row pattern hono-kv-demo uses); payload stays a JSON string.
  store.setField(id, "id", id);
  store.setField(id, "device", device);
  store.setField(id, "s3Key", s3Key);
  store.setField(id, "acceptedAt", new Date().toISOString());
  store.setField(id, "payloadJson", JSON.stringify(payload === undefined ? null : payload));
  store.flush();

  // 4) Cloud notification through SQS.
  const messageId = await sqsSendMessage(queueUrl, JSON.stringify({ id, device, s3Key }));

  // 5) Created.
  jsonReply(res, 201, { ok: true, id, s3Key, messageId });
}

function handleLocal(req, res, urlPath) {
  let id = "";
  const qIdx = urlPath.indexOf("?");
  if (urlPath.startsWith("/local/")) {
    id = decodeURIComponent(urlPath.slice("/local/".length));
  } else if (qIdx >= 0 && urlPath.startsWith("/local")) {
    for (const pair of urlPath.slice(qIdx + 1).split("&")) {
      const [k, v] = pair.split("=");
      if (k === "id") id = decodeURIComponent(v || "");
    }
  }
  if (!id) return jsonReply(res, 400, { error: "id required" });
  // Reassemble the persisted entry from KV fields (primitives only; payloadJson is embedded).
  if (!store.hasField(id, "device")) return jsonReply(res, 404, { error: "not found", id });
  const entry = {
    id: store.getField(id, "id") ?? id,
    device: store.getField(id, "device"),
    s3Key: store.getField(id, "s3Key"),
    acceptedAt: store.getField(id, "acceptedAt"),
    payload: JSON.parse(store.getField(id, "payloadJson") ?? "null"),
  };
  jsonReply(res, 200, entry);
}

let server = null;

server = https.createServer({ cert: fs.readFileSync(certPath, "utf8"), key: fs.readFileSync(keyPath, "utf8") }, async (req, res) => {
  try {
    const urlPath = req.url || "/";
    if (req.method === "POST" && urlPath.startsWith("/telemetry")) {
      await handleTelemetry(req, res);
    } else if (req.method === "GET" && (urlPath.startsWith("/local/") || urlPath.startsWith("/local?"))) {
      handleLocal(req, res, urlPath);
    } else if (req.method === "POST" && urlPath.startsWith("/shutdown")) {
      jsonReply(res, 200, { ok: true, bye: true });
      setTimeout(() => server.close(), 30);
    } else {
      jsonReply(res, 404, { error: "no route", url: urlPath });
    }
  } catch (err) {
    jsonReply(res, 500, { error: String((err && err.message) || err) });
  }
});

server.listen(0, "127.0.0.1");
console.log(`READY:${server.address().port}`);
