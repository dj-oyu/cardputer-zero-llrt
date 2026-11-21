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
