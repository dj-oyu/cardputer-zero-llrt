# Phase 7 Specification: `zpm` (Zero Package Manager) + Hono E2E

## Goal
Provide a lightweight, SD-card-friendly npm shim (`zpm`) designed for M5Stack CardputerZero (`cz-llrt`, Cortex-A53, 512 MB RAM, ext4 SD card + `/tmp` tmpfs) that:
1. Downloads & verifies npm packages in `/tmp` tmpfs (never unpacking hundreds of files onto SD).
2. Bundles each package into a **single ESM file** (`node_modules/<pkg>/index.mjs`) using `esbuild` (`--minify` by default, `--no-minify` optional).
3. Rejects native C/C++ addons (`binding.gyp`, `*.node`, `gypfile: true`) and bloated dependency graphs (`> 20` transitive packages or `> 5 MiB` tarball).
4. Runs transparently under **both** `cz-llrt` (LLRT v0.10.0-beta on CardputerZero) and Node.js v22 (host/sandbox).
5. Proves real-world framework compatibility with **Hono** (`zpm install hono`) serving `cardputer:kv` rows (`c.json(sessions.row(id))` and `c.json({ ok: true, session: sessions.row(id) })`) and transparent Lazy JSON (`await c.req.json()` on `>= 4 KiB` body yields `czJson.isLazy(body) === true`).

## Critical LLRT v0.10.0-beta Compatibility Rules (MANDATORY)
`tools/zpm/zpm.mjs` MUST NOT use APIs missing from LLRT v0.10.0-beta:
- **FORBIDDEN**: `fs.existsSync`, `child_process.spawnSync`, `child_process.execSync`.
- **USE INSTEAD**:
  - `fs.promises` (`readFile`, `writeFile`, `mkdir`, `mkdtemp`, `rm`, `readdir`, `stat`, `access`, `rename`) and `accessSync` wrapped in `try/catch` for existence checks.
  - `child_process.spawn` wrapped in an async `Promise` helper (`runCmd(cmd, args, opts)`).
  - `node:crypto` `createHash("sha512").update(buf).digest("base64")` for SRI integrity verification.
  - `node:path`, `node:os`, `globalThis.fetch`, `process`.

## Deliverables

### 1. `tools/zpm/zpm.mjs`
Executable ESM CLI (`#!/usr/bin/env cz-llrt` or invoked via runtime).

Environment variables:
- `ZPM_REGISTRY`: registry base URL (default: `https://registry.npmjs.org`).
- `ZPM_ESBUILD`: path to `esbuild` binary (default: `esbuild`).
- `ZPM_RUNTIME`: runtime used by `zpm run` when substituting leading `node ` (default: `cz-llrt`).
- `ZPM_TMPDIR`: staging tmpfs root (default: `/tmp`).

Commands:
- `zpm --version` / `zpm -v` (and via `npm` shim):
  - Exit `0`. Prints a clear identity line containing `zpm`, `shim`, `esbuild`, and `cardputer:*` so automated agents (e.g. ZeroClaw) immediately recognize the CardputerZero environment.
- `zpm install [pkg[@range] ...] [--no-minify]` (aliases: `i`, `add`):
  - If no package args are given, reads `dependencies` from `./package.json`.
  - Fetches `<ZPM_REGISTRY>/<encodedPkg>` with header `Accept: application/vnd.npm.install-v1+json`.
  - Resolves version (`latest`, exact version, or `^`/`~` range against `Object.keys(meta.versions)`).
  - **Guards** (fail with non-zero exit & clear stderr message, leaving `node_modules/<pkg>` untouched and cleaning up temp dir):
    - Rejects native addons: `versionMeta.gypfile === true`, or `scripts.install`/`scripts.preinstall` referencing `node-gyp`/`prebuild`, or `binding.gyp` / `*.node` found in extracted files.
    - Rejects tarballs `> 5 * 1024 * 1024` bytes (5 MiB).
    - Rejects dependency graphs with `> 20` total packages.
    - Verifies `dist.integrity` (`sha512-<base64>`) against downloaded tarball bytes; rejects on mismatch.
  - Extracts tarball(s) in `<ZPM_TMPDIR>/zpm-<rand>/` via `tar -xzf` (recursively staging pure-JS transitive `dependencies` under `<stage>/node_modules/<dep>`).
  - Resolves ESM entry point from `package.json`: `exports["."]` (handling string or `{ import, default, browser }` nested conditions), then `module`, then `main`, then `index.js`.
  - Runs `esbuild`:
    ```
    <ZPM_ESBUILD> <entry> --bundle --format=esm --platform=neutral \
      --main-fields=module,main --conditions=import,module,default \
      --external:node:* --external:cardputer:* [--minify] \
      --outfile=node_modules/<pkg>/index.mjs
    ```
    (`--minify` enabled by default; omitted when `--no-minify` is passed.)
  - Writes `node_modules/<pkg>/package.json`:
    ```json
    {
      "name": "<pkg>",
      "version": "<resolvedVersion>",
      "type": "module",
      "main": "./index.mjs",
      "exports": { ".": "./index.mjs" }
    }
    ```
  - Updates `./package.json` (`dependencies[pkg] = "^<version>"`) and `./zpm-lock.json` (`packages[pkg] = { version, integrity, bundledBytes }`).
  - Always removes `<ZPM_TMPDIR>/zpm-<rand>/` in `finally`.
- `zpm run <script>`:
  - Looks up `scripts[script]` in `./package.json` (non-zero exit if missing).
  - Replaces leading `node ` with `<ZPM_RUNTIME> ` and runs via `sh -c` inheriting stdio, exiting with the child's exit code.
- `zpm bundle <entry> -o <out.mjs> [--no-minify]`:
  - Bundles `<entry>` via `esbuild` into `<out.mjs>` with `--bundle --format=esm --platform=neutral --external:node:* --external:cardputer:*` (and `--minify` unless `--no-minify`).

### 2. `tools/zpm/npm`
Executable POSIX shell script (`chmod +x`, LF line endings):
- Invokes `zpm.mjs` next to itself using `${ZPM_RUNTIME:-cz-llrt}` if available in `PATH`, falling back to `node`.

### 3. `examples/hono-adapter.mjs`
Thin `node:http` ↔ Web Standard `Request`/`Response` adapter exporting `serve(app, { port, hostname, onListen })`:
- Works on `cz-llrt` (native Web `Request`/`Response`/`Headers`) AND on standalone `cz-js` (provides a minimal fallback `Headers`/`Request`/`Response` polyfill when `typeof Request === "undefined"` so `hono` runs in both environments; `req.json()` uses `JSON.parse(bodyText)` so `cz-json` transparent Lazy Proxy activates automatically on `>= 4 KiB` bodies!).
- Returns the `node:http` `Server` instance from `http.createServer(...)`.

### 4. `examples/hono-kv-demo.mjs`
Runnable demo server (imports `Hono` from `./node_modules/hono/index.mjs` or `HONO_MODULE` env/default `./node_modules/hono/index.mjs`, `serve` from `./hono-adapter.mjs`, `cardputer:kv`, `cardputer:json`):
- Listens on `Number(process.env?.PORT || globalThis.__PORT || 0)` (or CLI/env `CZ_DEMO_PORT`), prints `HONO_READY <port>` on listen.
- Routes:
  - `GET /health` -> `c.json({ ok: true, runtime: "cardputer-zero-llrt" })`
  - `GET /session/:id` -> returns `c.json(sessions.row(id))` (pre-populated `s1` with `{ user: "jimmy", role: "admin", count: 7 }`)
  - `GET /session-wrap/:id` -> returns `c.json({ ok: true, session: sessions.row(id) })`
  - `POST /big-json` -> parses `const body = await c.req.json()`, returns `c.json({ lazy: czJson.isLazy(body), item42: body.items[42].title })`
  - `POST /shutdown` -> responds `200 { bye: true }` and closes the server cleanly.

## Verification
Run `node /refs/phase7/verify_phase7.mjs` (plus `cargo test --workspace -- --test-threads=1`).
All checks must pass with zero regressions.
