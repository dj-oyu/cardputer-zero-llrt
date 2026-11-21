//! `cz-zpm` — native Rust multicall package manager (`zpm` / `npm`) for `cz-js`.
//!
//! Designed to be linked directly into `cz-js` as a multicall binary
//! (`ln -s cz-js zpm`, `ln -s cz-js npm`, `ln -s cz-js node`) and also built as a
//! standalone `cz-zpm` binary for fast isolated testing. All networking, TLS, gzip and tar
//! handling is **pure Rust synchronous code** (`ureq` v3 over `cz-net`'s `sync_agent` — the exact
//! same system-CA + `ring` stack as `cz-js` via `cz_net::tls::build_client_config`, ZERO bundled
//! `webpki-roots`/`tokio`/`hyper` — plus `flate2` rust_backend +
//! `tar` + `sha2`): the external `tar` binary is NEVER spawned and `.tgz` bytes are NEVER written
//! to disk — tarballs stream in memory (`5 MiB` hard cap), `sha512-<base64>` integrity is
//! verified BEFORE extraction, zip-slip path traversal and symlinks/hardlinks are rejected,
//! native addon packages (`gypfile`, `node-gyp`/`prebuild` install scripts, `binding.gyp`,
//! `*.node`) are rejected, dependency graphs over `20` packages are rejected, and staging dirs
//! `<ZPM_TMPDIR>/zpm-*` (default `/tmp`, never `TMPDIR`) are ALWAYS cleaned via RAII `Drop`.
//!
//! Phase 7C lifecycle commands: `zpm update [pkg ...] [--latest] [--dry-run] [--no-minify]`
//! (`up`/`upgrade` aliases) resolves the highest version satisfying the `package.json` semver
//! range (or `dist-tags.latest` with `--latest`); up-to-date packages are skipped with ZERO
//! downloads and ZERO writes, `--dry-run` prints plans without touching the filesystem, and
//! commits run the full safe staging pipeline then ATOMICALLY replace `index.mjs` (temp file +
//! rename), so a failed update leaves the installed bundle, manifest and `zpm-lock.json`
//! completely intact. Also: `zpm outdated [--json]`, `zpm remove <pkg ...>`
//! (`rm`/`uninstall`/`un`) and `zpm list` (`ls`).

use std::collections::{HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command as StdCommand;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use flate2::read::GzDecoder;
use sha2::{Digest, Sha512};
use tar::{Archive, EntryType};
use serde_json::Value as Json;
use ureq::config::Config;
use ureq::Agent;

/// Strict streaming cap for downloaded tarballs: 5 MiB.
pub const TARBALL_CAP: usize = 5 * 1024 * 1024;
/// Maximum cumulative decompressed bytes extracted from a single tarball: 32 MiB (gzip-bomb cap).
pub const EXTRACTED_CAP: usize = 32 * 1024 * 1024;
/// Maximum total packages (root + transitive deps) per install graph.
pub const MAX_PACKAGES: usize = 20;

const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
const DEFAULT_ESBUILD: &str = "esbuild";
const DEFAULT_RUNTIME: &str = "cz-js";
// NOTE: NEVER `TMPDIR` (on-device that defaults to /data/local/tmp).
const DEFAULT_TMPDIR: &str = "/tmp";

// ---------------------------------------------------------------------------------------------
// Multicall dispatch
// ---------------------------------------------------------------------------------------------

/// Pure helper for unit testing multicall dispatch logic:
/// - `argv[0]` basename == `zpm`/`npm` -> run CLI with `args[1..]`.
/// - `argv[1] == "zpm"` (e.g. `cz-js zpm ...`) -> run CLI with `args[2..]`.
/// - Otherwise `None` so the JS runtime continues normal startup.
pub fn parse_multicall_args(args: &[String]) -> Option<Vec<String>> {
    let argv0 = args.first()?;
    let base = Path::new(argv0).file_name()?.to_string_lossy().into_owned();
    if base == "zpm" || base == "npm" {
        return Some(args[1..].to_vec());
    }
    if args.get(1).map(|s| s.as_str()) == Some("zpm") {
        return Some(args[2..].to_vec());
    }
    None
}

/// Inspect `std::env::args()` for a multicall invocation; returns `Some(exit_code)` when this
/// binary was invoked as `zpm`/`npm` (symlink) or as `... zpm <cmd>`.
pub fn maybe_run_multicall() -> Option<u8> {
    let all: Vec<String> = env::args().collect();
    let cli = parse_multicall_args(&all)?;
    Some(run_cli(&cli))
}

/// Run the zpm CLI directly with the given arguments (excluding program name).
pub fn run_cli(args: &[String]) -> u8 {
    let sub = args.first().map(|s| s.as_str());
    match sub {
        None | Some("--help") | Some("-h") => {
            println!("{}", help_text(None));
            return 0;
        }
        Some("help") => {
            let target = args.get(1).map(|s| s.as_str());
            println!("{}", help_text(target));
            return 0;
        }
        Some("--version") | Some("-v") | Some("version") => {
            println!("{}", version_line());
            return 0;
        }
        _ => {}
    }

    if let Some(cmd) = sub {
        if has_help_flag(&args[1..]) {
            if let Some(txt) = subcommand_help(cmd) {
                println!("{txt}");
                return 0;
            }
        }
    }

    match sub {
        Some("install") | Some("i") | Some("add") => cmd_install(&args[1..]),
        Some("update") | Some("up") | Some("upgrade") => cmd_update(&args[1..]),
        Some("outdated") => cmd_outdated(&args[1..]),
        Some("remove") | Some("rm") | Some("uninstall") | Some("un") => cmd_remove(&args[1..]),
        Some("list") | Some("ls") => cmd_list(&args[1..]),
        Some("run") => cmd_run(&args[1..]),
        Some("bundle") => cmd_bundle(&args[1..]),
        _ => {
            eprintln!(
                "zpm: unknown command '{}'. Run `zpm --help` for usage.",
                sub.unwrap_or("")
            );
            1
        }
    }
}

fn has_help_flag(args: &[String]) -> bool {
    for a in args {
        if a == "--" {
            break;
        }
        if a == "--help" || a == "-h" {
            return true;
        }
    }
    false
}

fn subcommand_help(cmd: &str) -> Option<&'static str> {
    match cmd {
        "install" | "i" | "add" => Some(
            "usage: zpm install [pkg[@range] ...] [--no-minify]\n\
             aliases: i, add\n\n\
             Install pure-JS npm package(s) as single-file ESM bundles (node_modules/<pkg>/index.mjs).\n\
             With no package arguments, installs all dependencies listed in ./package.json.\n\n\
             Options:\n\
               --no-minify    Keep esbuild output unminified (default: minified)\n\
               -h, --help     Show this help\n\n\
             Safety guards:\n\
               - Stages in tmpfs ($ZPM_TMPDIR, default /tmp) and cleans up automatically\n\
               - Verifies SHA-512 integrity, rejects zip-slip paths, >5 MiB tarballs, >20 deps\n\
               - Rejects native C/C++ addons (binding.gyp, *.node, gypfile)\n\
               - Marks node:* and cardputer:* as external builtins",
        ),
        "update" | "up" | "upgrade" => Some(
            "usage: zpm update [pkg ...] [--latest] [--dry-run] [--no-minify]\n\
             aliases: up, upgrade\n\n\
             Update installed package(s) within package.json semver ranges (or all dependencies).\n\
             Skips download/bundle when already up to date; rolls back atomically on failure.\n\n\
             Options:\n\
               --latest       Upgrade beyond semver range to dist-tags.latest and update package.json\n\
               --dry-run      Show planned updates without downloading or writing files\n\
               --no-minify    Keep esbuild output unminified\n\
               -h, --help     Show this help",
        ),
        "outdated" => Some(
            "usage: zpm outdated [--json]\n\n\
             Check installed packages in ./package.json against the registry.\n\n\
             Options:\n\
               --json         Emit machine-readable JSON array [{name, current, wanted, latest}]\n\
               -h, --help     Show this help",
        ),
        "remove" | "rm" | "uninstall" | "un" => Some(
            "usage: zpm remove <pkg ...>\n\
             aliases: rm, uninstall, un\n\n\
             Remove installed package(s) from node_modules/, package.json, and zpm-lock.json.\n\n\
             Options:\n\
               -h, --help     Show this help",
        ),
        "list" | "ls" => Some(
            "usage: zpm list\n\
             aliases: ls\n\n\
             List installed packages from zpm-lock.json (or node_modules/*/package.json).\n\n\
             Options:\n\
               -h, --help     Show this help",
        ),
        "run" => Some(
            "usage: zpm run <script>\n\n\
             Execute scripts.<script> from ./package.json via `sh -c`,\n\
             substituting a leading `node ` with `$ZPM_RUNTIME` (default: cz-js).\n\n\
             Options:\n\
               -h, --help     Show this help",
        ),
        "bundle" => Some(
            "usage: zpm bundle <entry> -o <out.mjs> [--no-minify]\n\n\
             Bundle a local JS/TS entry file into a single ESM file via esbuild\n\
             with --external:node:* and --external:cardputer:*.\n\n\
             Options:\n\
               -o, --out <file>   Output bundle path (required)\n\
               --no-minify        Keep output unminified (default: minified)\n\
               -h, --help         Show this help",
        ),
        _ => None,
    }
}

fn help_text(sub: Option<&str>) -> String {
    if let Some(cmd) = sub {
        if let Some(txt) = subcommand_help(cmd) {
            return txt.to_string();
        }
    }
    format!(
        "{}\n\n\
         SD-card-friendly npm shim for CardputerZero: stages tarballs in tmpfs (/tmp),\n\
         verifies SHA-512, rejects native addons, and bundles each package into a single\n\
         ESM file (`node_modules/<pkg>/index.mjs`) with `node:*` & `cardputer:*` external.\n\n\
         Usage:\n\
           zpm <command> [options]   (or invoke via `npm` symlink)\n\n\
         Commands:\n\
           install, i, add   Install package(s) as single-file ESM bundles\n\
           update, up        Update installed package(s) (--latest, --dry-run)\n\
           outdated          Show outdated dependencies (--json)\n\
           remove, rm, un    Remove installed package(s)\n\
           list, ls          List installed packages\n\
           run               Run a script from package.json\n\
           bundle            Bundle a local entry file with esbuild\n\
           help [command]    Show help for zpm or a subcommand\n\n\
         Global options:\n\
           -v, --version     Print runtime/shim identity\n\
           -h, --help        Show help (works on any subcommand too)\n\n\
         Environment:\n\
           ZPM_REGISTRY      npm registry URL (default: {DEFAULT_REGISTRY})\n\
           ZPM_ESBUILD       esbuild binary path (default: {DEFAULT_ESBUILD})\n\
           ZPM_RUNTIME       JS runtime for `zpm run` (default: {DEFAULT_RUNTIME})\n\
           ZPM_TMPDIR        tmpfs staging dir (default: /tmp)",
        version_line()
    )
}

/// Identity line for `--version` / `-v` (contains: zpm, native, shim, esbuild, cardputer:*).
fn version_line() -> String {
    format!(
        "zpm {} \u{2014} native multicall shim for {} \u{2014} esbuild bundler, externals: node:* cardputer:*",
        env!("CARGO_PKG_VERSION"),
        DEFAULT_RUNTIME
    )
}

// ---------------------------------------------------------------------------------------------
// Environment helpers
// ---------------------------------------------------------------------------------------------

fn env_or(key: &str, default: &str) -> String {
    match env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

fn registry_base() -> String {
    env_or("ZPM_REGISTRY", DEFAULT_REGISTRY).trim_end_matches('/').to_string()
}

fn esbuild_bin() -> String {
    env_or("ZPM_ESBUILD", DEFAULT_ESBUILD)
}

fn runtime_bin() -> String {
    env_or("ZPM_RUNTIME", DEFAULT_RUNTIME)
}

fn tmp_base() -> PathBuf {
    // ZPM_TMPDIR only — deliberately NEVER TMPDIR.
    PathBuf::from(env_or("ZPM_TMPDIR", DEFAULT_TMPDIR))
}

// ---------------------------------------------------------------------------------------------
// RAII staging dir (ALWAYS cleaned, even on error / early return)
// ---------------------------------------------------------------------------------------------

/// RAII guard for `<ZPM_TMPDIR>/zpm-<pid>-<rand>` staging dirs: `Drop` removes the whole tree.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a unique `zpm-*` staging dir under `ZPM_TMPDIR` (default `/tmp`, never `TMPDIR`).
    pub fn new(prefix: &str) -> Result<Self, String> {
        let base = tmp_base();
        fs::create_dir_all(&base).map_err(|e| format!("cannot create {}: {e}", base.display()))?;
        let pid = std::process::id();
        // Cheap entropy from wall-clock nanos + pid for uniqueness across reruns.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        for attempt in 0..32u64 {
            let name = format!("{prefix}-{pid}-{:x}{:x}", nanos.wrapping_mul(0x9e3779b97f4a7c15), attempt);
            let candidate = base.join(name);
            match fs::create_dir(&candidate) {
                Ok(()) => return Ok(Self { path: candidate }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot create {}: {e}", candidate.display())),
            }
        }
        Err("could not allocate unique zpm-* staging dir".to_string())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Always clean up staging; ignore missing dirs.
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------------------------
// URL helpers (redirect + relative resolution)
// ---------------------------------------------------------------------------------------------

/// Validate a strict npm package name (lowercase ASCII `[a-z0-9-~][a-z0-9-._~]*`,
/// optional single `@scope/`, length `1..=214`). Makes filesystem traversal (`..`, `.`, `\`,
/// extra `/`) and URL/query smuggling (`?`, `#`, `%`, whitespace, control chars) impossible.
pub fn validate_pkg_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 214 {
        return Err(format!("invalid npm package name {name:?}: bad length"));
    }
    fn check_seg(seg: &str, full: &str) -> Result<(), String> {
        if seg.is_empty() || seg == "." || seg == ".." || seg.starts_with('.') || seg.starts_with('_') {
            return Err(format!("invalid npm package name {full:?}"));
        }
        let bytes = seg.as_bytes();
        if !matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9' | b'-' | b'~') {
            return Err(format!("invalid npm package name {full:?}"));
        }
        for &b in &bytes[1..] {
            if !matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
                return Err(format!("invalid npm package name {full:?}"));
            }
        }
        Ok(())
    }
    if let Some(rest) = name.strip_prefix('@') {
        let (scope, pkg) = rest
            .split_once('/')
            .ok_or_else(|| format!("invalid scoped npm package name {name:?}"))?;
        if pkg.contains('/') {
            return Err(format!("invalid scoped npm package name {name:?}"));
        }
        check_seg(scope, name)?;
        check_seg(pkg, name)
    } else {
        if name.contains('/') {
            return Err(format!("invalid npm package name {name:?}"));
        }
        check_seg(name, name)
    }
}

/// Validate that a relative entry path stays strictly inside its package root: rejects empty
/// strings, NULs, backslashes, absolute/prefixed paths, and `.`/`..` traversal segments.
pub fn validate_safe_rel_path(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\0') || trimmed.contains('\\') {
        return Err(format!("unsafe package entry path {raw:?}"));
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        return Err(format!("absolute package entry path rejected: {raw:?}"));
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("path traversal in package entry rejected: {raw:?}"));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(format!("empty package entry path: {raw:?}"));
    }
    Ok(out)
}

/// Percent-encode a package specifier for use in an HTTP path (keeps `/` and `@` for scopes).
fn encode_pkg_spec(spec: &str) -> String {
    let mut out = String::new();
    for b in spec.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '~' | '@' | '/') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}", b = b));
        }
    }
    out
}

/// Minimal absolute-URL splitter over borrowed slices (`http`/`https` only) — zero extra crates,
/// zero per-call allocations beyond the eventual joined target.
struct UrlParts<'a> {
    scheme: &'a str,
    authority: &'a str,
    path: &'a str,
}

fn split_url(url: &str) -> Result<UrlParts<'_>, String> {
    let sep = url.find("://").ok_or(format!("invalid uri {url:?}: missing scheme"))?;
    let scheme = &url[..sep];
    if scheme != "http" && scheme != "https" {
        return Err(format!("invalid uri {url:?}: unsupported scheme {scheme:?}"));
    }
    let rest = &url[sep + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() {
        return Err(format!("invalid uri {url:?}: empty authority"));
    }
    Ok(UrlParts {
        scheme,
        authority,
        path: &rest[end..],
    })
}

/// Resolve a possibly-relative `location`/tarball URL against an absolute base URL.
fn join_url(base: &str, rel: &str) -> Result<String, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("empty redirect/target url".to_string());
    }
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return Ok(rel.to_string());
    }
    let bu = split_url(base)?;
    let target = if rel.starts_with('/') {
        rel.to_string()
    } else {
        // Relative to the base path's directory.
        let dir = match bu.path.rfind('/') {
            Some(i) => &bu.path[..i + 1],
            None => "/",
        };
        format!("{dir}{rel}")
    };
    Ok(format!("{}://{}{}", bu.scheme, bu.authority, target))
}

/// Resolve a redirect `Location` against `current_url`, rejecting HTTPS->HTTP downgrades.
fn check_redirect_target(current_url: &str, location: &str) -> Result<String, String> {
    let cur = split_url(current_url)?;
    let next_uri = join_url(current_url, location)?;
    let next = split_url(&next_uri)?;
    if cur.scheme == "https" && next.scheme != "https" {
        return Err(format!(
            "refusing HTTPS->HTTP redirect downgrade from {current_url} to {next_uri}"
        ));
    }
    Ok(next_uri)
}

// ---------------------------------------------------------------------------------------------
// HTTP fetch (ureq v3 over cz-net's sync transport; system CA + ring via build_client_config;
// HTTP and HTTPS; <=5 manual redirects; strict streaming cap). No tokio, no hyper.
// ---------------------------------------------------------------------------------------------

/// Build the synchronous ureq agent ONCE per CLI command. Redirect following is DISABLED
/// (`max_redirects(0)` + no error) so the manual <=5-hop loop below keeps full control of
/// relative `Location` resolution, exactly like the previous hyper client did; non-2xx must
/// RESOLVE so the identical `HTTP {status}` guard messages fire. TLS is the exact same
/// system-CA + `ring` stack `cz-js` uses: `cz_net::tls::build_client_config(&TlsClientOptions::
/// default())` — never `webpki-roots`.
fn build_client() -> Result<Agent, String> {
    // Device-realistic bounds (CardputerZero flaky links): DNS/connect/first-read must not hang
    // forever. Env override ZPM_TIMEOUT_MS tunes the READ bound (1..60_000 ms), else 10 s default.
    let read_secs = match std::env::var("ZPM_TIMEOUT_MS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(ms) if (1..=60_000).contains(&ms) => ms,
            _ => return Err("bad ZPM_TIMEOUT_MS (want 1..60000)".to_string()),
        },
        Err(_) => 10_000,
    };
    let cfg = Config::builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        // No pooling: zpm makes a handful of requests per command then exits.
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .timeout_resolve(Some(std::time::Duration::from_secs(5)))
        .timeout_connect(Some(std::time::Duration::from_secs(10)))
        .timeout_per_read(Some(std::time::Duration::from_millis(read_secs)))
        .no_delay(true)
        .build();
    let tls = cz_net::tls::build_client_config(&cz_net::tls::TlsClientOptions::default())?;
    Ok(cz_net::fetch::sync_agent(cfg, Some(tls)))
}

/// GET a URL with optional `Accept` header; follows up to 5 (301/302/307/308) redirects (rejecting
/// HTTPS->HTTP downgrades); streams the body into memory under the strict 5 MiB cap through a
/// fixed 64 KiB stack chunk buffer.
fn fetch_bytes(agent: &Agent, url: &str, accept: Option<&str>) -> Result<Vec<u8>, String> {
    split_url(url)?;
    let mut uri = url.to_string();
    let mut hops = 0usize;
    loop {
        let mut builder = agent.get(uri.as_str());
        if let Some(a) = accept {
            builder = builder.header("accept", a);
        }
        let resp = builder
            .call()
            .map_err(|e| format!("request failed for {url}: {e}"))?;
        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 307 | 308) {
            hops += 1;
            if hops > 5 {
                return Err(format!("too many redirects (>5) for {url}"));
            }
            let loc = resp
                .headers()
                .get("location")
                .ok_or(format!("redirect {status} without Location for {url}"))?;
            uri = check_redirect_target(&uri, &String::from_utf8_lossy(loc.as_bytes()))?;
            continue; // drop redirect body
        }
        if !(200..=299).contains(&status) {
            return Err(format!("HTTP {status} for {url}"));
        }
        let mut reader = resp.into_body().into_reader();
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() + n > TARBALL_CAP {
                        return Err(format!(
                            "download exceeds strict {} MiB streaming cap",
                            TARBALL_CAP / (1024 * 1024)
                        ));
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("body stream error: {e}")),
            }
        }
        return Ok(buf);
    }
}

// ---------------------------------------------------------------------------------------------
// sha512-<base64> integrity verification (BEFORE extraction, in memory)
// ---------------------------------------------------------------------------------------------

/// Verify a `sha512-<base64>` SRI string against in-memory bytes. `Ok(true)` == match.
pub fn verify_integrity(bytes: &[u8], sri: &str) -> Result<bool, String> {
    let b64 = match sri.strip_prefix("sha512-") {
        Some(rest) => rest.trim(),
        None => return Err(format!("unsupported/missing integrity algorithm: {sri:?}")),
    };
    let expected = BASE64
        .decode(b64)
        .map_err(|e| format!("bad base64 in integrity {sri:?}: {e}"))?;
    let got = sha512_digest(bytes);
    Ok(got.as_slice() == expected.as_slice())
}

/// Compute the SHA-512 digest of `bytes` exactly once (shared by SRI verification and by the
/// recorded integrity string — Phase 10D removes the second hashing pass over up-to-5 MiB
/// tarballs).
fn sha512_digest(bytes: &[u8]) -> sha2::digest::Output<Sha512> {
    Sha512::new().chain_update(bytes).finalize()
}

/// Format a computed SHA-512 digest as a `sha512-<base64>` SRI string.
fn sri_from_digest(digest: &sha2::digest::Output<Sha512>) -> String {
    format!("sha512-{}", BASE64.encode(digest.as_slice()))
}

// ---------------------------------------------------------------------------------------------
// Tar safety guards + in-memory extraction (flate2 GzDecoder + tar Archive; NEVER `tar` binary)
// ---------------------------------------------------------------------------------------------

/// Zip-slip guard: sanitize a tar entry path. Rejects `..` (`ParentDir`), absolute paths
/// (leading `/` or drive prefix) and collapses to a safe relative path; strips the npm
/// `package/` prefix.
pub fn sanitize_tar_path(raw: &str) -> Result<PathBuf, String> {
    let p = Path::new(raw);
    if p.is_absolute() {
        return Err(format!("absolute tar entry path rejected: {raw:?}"));
    }
    for c in p.components() {
        match c {
            Component::ParentDir => return Err(format!("zip-slip '..' rejected: {raw:?}")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("absolute tar entry path rejected: {raw:?}"))
            }
            _ => {}
        }
    }
    let comps: Vec<&std::ffi::OsStr> = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(seg) => Some(seg),
            _ => None,
        })
        .collect();
    let mut pb = PathBuf::new();
    let skip_package = comps.len() > 1 && comps[0] == std::ffi::OsStr::new("package");
    let segs = if skip_package { &comps[1..] } else { &comps[..] };
    for seg in segs {
        pb.push(seg);
    }
    if segs.is_empty() {
        return Err(format!("empty tar entry path: {raw:?}"));
    }
    Ok(pb)
}

/// Native-addon guard on package metadata: rejects `gypfile: true` and `install` /
/// `preinstall` scripts referencing `node-gyp` or `prebuild`.
pub fn native_addon_reject(pkg: &Json) -> Option<String> {
    if pkg.get("gypfile").and_then(|v| v.as_bool()) == Some(true) {
        return Some("native addon package rejected (gypfile: true)".to_string());
    }
    let scripts = pkg.get("scripts");
    for key in ["install", "preinstall"] {
        if let Some(s) = scripts.and_then(|sc| sc.get(key)).and_then(|v| v.as_str()) {
            let lower = s.to_lowercase();
            if lower.contains("node-gyp") || lower.contains("prebuild") {
                return Some(format!("native addon install script rejected ({key}: {s:?})"));
            }
        }
    }
    None
}

fn entry_is_native_artifact(rel: &Path) -> bool {
    let name = rel.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    name == "binding.gyp" || name.ends_with(".node")
}

/// Stream tarball bytes (`gzip` -> `tar`) purely in memory and extract ONLY safe regular files
/// into `dest`. Enforces zip-slip, symlink/hardlink and native-addon guards; returns the parsed
/// root `package.json`. NEVER spawns external `tar`; NEVER touches `.tgz` on disk.
pub fn extract_tarball(bytes: &[u8], dest: &Path) -> Result<Json, String> {
    // Phase 10D: borrow the tarball bytes through the cursor instead of copying the whole
    // (up-to-5 MiB) `.tgz` buffer (`Cursor<&[u8]>` implements `Read` directly).
    // Bound decompressed stream bytes + track cumulative extracted bytes against gzip bombs.
    let dec = GzDecoder::new(Cursor::new(bytes)).take((EXTRACTED_CAP as u64) * 2 + 1024 * 1024);
    let mut ar = Archive::new(dec);
    let mut pkg_json: Option<Json> = None;
    let mut total_extracted: u64 = 0;
    for entry in ar.entries().map_err(|e| format!("tar parse error: {e}"))? {
        let mut entry = entry.map_err(|e| format!("tar entry error: {e}"))?;
        let ty = entry.header().entry_type();
        if matches!(ty, EntryType::Symlink | EntryType::Link) {
            return Err(format!(
                "symlink/hardlink tar entry rejected: {:?}",
                String::from_utf8_lossy(entry.path_bytes().as_ref())
            ));
        }
        let raw = String::from_utf8_lossy(entry.path_bytes().as_ref()).into_owned();
        let rel = sanitize_tar_path(&raw)?;
        if entry_is_native_artifact(&rel) {
            return Err(format!("native addon artifact rejected: {raw:?}"));
        }
        let target = dest.join(&rel);
        if ty.is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("mkdir failed: {e}"))?;
            continue;
        }
        if !ty.is_file() {
            // Skip exotic special files (fifos/devices) entirely.
            continue;
        }
        let size = entry.header().size().map_err(|e| format!("bad tar size: {e}"))?;
        if size > TARBALL_CAP as u64 {
            return Err(format!("tar entry {raw:?} exceeds 5 MiB cap"));
        }
        let projected_total = total_extracted
            .checked_add(size)
            .ok_or_else(|| "cumulative extracted size overflow".to_string())?;
        if projected_total > EXTRACTED_CAP as u64 {
            return Err(format!(
                "cumulative extracted size exceeds {} MiB cap",
                EXTRACTED_CAP / (1024 * 1024)
            ));
        }
        let remaining = (EXTRACTED_CAP as u64).saturating_sub(total_extracted);
        let entry_cap = std::cmp::min(TARBALL_CAP as u64, remaining);
        let mut content: Vec<u8> = Vec::new();
        entry
            .by_ref()
            .take(entry_cap + 1)
            .read_to_end(&mut content)
            .map_err(|e| format!("tar read error for {raw:?}: {e}"))?;
        if content.len() as u64 > size {
            return Err(format!("tar entry {raw:?} read exceeded declared size"));
        }
        total_extracted = total_extracted
            .checked_add(content.len() as u64)
            .ok_or_else(|| "cumulative extracted size overflow".to_string())?;
        if total_extracted > EXTRACTED_CAP as u64 {
            return Err(format!(
                "cumulative extracted size exceeds {} MiB cap",
                EXTRACTED_CAP / (1024 * 1024)
            ));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
        }
        let mut f = fs::File::create(&target).map_err(|e| format!("write failed: {e}"))?;
        f.write_all(&content).map_err(|e| format!("write failed: {e}"))?;
        if rel == Path::new("package.json") {
            pkg_json = Some(
                serde_json::from_slice::<Json>(&content)
                    .map_err(|e| format!("invalid package.json in tarball: {e}"))?,
            );
        }
    }
    pkg_json.ok_or("npm tarball is missing package/package.json".to_string())
}

// ---------------------------------------------------------------------------------------------
// Version resolution (latest / exact / ^ / ~ / >=)
// ---------------------------------------------------------------------------------------------

/// Parse `x.y.z` (tolerating a leading `v`; ignoring `-pre`/`+build` suffixes for ordering).
pub fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().strip_prefix('v').unwrap_or(s);
    let core = s.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?))
}

fn is_prerelease(s: &str) -> bool {
    s.contains('-')
}

fn satisfies(range: &str, ver: (u64, u64, u64), base: (u64, u64, u64)) -> bool {
    if range.starts_with('^') {
        // ^base: same major, version >= base.
        ver.0 == base.0 && ver >= base
    } else if range.starts_with('~') {
        // ~base: same major.minor, patch >= base patch.
        ver.0 == base.0 && ver.1 == base.1 && ver >= base
    } else if let Some(rest) = range.strip_prefix(">=") {
        parse_semver(rest.trim()).map(|b| ver >= b).unwrap_or(false)
    } else if let Some(rest) = range.strip_prefix('=') {
        rest.trim().is_empty() || parse_semver(rest.trim()) == Some(ver)
    } else {
        false
    }
}

/// Pick a concrete version from `versions` honoring `want`: `""`/`latest`/`*` -> latest dist-tag,
/// exact, `^`, `~`, `>=`. Prereleases are skipped unless they match exactly.
pub fn select_version(versions: &[String], latest: Option<&str>, want: &str) -> Option<String> {
    let want = want.trim();
    if want.is_empty() || want == "latest" || want == "*" || want == ">=0.0.0" {
        return latest.map(String::from).or_else(|| versions.last().cloned());
    }
    // Exact string match first (covers prereleases too).
    let normalized = want.strip_prefix('v').unwrap_or(want);
    for v in versions {
        if v == normalized || v == want {
            return Some(v.clone());
        }
    }
    // Strip range operators (^ ~ > < =) before parsing the comparator base.
    let base = parse_semver(want.trim_start_matches(['^', '~', '>', '<', '=', 'v', ' ']))?;
    versions
        .iter()
        .filter(|v| !is_prerelease(v))
        .filter_map(|v| parse_semver(v).map(|t| (t, v.clone())))
        .filter(|(t, _)| satisfies(want, *t, base))
        .max_by_key(|(t, _)| *t)
        .map(|(_, v)| v)
}

// ---------------------------------------------------------------------------------------------
// ESM entry resolution
// ---------------------------------------------------------------------------------------------

fn clean_rel(s: &str) -> String {
    s.trim().trim_start_matches("./").to_string()
}

fn exports_string(v: &Json) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(clean_rel(s));
    }
    if let Some(o) = v.as_object() {
        for key in ["import", "default", "browser"] {
            if let Some(sub) = o.get(key) {
                if let Some(s) = sub.as_str() {
                    return Some(clean_rel(s));
                }
                // Nested conditional object: pick a string leaf.
                if let Some(so) = sub.as_object() {
                    for k2 in ["import", "default", "require"] {
                        if let Some(s2) = so.get(k2).and_then(|x| x.as_str()) {
                            return Some(clean_rel(s2));
                        }
                    }
                    for val in so.values() {
                        if let Some(s2) = val.as_str() {
                            return Some(clean_rel(s2));
                        }
                    }
                }
            }
        }
    }
    None
}

/// Resolve the ESM entry point: `exports["."]` (`import`/`default`/`browser`/string), then
/// `module`, then `main`, then `index.js`.
pub fn resolve_esm_entry(pkg: &Json) -> String {
    if let Some(exp) = pkg.get("exports").and_then(|e| e.get(".")) {
        if let Some(s) = exports_string(exp) {
            if !s.is_empty() {
                return s;
            }
        }
    }
    for key in ["module", "main"] {
        if let Some(s) = pkg.get(key).and_then(|v| v.as_str()) {
            let s = clean_rel(s);
            if !s.is_empty() {
                return s;
            }
        }
    }
    "index.js".to_string()
}

// ---------------------------------------------------------------------------------------------
// install (BFS dep tree, <=20 packages, staged + esbuild bundled)
// ---------------------------------------------------------------------------------------------

fn split_spec(spec: &str) -> Result<(String, String), String> {
    // Scoped names start with '@': search from index 1.
    let (name, range) = match spec.rfind('@') {
        Some(idx) if idx > 0 => (spec[..idx].to_string(), spec[idx + 1..].to_string()),
        _ => (spec.to_string(), String::new()),
    };
    validate_pkg_name(&name)?;
    Ok((name, range))
}

/// Fetch the abbreviated install metadata JSON and select `range`'s version.
fn pick_from_meta(meta: &Json, want: &str) -> Result<(String, Json), String> {
    if let Some(versions_obj) = meta.get("versions").and_then(|v| v.as_object()) {
        let versions: Vec<String> = versions_obj.keys().cloned().collect();
        let latest = meta
            .get("dist-tags")
            .and_then(|d| d.get("latest"))
            .and_then(|v| v.as_str());
        let ver = select_version(&versions, latest, want)
            .ok_or(format!("no version matching {want:?} for package"))?;
        let vobj = versions_obj.get(&ver).cloned().unwrap_or_else(|| Json::Object(serde_json::Map::new()));
        return Ok((ver, vobj));
    }
    // Compact single-version registries (mirrors/fixtures publish `{name, version, tarball,
    // integrity}` with no `dist-tags`/`versions` map). Honor `want` through the SAME semver
    // selector, then synthesize the standard `{dist:{tarball,integrity}}` record so the rest of
    // the pipeline (SHA-512 SRI BEFORE extraction etc.) is untouched. A bare filename becomes the
    // canonical npm path `<registry>/<pkg>/-/<file>`; absolute or `/root` refs keep working via
    // `join_url` downstream.
    let ver = meta
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or("registry metadata missing versions".to_string())?;
    let picked = select_version(&[ver.to_string()], Some(ver), want)
        .ok_or(format!("no version matching {want:?} for package"))?;
    let mut dist = serde_json::Map::new();
    if let Some(t) = meta.get("tarball").and_then(|v| v.as_str()) {
        // Bare filenames get npm's canonical root-relative path `/<pkg>/-/<file>`; absolute or
        // already-rooted refs flow through `join_url` unchanged.
        let full = match meta.get("name").and_then(|v| v.as_str()) {
            Some(n) if !t.contains("://") && !t.starts_with('/') => {
                let mut p = String::from("/");
                p.push_str(&encode_pkg_spec(n));
                p.push_str("/-/");
                p.push_str(t);
                p
            }
            _ => t.to_string(),
        };
        dist.insert("tarball".to_string(), Json::String(full));
    }
    if let Some(i) = meta.get("integrity").and_then(|v| v.as_str()) {
        dist.insert("integrity".to_string(), Json::String(i.to_string()));
    }
    let mut vobj = meta.clone();
    vobj["dist"] = Json::Object(dist);
    Ok((picked, vobj))
}

fn dep_map(pkg: &Json) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    if let Some(o) = pkg.get("dependencies").and_then(|v| v.as_object()) {
        for (k, v) in o {
            validate_pkg_name(k)?;
            let range = v.as_str().unwrap_or("*").to_string();
            out.push((k.clone(), range));
        }
    }
    Ok(out)
}

struct RootInfo {
    name: String,
    version: String,
    integrity: String,
    entry_rel: String, // ESM entry relative to the staged root package dir
}

/// BFS-fetch a package + its transitive pure-JS deps into staging (root at `stage/package`,
/// deps at `stage/node_modules/<dep>`). Guards: <=20 pkgs, sha512 integrity BEFORE extraction,
/// zip-slip/native rejection.
fn fetch_dep_tree(client: &Agent, registry: &str, stage: &Path, specs: Vec<(String, String)>) -> Result<RootInfo, String> {
    // Queue items: (name, range, staged dest, is_root).
    let mut queue: VecDeque<(String, String, PathBuf, bool)> = VecDeque::new();
    for (n, r) in specs {
        validate_pkg_name(&n)?;
        queue.push_back((n, r, stage.join("package"), true));
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut count = 0usize;
    let mut root: Option<RootInfo> = None;
    while let Some((name, range, dest, is_root)) = queue.pop_front() {
        if seen.contains(&name) {
            continue; // dedupe same-name packages across the graph
        }
        seen.insert(name.clone());
        count += 1;
        if count > MAX_PACKAGES {
            return Err(format!("dependency graph exceeds {} packages", MAX_PACKAGES));
        }
        fs::create_dir_all(&dest).map_err(|e| format!("mkdir staging failed: {e}"))?;

        let meta_url = format!("{registry}/{}", encode_pkg_spec(&name));
        let meta_bytes = fetch_bytes(client, &meta_url, Some("application/vnd.npm.install-v1+json"))?;
        let meta: Json =
            serde_json::from_slice(&meta_bytes).map_err(|e| format!("invalid registry metadata for {name}: {e}"))?;
        let (ver, vobj) = pick_from_meta(&meta, &range)?;
        // Native addon guard can fire straight from registry metadata too.
        if let Some(msg) = native_addon_reject(&vobj) {
            return Err(format!("{name}@{ver}: {msg}"));
        }
        let tarball = vobj
            .get("dist")
            .and_then(|d| d.get("tarball"))
            .and_then(|t| t.as_str())
            .ok_or(format!("{name}@{ver}: metadata missing dist.tarball"))?;
        let tarball_url = join_url(&format!("{registry}/"), tarball)?;
        let tb = fetch_bytes(client, &tarball_url, None)?;
        let sri = vobj
            .get("dist")
            .and_then(|d| d.get("integrity"))
            .and_then(|i| i.as_str())
            .unwrap_or("");
        // Verify sha512 integrity BEFORE extracting anything — and hash the tarball EXACTLY ONCE:
        // the same digest both checks the declared SRI and supplies the recorded integrity string
        // (no second SHA-512 pass over up-to-5 MiB of bytes).
        let b64 = match sri.strip_prefix("sha512-") {
            Some(rest) => rest.trim(),
            None => return Err(format!("unsupported/missing integrity algorithm: {sri:?}")),
        };
        let expected = BASE64
            .decode(b64)
            .map_err(|e| format!("bad base64 in integrity {sri:?}: {e}"))?;
        let got = sha512_digest(&tb);
        if got.as_slice() != expected.as_slice() {
            return Err(format!("{name}@{ver}: sha512 integrity MISMATCH (refusing to extract)"));
        }
        let pkg_json = extract_tarball(&tb, &dest)?;
        if let Some(msg) = native_addon_reject(&pkg_json) {
            return Err(format!("{name}@{ver}: {msg}"));
        }
        let integrity = sri_from_digest(&got);
        for (dn, dr) in dep_map(&pkg_json)? {
            let dep_dest = stage.join("node_modules").join(&dn);
            queue.push_back((dn, dr, dep_dest, false));
        }
        if is_root {
            root = Some(RootInfo {
                name: name.clone(),
                version: ver.clone(),
                integrity: integrity.clone(),
                entry_rel: resolve_esm_entry(&pkg_json),
            });
        }
    }
    root.ok_or("root package was not fetched".to_string())
}

const ESBUILD_BUNDLE_ARGS: &[&str] = &[
    "--bundle",
    "--format=esm",
    "--platform=neutral",
    "--main-fields=module,main",
    "--conditions=import,module,default",
    "--external:node:*",
    "--external:cardputer:*",
];

/// Run `esbuild` to bundle into a single ESM file.
fn run_esbuild(esbuild: &str, cwd: &Path, entry: &str, outfile: &Path, extra: &[&str], minify: bool, capture: bool) -> Result<(), String> {
    let mut cmd = StdCommand::new(esbuild);
    cmd.current_dir(cwd).arg(entry).args(extra);
    if minify {
        cmd.arg("--minify");
    }
    cmd.arg(format!("--outfile={}", outfile.display()));
    if capture {
        let out = cmd.output().map_err(|e| format!("failed to spawn esbuild ({esbuild}): {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "esbuild failed with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(())
    } else {
        let st = cmd.status().map_err(|e| format!("failed to spawn esbuild ({esbuild}): {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("esbuild exited with {st}"))
        }
    }
}

/// Structured final package manifest written to `node_modules/<pkg>/package.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct FinalManifest {
    name: String,
    version: String,
    #[serde(rename = "type")]
    pkg_type: String,
    main: String,
    exports: serde_json::Map<String, Json>,
}

/// One entry in `./zpm-lock.json` `packages`.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LockEntry {
    version: String,
    integrity: String,
    bundled_bytes: u64,
}

impl LockEntry {
    fn to_value(&self) -> Result<Json, String> {
        serde_json::to_value(self).map_err(|e| format!("serialize lock entry: {e}"))
    }
}

fn update_root_package_json(cwd: &Path, name: &str, version: &str) -> Result<(), String> {
    let path = cwd.join("package.json");
    let mut doc: Json = match fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("invalid ./package.json: {e}"))?,
        Err(_) => Json::Object(serde_json::Map::new()),
    };
    if !doc.is_object() {
        doc = Json::Object(serde_json::Map::new());
    }
    let obj = doc.as_object_mut().unwrap();
    let deps = obj
        .entry("dependencies")
        .or_insert_with(|| Json::Object(serde_json::Map::new()));
    if !deps.is_object() {
        *deps = Json::Object(serde_json::Map::new());
    }
    deps.as_object_mut()
        .unwrap()
        .insert(name.to_string(), Json::String(format!("^{version}")));
    let text = serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize package.json: {e}"))?;
    fs::write(&path, text + "\n").map_err(|e| format!("write package.json: {e}"))
}

fn update_lockfile(cwd: &Path, name: &str, version: &str, integrity: &str, bundled_bytes: u64) -> Result<(), String> {
    let path = cwd.join("zpm-lock.json");
    let mut doc: Json = match fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|_| Json::Object(serde_json::Map::new())),
        Err(_) => Json::Object(serde_json::Map::new()),
    };
    if !doc.is_object() {
        doc = Json::Object(serde_json::Map::new());
    }
    let obj = doc.as_object_mut().unwrap();
    obj.insert("lockfileVersion".to_string(), serde_json::json!(1));
    let pkgs = obj
        .entry("packages")
        .or_insert_with(|| Json::Object(serde_json::Map::new()));
    if !pkgs.is_object() {
        *pkgs = Json::Object(serde_json::Map::new());
    }
    pkgs.as_object_mut().unwrap().insert(
        name.to_string(),
        LockEntry {
            version: version.to_string(),
            integrity: integrity.to_string(),
            bundled_bytes,
        }
        .to_value()?,
    );
    let text = serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize zpm-lock.json: {e}"))?;
    fs::write(&path, text + "\n").map_err(|e| format!("write zpm-lock.json: {e}"))
}

fn cmd_install(args: &[String]) -> u8 {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut minify = true;
    let mut specs: Vec<String> = Vec::new();
    for a in args {
        if a == "--no-minify" {
            minify = false;
        } else if a.starts_with("--") {
            eprintln!("zpm install: ignoring unknown flag {a}");
        } else {
            specs.push(a.clone());
        }
    }
    // No positional packages -> install dependencies from ./package.json.
    if specs.is_empty() {
        let pj = cwd.join("package.json");
        let doc: Json = match fs::read(&pj) {
            Ok(b) => match serde_json::from_slice(&b) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("zpm: invalid ./package.json: {e}");
                    return 1;
                }
            },
            Err(_) => {
                eprintln!("zpm: no packages given and no ./package.json to install from");
                return 1;
            }
        };
        let deps = match dep_map(&doc) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("zpm: {e}");
                return 1;
            }
        };
        for (n, r) in deps {
            specs.push(if r.is_empty() || r == "*" { n } else { format!("{n}@{r}") });
        }
        if specs.is_empty() {
            println!("zpm: nothing to install (no dependencies in ./package.json)");
            return 0;
        }
    }

    let mut parsed_specs: Vec<(String, String, String)> = Vec::with_capacity(specs.len());
    for spec in &specs {
        match split_spec(spec) {
            Ok((name, range)) => parsed_specs.push((spec.clone(), name, range)),
            Err(e) => {
                eprintln!("zpm install {spec}: {e}");
                return 1;
            }
        }
    }

    let registry = registry_base();
    let esbuild = esbuild_bin();
    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zpm install: {e}");
            return 1;
        }
    };

    for (spec, name, range) in parsed_specs {
        // Fetch + bundle ENTIRELY in staging (all guards included) BEFORE any project write: a
        // failure here leaves the previously installed package and lockfile completely intact, and
        // the RAII TempDir keeps ZPM_TMPDIR clean.
        let staged = match stage_package_bundle(&client, &registry, &esbuild, &name, &range, minify) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("zpm install {spec}: {e}");
                return 1;
            }
        };
        // Atomic commit into ./node_modules/<pkg>/ (index.mjs + package.json only): temp file ->
        // rename, then refresh manifest, ./package.json range (^version) and zpm-lock.json.
        let n = match finalize_installed_pkg(&cwd, &staged.root, &staged.staged_bundle, true) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("zpm install {spec}: {e}");
                return 1;
            }
        };
        println!(
            "zpm: installed {name}@{} -> node_modules/{name}/index.mjs ({n} bytes)",
            staged.root.version
        );
    }
    0
}

// ---------------------------------------------------------------------------------------------
// Staging + atomic commit pipeline (shared by install and update)
// ---------------------------------------------------------------------------------------------

/// A fully fetched+bundled but UNCOMMITTED package: everything lives inside the RAII `tmp`
/// staging dir, so dropping this guard (on success or failure) always leaves ZPM_TMPDIR clean.
struct StagedBundle {
    root: RootInfo,
    staged_bundle: PathBuf,
    /// RAII guard: intentionally never read — held alive so `Drop` wipes the staging dir from
    /// `ZPM_TMPDIR` once the atomic commit (or any failure path) finishes.
    #[allow(dead_code)]
    tmp: TempDir,
}

/// Run the full safe in-memory pipeline (5 MiB cap, SHA-512 SRI verified BEFORE extraction,
/// zip-slip, native-addon and <=20-deps guards) into a fresh `<ZPM_TMPDIR>/zpm-*` staging dir,
/// then esbuild-bundle it. NOTHING is written to the project dir; on any error the RAII guard
/// wipes staging, so a failed install/update can never corrupt the current install.
fn stage_package_bundle(
    client: &Agent,
    registry: &str,
    esbuild: &str,
    name: &str,
    range: &str,
    minify: bool,
) -> Result<StagedBundle, String> {
    validate_pkg_name(name)?;
    let stage = TempDir::new("zpm")?;
    let root = fetch_dep_tree(client, registry, stage.path(), vec![(name.to_string(), range.to_string())])?;
    // Ensure `entry_rel` cannot traverse outside `<stage>/package` when passed to esbuild.
    let pkg_root = stage.path().join("package");
    let safe_entry = validate_safe_rel_path(&root.entry_rel)?;
    let entry_full = pkg_root.join(&safe_entry);
    if let Ok(canon_root) = fs::canonicalize(&pkg_root) {
        let mut probe = entry_full.as_path();
        while !probe.exists() {
            match probe.parent() {
                Some(p) => probe = p,
                None => break,
            }
        }
        let canon_probe = fs::canonicalize(probe)
            .map_err(|e| format!("invalid package entry {:?}: {e}", root.entry_rel))?;
        if !canon_probe.starts_with(&canon_root) {
            return Err(format!("package entry {:?} escapes package root", root.entry_rel));
        }
    }
    let entry_slash = safe_entry
        .iter()
        .map(|s| s.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    // Bundle from the staging tree: root entry lives at <stage>/package/<entry>, deps are
    // resolved by esbuild via <stage>/node_modules (never written to the project dir).
    let out_rel = PathBuf::from("node_modules").join(name).join("index.mjs");
    let entry_arg = format!("package/{entry_slash}");
    run_esbuild(esbuild, stage.path(), &entry_arg, &out_rel, ESBUILD_BUNDLE_ARGS, minify, true)?;
    let staged_bundle = stage.path().join(&out_rel);
    if !staged_bundle.is_file() {
        return Err("esbuild produced no bundle".to_string());
    }
    Ok(StagedBundle {
        root,
        staged_bundle,
        tmp: stage,
    })
}

/// Commit a fully-staged bundle into `./node_modules/<pkg>/`: ATOMIC `index.mjs` replace (copy
/// to a temp file in the SAME directory -> rename; tmp removed on failure), then write the
/// final package.json manifest, optionally rewrite the `./package.json` dependency range to
/// `^<version>` (`record_range`) and refresh `zpm-lock.json`. Returns bundled byte size.
fn finalize_installed_pkg(cwd: &Path, root: &RootInfo, staged_bundle: &Path, record_range: bool) -> Result<u64, String> {
    validate_pkg_name(&root.name)?;
    let final_dir = cwd.join("node_modules").join(root.name.as_str());
    fs::create_dir_all(&final_dir).map_err(|e| format!("mkdir failed: {e}"))?;
    let final_out = final_dir.join("index.mjs");
    let tmp_out = final_dir.join(format!("index.mjs.tmp-{}", std::process::id()));
    let n = fs::copy(staged_bundle, &tmp_out).map_err(|e| format!("copy bundle failed: {e}"))?;
    if let Err(e) = fs::rename(&tmp_out, &final_out) {
        let _ = fs::remove_file(&tmp_out);
        return Err(format!("atomic replace of index.mjs failed: {e}"));
    }
    let mut exports = serde_json::Map::new();
    exports.insert(".".to_string(), Json::String("./index.mjs".to_string()));
    let manifest = FinalManifest {
        name: root.name.clone(),
        version: root.version.clone(),
        pkg_type: "module".to_string(),
        main: "./index.mjs".to_string(),
        exports,
    };
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| format!("serialize manifest: {e}"))?;
    fs::write(final_dir.join("package.json"), text + "\n").map_err(|e| format!("write manifest: {e}"))?;
    if record_range {
        update_root_package_json(cwd, &root.name, &root.version)?;
    }
    update_lockfile(cwd, &root.name, &root.version, &root.integrity, n)?;
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// update / outdated / remove / list (Phase 7C)
// ---------------------------------------------------------------------------------------------

/// Fetch registry metadata for one package (`Accept: application/vnd.npm.install-v1+json`).
fn fetch_metadata(client: &Agent, registry: &str, name: &str) -> Result<Json, String> {
    let url = format!("{registry}/{}", encode_pkg_spec(name));
    let bytes = fetch_bytes(client, &url, Some("application/vnd.npm.install-v1+json"))?;
    serde_json::from_slice::<Json>(&bytes).map_err(|e| format!("invalid registry metadata for {name}: {e}"))
}

/// Parse JSON bytes tolerantly into an object default (used for lockfiles/optional manifests).
fn parse_json_default(bytes: &[u8]) -> Json {
    serde_json::from_slice(bytes).unwrap_or_else(|_| Json::Object(serde_json::Map::new()))
}

/// Currently installed version of `name`: `zpm-lock.json` entry first, then
/// `node_modules/<name>/package.json`, else `None` (not installed).
fn installed_version(cwd: &Path, name: &str) -> Option<String> {
    if let Ok(b) = fs::read(cwd.join("zpm-lock.json")) {
        if let Some(s) = parse_json_default(&b)
            .get("packages")
            .and_then(|p| p.get(name))
            .and_then(|e| e.get("version"))
            .and_then(|v| v.as_str())
        {
            return Some(s.to_string());
        }
    }
    if let Ok(b) = fs::read(cwd.join("node_modules").join(name).join("package.json")) {
        if let Some(s) = parse_json_default(&b).get("version").and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

/// Sorted keys of an optional JSON object (deterministic CLI output order).
fn object_keys_sorted(obj_opt: Option<&Json>) -> Vec<String> {
    let mut keys: Vec<String> = match obj_opt.and_then(|o| o.as_object()) {
        Some(o) => o.keys().cloned().collect(),
        None => Vec::new(),
    };
    keys.sort();
    keys
}

fn object_has_key(obj_opt: Option<&Json>, key: &str) -> bool {
    obj_opt.and_then(|o| o.as_object()).map(|o| o.contains_key(key)).unwrap_or(false)
}

/// Resolve an update target version from fetched metadata: with `--latest` take
/// `dist-tags.latest`; otherwise the highest version satisfying the package.json range.
fn resolve_update_target(meta: &Json, want: &str, want_latest: bool) -> Result<String, String> {
    if want_latest {
        return meta
            .get("dist-tags")
            .and_then(|d| d.get("latest"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or("registry metadata missing dist-tags.latest".to_string());
    }
    pick_from_meta(meta, want).map(|(v, _)| v)
}

/// A package is outdated iff `current != wanted || current != latest`.
pub fn is_outdated(current: &str, wanted: &str, latest: &str) -> bool {
    current != wanted || current != latest
}

/// `{ name, current, wanted, latest }` outdated record (all string fields).
pub fn outdated_record(name: &str, current: &str, wanted: &str, latest: &str) -> Json {
    serde_json::json!({ "name": name, "current": current, "wanted": wanted, "latest": latest })
}

/// Drop `key` from object `section` of a JSON file (pretty rewrite + trailing newline).
/// `Ok(false)` when the file/section/key is absent; `Err` on an unreadable-corrupt file.
fn drop_json_key(path: &Path, section: &str, key: &str) -> Result<bool, String> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return Ok(false),
    };
    let mut doc: Json = serde_json::from_slice(&bytes).map_err(|e| format!("invalid {}: {e}", path.display()))?;
    let hit = match doc.get_mut(section).and_then(|s| s.as_object_mut()) {
        Some(o) => o.remove(key).is_some(),
        None => false,
    };
    if !hit {
        return Ok(false);
    }
    let text = serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize {}: {e}", path.display()))?;
    fs::write(path, text + "\n").map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(true)
}

/// Installed `(name, version, bundled_bytes)` entries: `zpm-lock.json` first; falls back to a
/// `node_modules/*/package.json` scan (`index.mjs` byte size). Sorted by package name.
fn list_entries(cwd: &Path) -> Vec<(String, String, u64)> {
    let mut out: Vec<(String, String, u64)> = Vec::new();
    if let Ok(b) = fs::read(cwd.join("zpm-lock.json")) {
        let doc = parse_json_default(&b);
        if let Some(pkgs) = doc.get("packages").and_then(|p| p.as_object()) {
            for name in object_keys_sorted(doc.get("packages")) {
                let e = pkgs.get(&name).cloned().unwrap_or(Json::Null);
                let version = e.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let bytes = e
                    .get("bundledBytes")
                    .and_then(|v| v.as_u64())
                    .or_else(|| fs::metadata(cwd.join("node_modules").join(&name).join("index.mjs")).map(|m| m.len()).ok())
                    .unwrap_or(0);
                out.push((name, version, bytes));
            }
        }
    }
    if !out.is_empty() {
        return out;
    }
    // Fallback scan when the lockfile is missing/empty.
    let nm = cwd.join("node_modules");
    let rd = match fs::read_dir(&nm) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for ent in rd.flatten() {
        let dir = ent.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest = match fs::read(dir.join("package.json")) {
            Ok(b) => parse_json_default(&b),
            Err(_) => continue,
        };
        let name = match manifest.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let version = match manifest.get("version").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let bytes = fs::metadata(dir.join("index.mjs")).map(|m| m.len()).unwrap_or(0);
        out.push((name, version, bytes));
    }
    out.sort();
    out
}

/// `zpm update [pkg ...] [--latest] [--dry-run] [--no-minify]` (aliases `up`, `upgrade`).
/// Default: highest version satisfying the `package.json` semver range (`^`, `~`, exact,
/// `*`/`latest`). `--latest`: `dist-tags.latest` + rewrite `dependencies[pkg]` to
/// `^<newVersion>`. Up-to-date packages are skipped with ZERO tarball downloads and ZERO
/// writes; `--dry-run` prints plans with ZERO filesystem writes (metadata GETs only).
/// Commits run the full safe staging pipeline and ATOMIC rename replacement, so a failed
/// update keeps the previous `index.mjs`, manifest, `zpm-lock.json` and ZPM_TMPDIR intact.
fn cmd_update(args: &[String]) -> u8 {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut minify = true;
    let mut want_latest = false;
    let mut dry_run = false;
    let mut names: Vec<String> = Vec::new();
    for a in args {
        match a.as_str() {
            "--no-minify" => minify = false,
            "--latest" => want_latest = true,
            "--dry-run" => dry_run = true,
            s if s.starts_with("--") => eprintln!("zpm update: ignoring unknown flag {s}"),
            s => names.push(s.to_string()),
        }
    }
    let pkg_json = cwd.join("package.json");
    let doc: Json = match fs::read(&pkg_json) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("zpm: invalid ./package.json: {e}");
                return 1;
            }
        },
        Err(_) => Json::Object(serde_json::Map::new()),
    };
    let lock = parse_json_default(&fs::read(cwd.join("zpm-lock.json")).unwrap_or_default());
    let targets: Vec<String> = if names.is_empty() {
        let t = object_keys_sorted(doc.get("dependencies"));
        if t.is_empty() {
            println!("zpm: nothing to update (no dependencies in ./package.json)");
            return 0;
        }
        t
    } else {
        // Requested packages MUST be listed in package.json dependencies OR zpm-lock.json.
        for n in &names {
            if let Err(e) = validate_pkg_name(n) {
                eprintln!("zpm update {n}: {e}");
                return 1;
            }
            if !object_has_key(doc.get("dependencies"), n) && !object_has_key(lock.get("packages"), n) {
                eprintln!("zpm update: {n} is not listed in ./package.json or zpm-lock.json");
                eprintln!("usage: zpm update [pkg ...] [--latest] [--dry-run] [--no-minify]");
                return 1;
            }
        }
        names
    };
    for name in &targets {
        if let Err(e) = validate_pkg_name(name) {
            eprintln!("zpm update {name}: {e}");
            return 1;
        }
    }

    let registry = registry_base();
    let esbuild = esbuild_bin();
    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zpm update: {e}");
            return 1;
        }
    };
    let mut code = 0u8;
    for name in targets {
        let want = match doc
            .get("dependencies")
            .and_then(|d| d.as_object())
            .and_then(|o| o.get(&name))
            .and_then(|v| v.as_str())
        {
            Some(r) => r.to_string(),
            None => "*".to_string(),
        };
        // Metadata-only resolution first (no tarball yet) enables the zero-download no-op.
        let meta = match fetch_metadata(&client, &registry, &name) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("zpm update {name}: {e}");
                code = 1;
                continue;
            }
        };
        let target = match resolve_update_target(&meta, &want, want_latest) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("zpm update {name}: {e}");
                code = 1;
                continue;
            }
        };
        let current = installed_version(&cwd, &name);
        if current.as_deref() == Some(target.as_str()) {
            println!("zpm: {name}@{target} is already up to date");
            continue; // ZERO downloads, ZERO writes
        }
        if dry_run {
            let old = current.unwrap_or_else(|| "not installed".to_string());
            println!("zpm: would update {name} {old} -> {target}");
            continue; // no staging dir, no tarball, no filesystem writes
        }
        let old = current.unwrap_or_else(|| "not installed".to_string());
        // Fetch the EXACT resolved version through the safe pipeline; failure keeps everything.
        let staged = match stage_package_bundle(&client, &registry, &esbuild, &name, &target, minify) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("zpm update {name}: {e}");
                code = 1;
                continue;
            }
        };
        // --latest also rewrites package.json dependencies[pkg] to ^<newVersion>; a plain
        // semver update keeps the user's existing range untouched.
        match finalize_installed_pkg(&cwd, &staged.root, &staged.staged_bundle, want_latest) {
            Ok(n) => println!("zpm: updated {name} {old} -> {target} -> node_modules/{name}/index.mjs ({n} bytes)"),
            Err(e) => {
                eprintln!("zpm update {name}: {e}");
                code = 1;
            }
        }
    }
    code
}

/// `zpm outdated [--json]`: inspect every `package.json` dependency against the registry and
/// report `{ name, current, wanted, latest }` for packages where `current != wanted ||
/// current != latest`. With `--json` stdout is PURE valid JSON (diagnostics go to stderr);
/// otherwise a clean aligned human table. Always exit 0 unless a registry fetch failed.
fn cmd_outdated(args: &[String]) -> u8 {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut json_mode = false;
    for a in args {
        if a == "--json" {
            json_mode = true;
        } else if a.starts_with("--") {
            eprintln!("zpm outdated: ignoring unknown flag {a}");
        } else {
            eprintln!("zpm outdated: ignoring unexpected argument {a}");
        }
    }
    let doc: Json = match fs::read(cwd.join("package.json")) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("zpm: invalid ./package.json: {e}");
                return 1;
            }
        },
        Err(_) => Json::Object(serde_json::Map::new()),
    };
    let names = object_keys_sorted(doc.get("dependencies"));
    for name in &names {
        if let Err(e) = validate_pkg_name(name) {
            eprintln!("zpm outdated {name}: {e}");
            return 1;
        }
    }
    let registry = registry_base();
    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zpm outdated: {e}");
            return 1;
        }
    };
    let mut records: Vec<Json> = Vec::new();
    let mut code = 0u8;
    for name in names {
        let want = match doc
            .get("dependencies")
            .and_then(|d| d.as_object())
            .and_then(|o| o.get(&name))
            .and_then(|v| v.as_str())
        {
            Some(r) => r.to_string(),
            None => "*".to_string(),
        };
        let meta = match fetch_metadata(&client, &registry, &name) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("zpm outdated {name}: {e}");
                code = 1;
                continue;
            }
        };
        let latest = meta
            .get("dist-tags")
            .and_then(|d| d.get("latest"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let wanted = resolve_update_target(&meta, &want, false).unwrap_or_default();
        let current = installed_version(&cwd, &name).unwrap_or_default();
        if is_outdated(&current, &wanted, &latest) {
            records.push(outdated_record(&name, &current, &wanted, &latest));
        }
    }
    if json_mode {
        match serde_json::to_string_pretty(&records) {
            Ok(t) => println!("{t}"),
            Err(e) => {
                eprintln!("zpm outdated: serialize: {e}");
                code = 1;
            }
        }
    } else if records.is_empty() {
        println!("zpm: all packages up to date");
    } else {
        let head = ["name", "current", "wanted", "latest"];
        let rows: Vec<[String; 4]> = records
            .iter()
            .map(|r| {
                [
                    r["name"].as_str().unwrap_or("").to_string(),
                    r["current"].as_str().unwrap_or("").to_string(),
                    r["wanted"].as_str().unwrap_or("").to_string(),
                    r["latest"].as_str().unwrap_or("").to_string(),
                ]
            })
            .collect();
        let widths: Vec<usize> = (0..4)
            .map(|i| head[i].len().max(rows.iter().map(|r| r[i].len()).max().unwrap_or(0)))
            .collect();
        println!("{}", head.iter().enumerate().map(|(i, h)| pad(h, widths[i])).collect::<Vec<_>>().join("  "));
        for r in &rows {
            println!(
                "{}",
                r.iter().enumerate().map(|(i, c)| pad(c, widths[i])).collect::<Vec<_>>().join("  ")
            );
        }
    }
    code
}

/// Left-align `s` into width `w` (clean table output without unstable `pad_end`).
fn pad(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(s.len())))
}

/// `zpm remove <pkg ...>` (aliases `rm`, `uninstall`, `un`): recursively delete
/// `node_modules/<pkg>` and drop the package from `./package.json` `dependencies` and
/// `zpm-lock.json` `packages`. Non-zero when no package args are given or a named package is
/// not installed/listed.
fn cmd_remove(args: &[String]) -> u8 {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut names: Vec<String> = Vec::new();
    for a in args {
        if a.starts_with("--") {
            eprintln!("zpm remove: ignoring unknown flag {a}");
        } else {
            names.push(a.clone());
        }
    }
    if names.is_empty() {
        eprintln!("usage: zpm remove <pkg> ...");
        return 1;
    }
    for name in &names {
        if let Err(e) = validate_pkg_name(name) {
            eprintln!("zpm remove {name}: {e}");
            return 1;
        }
    }
    let pkg_json = cwd.join("package.json");
    let doc: Json = match fs::read(&pkg_json) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("zpm: invalid ./package.json: {e}");
                return 1;
            }
        },
        Err(_) => Json::Object(serde_json::Map::new()),
    };
    let lock_path = cwd.join("zpm-lock.json");
    let lock = parse_json_default(&fs::read(&lock_path).unwrap_or_default());
    let mut code = 0u8;
    for name in &names {
        let dir = cwd.join("node_modules").join(name.as_str());
        let listed = dir.is_dir() || object_has_key(doc.get("dependencies"), name) || object_has_key(lock.get("packages"), name);
        if !listed {
            eprintln!("zpm remove: {name} is not installed or listed");
            code = 1;
            continue;
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("zpm remove {name}: delete failed: {e}");
                code = 1;
                continue;
            }
        }
        if let Err(e) = drop_json_key(&pkg_json, "dependencies", name) {
            eprintln!("zpm remove {name}: {e}");
            code = 1;
        }
        if let Err(e) = drop_json_key(&lock_path, "packages", name) {
            eprintln!("zpm remove {name}: {e}");
            code = 1;
        }
        println!("zpm: removed {name}");
    }
    code
}

/// `zpm list` (alias `ls`): print installed packages from `zpm-lock.json` (falling back to a
/// `node_modules/*/package.json` scan) as `name@version` plus bundled byte size. Always exit 0.
fn cmd_list(args: &[String]) -> u8 {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for a in args {
        if a.starts_with("--") {
            eprintln!("zpm list: ignoring unknown flag {a}");
        } else {
            eprintln!("zpm list: ignoring unexpected argument {a}");
        }
    }
    let entries = list_entries(&cwd);
    if entries.is_empty() {
        println!("zpm: no packages installed");
        return 0;
    }
    let labels: Vec<String> = entries.iter().map(|(n, v, _)| format!("{n}@{v}")).collect();
    let w = labels.iter().map(|l| l.len()).max().unwrap_or(0);
    for (label, (_, _, bytes)) in labels.iter().zip(&entries) {
        println!("{}   {} bytes", pad(label, w), bytes);
    }
    0
}

/// Rewrite a leading `node `/`node\t` invocation to `<runtime> ` (pure helper, unit-tested).
pub fn rewrite_node_prefix(cmd: &str, runtime: &str) -> String {
    let t = cmd.trim_start();
    if t.starts_with("node ") || t.starts_with("node\t") {
        format!("{runtime}{}", &t[4..])
    } else {
        cmd.to_string()
    }
}

/// `zpm run <script>`: look up `scripts[script]`, rewrite a leading `node ` to `<ZPM_RUNTIME> `,
/// execute via `sh -c` inheriting stdio.
fn cmd_run(args: &[String]) -> u8 {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let script = match args.first() {
        Some(s) => s,
        None => {
            eprintln!("usage: zpm run <script>");
            return 1;
        }
    };
    let pj = cwd.join("package.json");
    let doc: Json = match fs::read(&pj) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("zpm run: invalid ./package.json: {e}");
                return 1;
            }
        },
        Err(_) => {
            eprintln!("zpm run: no ./package.json in {}", cwd.display());
            return 1;
        }
    };
    let cmd_str = doc
        .get("scripts")
        .and_then(|s| s.get(script.as_str()))
        .and_then(|v| v.as_str())
        .map(|c| rewrite_node_prefix(c, &runtime_bin()));
    let cmd_str = match cmd_str {
        Some(c) => c,
        None => {
            eprintln!("zpm run: script {script:?} not found in ./package.json");
            return 1;
        }
    };
    match StdCommand::new("sh").arg("-c").arg(cmd_str).current_dir(&cwd).status() {
        Ok(st) => st.code().map(|c| c as u8).unwrap_or(1),
        Err(e) => {
            eprintln!("zpm run: failed to spawn sh: {e}");
            1
        }
    }
}

/// `zpm bundle <entry> -o <out.mjs> [--no-minify]`
fn cmd_bundle(args: &[String]) -> u8 {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut minify = true;
    let mut entry: Option<String> = None;
    let mut out: Option<String> = None;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--no-minify" => minify = false,
            "-o" | "--outfile" => out = iter.next().cloned(),
            s if s.starts_with("--outfile=") => out = Some(s.split_once('=').map(|(_, v)| v.to_string()).unwrap_or_default()),
            s if s.starts_with("--") => eprintln!("zpm bundle: ignoring unknown flag {s}"),
            s if entry.is_none() => entry = Some(s.to_string()),
            _ => {}
        }
    }
    let (entry, out) = match (entry, out) {
        (Some(e), Some(o)) => (e, o),
        _ => {
            eprintln!("usage: zpm bundle <entry> -o <out.mjs> [--no-minify]");
            return 1;
        }
    };
    let extra: Vec<&str> = vec![
        "--bundle",
        "--format=esm",
        "--platform=neutral",
        "--external:node:*",
        "--external:cardputer:*",
    ];
    if let Err(e) = run_esbuild(&esbuild_bin(), &cwd, &entry, &PathBuf::from(out), &extra, minify, false) {
        eprintln!("zpm bundle: {e}");
        return 1;
    }
    0
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::{Builder, Header};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn bounded_io_timeout_knob_validation_and_defaults() {
        // Harness suite runs with --test-threads=1 (env mutations below assume serialization).
        // Default bounds must build.
        std::env::remove_var("ZPM_TIMEOUT_MS");
        assert!(build_client().is_ok());
        // Tunable within range...
        std::env::set_var("ZPM_TIMEOUT_MS", "2500");
        assert!(build_client().is_ok());
        std::env::set_var("ZPM_TIMEOUT_MS", "60000");
        assert!(build_client().is_ok());
        // ...and reject garbage loudly (never silently ignore).
        std::env::set_var("ZPM_TIMEOUT_MS", "0");
        assert_eq!(build_client().unwrap_err(), "bad ZPM_TIMEOUT_MS (want 1..60000)");
        std::env::set_var("ZPM_TIMEOUT_MS", "70000");
        assert!(build_client().is_err());
        std::env::set_var("ZPM_TIMEOUT_MS", "abc");
        assert!(build_client().is_err());
        std::env::remove_var("ZPM_TIMEOUT_MS");
    }

    #[test]
    fn multicall_zpm_basename_dispatch() {
        assert_eq!(parse_multicall_args(&s(&["zpm", "install", "hono"])), Some(s(&["install", "hono"])));
    }

    #[test]
    fn multicall_npm_basename_dispatch() {
        assert_eq!(
            parse_multicall_args(&s(&["/usr/local/bin/npm", "--version"])),
            Some(s(&["--version"]))
        );
    }

    #[test]
    fn multicall_subcommand_dispatch() {
        // The unified `cz-js` binary AND the standalone `cz-zpm` both accept the `zpm` subcommand.
        assert_eq!(parse_multicall_args(&s(&["cz-js", "zpm", "run", "dev"])), Some(s(&["run", "dev"])));
        assert_eq!(parse_multicall_args(&s(&["cz-zpm", "zpm", "--version"])), Some(s(&["--version"])));
    }

    #[test]
    fn multicall_passthrough_for_js_runtime() {
        assert_eq!(parse_multicall_args(&s(&["cz-js", "app.mjs"])), None);
        // `node <script>` must fall through to the JS runtime, never to zpm.
        assert_eq!(parse_multicall_args(&s(&["node", "app.mjs"])), None);
        assert_eq!(parse_multicall_args(&s(&["node", "-e", "1+1"])), None);
        assert_eq!(parse_multicall_args(&[]), None);
    }

    #[test]
    fn version_line_has_all_identity_tokens() {
        let line = version_line();
        for tok in ["zpm", "native", "shim", "esbuild", "cardputer:*"] {
            assert!(line.contains(tok), "version line missing token {tok}: {line}");
        }
    }

    #[test]
    fn zipslip_paths_rejected_and_safe_paths_stripped() {
        assert!(sanitize_tar_path("package/../../evil.txt").is_err());
        assert!(sanitize_tar_path("../../secret").is_err());
        assert!(sanitize_tar_path("/etc/passwd").is_err());
        assert!(sanitize_tar_path("a/b/../c").is_err());
        let ok = sanitize_tar_path("package/src/main.js").unwrap();
        assert_eq!(ok, PathBuf::from("src/main.js"));
        let ok2 = sanitize_tar_path("./pkg/./index.js").unwrap();
        assert_eq!(ok2, PathBuf::from("pkg/index.js"));
    }

    fn gzip_tar(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut ar = Builder::new(&mut enc);
            for (name, data) in entries {
                let mut h = Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_mtime(0);
                h.set_entry_type(EntryType::file());
                ar.append_data(&mut h, name.as_str(), Cursor::new(data)).unwrap();
            }
            ar.finish().unwrap();
        }
        enc.finish().unwrap()
    }

    /// Hand-craft a raw ustar entry (the `tar` Builder itself refuses `..` paths, so malicious
    /// fixtures must be built byte-by-byte like the verifier does).
    fn raw_ustar(name: &str, content: &str) -> Vec<u8> {
        let data = content.as_bytes();
        let mut hdr = [0u8; 512];
        hdr[..name.len().min(100)].copy_from_slice(name.as_bytes());
        hdr[100..108].copy_from_slice(b"0000644\0");
        hdr[108..116].copy_from_slice(b"0000000\0");
        hdr[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", data.len());
        hdr[124..136].copy_from_slice(size.as_bytes());
        let mtime = format!("{:011o}\0", 0);
        hdr[136..148].copy_from_slice(mtime.as_bytes());
        hdr[148..156].fill(b' ');
        hdr[156] = b'0';
        hdr[257..263].copy_from_slice(b"ustar\0");
        hdr[263..265].copy_from_slice(b"00");
        let sum: u32 = hdr.iter().map(|b| *b as u32).sum();
        let chk = format!("{:06o}\0 ", sum);
        hdr[148..156].copy_from_slice(chk.as_bytes());
        let pad = (512 - data.len() % 512) % 512;
        let mut out = hdr.to_vec();
        out.extend_from_slice(data);
        out.extend(std::iter::repeat(0u8).take(pad));
        out
    }

    fn gzip_raw(tar_bytes: Vec<u8>) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.write_all(&[0u8; 1024]).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn extract_rejects_zip_slip_tarball() {
        let tar_bytes = [
            raw_ustar("package/package.json", "{\"name\":\"bad\",\"version\":\"1.0.0\"}"),
            raw_ustar("package/../../evil.txt", "pwned\n"),
        ]
        .concat();
        let tgz = gzip_raw(tar_bytes);
        let dir = TempDir::new("zpm-test").unwrap();
        assert!(extract_tarball(&tgz, dir.path()).is_err());
        // nothing escaped the staging dir
        assert!(!dir.path().join("evil.txt").exists());
        assert!(!dir.path().join("package").join("../../evil.txt").exists());
    }

    #[test]
    fn extract_rejects_symlink_entries() {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut ar = Builder::new(&mut enc);
            let data = b"{\"name\":\"s\",\"version\":\"1.0.0\"}".to_vec();
            let mut h = Header::new_gnu();
            h.set_size(data.len() as u64);
            ar.append_data(&mut h, "package/package.json", Cursor::new(data)).unwrap();
            let mut lh = Header::new_gnu();
            lh.set_size(0);
            lh.set_entry_type(EntryType::Symlink);
            ar.append_link(&mut lh, "package/escape", "package/package.json").unwrap();
            ar.finish().unwrap();
        }
        let tgz = enc.finish().unwrap();
        let dir = TempDir::new("zpm-test").unwrap();
        assert!(extract_tarball(&tgz, dir.path()).is_err());
    }

    #[test]
    fn extract_rejects_native_artifacts_and_gypfile() {
        let tgz_gyp = gzip_tar(vec![
            ("package/package.json".into(), b"{\"name\":\"bad-native\",\"version\":\"1.0.0\",\"gypfile\":true}".to_vec()),
            ("package/binding.gyp".into(), b"{}\n".to_vec()),
        ]);
        let dir = TempDir::new("zpm-test").unwrap();
        assert!(extract_tarball(&tgz_gyp, dir.path()).is_err());

        let tgz_node = gzip_tar(vec![
            ("package/package.json".into(), b"{\"name\":\"x\",\"version\":\"1.0.0\"}".to_vec()),
            ("package/build/x.node".into(), b"\x7fELF".to_vec()),
        ]);
        let dir2 = TempDir::new("zpm-test").unwrap();
        assert!(extract_tarball(&tgz_node, dir2.path()).is_err());

        let meta: Json = serde_json::json!({"scripts": {"install": "node-gyp rebuild"}});
        assert!(native_addon_reject(&meta).is_some());
        let meta2: Json = serde_json::json!({"scripts": {"preinstall": "prebuild-install || source ./env"}});
        assert!(native_addon_reject(&meta2).is_some());
        let okmeta: Json = serde_json::json!({"scripts": {"build": "echo hi"}});
        assert!(native_addon_reject(&okmeta).is_none());
    }

    #[test]
    fn extract_clean_package_roundtrip() {
        let tgz = gzip_tar(vec![
            ("package/package.json".into(), b"{\"name\":\"mini\",\"version\":\"1.0.0\",\"main\":\"index.js\"}".to_vec()),
            ("package/index.js".into(), b"export const x=1;\n".to_vec()),
        ]);
        let dir = TempDir::new("zpm-test").unwrap();
        let pj = extract_tarball(&tgz, dir.path()).unwrap();
        assert_eq!(pj.get("name").and_then(|v| v.as_str()), Some("mini"));
        assert!(dir.path().join("index.js").is_file());
    }

    #[test]
    fn integrity_verify_match_mismatch_and_format() {
        let data = b"tarball-bytes".to_vec();
        let sri = sri_from_digest(&sha512_digest(&data));
        assert!(verify_integrity(&data, &sri).unwrap());
        assert!(!verify_integrity(b"other", &sri).unwrap());
        // base64 sanity: decoded digest must be 64 bytes (SHA-512)
        let b64 = sri.strip_prefix("sha512-").unwrap();
        assert_eq!(BASE64.decode(b64).unwrap().len(), 64);
        assert!(verify_integrity(&data, "md5-abc=").is_err());
        assert!(verify_integrity(&data, "").is_err());
    }

    #[test]
    fn semver_selection_rules() {
        let vers = s(&["0.9.0", "1.0.0", "1.2.3", "1.5.9", "2.0.0", "1.3.0-beta.1"]);
        assert_eq!(select_version(&vers, Some("2.0.0"), "").unwrap(), "2.0.0");
        assert_eq!(select_version(&vers, Some("2.0.0"), "latest").unwrap(), "2.0.0");
        assert_eq!(select_version(&vers, None, "^1.0.0").unwrap(), "1.5.9");
        assert_eq!(select_version(&vers, None, "~1.2.0").unwrap(), "1.2.3");
        assert_eq!(select_version(&vers, None, "1.0.0").unwrap(), "1.0.0");
        assert_eq!(select_version(&vers, None, "^3.0.0"), None);
        // exact prerelease is allowed; caret ranges skip it
        assert_eq!(select_version(&vers, None, "1.3.0-beta.1").unwrap(), "1.3.0-beta.1");
        assert_eq!(parse_semver("v2.1.0+build").unwrap(), (2, 1, 0));
    }

    #[test]
    fn compact_single_version_metadata_falls_back_to_canonical_npm_path() {
        // Phase 14 fixture shape: `{name, version, tarball, integrity}` WITHOUT `dist-tags`/
        // `versions`. The fallback synthesizes the standard `dist` record AND maps the bare
        // filename onto npm's canonical `<pkg>/-/<file>` path so `join_url` resolves it.
        let flat: Json = serde_json::json!({
            "name": "demo",
            "version": "2.3.4",
            "tarball": "demo-2.3.4.tgz",
            "integrity": "sha512-AABB==",
        });
        let (ver, vobj) = pick_from_meta(&flat, "latest").unwrap();
        assert_eq!(ver, "2.3.4");
        assert_eq!(
            vobj.get("dist").and_then(|d| d.get("tarball")).and_then(|t| t.as_str()),
            Some("/demo/-/demo-2.3.4.tgz")
        );
        assert_eq!(
            vobj.get("dist").and_then(|d| d.get("integrity")).and_then(|i| i.as_str()),
            Some("sha512-AABB==")
        );
        // caret/exact ranges still route through the SAME semver selector:
        assert_eq!(pick_from_meta(&flat, "^2.0.0").unwrap().0, "2.3.4");
        assert_eq!(pick_from_meta(&flat, "2.3.4").unwrap().0, "2.3.4");
        assert!(pick_from_meta(&flat, "^3.0.0").is_err());
        // Absolute tarball URLs stay untouched; a name-less flat record keeps its relative path.
        let abs: Json = serde_json::json!({"version": "1.0.0", "tarball": "https://x.test/y.tgz"});
        assert_eq!(
            pick_from_meta(&abs, "").unwrap().1.get("dist").and_then(|d| d.get("tarball")).and_then(|t| t.as_str()),
            Some("https://x.test/y.tgz")
        );
        let nn: Json = serde_json::json!({"version": "0.1.0", "tarball": "other.tgz"});
        assert_eq!(
            pick_from_meta(&nn, "").unwrap().1.get("dist").and_then(|d| d.get("tarball")).and_then(|t| t.as_str()),
            Some("other.tgz")
        );
        // Truly malformed metadata (no `versions` AND no `version`) keeps the classic error.
        let bare: Json = serde_json::json!({"name": "x"});
        assert_eq!(pick_from_meta(&bare, "latest").unwrap_err(), "registry metadata missing versions");
    }

    #[test]
    fn esm_entry_resolution_order() {
        let hono_like: Json = serde_json::json!({
            "exports": { ".": {"types": "./dist/types/index.d.ts", "import": "./dist/index.js", "require": "./dist/cjs/index.js"} },
            "main": "dist/cjs/index.js",
        });
        assert_eq!(resolve_esm_entry(&hono_like), "dist/index.js");
        let str_export: Json = serde_json::json!({"exports": {".": "./lib/mod.mjs"}});
        assert_eq!(resolve_esm_entry(&str_export), "lib/mod.mjs");
        let default_only: Json = serde_json::json!({"exports": {".": {"browser": "./b.js", "default": "./d.js"}}});
        assert_eq!(resolve_esm_entry(&default_only), "d.js"); // lookup order: import, default, browser
        let module_main: Json = serde_json::json!({"module": "esm/main.js", "main": "cjs/main.js"});
        assert_eq!(resolve_esm_entry(&module_main), "esm/main.js");
        let bare: Json = serde_json::json!({});
        assert_eq!(resolve_esm_entry(&bare), "index.js");
    }

    #[test]
    fn raii_tempdir_always_cleaned() {
        let p = {
            let t = TempDir::new("zpm").unwrap();
            fs::write(t.path().join("marker.txt"), b"x").unwrap();
            assert!(t.path().is_dir());
            t.path().to_path_buf()
        }; // Drop fires here
        assert!(!p.exists(), "RAII TempDir must clean up on drop");
    }

    #[test]
    fn limit_constants_match_spec() {
        assert_eq!(MAX_PACKAGES, 20);
        assert_eq!(TARBALL_CAP, 5 * 1024 * 1024);
    }

    #[test]
    fn node_prefix_rewrite() {
        assert_eq!(
            rewrite_node_prefix("node -e \"console.log(1)\"", "cz-llrt"),
            "cz-llrt -e \"console.log(1)\""
        );
        assert_eq!(rewrite_node_prefix("echo hi", "cz-llrt"), "echo hi");
    }

    #[test]
    fn url_helpers_encode_and_join() {
        assert_eq!(encode_pkg_spec("@scope/pkg"), "@scope/pkg");
        assert_eq!(encode_pkg_spec("has space"), "has%20space");
        assert_eq!(join_url("http://127.0.0.1:8080/x", "/tgz/a.tgz").unwrap(), "http://127.0.0.1:8080/tgz/a.tgz");
        assert_eq!(join_url("http://127.0.0.1:8080/a/b", "c.tgz").unwrap(), "http://127.0.0.1:8080/a/c.tgz");
        assert_eq!(join_url("http://x/y", "https://other/z").unwrap(), "https://other/z");
    }

    #[test]
    fn cli_shape_no_network() {
        // run_cli on unknown command returns a non-zero code without touching network.
        let code = run_cli(&s(&["definitely-not-a-command"]));
        assert_ne!(code, 0);
        assert_eq!(run_cli(&s(&["--version"])), 0);
    }

    #[test]
    fn install_rejects_unreachable_registry_cleanly() {
        // Connection to a closed port must fail (exit != 0) and never panic.
        let saved = std::env::var("ZPM_REGISTRY").ok();
        std::env::set_var("ZPM_REGISTRY", "http://127.0.0.1:1");
        let code = run_cli(&s(&["install", "definitely-missing-pkg"]));
        match saved {
            Some(v) => std::env::set_var("ZPM_REGISTRY", v),
            None => std::env::remove_var("ZPM_REGISTRY"),
        }
        assert_ne!(code, 0);
    }

    // ---------------------------------------------------------------- Phase 7C helpers ---

    fn demo_meta() -> Json {
        serde_json::json!({
            "name": "demo-lib",
            "dist-tags": { "latest": "2.0.0" },
            "versions": {
                "1.0.0": { "name": "demo-lib", "version": "1.0.0" },
                "1.2.0": { "name": "demo-lib", "version": "1.2.0" },
                "2.0.0": { "name": "demo-lib", "version": "2.0.0" }
            }
        })
    }

    #[test]
    fn update_target_resolution_semver_vs_latest() {
        let meta = demo_meta();
        assert_eq!(resolve_update_target(&meta, "^1.0.0", false).unwrap(), "1.2.0");
        assert_eq!(resolve_update_target(&meta, "~1.2.0", false).unwrap(), "1.2.0");
        assert_eq!(resolve_update_target(&meta, "1.0.0", false).unwrap(), "1.0.0");
        assert_eq!(resolve_update_target(&meta, "*", false).unwrap(), "2.0.0");
        // --latest ignores the range and takes dist-tags.latest
        assert_eq!(resolve_update_target(&meta, "^1.0.0", true).unwrap(), "2.0.0");
        let bare: Json = serde_json::json!({ "versions": {} });
        assert!(resolve_update_target(&bare, "^1.0.0", true).is_err());
        assert!(resolve_update_target(&bare, "^9.0.0", false).is_err());
    }

    #[test]
    fn outdated_predicates_and_record_shape() {
        assert!(is_outdated("1.0.0", "1.2.0", "2.0.0"));
        assert!(is_outdated("1.2.0", "1.2.0", "2.0.0"));
        assert!(!is_outdated("2.0.0", "2.0.0", "2.0.0"));
        let rec = outdated_record("demo-lib", "1.0.0", "1.2.0", "2.0.0");
        for k in ["name", "current", "wanted", "latest"] {
            assert!(rec.get(k).and_then(|v| v.as_str()).is_some(), "record missing {k}");
        }
        assert_eq!(rec["wanted"].as_str().unwrap(), "1.2.0");
    }

    #[test]
    fn installed_version_prefers_lockfile_then_manifest() {
        let dir = TempDir::new("zpm-lv").unwrap();
        let nm = dir.path().join("node_modules").join("pkg");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("package.json"), "{\"name\":\"pkg\",\"version\":\"1.1.0\"}").unwrap();
        assert_eq!(installed_version(dir.path(), "pkg").unwrap(), "1.1.0");
        fs::write(dir.path().join("zpm-lock.json"), "{\"packages\":{\"pkg\":{\"version\":\"1.2.0\"}}}").unwrap();
        assert_eq!(installed_version(dir.path(), "pkg").unwrap(), "1.2.0");
        assert!(installed_version(dir.path(), "missing").is_none());
    }

    #[test]
    fn finalize_replaces_atomically_and_refreshes_lock_and_manifest() {
        let cwd = TempDir::new("zpm-fin").unwrap();
        let src_dir = TempDir::new("zpm-src").unwrap();
        let src = src_dir.path().join("index.mjs");
        fs::write(&src, "export const VER = \"1.0.0\";\n").unwrap();
        let root = RootInfo {
            name: "demo-lib".to_string(),
            version: "1.0.0".to_string(),
            integrity: "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==".to_string(),
            entry_rel: "index.js".to_string(),
        };
        let n = finalize_installed_pkg(cwd.path(), &root, &src, false).unwrap();
        assert!(n > 0);
        let pkg_dir = cwd.path().join("node_modules").join("demo-lib");
        let files: Vec<String> = fs::read_dir(&pkg_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 2, "only index.mjs + package.json, got {files:?}");
        assert_eq!(fs::read(pkg_dir.join("index.mjs")).unwrap(), b"export const VER = \"1.0.0\";\n".to_vec());
        // No tmp leftovers anywhere (ZPM_TMPDIR semantics):
        assert!(!pkg_dir.join(format!("index.mjs.tmp-{}", std::process::id())).exists());
        // Second commit ATOMICALLY replaces the bundle and bumps version/lock/range.
        fs::write(&src, "export const VER = \"1.2.0\";\n").unwrap();
        let root2 = RootInfo {
            name: "demo-lib".to_string(),
            version: "1.2.0".to_string(),
            integrity: root.integrity.clone(),
            entry_rel: "index.js".to_string(),
        };
        let n2 = finalize_installed_pkg(cwd.path(), &root2, &src, true).unwrap();
        assert!(n2 > 0);
        assert_eq!(fs::read(pkg_dir.join("index.mjs")).unwrap(), b"export const VER = \"1.2.0\";\n".to_vec());
        let lock: Json = serde_json::from_slice(&fs::read(cwd.path().join("zpm-lock.json")).unwrap()).unwrap();
        assert_eq!(lock["packages"]["demo-lib"]["version"].as_str().unwrap(), "1.2.0");
        assert_eq!(lock["packages"]["demo-lib"]["bundledBytes"].as_u64().unwrap(), n2);
        let pj: Json = serde_json::from_slice(&fs::read(cwd.path().join("package.json")).unwrap()).unwrap();
        assert_eq!(pj["dependencies"]["demo-lib"].as_str().unwrap(), "^1.2.0");
        let man: Json = serde_json::from_slice(&fs::read(pkg_dir.join("package.json")).unwrap()).unwrap();
        assert_eq!(man["version"].as_str().unwrap(), "1.2.0");
        assert_eq!(man["exports"]["."].as_str().unwrap(), "./index.mjs");
    }

    #[test]
    fn drop_json_key_removes_and_reports_missing() {
        let dir = TempDir::new("zpm-rm").unwrap();
        let pj = dir.path().join("package.json");
        fs::write(&pj, "{\"dependencies\":{\"a\":\"^1.0.0\",\"b\":\"^2.0.0\"}}").unwrap();
        let lock = dir.path().join("zpm-lock.json");
        fs::write(&lock, "{\"packages\":{\"a\":{\"version\":\"1.0.0\"}}}").unwrap();
        assert!(drop_json_key(&pj, "dependencies", "a").unwrap());
        assert!(!drop_json_key(&pj, "dependencies", "a").unwrap()); // second time: absent
        let doc: Json = serde_json::from_slice(&fs::read(&pj).unwrap()).unwrap();
        assert!(doc["dependencies"].get("a").is_none());
        assert!(doc["dependencies"].get("b").is_some()); // untouched sibling kept
        assert!(drop_json_key(&lock, "packages", "a").unwrap());
        assert!(!drop_json_key(&dir.path().join("nope.json"), "packages", "x").unwrap());
        // corrupt file must error, not silently pass
        fs::write(&pj, "not json").unwrap();
        assert!(drop_json_key(&pj, "dependencies", "b").is_err());
    }

    #[test]
    fn list_entries_lock_then_fallback_scan() {
        let dir = TempDir::new("zpm-ls").unwrap();
        let nm = dir.path().join("node_modules").join("demo-lib");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("index.mjs"), "export const VER = \"1.2.0\";\n").unwrap();
        fs::write(nm.join("package.json"), "{\"name\":\"demo-lib\",\"version\":\"1.2.0\"}").unwrap();
        // lock path: version + bundledBytes win
        fs::write(dir.path().join("zpm-lock.json"), "{\"packages\":{\"demo-lib\":{\"version\":\"1.2.0\",\"bundledBytes\":42}}}").unwrap();
        let e = list_entries(dir.path());
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].0, "demo-lib");
        assert_eq!(e[0].1, "1.2.0");
        assert_eq!(e[0].2, 42);
        // fallback scan after lock removal reports manifest version + index.mjs size
        fs::remove_file(dir.path().join("zpm-lock.json")).unwrap();
        let e2 = list_entries(dir.path());
        assert_eq!(e2.len(), 1);
        assert_eq!(e2[0].1, "1.2.0");
        assert_eq!(e2[0].2, fs::metadata(nm.join("index.mjs")).unwrap().len());
        // empty project -> empty list
        let empty = TempDir::new("zpm-ls0").unwrap();
        assert!(list_entries(empty.path()).is_empty());
    }

    #[test]
    fn pad_aligns_table_columns() {
        assert_eq!(pad("abc", 5), "abc  ");
        assert_eq!(pad("abcdef", 3), "abcdef");
    }

    #[test]
    fn remove_needs_pkg_and_list_exits_zero() {
        // `remove` without package args must exit non-zero; `list` always exits 0.
        assert_ne!(run_cli(&s(&["remove"])), 0);
        assert_eq!(run_cli(&s(&["ls"])), 0);
    }

    #[test]
    fn help_flags_succeed_for_root_and_all_subcommands() {
        assert_eq!(run_cli(&[]), 0);
        assert_eq!(run_cli(&s(&["--help"])), 0);
        assert_eq!(run_cli(&s(&["-h"])), 0);
        assert_eq!(run_cli(&s(&["help"])), 0);
        assert_eq!(run_cli(&s(&["help", "install"])), 0);
        for cmd in ["install", "i", "add", "update", "up", "outdated", "remove", "rm", "list", "ls", "run", "bundle"] {
            assert_eq!(run_cli(&s(&[cmd, "--help"])), 0, "failed --help for {cmd}");
            assert_eq!(run_cli(&s(&[cmd, "-h"])), 0, "failed -h for {cmd}");
            assert!(subcommand_help(cmd).is_some(), "missing help text for {cmd}");
        }
        assert!(help_text(None).contains("ZPM_REGISTRY"));
        assert!(!has_help_flag(&s(&["myscript", "--", "--help"])));
    }

    #[test]
    fn package_name_validation_rejects_traversal_and_accepts_npm_names() {
        assert!(validate_pkg_name("hono").is_ok());
        assert!(validate_pkg_name("mini-calc.js").is_ok());
        assert!(validate_pkg_name("@scope/pkg-1_a").is_ok());

        for bad in [
            "",
            ".hidden",
            "_priv",
            "..",
            "../evil",
            "../../evil",
            "foo/bar",
            "@scope/../../evil",
            "@scope/a/b",
            "@scope/",
            "@/pkg",
            "@.bad/pkg",
            "pkg\\evil",
            "pkg?x=1",
            "pkg#frag",
            "BadUpper",
        ] {
            assert!(validate_pkg_name(bad).is_err(), "must reject bad package name {bad:?}");
            assert!(split_spec(&format!("{bad}@^1.0.0")).is_err(), "split_spec must reject {bad:?}");
        }
        assert_eq!(split_spec("@scope/pkg@^1.2.3").unwrap(), ("@scope/pkg".into(), "^1.2.3".into()));
        assert_eq!(split_spec("@scope/pkg").unwrap(), ("@scope/pkg".into(), String::new()));

        let bad_deps: Json = serde_json::json!({"dependencies": {"../../evil": "*"}});
        assert!(dep_map(&bad_deps).is_err());

        // `cmd_remove` must refuse traversal args without deleting anything.
        let sandbox = TempDir::new("zpm-rm-guard").unwrap();
        let victim = sandbox.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        let prev = env::current_dir().unwrap();
        env::set_current_dir(sandbox.path()).unwrap();
        let code = run_cli(&s(&["remove", "../../victim"]));
        env::set_current_dir(prev).unwrap();
        assert_ne!(code, 0);
        assert!(victim.is_dir(), "path traversal remove must not delete victim");
    }

    #[test]
    fn entry_rel_containment_rejects_traversal() {
        assert!(validate_safe_rel_path("../../evil.mjs").is_err());
        assert!(validate_safe_rel_path("dist/../../evil.mjs").is_err());
        assert!(validate_safe_rel_path("/etc/passwd").is_err());
        assert!(validate_safe_rel_path("dist\\evil.mjs").is_err());
        assert!(validate_safe_rel_path("").is_err());
        assert!(validate_safe_rel_path("./").is_err());
        assert_eq!(validate_safe_rel_path("./dist/index.mjs").unwrap(), PathBuf::from("dist/index.mjs"));
    }

    #[test]
    fn extract_rejects_gzip_bomb_over_cumulative_32_mib_cap() {
        assert_eq!(EXTRACTED_CAP, 32 * 1024 * 1024);
        // Seven 5 MiB entries = 35 MiB uncompressed (> 32 MiB EXTRACTED_CAP), yet < 100 KiB gzipped.
        let five_mib = vec![0u8; 5 * 1024 * 1024];
        let mut entries = vec![(
            "package/package.json".to_string(),
            b"{\"name\":\"bomb\",\"version\":\"1.0.0\"}".to_vec(),
        )];
        for i in 0..7 {
            entries.push((format!("package/blob-{i}.bin"), five_mib.clone()));
        }
        let tgz = gzip_tar(entries);
        assert!(tgz.len() < TARBALL_CAP, "compressed fixture must stay under TARBALL_CAP");
        let dir = TempDir::new("zpm-bomb").unwrap();
        let err = extract_tarball(&tgz, dir.path()).unwrap_err();
        assert!(err.contains("32 MiB"), "expected cumulative cap error, got: {err}");
    }

    #[test]
    fn redirect_downgrade_from_https_to_http_is_rejected() {
        let err = check_redirect_target("https://registry.npmjs.org/hono", "http://evil.test/hono.tgz").unwrap_err();
        assert!(err.contains("HTTPS->HTTP"), "got: {err}");
        assert_eq!(
            check_redirect_target("https://registry.npmjs.org/a/b", "/c/d").unwrap(),
            "https://registry.npmjs.org/c/d"
        );
        assert_eq!(
            check_redirect_target("https://registry.npmjs.org/a", "https://cdn.example/b").unwrap(),
            "https://cdn.example/b"
        );
        assert_eq!(
            check_redirect_target("http://127.0.0.1:8080/a", "/b").unwrap(),
            "http://127.0.0.1:8080/b"
        );
    }
}
