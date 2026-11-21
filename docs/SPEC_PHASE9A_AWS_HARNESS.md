# Cardputer Zero LLRT — Phase 9A Technical Specification (`process`, `node:crypto`, and `aws-emulator` Integration Harness)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM, Debian trixie).
Execution environment: `pi-sandbox` with `-WithAwsEmulator` (`AWS_ENDPOINT_URL`, `AWS_ACCESS_KEY_ID=test`, `AWS_SECRET_ACCESS_KEY=test`, `AWS_DEFAULT_REGION=us-east-1`).
All existing Phase 1–8C tests MUST continue to pass without regression.

---

## 1. Goal & Expansion Path

Phase 9A adds the two minimal runtime primitives required for real cloud/AWS REST clients on `cz-js` (`process` + `ring`-backed `node:crypto`), and builds a clean, reusable **Edge-to-Cloud AWS integration suite** (`examples/aws-edge/`) verified live against `aws-emulator` (S3, DynamoDB, SQS) alongside `cz-zpm`, `node:https`, `cardputer:kv`, and `cz-json`.

This lays a modular foundation for future Phase 9B expansion (Lambda RIE, Step Functions ASL, DLQ/retry pipelines).

---

## 2. Runtime Additions

### 2.1 `globalThis.process` + `"node:process"` / `"process"` (`crates/cz-modules/src/process_module.rs` & `cz-runner`)
- Install `globalThis.process` from `cz-runner/src/main.rs` via `cz_modules::install_process(&ctx, &script_args)` (right alongside `install_json` and `install_fetch`), AND register `ProcessModule` for `"node:process"` / `"process"` (`ModuleKind::Process`, re-exported as `pub use process_module::ProcessModule;`).
- Surface on `process`:
  - `process.env`: plain JS object populated from `std::env::vars()` (string keys and values).
  - `process.argv`: `string[]` (`[exe, scriptPath, ...scriptArgs]`).
  - `process.cwd()`: returns `std::env::current_dir()` as a UTF-8 string.
  - `process.exit(code?)`: flushes stdout/stderr and exits with integer `code` (default `0`).
  - `process.platform`: `"linux"` (or `std::env::consts::OS`).
  - `process.arch`: `"arm64"` on `aarch64`, `"x64"` on `x86_64`, else `std::env::consts::ARCH`.

### 2.2 `node:crypto` / `crypto` (`crates/cz-modules/src/crypto_module.rs`)
- Backed by **`ring 0.17`** (already in workspace `Cargo.lock` — **zero new crates, zero OpenSSL/C TLS libraries!**).
- Keep `crates/cz-hal/Cargo.toml` strictly `libc`-only; add `ring = "0.17"` directly to `crates/cz-modules/Cargo.toml` (or expose tiny pure helpers from `cz-net` if preferred — either way, zero new crates in `Cargo.lock`!).
- Register `CryptoModule` in `crates/cz-modules/src/lib.rs` for `"node:crypto"` and `"crypto"` (`ModuleKind::Crypto`, re-exported as `pub use crypto_module::CryptoModule;`).
- Export named symbols AND `default` object:
  - `createHash(algorithm: "sha256" | "sha512")` (case-insensitive, also accept `"sha-256"` / `"sha-512"`):
    - `.update(data: string | Uint8Array | ArrayBuffer)` → returns `this` (chainable)
    - `.digest(encoding?: "hex" | "base64")` → `string` when encoding given, `Uint8Array` when omitted
  - `createHmac(algorithm: "sha256" | "sha512", key: string | Uint8Array | ArrayBuffer)`:
    - `.update(data: string | Uint8Array | ArrayBuffer)` → returns `this` (chainable)
    - `.digest(encoding?: "hex" | "base64")` → `string` when encoding given, `Uint8Array` when omitted
  - `randomBytes(size: number): Uint8Array` (uses `ring::rand::SystemRandom`, rejects negative / `> 65536` sizes with `RangeError`).
  - `randomUUID(): string` (RFC 4122 v4 UUID lowercase string).
  - `timingSafeEqual(a: Uint8Array, b: Uint8Array): boolean` (constant-time comparison via `ring::constant_time::verify_slices_are_equal`; throws `RangeError` if `a.byteLength !== b.byteLength`).
- Add `>= 8` Rust unit tests covering SHA-256/SHA-512 NIST vectors, HMAC-SHA256 RFC 4231 vector, `randomUUID` format, `timingSafeEqual`, and `process` globals.

---

## 3. Reusable AWS Edge-to-Cloud Modules (`examples/aws-edge/`)

Create two clean, zero-external-SDK ESM files in `examples/aws-edge/` (structured for future Phase 9B expansion):

### 3.1 `examples/aws-edge/aws_client.mjs`
- Reads `AWS_ENDPOINT_URL`, `AWS_ACCESS_KEY_ID` (default `"test"`), `AWS_SECRET_ACCESS_KEY` (default `"test"`), and `AWS_DEFAULT_REGION` (default `"us-east-1"`) from `process.env` (or explicit options).
- Implements standard **AWS Signature Version 4 (SigV4)** signing using `node:crypto` (`createHash("sha256")`, `createHmac("sha256", ...)`), including `x-amz-date`, `x-amz-content-sha256`, and `Authorization: AWS4-HMAC-SHA256 ...`.
- Exports clean async helpers using `cz-js`'s native `fetch()`:
  - `healthCheck(endpoint?)` → `GET /__admin/health`
  - **S3 (path-style)**: `s3CreateBucket(bucket)`, `s3PutObject(bucket, key, body, contentType?)`, `s3GetObject(bucket, key)` (returns `{ status, bytes, text }`), `s3DeleteObject(bucket, key)`, `s3DeleteBucket(bucket)`
  - **DynamoDB (JSON 1.0 target headers `X-Amz-Target: DynamoDB_20120810.*`)**:
    - `ddbCreateTable(tableName, pkName)` (uses `BillingMode: "PAY_PER_REQUEST"`)
    - `ddbWaitTableActive(tableName, timeoutMs?)` (polls `DescribeTable` every 300ms until `TableStatus === "ACTIVE"`)
    - `ddbPutItem(tableName, item, conditionExpression?)` (returns `{ ok: true }` or `{ ok: false, conditionalFailed: true }` on `ConditionalCheckFailedException`)
    - `ddbGetItem(tableName, key)`
    - `ddbScan(tableName)`
    - `ddbDeleteTable(tableName)`
  - **SQS (Query / JSON protocol supported by `aws-emulator`)**:
    - `sqsCreateQueue(queueName)` → returns `queueUrl`
    - `sqsSendMessage(queueUrl, body)` → returns `messageId`
    - `sqsReceiveMessages(queueUrl, maxMessages?)` → returns array of `{ messageId, receiptHandle, body }`
    - `sqsDeleteMessage(queueUrl, receiptHandle)`
    - `sqsDeleteQueue(queueUrl)`

### 3.2 `examples/aws-edge/edge_gateway.mjs`
- Standalone runnable `cz-js` HTTPS edge gateway accepting config via env vars (`GATEWAY_CERT`, `GATEWAY_KEY`, `GATEWAY_KV_PATH`, `AWS_BUCKET`, `AWS_TABLE`, `AWS_QUEUE_URL`):
  - Starts `node:https` server on `127.0.0.1:0` and prints `READY:<port>` to stdout.
  - `POST /telemetry` with JSON `{ id, device, payload }`:
    1. Enforces idempotency via DynamoDB `ddbPutItem(table, { pk: { S: id }, device: { S: device }, s3Key: { S: ... } }, "attribute_not_exists(pk)")`.
       - If `conditionalFailed`: responds `409 Conflict` with `{ error: "duplicate", id }` without overwriting S3/KV.
    2. Writes payload JSON to S3 (`s3PutObject(bucket, \`telemetry/${id}.json\`, ...)`).
    3. Saves summary locally in `cardputer:kv` (with `persistPath: GATEWAY_KV_PATH` + `store.flush()`).
    4. Sends notification JSON `{ id, device, s3Key }` to SQS (`sqsSendMessage`).
    5. Responds `201 Created` with `{ ok: true, id, s3Key, messageId }`.
  - `GET /local/:id` (or `/local?id=...`): reads back the persisted entry from `cardputer:kv` and returns `200` JSON.
  - `POST /shutdown`: responds `200` and calls `server.close()` so `cz-js` exits cleanly with status `0`.

---

## 4. `aws-emulator` Known Constraints (from Playbook §7)
- Always pass `BillingMode: "PAY_PER_REQUEST"` to DynamoDB `CreateTable`, and poll `DescribeTable` until `TableStatus === "ACTIVE"` (takes ~1–7s).
- `ConditionalCheckFailedException` is a real semantic rejection — NEVER retry it.
- S3 MUST use path-style URLs (`${AWS_ENDPOINT_URL}/${bucket}/${key}`).
- Always suffix resource names with a timestamp (`cz-p9a-<ts>`) and clean up in `finally`.

---

## 5. TypeScript Declarations (`types/cardputer.d.ts`)
- Add global `process` declaration (`env`, `argv`, `cwd()`, `exit()`, `platform`, `arch`) and `declare module "node:process"` / `declare module "process"`.
- Add `CzHash`, `CzHmac`, `declare module "node:crypto"`, and `declare module "crypto"`.

---

## 6. Verification Requirements
1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase9a/verify_phase9a_aws.mjs` passes all 7 checks against live `aws-emulator`.
3. `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` passes with zero warnings.
4. Phase 8C (`node:tls`), Phase 8B (`node:https`), and Phase 8A (`fetch`+TLS) regressions all pass.
