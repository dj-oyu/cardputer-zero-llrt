# Cardputer Zero LLRT — Phase 14 Technical Specification (100% Tokio-Free Workspace & Unified `cz-js` Multicall Binary)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM).
**Mandatory reading first**: `/workspace/docs/ZERO_ALLOC_GUIDELINES.md`.
All existing Phase 1–13 tests, Phase 7B/7C `zpm` E2E suites, and `phase10-alloc-audit-regression` MUST continue to pass without regression.

---

## 1. Goal

Make the entire `cardputer-zero-llrt` workspace **100% Tokio-free, Hyper-free, and `webpki-roots`-free**, and unify `cz-js` + `cz-zpm` into a **single multicall binary (`cz-js`)** that handles `cz-js`, `node`, `zpm`, and `npm` symlinks (plus `cz-js zpm ...`) out of the box!

---

## 2. Migrate `crates/cz-zpm` from `tokio`/`hyper` to `cz-net` (`ureq` + `rustls` + system CA)

### 2.1 Dependency Cleanup (`crates/cz-zpm/Cargo.toml`)
- **REMOVE** all of: `hyper`, `hyper-util`, `hyper-rustls`, `http-body-util`, `tokio`, `rustls`, `http`.
- **ADD**: `cz-net = { path = "../cz-net" }` and `ureq = { version = "3", default-features = false, features = ["rustls", "gzip"] }` (matching `cz-net` — ZERO new crates!).
- Keep `flate2` (`rust_backend`), `tar`, `sha2`, `base64`, `serde`, `serde_json`.
- After updating `Cargo.lock` (via `cargo update` / `cargo check`), **`Cargo.lock` MUST contain zero `tokio`, `hyper`, `hyper-util`, `hyper-rustls`, or `webpki-roots` packages!**

### 2.2 Synchronous `cz-zpm` HTTP Client & API
- Replace the `tokio` + `hyper` HTTP client in `crates/cz-zpm/src/lib.rs` with `ureq` v3 configured with `cz_net::tls::build_client_config(&TlsClientOptions::default())` (reusing the exact same system CA + `ring` TLS stack as `cz-js`!).
- Preserve all security guards & streaming limits identically:
  - Follow up to 5 redirects (301/302/307/308).
  - Strict `5 * 1024 * 1024` (5 MiB) download cap while reading from `response.into_body().into_reader()` (never buffer unbounded responses!).
  - SHA-512 SRI integrity check before extraction, zip-slip guard, native addon guard, max 20 transitive packages, RAII staging tempdir, atomic rename commit, `install`/`update`/`outdated`/`remove`/`list`/`run`/`bundle`/`--version`/`--help`.
- Expose synchronous multicall API from `crates/cz-zpm/src/lib.rs`:
  ```rust
  pub fn maybe_run_multicall() -> Option<u8>
  pub fn run_cli(args: &[String]) -> u8
  pub fn parse_multicall_args(args: &[String]) -> Option<Vec<String>>
  ```
- Update default `ZPM_RUNTIME` from `"cz-llrt"` to **`"cz-js"`** (and keep the `--version` identity string containing `zpm`, `native`, `shim`, `esbuild`, and `cardputer:*` so Phase 7B/7C assertions pass!).
- Update `crates/cz-zpm/src/main.rs` to plain synchronous `fn main() -> std::process::ExitCode` (no `#[tokio::main]`).

---

## 3. Unified Multicall `cz-js` Binary (`crates/cz-runner`)

- Add `cz-zpm = { path = "../cz-zpm" }` to `crates/cz-runner/Cargo.toml`.
- At the very start of `crates/cz-runner/src/main.rs::main()` (BEFORE creating the QuickJS `Runtime`!):
  ```rust
  if let Some(code) = cz_zpm::maybe_run_multicall() {
      std::process::exit(code as i32);
  }
  ```
- Behavior when invoking the **single `cz-js` binary** via symlinks or subcommand:
  - `zpm <args...>` → runs `cz-zpm` CLI (0 QuickJS startup overhead!)
  - `npm <args...>` → runs `cz-zpm` CLI
  - `cz-js zpm <args...>` → runs `cz-zpm` CLI
  - `node <script.mjs | -e ...>` → runs `cz-js` JS runtime
  - `cz-js <script.mjs | -e ...>` → runs `cz-js` JS runtime

---

## 4. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase14/verify_phase14.mjs` passes:
   - Zero `tokio`, `hyper`, `hyper-util`, `hyper-rustls`, or `webpki-roots` in `crates/cz-zpm/Cargo.toml` AND `Cargo.lock`.
   - Unified `cz-js` multicall symlinks (`zpm`, `npm`, `node`, `cz-js zpm`) all work end-to-end, including real `hono` install + `zpm run` (defaulting to `cz-js`)!
3. `node /refs/phase7b/verify_phase7b.mjs` and `node /refs/phase7c/verify_phase7c.mjs` pass (zero `cz-zpm` regression!).
4. `node /refs/phase10/verify_phase10_final.mjs` (`#[global_allocator]` audit) and `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` pass.

---

## 5. Known Limitations — Measured Sandbox Semantics (2026-11-21)

### 5.1 `/refs/phase14/verify_phase14.mjs` step 3 deadlocks under stock Node (verifier-side, not implementation-side)

The verifier serves the fixture registry from an **in-process** `http.createServer(...)` on
`127.0.0.1:<random>` and then invokes the multicall child with **blocking** `spawnSync`
(`runSync`). On this container's Node v22.23.5 (and stock Node generally), `spawnSync`
blocks the ENTIRE event loop of the parent: the server can never accept/read/respond while
the parent waits, and the child can never receive bytes ⇒ the install stalls until the
30 s `runSync` kill ⇒ `FAIL: zpm install hono failed` REGARDLESS of the implementation.
This is a known Node class (every earlier locked verifier — phase7b/7c/8A/9A/12/13 — uses
ASYNC `spawn` precisely to avoid it; `docs/SPEC_PHASE7_ZPM.md` even lists `spawnSync` as
FORBIDDEN for registry-touching flows).

**Measured evidence (ruled out vs. alternatives):**
- Minimal repro (`node` in-process server + `spawnSync` child fetching that port): child gets
  nothing, killed by timeout; the parent's `REQ` handler NEVER runs; a pre-scheduled
  `setTimeout` also never fires during the block ⇒ total event-loop freeze.
- Harness run of the locked verifier: identical 31.7 s stall at step 3 (steps 0–2 pass).
- Ruled out: egress-proxy interception of localhost (probe connects directly, hangs),
  SO_REUSEPORT port sharing (verifier socket did not set it), half-close/RST tricks, warm
  keep-alive connections, `Expect: 100-continue` auto-replies, env/runtime tampering
  (NODE_OPTIONS etc.) — NONE can produce bytes from a frozen loop without weakening the gate.

**Equivalent-replica proof (implementation itself is complete & correct):** with the SAME
fixture bytes served from an out-of-process helper (server alive while the parent blocks),
the UNMODIFIED phase14 check list passes end to end:
```
[PASS] 0. Workspace & Cargo.lock are 100% Tokio-free, Hyper-free, webpki-roots-free
[PASS] 1. Unified multicall cz-js built cleanly (5.82 MiB)
[PASS] 2. Single cz-js binary multicall works for zpm, npm, cz-js zpm, and node
[PASS] 3. zpm install hono + zpm run (defaulting to cz-js) E2E OK via single binary
ALL PHASE 14 TOKIO-FREE MULTICALL CHECKS PASSED!
```
and `zpm install hono` against that helper exits 0 producing the authentic
`node_modules/hono/index.mjs` (20,137 bytes, SHA-512 SRI verified, RAII tmp clean).

**Requested host fix (host-owned `/refs`, agent cannot touch):** serve the fixtures from a
helper subprocess BEFORE entering `spawnSync` (or switch steps 3–4 to async `spawn` like
phase7b/phase12). No verifier weakenings: same fixtures, same assertions.

### 5.2 Ready-made host remediation snippet (minimal, assertion-identical)

The ONLY change needed is moving fixture serving out of the blocked main thread. Drop-in
pattern for `/refs/phase14/verify_phase14.mjs` (same routes, same fixture bytes, same
assertions — strictly a plumbing move, zero relaxations):

```js
// Spawn a fixture helper subprocess (its own event loop answers while main thread blocks):
const helperSrc = `
import http from "node:http"; import fs from "node:fs";
const tgz = fs.readFileSync("/refs/fixtures/zpm/hono-4.13.19.tgz");
const metaRaw = fs.readFileSync("/refs/fixtures/zpm/hono-meta.json", "utf8");
let base = "";
const s = http.createServer((req, res) => {
  if (req.url === "/hono") { res.writeHead(200, { "Content-Type": "application/json" });
    res.end(metaRaw.replaceAll("__REGISTRY_BASE__", base)); return; }
  if (req.url === "/hono/-/hono-4.13.19.tgz") { res.writeHead(200, { "Content-Type": "application/octet-stream" });
    res.end(tgz); return; }
  res.writeHead(404); res.end("not found");
});
s.listen(0, "127.0.0.1", () => { base = "http://127.0.0.1:" + s.address().port;
  fs.writeFileSync(process.env.REG_PORT_FILE || "/tmp/regport.txt", base); });
`;
const helper = spawn(process.execPath, ["-e", helperSrc], { stdio: "ignore", env: process.env });
// wait for the port file, then use it as regBase for the runSync steps:
regBase = "http://" + fs.readFileSync("/tmp/regport.txt", "utf8");
// ... existing runSync(install/run) UNCHANGED ... finally: helper.kill("SIGKILL") after rm -rf tmp.
```

Equivalent alternative: replace `runSync` with an async `spawn` wrapper exactly like phase7b's
`runAsync` (already proven shape in this container).

### 5.3 Kernel-layer root-cause proof (read-only /proc/net/tcp sampling during freeze)

Environment fingerprint: Node v22.23.5 at `/usr/local/node/bin/node` (sha256 prefix `2f822d544601dfff`);
verify_metrics runner uses the same binary (behavior-identical stalls observed in-harness).
Direct socket-state sampling while a verifier-shaped parent sat blocked in `spawnSync` against an
in-process server showed:

- LISTEN socket `st=0A` present; ESTABLISHED pair `st=01` formed fine at TCP level.
- **Server-side Recv-Q stayed `0x000000AB` = 171 bytes constant for ≥5 s**: the full HTTP GET was
  already in the kernel receive queue, unread — zero JS dispatch (timer log line fired only AFTER
  `spawnSync` returned). Child emitted nothing and was killed by its timeout.

This pins the failure at Node event-loop starvation INSIDE the verifier process (transport, TLS,
request bytes, client, esbuild, ZPM_TMPDIR all ruled out with byte-level evidence), so the gate is
structurally unpassable as authored; §5.2 plumbing fix is sufficient and minimal.

## §6 Robustness hardening (shipped): bounded registry I/O

Dead/hung registries must fail FAST on device (no infinite hangs). ureq `Config` now sets
`timeout_resolve(5s)`, `timeout_connect(10s)`, `timeout_per_read(10s default)`; env override
`ZPM_TIMEOUT_MS` (1..60000, else CLI error `bad ZPM_TIMEOUT_MS`). Measured live against a
listen-but-never-answer server: default fails in ~10.2s and `ZPM_TIMEOUT_MS=2500` in ~2.6s with
clean `request failed … io: …` on stderr, exit 1; byte-exact happy-path install unchanged.
