# Phase 7B Specification: Native Rust `cz-zpm` (Multicall `zpm`/`npm` for `cz-llrt`)

## Goal
Replace the JS `zpm.mjs` + shell `npm` shim with a **native Rust crate (`crates/cz-zpm`)** designed to be linked directly into `cz-llrt` as a **multicall binary** (`ln -s cz-llrt zpm`, `ln -s cz-llrt npm`, `ln -s cz-llrt node`, and `cz-llrt zpm ...`), while also providing a standalone `cz-zpm` binary for fast isolated testing.

## Architecture & Dependencies
Add `"crates/cz-zpm"` to the workspace `Cargo.toml`.

Use pure-Rust crates that align with `awslabs/llrt` v0.10.0-beta so `cz-llrt` shares TLS/HTTP/compression/JSON stacks with minimal binary size overhead (~300 KB):
- `hyper = { version = "1", features = ["client", "http1", "http2"] }`
- `hyper-util = { version = "0.1", features = ["client", "client-legacy", "http1", "http2", "tokio"] }`
- `hyper-rustls = { version = "0.27", default-features = false, features = ["http1", "http2", "ring", "tls12", "webpki-roots"] }`
- `http = "1"`, `http-body-util = "0.1"`
- `rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }`
- `tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "time"] }`
- `flate2 = { version = "1.1", default-features = false, features = ["rust_backend"] }`
- `tar = "0.4"`
- `sha2 = "0.10"`, `base64 = "0.22"`
- `serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`

Do NOT add C-library dependencies (no OpenSSL, no libcurl, no system zlib) so aarch64 musl cross-compilation works out of the box.

## Public Multicall API (`crates/cz-zpm/src/lib.rs`)
```rust
/// Inspect `std::env::args()` for multicall invocation:
/// - `argv[0]` basename == `"zpm"` or `"npm"` -> runs CLI with `&args[1..]`, returns `Some(exit_code)`.
/// - `argv.get(1)` == Some(`"zpm"`) -> runs CLI with `&args[2..]`, returns `Some(exit_code)`.
/// - Otherwise returns `None` (so `cz-llrt` continues normal JS runtime startup).
pub async fn maybe_run_multicall_async() -> Option<u8>

/// Run the zpm CLI directly with the given arguments (excluding program name).
pub async fn run_cli_async(args: &[String]) -> u8

/// Pure helper for unit testing multicall dispatch logic.
pub fn parse_multicall_args(args: &[String]) -> Option<Vec<String>>
```

## Standalone Binary (`crates/cz-zpm/src/main.rs`)
- `#[tokio::main] async fn main() -> std::process::ExitCode`
- Calls `maybe_run_multicall_async().await`; if `None`, calls `run_cli_async(&args[1..]).await`.
- Thus `cz-zpm install hono`, `./zpm install hono` (symlink), `./npm --version` (symlink), and `cz-zpm zpm install hono` ALL work.

## Core Features & Security Guards
Environment variables:
- `ZPM_REGISTRY` (default `https://registry.npmjs.org`)
- `ZPM_ESBUILD` (default `esbuild`)
- `ZPM_RUNTIME` (default `cz-llrt`)
- `ZPM_TMPDIR` (default `/tmp` — NEVER default to `TMPDIR` like `/data/local/tmp`!)

### 1. `--version` / `-v`
- Exit `0`. Prints a clear identity line containing `zpm`, `native`, `shim`, `esbuild`, and `cardputer:*`.

### 2. `install` / `i` / `add` `[pkg[@range] ...] [--no-minify]`
- If no package args are given, installs `dependencies` from `./package.json`.
- Fetches `<ZPM_REGISTRY>/<encodedPkg>` with header `Accept: application/vnd.npm.install-v1+json` (supports HTTP and HTTPS, follows up to 5 redirects 301/302/307/308).
- Resolves version (`latest`, exact, `^`, `~` ranges).
- **In-memory streaming tarball pipeline** (NEVER writes `.tgz` to disk; NEVER spawns external `tar`):
  - Streams response body with a strict `5 * 1024 * 1024` byte (5 MiB) cap.
  - Verifies `dist.integrity` (`sha512-<base64>`) in memory via `sha2::Sha512` + `base64` BEFORE extracting.
  - Streams `flate2::read::GzDecoder` -> `tar::Archive`:
    - **Zip-slip guard**: rejects any entry whose path contains `..` (`Component::ParentDir`), starts with `/`, or is a symlink/hardlink (`EntryType::Symlink | EntryType::Link`).
    - **Native addon guard**: rejects `gypfile: true`, `scripts.install`/`scripts.preinstall` containing `node-gyp`/`prebuild`, OR any tar entry named `binding.gyp` or ending in `.node`.
  - Rejects dependency graphs with `> 20` total packages.
  - Extracts safe regular files into `<ZPM_TMPDIR>/zpm-<pid>-<rand>/` using an **RAII `Drop` guard** so temp staging is ALWAYS cleaned up even on error.
  - Recursively fetches & extracts pure-JS transitive `dependencies` into `<stage>/node_modules/<dep>/`.
- Resolves ESM entry point (`exports["."]` -> `import`/`default`/`browser`/string, then `module`, then `main`, then `index.js`).
- Invokes `<ZPM_ESBUILD>`:
  ```
  <ZPM_ESBUILD> <entry> --bundle --format=esm --platform=neutral \
    --main-fields=module,main --conditions=import,module,default \
    --external:node:* --external:cardputer:* [--minify] \
    --outfile=node_modules/<pkg>/index.mjs
  ```
  (`--minify` enabled by default; omitted when `--no-minify` is passed.)
- Writes `node_modules/<pkg>/package.json` (`name`, `version`, `"type": "module"`, `"main": "./index.mjs"`, `"exports": { ".": "./index.mjs" }`).
- Updates `./package.json` (`dependencies[pkg] = "^<version>"`) and `./zpm-lock.json` (`packages[pkg] = { version, integrity, bundledBytes }`).

### 3. `run <script>`
- Looks up `scripts[script]` in `./package.json` (non-zero exit if missing).
- Replaces leading `node ` with `<ZPM_RUNTIME> ` and executes via `sh -c` inheriting stdio, returning the child exit code.

### 4. `bundle <entry> -o <out.mjs> [--no-minify]`
- Runs `<ZPM_ESBUILD>` to bundle `<entry>` into `<out.mjs>` with `--bundle --format=esm --platform=neutral --external:node:* --external:cardputer:*` (and `--minify` unless `--no-minify`).

## Verification
1. `cargo test --workspace -- --test-threads=1` (all existing 228 tests + new `cz-zpm` unit tests pass, zero warnings).
2. `node /refs/phase7b/verify_phase7b.mjs` (10 end-to-end checks via real `zpm`/`npm` symlinks, zip-slip tarball rejection, real Hono 4.13.19 + `esbuild` + `cz-js` E2E).
