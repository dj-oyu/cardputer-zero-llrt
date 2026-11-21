# Phase 7C Specification: `zpm update`, `outdated`, `remove`, `list`

## Goal
Extend `crates/cz-zpm` (native Rust multicall `zpm`/`npm`) with safe, SD-card-friendly package lifecycle commands:
- `zpm update [pkg ...] [--latest] [--dry-run] [--no-minify]` (aliases: `up`, `upgrade`)
- `zpm outdated [--json]`
- `zpm remove <pkg ...>` (aliases: `rm`, `uninstall`, `un`)
- `zpm list` (alias: `ls`)

## Requirements

### 1. `zpm update [pkg ...] [--latest] [--dry-run] [--no-minify]`
- Reads `./package.json` (`dependencies`) and `./zpm-lock.json` (`packages`).
- If package names are provided, targets only those packages (exit non-zero if a requested package is not listed in `dependencies` / `zpm-lock.json`).
- If no package names are given, targets all keys in `./package.json` `dependencies`.
- For each target package:
  - Fetches `<ZPM_REGISTRY>/<encodedPkg>` with `Accept: application/vnd.npm.install-v1+json`.
  - Resolves target version:
    - Default: highest version satisfying the semver range in `package.json` (`^`, `~`, exact, `*`/`latest`).
    - With `--latest`: resolves `dist-tags.latest` (and updates `package.json` `dependencies[pkg]` to `"^<newVersion>"`).
  - Compares against currently installed version (from `zpm-lock.json` or `node_modules/<pkg>/package.json`):
    - If already equal: skip download/bundle (zero SD writes), print `zpm: <pkg>@<ver> is already up to date`.
    - If `--dry-run`: print `zpm: would update <pkg> <old> -> <new>` and make ZERO filesystem writes.
    - Otherwise: run the full safe in-memory pipeline (5 MiB cap, SHA-512 SRI verification, zip-slip guard, native addon guard, >20 deps guard, RAII `/tmp` staging, `esbuild` bundle) and **atomically replace** `node_modules/<pkg>/index.mjs` (write temp file -> rename) and update `node_modules/<pkg>/package.json` + `zpm-lock.json`.
  - **Failure / Rollback safety**: If an update fails (e.g., bad SHA-512 integrity, native addon in new version, network error), the previously installed `node_modules/<pkg>/index.mjs`, `node_modules/<pkg>/package.json`, and `zpm-lock.json` MUST remain intact and untouched, and `ZPM_TMPDIR` MUST remain clean!

### 2. `zpm outdated [--json]`
- Inspects all packages in `./package.json` `dependencies` against `<ZPM_REGISTRY>`.
- Computes `{ name, current, wanted, latest }` for each package (where `wanted` is max matching `package.json` range and `latest` is `dist-tags.latest`).
- With `--json`: prints a valid JSON array of `{ name, current, wanted, latest }` (including packages where `current !== wanted || current !== latest`, or all packages with `outdated: boolean`).
- Without `--json`: prints a clean human-readable table. Exit `0`.

### 3. `zpm remove <pkg ...>` (aliases: `rm`, `uninstall`, `un`)
- Removes `node_modules/<pkg>` recursively, deletes `<pkg>` from `./package.json` `dependencies` and `./zpm-lock.json` `packages`.
- Exits non-zero if no package args are given or if a named package is not installed/listed.

### 4. `zpm list` (alias: `ls`)
- Lists installed packages from `zpm-lock.json` (falling back to `node_modules/*/package.json`) with `name@version` and bundled size. Exit `0`.

## Verification
1. `node /refs/phase7c/verify_phase7c.mjs` (tests `update`, `--dry-run`, `--latest`, rollback safety on corrupt update, no-op up-to-date, `outdated --json`, `list`, `remove`).
2. `node /refs/phase7b/verify_phase7b.mjs` (zero regression on install/multicall/zip-slip/Hono E2E).
3. `cargo test --workspace -- --test-threads=1` (zero warnings, all unit tests pass).
