#!/usr/bin/env cz-llrt
// `zpm` - Zero Package Manager: an SD-card-friendly npm shim for CardputerZero (`cz-llrt`).
//
// Design rules (LLRT v0.10.0-beta compatible AND Node.js v22 compatible):
//  * `fs.promises` for ALL filesystem I/O; existence checks use `accessSync` in `try/catch`.
//  * External binaries run through `child_process.spawn` wrapped in a Promise (`runCmd`);
//    stdio-inheriting runs use `runInherit`. Nothing synchronous-process APIs are used.
//  * Tarballs stage in `<ZPM_TMPDIR>/zpm-*` (tmpfs on CardputerZero) and ALWAYS get removed in
//    a `finally` block - hundreds of files never land on the ext4 SD card, only the single
//    bundled `node_modules/<pkg>/index.mjs` does.
//  * Integrity: `crypto.createHash("sha512")` over the exact tarball bytes vs `sha512-<b64>` SRI.
//  * Safety guards reject native C/C++ addons (`gypfile`, node-gyp/prebuild install scripts,
//    `binding.gyp` / `*.node` files), > 5 MiB tarballs and dependency graphs > 20 packages.
//
// Environment:
//  * ZPM_REGISTRY  npm registry base URL     (default https://registry.npmjs.org)
//  * ZPM_ESBUILD   esbuild binary path       (default esbuild)
//  * ZPM_RUNTIME   runtime for `zpm run`     (default cz-llrt; replaces leading "node ")
//  * ZPM_TMPDIR    staging tmpfs root        (default /tmp)

import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import cp from "node:child_process";
import { createHash } from "node:crypto";

const ZPM_VERSION = "0.1.0";
const MAX_TARBALL_BYTES = 5 * 1024 * 1024; // 5 MiB
const MAX_GRAPH_PACKAGES = 20;

const fsp = fs.promises;
const REGISTRY = process.env.ZPM_REGISTRY || "https://registry.npmjs.org";
const ESBUILD = process.env.ZPM_ESBUILD || "esbuild";
const RUNTIME = process.env.ZPM_RUNTIME || "cz-llrt";
const TMP_ROOT = process.env.ZPM_TMPDIR || (os.tmpdir ? os.tmpdir() : "/tmp");

function err(msg) {
  console.error("zpm error: " + msg);
}

/** Existence probe via accessSync in try/catch (LLRT has no other existence helper). */
function exists(p) {
  try {
    fs.accessSync(p);
    return true;
  } catch (_e) {
    return false;
  }
}

/** Async `spawn` wrapped in a Promise - captures stdout/stderr and resolves { code, ... }. */
function runCmd(cmd, args, opts = {}) {
  return new Promise((resolve, reject) => {
    let child;
    try {
      child = cp.spawn(cmd, args, { cwd: opts.cwd, stdio: ["ignore", "pipe", "pipe"] });
    } catch (e) {
      reject(e);
      return;
    }
    let out = "";
    let errText = "";
    child.stdout.on("data", (d) => { out += typeof d === "string" ? d : String(d); });
    child.stderr.on("data", (d) => { errText += typeof d === "string" ? d : String(d); });
    child.on("error", (e) => reject(e));
    child.on("close", (code) => resolve({ code, stdout: out, stderr: errText }));
  });
}

/** Spawn with inherited stdio (`zpm run`), resolving with the child's exit code. */
function runInherit(command) {
  return new Promise((resolve, reject) => {
    let child;
    try {
      child = cp.spawn("sh", ["-c", command], { stdio: "inherit" });
    } catch (e) {
      reject(e);
      return;
    }
    child.on("error", (e) => reject(e));
    child.on("close", (code) => resolve(code === null ? 1 : code));
  });
}

// ---------------------------------------------------------------------------
// Semver-lite resolution against Object.keys(meta.versions)
// ---------------------------------------------------------------------------

function parseVer(s) {
  const core = String(s).replace(/^v/, "").split("-")[0];
  const parts = core.split(".").map((x) => Number(x) || 0);
  while (parts.length < 3) parts.push(0);
  return parts;
}

function cmpVer(a, b) {
  const pa = parseVer(a);
  const pb = parseVer(b);
  for (let i = 0; i < 3; i++) {
    if (pa[i] !== pb[i]) return pa[i] < pb[i] ? -1 : 1;
  }
  // Prereleases sort BELOW the same core version (kept simple, but deterministic).
  const preA = String(a).includes("-") ? 0 : 1;
  const preB = String(b).includes("-") ? 0 : 1;
  return preA - preB;
}

function maxOf(list) {
  let best = null;
  for (const v of list) if (best === null || cmpVer(v, best) > 0) best = v;
  return best;
}

function resolveVersion(meta, range) {
  const versions = Object.keys(meta.versions || {});
  if (!range || range === "latest" || range === "*" || range === "") {
    const latest = meta["dist-tags"] && meta["dist-tags"].latest;
    if (latest && meta.versions[latest]) return latest;
    return maxOf(versions);
  }
  if (meta.versions[range]) return range; // exact version
  let mode = null, base = null;
  if (range.startsWith("^")) { mode = "caret"; base = parseVer(range.slice(1)); }
  else if (range.startsWith("~")) { mode = "tilde"; base = parseVer(range.slice(1)); }
  else if (/^\d/.test(range)) {
    // Bare partial like "2" or "1.2" is treated loosely as caret with wildcard tails.
    mode = "caret"; base = parseVer(range);
    const dots = (String(range).match(/\./g) || []).length;
    if (dots === 0) { base[1] = -1; base[2] = -1; }
    else if (dots === 1) { base[2] = -1; }
  } else {
    // Unsupported range grammar: take the newest available version.
    return maxOf(versions);
  }
  const baseStr = base.join(".");
  const cand = versions.filter((v) => {
    const p = parseVer(v);
    if (p[0] !== base[0]) return false;
    if (mode === "caret" && base[0] === 0 && base[1] >= 0 && p[1] !== base[1]) return false;
    if (base[1] >= 0 && mode === "tilde" && p[1] !== base[1]) return false;
    if (mode === "caret" && base[1] >= 0 && base[2] < 0 && p[1] !== base[1]) return false;
    if (cmpVer(v, baseStr) < 0) return false;
    return true;
  });
  return maxOf(cand);
}

// ---------------------------------------------------------------------------
// Registry helpers
// ---------------------------------------------------------------------------

function pkgPath(name) {
  // Scoped packages encode as @scope%2Fname; plain names stay verbatim.
  return name.startsWith("@") ? name.replace("/", "%2F") : name;
}

async function fetchMeta(name) {
  const url = REGISTRY + "/" + pkgPath(name);
  const res = await fetch(url, {
    headers: { Accept: "application/vnd.npm.install-v1+json" },
  });
  if (!res.ok) throw new Error("registry metadata fetch failed for " + name + " (HTTP " + res.status + ")");
  const meta = await res.json();
  if (!meta || !meta.versions) throw new Error("registry metadata malformed for " + name);
  return meta;
}

async function fetchTarballBytes(tarballUrl) {
  const res = await fetch(tarballUrl);
  if (!res.ok) throw new Error("tarball fetch failed (" + tarballUrl + ", HTTP " + res.status + ")");
  const buf = await res.arrayBuffer();
  return new Uint8Array(buf);
}

/** SRI check: sha512 over exact bytes, base64 - matches `sha512-<...>` integrity strings. */
function sri512(bytes) {
  return "sha512-" + createHash("sha512").update(bytes).digest("base64");
}

function checkIntegrity(bytes, integrity, label) {
  if (!integrity) return; // registries without SRI metadata: accept (real npm always has it)
  if (!String(integrity).startsWith("sha512-")) {
    throw new Error(label + ": unsupported integrity algorithm (need sha512-<base64>)");
  }
  const got = sri512(bytes);
  if (got !== String(integrity)) {
    throw new Error(label + ": sha512 INTEGRITY MISMATCH\n  expected " + integrity + "\n  actual   " + got);
  }
}

// ---------------------------------------------------------------------------
// Native-addon & size guards
// ---------------------------------------------------------------------------

function nativeMetaRejection(vmeta) {
  if (vmeta.gypfile === true) return "gypfile:true (native C/C++ addon)";
  const scripts = vmeta.scripts || {};
  for (const key of ["install", "preinstall"]) {
    const cmd = String(scripts[key] || "");
    if (cmd.includes("node-gyp") || cmd.includes("prebuild")) {
      return key + " script references node-gyp/prebuild: " + cmd;
    }
  }
  return null;
}

/** Recursive scan of an extracted package tree for binding.gyp / *.node artifacts. */
async function findNativeArtifacts(dir) {
  let entries;
  try {
    entries = await fsp.readdir(dir, { withFileTypes: true });
  } catch (_e) {
    return null;
  }
  for (const ent of entries) {
    const full = path.join(dir, ent.name);
    if (ent.isDirectory()) {
      const sub = await findNativeArtifacts(full);
      if (sub) return sub;
    } else if (ent.name === "binding.gyp" || ent.name.endsWith(".node")) {
      return full;
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// ESM entry resolution from package.json
// ---------------------------------------------------------------------------

function pickExportString(cond, depth) {
  if (depth > 6) return null;
  if (typeof cond === "string") return cond;
  if (cond && typeof cond === "object") {
    for (const key of ["import", "module", "default", "browser"]) {
      const got = pickExportString(cond[key], depth + 1);
      if (got) return got;
    }
  }
  return null;
}

async function resolveEntry(pkgDir, pkgJson) {
  const candidates = [];
  const exp = pkgJson.exports && pkgJson.exports["."];
  const fromExports = pickExportString(exp, 0);
  if (fromExports) candidates.push(fromExports.replace(/^\.\/?/, ""));
  if (pkgJson.module) candidates.push(String(pkgJson.module).replace(/^\.\/?/, ""));
  if (pkgJson.main) candidates.push(String(pkgJson.main).replace(/^\.\/?/, ""));
  candidates.push("index.js", "index.mjs");
  for (const rel of candidates) {
    const full = path.join(pkgDir, rel);
    if (exists(full)) return full;
  }
  throw new Error("no ESM entry point found in package (exports/module/main/index all missing)");
}

// ---------------------------------------------------------------------------
// Dependency graph walk (fetch metadata only; guards run before any extraction)
// ---------------------------------------------------------------------------

async function walkGraph(rootName, rootVersion, rootVersionMeta) {
  const seen = new Map(); // name -> resolved version
  seen.set(rootName, rootVersion);
  const queue = [[rootVersionMeta.dependencies || {}]];
  while (queue.length > 0) {
    const [deps] = queue.shift();
    for (const [depName, range] of Object.entries(deps)) {
      const depMeta = await fetchMeta(depName);
      const depVer = resolveVersion(depMeta, range);
      if (!depVer || !depMeta.versions[depVer]) {
        throw new Error(depName + ": cannot resolve range \"" + range + "\" requested by " + rootName);
      }
      if (seen.has(depName)) continue; // dedupe by name (npm-like flat staging)
      seen.set(depName, depVer);
      const dvmeta = depMeta.versions[depVer];
      const nat = nativeMetaRejection(dvmeta);
      if (nat) throw new Error(depName + "@" + depVer + " rejected: " + nat);
      queue.push([dvmeta.dependencies || {}]);
      if (seen.size > MAX_GRAPH_PACKAGES) {
        throw new Error(
          "dependency graph too large: " + seen.size + " packages (zpm cap is " + MAX_GRAPH_PACKAGES + ")"
        );
      }
    }
  }
  return seen;
}

// ---------------------------------------------------------------------------
// Staging + bundling a package
// ---------------------------------------------------------------------------

async function extractInto(stageTgz, destDir) {
  await fsp.mkdir(destDir, { recursive: true });
  const r = await runCmd("tar", ["-xzf", stageTgz, "-C", destDir]);
  if (r.code !== 0) throw new Error("tar -xzf failed: " + r.stderr.trim());
  const pkgRoot = path.join(destDir, "package");
  if (!exists(pkgRoot)) throw new Error("tarball had no top-level package/ directory");
  return pkgRoot;
}

async function bundlePkg(name, range, opts) {
  const meta = await fetchMeta(name);
  const version = resolveVersion(meta, range);
  if (!version || !meta.versions[version]) {
    throw new Error(name + ": cannot resolve range \"" + (range || "latest") + "\"");
  }
  const vmeta = meta.versions[version];
  const nat = nativeMetaRejection(vmeta);
  if (nat) throw new Error(name + "@" + version + " rejected: " + nat);

  // Graph guard BEFORE downloading/extraction.
  const graph = await walkGraph(name, version, vmeta);

  if (!vmeta.dist || !vmeta.dist.tarball) throw new Error(name + "@" + version + ": metadata lacks dist.tarball");

  const bytes = await fetchTarballBytes(vmeta.dist.tarball);
  if (bytes.length > MAX_TARBALL_BYTES) {
    throw new Error(
      name + "@" + version + ": tarball too large (" + bytes.length + " bytes > 5 MiB cap)"
    );
  }
  checkIntegrity(bytes, vmeta.dist.integrity, name + "@" + version);

  // Everything stages in tmpfs and is ALWAYS removed in `finally`.
  const stage = await fsp.mkdtemp(path.join(TMP_ROOT, "zpm-"));
  try {
    const tgz = path.join(stage, "pkg.tgz");
    await fsp.writeFile(tgz, bytes);
    const pkgRoot = await extractInto(tgz, stage);
    const work = path.join(stage, "work");
    await fsp.rename(pkgRoot, work);

    // Recursively stage pure-JS transitive deps under <work>/node_modules/<dep>.
    const queue = [[name, vmeta]];
    const done = new Set([name]);
    while (queue.length > 0) {
      const [curName, curMeta] = queue.pop();
      const deps = curMeta.dependencies || {};
      for (const [depName, depRange] of Object.entries(deps)) {
        if (done.has(depName)) continue;
        done.add(depName);
        const depVer = graph.get(depName);
        const depMeta = await fetchMeta(depName);
        const dvmeta = depMeta.versions[depVer];
        if (!dvmeta || !dvmeta.dist || !dvmeta.dist.tarball) {
          throw new Error(depName + "@" + depVer + ": metadata lacks dist.tarball");
        }
        const dbytes = await fetchTarballBytes(dvmeta.dist.tarball);
        if (dbytes.length > MAX_TARBALL_BYTES) {
          throw new Error(depName + "@" + depVer + ": tarball too large (> 5 MiB cap)");
        }
        checkIntegrity(dbytes, dvmeta.dist.integrity, depName + "@" + depVer);
        const dtgz = path.join(stage, "dep-" + depName.replace(/[@/]/g, "_") + ".tgz");
        await fsp.writeFile(dtgz, dbytes);
        // Extract into a private slot first, then rename the tarball's package/ dir onto the
        // final node_modules/<dep> path (scoped names nest), so esbuild resolves bare imports.
        const slot = path.join(stage, "slot-" + depName.replace(/[@/]/g, "_"));
        const extracted = await extractInto(dtgz, slot);
        const depDest = path.join(work, "node_modules", ...depName.split("/"));
        await fsp.mkdir(path.dirname(depDest), { recursive: true });
        await fsp.rm(depDest, { recursive: true, force: true });
        await fsp.rename(extracted, depDest);
        queue.push([depName, dvmeta]);
      }
    }

    const pkgJsonPath = path.join(work, "package.json");
    if (!exists(pkgJsonPath)) throw new Error(name + ": extracted package lacks package.json");
    const pkgJson = JSON.parse(await fsp.readFile(pkgJsonPath, "utf8"));
    const entry = await resolveEntry(work, pkgJson);

    // Post-extract native artifact scan (binding.gyp / *.node) on the full staged tree.
    const artifact = await findNativeArtifacts(work);
    if (artifact) {
      throw new Error(name + "@" + version + " rejected: native artifact found in tarball (" + path.basename(artifact) + ")");
    }

    const outDir = path.join(process.cwd(), "node_modules", ...name.split("/"));
    const outFile = path.join(outDir, "index.mjs");
    await fsp.mkdir(outDir, { recursive: true });

    const esbArgs = [
      entry,
      "--bundle",
      "--format=esm",
      "--platform=neutral",
      "--main-fields=module,main",
      "--conditions=import,module,default",
      "--external:node:*",
      "--external:cardputer:*",
    ];
    if (!opts.noMinify) esbArgs.push("--minify");
    esbArgs.push("--outfile=" + outFile);
    const r = await runCmd(ESBUILD, esbArgs, { cwd: work });
    if (r.code !== 0) {
      throw new Error("esbuild failed for " + name + "@" + version + ":\n" + (r.stderr || r.stdout));
    }

    const outMeta = await fsp.stat(outFile);
    await fsp.writeFile(
      path.join(outDir, "package.json"),
      JSON.stringify(
        { name: name, version: version, type: "module", main: "./index.mjs", exports: { ".": "./index.mjs" } },
        null,
        2
      ) + "\n",
      "utf8"
    );

    // Project manifest + zpm lockfile updates (merge-preserving).
    await recordInstall(name, version, vmeta.dist.integrity || sri512(bytes), outMeta.size);
    console.log(
      "zpm: installed " + name + "@" + version + " -> node_modules/" + name + "/index.mjs (" +
      outMeta.size + " bytes" + (opts.noMinify ? ", unminified" : "") + ")"
    );
  } finally {
    await fsp.rm(stage, { recursive: true, force: true });
  }
}

async function readJsonOpt(file) {
  try {
    return JSON.parse(await fsp.readFile(file, "utf8"));
  } catch (_e) {
    return null;
  }
}

async function recordInstall(name, version, integrity, bundledBytes) {
  const projPkgPath = path.join(process.cwd(), "package.json");
  const projPkg = (await readJsonOpt(projPkgPath)) || {};
  projPkg.dependencies = projPkg.dependencies || {};
  projPkg.dependencies[name] = "^" + version;
  await fsp.writeFile(projPkgPath, JSON.stringify(projPkg, null, 2) + "\n", "utf8");

  const lockPath = path.join(process.cwd(), "zpm-lock.json");
  const lock = (await readJsonOpt(lockPath)) || { lockVersion: 1, packages: {} };
  lock.packages = lock.packages || {};
  lock.packages[name] = { version: version, integrity: integrity, bundledBytes: bundledBytes };
  await fsp.writeFile(lockPath, JSON.stringify(lock, null, 2) + "\n", "utf8");
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

function cmdVersion() {
  console.log(
    "zpm " + ZPM_VERSION + " - Zero Package Manager (npm shim for CardputerZero cz-llrt; " +
    "bundler: esbuild minified ESM single-file; cardputer:* builtins external)"
  );
  return 0;
}

async function cmdInstall(args) {
  const opts = { noMinify: false };
  const pkgs = [];
  for (const a of args) {
    if (a === "--no-minify") opts.noMinify = true;
    else if (a.startsWith("-")) throw new Error("unknown flag for zpm install: " + a);
    else pkgs.push(a);
  }
  if (pkgs.length === 0) {
    const projPkg = await readJsonOpt(path.join(process.cwd(), "package.json"));
    const deps = projPkg && projPkg.dependencies ? Object.entries(projPkg.dependencies) : [];
    if (deps.length === 0) throw new Error("no packages given and package.json has no dependencies");
    for (const [n, r] of deps) pkgs.push(n + "@" + r);
  }
  for (const spec of pkgs) {
    let name = spec, range = null;
    if (spec.startsWith("@")) {
      const at = spec.indexOf("@", 1);
      if (at > 0) { name = spec.slice(0, at); range = spec.slice(at + 1); }
    } else {
      const at = spec.indexOf("@");
      if (at > 0) { name = spec.slice(0, at); range = spec.slice(at + 1); }
    }
    await bundlePkg(name, range, opts);
  }
  return 0;
}

async function cmdRun(args) {
  if (args.length === 0) throw new Error("zpm run needs a script name from package.json");
  const scriptName = args[0];
  const projPkg = await readJsonOpt(path.join(process.cwd(), "package.json"));
  const scripts = projPkg && projPkg.scripts ? projPkg.scripts : {};
  const cmdLine = scripts[scriptName];
  if (typeof cmdLine !== "string") {
    throw new Error('missing script "' + scriptName + '" in package.json');
  }
  const substituted = cmdLine.replace(/^node(?=\s|$)/, RUNTIME);
  return await runInherit(substituted);
}

async function cmdBundle(args) {
  let entry = null;
  let out = null;
  const opts = { noMinify: false };
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "-o" || a === "--outfile") { out = args[++i]; continue; }
    if (a.startsWith("--outfile=")) { out = a.slice("--outfile=".length); continue; }
    if (a === "--no-minify") { opts.noMinify = true; continue; }
    if (a.startsWith("-")) throw new Error("unknown flag for zpm bundle: " + a);
    if (entry === null) entry = a;
  }
  if (!entry || !out) throw new Error("usage: zpm bundle <entry> -o <out.mjs> [--no-minify]");
  const outFile = path.resolve(out);
  await fsp.mkdir(path.dirname(outFile), { recursive: true });
  const esbArgs = [
    entry,
    "--bundle",
    "--format=esm",
    "--platform=neutral",
    "--external:node:*",
    "--external:cardputer:*",
  ];
  if (!opts.noMinify) esbArgs.push("--minify");
  esbArgs.push("--outfile=" + outFile);
  const r = await runCmd(ESBUILD, esbArgs, { cwd: process.cwd() });
  if (r.code !== 0) {
    err(r.stderr || r.stdout);
    throw new Error("esbuild bundle failed for " + entry);
  }
  console.log("zpm: bundled " + entry + " -> " + out);
  return 0;
}

async function main(argv) {
  const cmd = argv[0];
  const rest = argv.slice(1);
  if (cmd === "--version" || cmd === "-v" || cmd === "version" || (cmd === undefined && rest.length === 0)) {
    return cmdVersion();
  }
  try {
    if (cmd === "install" || cmd === "i" || cmd === "add") return await cmdInstall(rest);
    if (cmd === "run") return await cmdRun(rest);
    if (cmd === "bundle") return await cmdBundle(rest);
    throw new Error("unknown command: " + cmd + " (supported: install|i|add, run, bundle, --version)");
  } catch (e) {
    err((e && e.message) || String(e));
    return 1;
  }
}

main(process.argv.slice(2)).then(
  (code) => process.exit(code),
  (e) => {
    err((e && e.stack) || String(e));
    process.exit(1);
  }
);
