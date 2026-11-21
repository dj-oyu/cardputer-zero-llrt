// examples/aws-edge/aws_client.mjs — zero-external-SDK AWS client for cz-js (Phase 9A).
//
// Signs every request with standard AWS Signature Version 4 using cz-js's ring-backed
// `node:crypto` (createHash/createHmac) and sends it through native fetch(). Runs unchanged
// on cz-js AND Node (verified live against the local aws-emulator via AWS_ENDPOINT_URL).
//
// Emulator/protocol notes (Playbook §7 + measured quirks):
//   * S3 MUST use path-style URLs (`${endpoint}/${bucket}/${key}`).
//   * DynamoDB is JSON 1.0 (`X-Amz-Target: DynamoDB_20120810.*`, Content-Type application/json).
//   * SQS is JSON protocol (`X-Amz-Target: AmazonSQS.*`); CreateQueue answers with a real
//     `https://sqs.<region>.amazonaws.com/...` QueueUrl, so transport ALWAYS goes to the
//     emulator endpoint root (`${endpoint}/`) with that QueueUrl carried in the JSON body.
//   * Always pass `BillingMode: "PAY_PER_REQUEST"`; poll DescribeTable until ACTIVE (~1–7 s).
//   * ConditionalCheckFailedException is a REAL semantic rejection — never retried here.

import crypto from "node:crypto";

const env = (typeof globalThis.process !== "undefined" && process.env) || {};

function opt(v, d) {
  return v === undefined || v === null ? d : v;
}

/** Effective config: explicit options win over process.env (defaults per sandbox contract). */
export function awsConfig(options = {}) {
  return {
    endpoint: opt(options.endpoint, env.AWS_ENDPOINT_URL || ""),
    accessKey: opt(options.accessKeyId, env.AWS_ACCESS_KEY_ID || "test"),
    secretKey: opt(options.secretAccessKey, env.AWS_SECRET_ACCESS_KEY || "test"),
    region: opt(options.region, env.AWS_DEFAULT_REGION || "us-east-1"),
  };
}

function uriEncode(segment) {
  // AWS SigV4 path encoding: RFC 3986 unreserved only (!'()*~ MUST be percent-encoded).
  return encodeURIComponent(String(segment)).replace(
    /[!'()*~]/g,
    (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase()
  );
}

function hmacBytes(key, data) {
  return crypto.createHmac("sha256", key).update(data).digest();
}

function hexDigest(payload) {
  // cz-js's ring-backed crypto.update accepts string | Uint8Array | ArrayBuffer directly;
  // an empty payload hashes the zero-length byte string (SigV4 UNSIGNED-style empty hash).
  const input = payload === null || payload === undefined || payload === ""
    ? new Uint8Array(0)
    : (typeof payload === "string" ? payload : new Uint8Array(payload));
  return crypto.createHash("sha256").update(input).digest("hex");
}

function amzDates() {
  const iso = new Date().toISOString().replace(/[:\-]/g, "").slice(0, 15) + "Z";
  return { amzdate: iso, shortdate: iso.slice(0, 8) };
}

/** Pure-JS URL splitter (cz-js realms may lack `URL`; Node-compatible output shape). */
function splitUrl(urlStr) {
  const m = /^([a-zA-Z][a-zA-Z0-9+.-]*:\/\/)([^/?#]+)([^?#]*)(\?[^#]*)?$/.exec(urlStr);
  if (!m) throw new Error("invalid url: " + urlStr);
  let authority = m[2];
  const at = authority.lastIndexOf("@");
  if (at >= 0) authority = authority.slice(at + 1);
  let hostname, port = "";
  if (authority.startsWith("[")) {
    const end = authority.indexOf("]");
    hostname = authority.slice(0, end + 1);
    const rest = authority.slice(end + 1);
    if (rest.startsWith(":")) port = rest.slice(1);
  } else {
    const colon = authority.lastIndexOf(":");
    hostname = colon >= 0 ? authority.slice(0, colon) : authority;
    port = colon >= 0 ? authority.slice(colon + 1) : "";
  }
  return {
    protocol: m[1].slice(0, m[1].indexOf("://")).toLowerCase(),
    hostname,
    port,
    pathname: m[3] || "/",
    search: m[4] ? m[4].slice(1) : "",
  };
}

/** UTF-8 decode for Uint8Array bodies without relying on a global TextDecoder. */
function utf8Decode(bytes) {
  let out = "";
  for (let i = 0; i < bytes.length;) {
    const b = bytes[i];
    if (b < 0x80) { out += String.fromCharCode(b); i += 1; }
    else if (b < 0xe0) { out += String.fromCharCode(((b & 0x1f) << 6) | (bytes[i + 1] & 0x3f)); i += 2; }
    else if (b < 0xf0) {
      out += String.fromCharCode(((b & 0x0f) << 12) | ((bytes[i + 1] & 0x3f) << 6) | (bytes[i + 2] & 0x3f));
      i += 3;
    } else {
      const cp = ((b & 0x07) << 18) | ((bytes[i + 1] & 0x3f) << 12) | ((bytes[i + 2] & 0x3f) << 6) | (bytes[i + 3] & 0x3f);
      const surrogate = cp - 0x10000;
      out += String.fromCharCode(0xd800 + (surrogate >> 10), 0xdc00 + (surrogate & 0x3ff));
      i += 4;
    }
  }
  return out;
}

/**
 * Full SigV4 sign for `urlStr`. Returns headers to send: extra headers plus x-amz-date,
 * x-amz-content-sha256 (S3) and the Authorization header. Canonical headers are keyed by
 * LOWERCASE names with trimmed single-space values (the exact rule aws-emulator enforces).
 */
export function sigv4Headers(service, method, urlStr, cfg, extraHeaders = {}, payload = "") {
  const { amzdate, shortdate } = amzDates();
  const url = splitUrl(urlStr);
  let host = url.hostname;
  if (url.port && !(url.protocol === "http" && url.port === "80") && !(url.protocol === "https" && url.port === "443")) {
    host += ":" + url.port;
  }
  const payloadHash = hexDigest(payload);
  const canon = { host, "x-amz-date": amzdate };
  if (service === "s3") canon["x-amz-content-sha256"] = payloadHash;
  for (const [k, v] of Object.entries(extraHeaders)) canon[k.toLowerCase()] = String(v);
  const names = Object.keys(canon).sort();
  const canonicalHeaders = names.map((n) => n + ":" + canon[n].trim().replace(/\s+/g, " ") + "\n").join("");
  const signedHeaders = names.join(";");
  const canonicalPath = url.pathname.split("/").map(uriEncode).join("/");
  const canonicalQuery = url.search.replace(/^\?/, "");
  const canonicalRequest = [method, canonicalPath, canonicalQuery, canonicalHeaders, signedHeaders, payloadHash].join("\n");
  const scope = `${shortdate}/${cfg.region}/${service}/aws4_request`;
  const stringToSign = ["AWS4-HMAC-SHA256", amzdate, scope, hexDigest(canonicalRequest)].join("\n");
  let k = hmacBytes("AWS4" + cfg.secretKey, shortdate);
  k = hmacBytes(k, cfg.region);
  k = hmacBytes(k, service);
  k = hmacBytes(k, "aws4_request");
  const signature = crypto.createHmac("sha256", k).update(stringToSign).digest("hex");

  const headers = { ...extraHeaders };
  headers["x-amz-date"] = amzdate;
  if (service === "s3") headers["x-amz-content-sha256"] = payloadHash;
  headers.Authorization =
    `AWS4-HMAC-SHA256 Credential=${cfg.accessKey}/${scope}, SignedHeaders=${signedHeaders}, Signature=${signature}`;
  return headers;
}

async function request(service, method, urlStr, cfg, extraHeaders = {}, body = null) {
  const payload = body === null || body === undefined ? "" : body;
  const headers = sigv4Headers(service, method, urlStr, cfg, extraHeaders, payload);
  const init = { method, headers };
  if (body !== null && body !== undefined) init.body = body;
  const res = await fetch(urlStr, init);
  return res;
}

// ---------------------------------------------------------------------------
// Admin health
// ---------------------------------------------------------------------------

/** GET /__admin/health → { ok, status, text } */
export async function healthCheck(endpoint, cfg = {}) {
  const c = awsConfig(cfg);
  const base = endpoint || c.endpoint;
  const res = await request("s3", "GET", `${base}/__admin/health`, { ...c, endpoint: base }, {}, null);
  const text = await res.text();
  return { ok: res.ok, status: res.status, text };
}

// ---------------------------------------------------------------------------
// S3 (path-style)
// ---------------------------------------------------------------------------

function s3Url(cfg, bucket, key) {
  return `${cfg.endpoint}/${bucket}${key ? "/" + key : ""}`;
}

export async function s3CreateBucket(bucket, cfg = {}) {
  const c = awsConfig(cfg);
  const res = await request("s3", "PUT", s3Url(c, bucket, ""), c, {}, "");
  if (!res.ok) throw new Error(`s3CreateBucket ${bucket} -> ${res.status}: ${(await res.text()).slice(0, 200)}`);
  return true;
}

export async function s3PutObject(bucket, key, body, contentType, cfg = {}) {
  const c = awsConfig(cfg);
  const ct = contentType || "application/octet-stream";
  const payload = typeof body === "string" ? body : new Uint8Array(body);
  const res = await request("s3", "PUT", s3Url(c, bucket, key), c, { "Content-Type": ct }, payload);
  if (!res.ok) throw new Error(`s3PutObject ${bucket}/${key} -> ${res.status}: ${(await res.text()).slice(0, 200)}`);
  return true;
}

/** GET object → { status, bytes: Uint8Array, text } */
export async function s3GetObject(bucket, key, cfg = {}) {
  const c = awsConfig(cfg);
  const res = await request("s3", "GET", s3Url(c, bucket, key), c, {}, null);
  const bytes = new Uint8Array(await res.arrayBuffer());
  return { status: res.status, bytes, text: utf8Decode(bytes) };
}

export async function s3DeleteObject(bucket, key, cfg = {}) {
  const c = awsConfig(cfg);
  const res = await request("s3", "DELETE", s3Url(c, bucket, key), c, {}, null);
  return res.status === 204 || res.status === 200;
}

export async function s3DeleteBucket(bucket, cfg = {}) {
  const c = awsConfig(cfg);
  const res = await request("s3", "DELETE", s3Url(c, bucket, ""), c, {}, null);
  return res.status === 204 || res.status === 200;
}

// ---------------------------------------------------------------------------
// DynamoDB (JSON 1.0 target protocol)
// ---------------------------------------------------------------------------

const JSON_HDRS = { "Content-Type": "application/json" };

async function ddbCall(target, bodyObj, cfg) {
  const c = awsConfig(cfg);
  const json = JSON.stringify(bodyObj);
  const res = await request("dynamodb", "POST", `${c.endpoint}/`, c, { ...JSON_HDRS, "X-Amz-Target": `DynamoDB_20120810.${target}` }, json);
  let parsed = null;
  try { parsed = await res.json(); } catch (_e) { parsed = null; }
  return { status: res.status, json: parsed };
}

/** PAY_PER_REQUEST table with a single string partition key `pkName`. */
export async function ddbCreateTable(tableName, pkName, cfg = {}) {
  const r = await ddbCall("CreateTable", {
    TableName: tableName,
    AttributeDefinitions: [{ AttributeName: pkName, AttributeType: "S" }],
    KeySchema: [{ AttributeName: pkName, KeyType: "HASH" }],
    BillingMode: "PAY_PER_REQUEST",
  }, cfg);
  if (r.status !== 200) throw new Error(`ddbCreateTable ${tableName} -> ${r.status}: ${JSON.stringify(r.json).slice(0, 250)}`);
  return true;
}

/** Poll DescribeTable every 300 ms until TableStatus === "ACTIVE" (emulator takes ~1–7 s). */
export async function ddbWaitTableActive(tableName, timeoutMs = 15000, cfg = {}) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const r = await ddbCall("DescribeTable", { TableName: tableName }, cfg);
    const status = r.json?.Table?.TableStatus;
    if (status === "ACTIVE") return true;
    if (r.status !== 200) throw new Error(`DescribeTable ${tableName} -> ${r.status}`);
    await new Promise((resolve) => setTimeout(resolve, 300));
  }
  throw new Error(`ddbWaitTableActive timeout after ${timeoutMs} ms (${tableName})`);
}

/**
 * PutItem with optional ConditionExpression. Returns `{ ok: true }` or
 * `{ ok: false, conditionalFailed: true }` — ConditionalCheckFailedException is a real
 * semantic rejection and is NEVER retried (Playbook §7).
 */
export async function ddbPutItem(tableName, item, conditionExpression, cfg = {}) {
  const body = { TableName: tableName, Item: item };
  if (conditionExpression) body.ConditionExpression = conditionExpression;
  const r = await ddbCall("PutItem", body, cfg);
  if (r.status === 200) return { ok: true };
  const type = String(r.json?.__type || "");
  if (r.status === 400 && type.includes("ConditionalCheckFailedException")) {
    return { ok: false, conditionalFailed: true };
  }
  throw new Error(`ddbPutItem ${tableName} -> ${r.status}: ${JSON.stringify(r.json).slice(0, 250)}`);
}

export async function ddbGetItem(tableName, key, cfg = {}) {
  const r = await ddbCall("GetItem", { TableName: tableName, Key: key }, cfg);
  if (r.status !== 200) throw new Error(`ddbGetItem ${tableName} -> ${r.status}: ${JSON.stringify(r.json).slice(0, 250)}`);
  return r.json?.Item ?? null;
}

export async function ddbScan(tableName, cfg = {}) {
  const r = await ddbCall("Scan", { TableName: tableName }, cfg);
  if (r.status !== 200) throw new Error(`ddbScan ${tableName} -> ${r.status}`);
  return Array.isArray(r.json?.Items) ? r.json.Items : [];
}

export async function ddbDeleteTable(tableName, cfg = {}) {
  const r = await ddbCall("DeleteTable", { TableName: tableName }, cfg);
  return r.status === 200;
}

// ---------------------------------------------------------------------------
// SQS (JSON protocol through the emulator endpoint root)
// ---------------------------------------------------------------------------

const SQS_TARGET = (action) => ({ "Content-Type": "application/json", "X-Amz-Target": `AmazonSQS.${action}` });

async function sqsCall(action, bodyObj, cfg) {
  const c = awsConfig(cfg);
  const json = JSON.stringify(bodyObj);
  // Transport is ALWAYS the endpoint root; the (AWS-shaped) QueueUrl rides inside the body.
  const res = await request("sqs", "POST", `${c.endpoint}/`, c, SQS_TARGET(action), json);
  let parsed = null;
  try { parsed = await res.json(); } catch (_e) { parsed = null; }
  return { status: res.status, json: parsed };
}

/** CreateQueue → the canonical QueueUrl (transport stays on AWS_ENDPOINT_URL). */
export async function sqsCreateQueue(queueName, cfg = {}) {
  const c = awsConfig(cfg);
  const r = await sqsCall("CreateQueue", { QueueName: queueName }, c);
  if (r.status !== 200 || !r.json?.QueueUrl) throw new Error(`sqsCreateQueue ${queueName} -> ${r.status}: ${JSON.stringify(r.json).slice(0, 250)}`);
  return r.json.QueueUrl;
}

export async function sqsSendMessage(queueUrl, messageBody, cfg = {}) {
  const r = await sqsCall("SendMessage", { QueueUrl: queueUrl, MessageBody: messageBody }, cfg);
  if (r.status !== 200 || !r.json?.MessageId) throw new Error(`sqsSendMessage -> ${r.status}: ${JSON.stringify(r.json).slice(0, 250)}`);
  return r.json.MessageId;
}

/** ReceiveMessages → [{ messageId, receiptHandle, body }, ...] */
export async function sqsReceiveMessages(queueUrl, maxMessages = 1, cfg = {}) {
  const r = await sqsCall("ReceiveMessage", { QueueUrl: queueUrl, MaxNumberOfMessages: maxMessages }, cfg);
  if (r.status !== 200) throw new Error(`sqsReceiveMessages -> ${r.status}`);
  const msgs = Array.isArray(r.json?.Messages) ? r.json.Messages : [];
  return msgs.map((m) => ({ messageId: m.MessageId, receiptHandle: m.ReceiptHandle, body: m.Body }));
}

export async function sqsDeleteMessage(queueUrl, receiptHandle, cfg = {}) {
  const r = await sqsCall("DeleteMessage", { QueueUrl: queueUrl, ReceiptHandle: receiptHandle }, cfg);
  return r.status === 200;
}

export async function sqsDeleteQueue(queueUrl, cfg = {}) {
  const r = await sqsCall("DeleteQueue", { QueueUrl: queueUrl }, cfg);
  return r.status === 200;
}
