# Dream archive (full evidence behind dream.md kernels)

### [Principle] D1 — QuickJS TLA settlement: pump jobs BETWEEN scopes
Never Promise::finish() (job spin; worker-I/O await deadlock); execute_pending_job inside Context::with panics RefCell. Evaluate WITHOUT finish; JS glue sets globalThis settle flags; pump I/O+jobs ONLY BETWEEN scopes; idle-grace catches deadlock. No Rust-wired .then (detached this): pass promise.into_value() to JS glue. Uncaught errors print name: message first line.

### [Dead End] D2 = D20 — File-edit & sandbox hygiene
Read regions verbatim before big edit oldText; prove non-write via git status after anomaly before retry; anchor uniqueness via node -e split-count (python3 absent). Markdown heading+body pair positionally: bare-body anchors orphan inserted prose -> insert at EOF or anchor full pair, verify awk H/body adjacency scan. Heredoc terminator ALONE on its own line; {:<w0} widths invalid -> local pad(); git -c safe.directory=/workspace per call; od -c replaces xxd. Assert intended transitions not incidental equality.

### [Principle] D3 — Stage-then-commit beats in-place mutation
RAII staging before any project write; commit via same-dir temp + fs::rename; corrupt hashes leave manifests byte-intact; --dry-run GETs only; verify-before-extract + size caps; installs reuse ONE staging pipeline.

### [Principle] D4 — Liveness registries: release ALL terminal paths; close servers first
Counts decrement at EVERY terminal point (header-only release leaked active=1 with correct stdout plus hang); counts include listeners so close servers FIRST then poll to zero; no instant has_pending() after async start; stop-flag before join; hang -> inspect live-id/listener registry.

### [Principle] D5 = D11 (+D21+D23) — Harness discipline: reproduce standalone, verifier trust, convergence
Self-written multi-case drivers sharing ports/state produce phantom failures that PASS standalone; panicked tests leak process-global registries -> cascading false fails: per-test queue-drain hygiene; diagnose cargo test --exact NAME + instrumented /tmp probe. NEVER edit .pi/verifier.json (SHA-256 locked vs /refs); stale verify_metrics ctx -> run gate commands verbatim in bash, sum weights manually. After TARGET MET + porcelain=0 use cheap git-status/HEAD confirms; rerun suite ONLY on state change. Memory ships via separate chore(memory) commits so DONE hashes stay intact; fabricated checkpoint lessons poison recall -> append measured evidence only.

### [Insight] D6 = D19 — rquickjs API + zero-copy buffer idioms
Value no Display -> Coerced::<String>::from_js; rquickjs::Result shadows std; globals().set(...) needs OUTER ?; declare() deref &&str; throw Exception::throw_message or Error::Io(io::Error::other(..)); catch CatchResultExt.catch(&ctx). Zero-copy JS buffers: TypedArray::as_slice()/ArrayBuffer::as_bytes(), no temp Vec, fill new_copy; Canvas::as_le_bytes() raw_parts cfg(LE)-gated; to_le_bytes() delegates w/ BE fallback. fb mock: parent dir EXISTS -> lazy create+sized FB_BYTES else NotFound.

### [Principle] D7 = D8 + D9 — TLS/rustls ground truth
Prove rejection by EFFECTS: mTLS alert may arrive after complete_io Ok; rejected handshake emits NO Data event. webpki self-signed-leaf-as-ca CaUsedAsEndEntity, PartialEq always false -> match Debug; accept ONLY byte-matching anchors. Root OnceLock per-process: CA-env tests need SEPARATE subprocesses or load_system_roots_uncached, serial mode. ServerName static: IP->IpAddress no-SNI, DNS try_from(String), empty INVALID. TLSSocket end(): Shutdown::Write only, keep reading; autoEnd -> Ok(0) -> End+Close both. destroy(): latch before shutdown(Both); stored try_clone wake-slot exits ms-level.

### [Insight] D10 — ureq/hyper/dyn-dispatch drift facts
ureq v3 owned Transport over Arc<Mutex<TcpStream>>; abort ~10ms via cloned Arc+shutdown(Both); max_idle_connections=0; manual redirects=max_redirects(0). Box<dyn Trait> rebind needs delegating impl (E0277); hand Read/Write on Box hits E0119 blanket conflict; inherent methods shadow traits (Trait::method(&x)). hyper1 Empty<Bytes>/BodyExt::frame(); tar rejects '..'; semver strip '^'.

### [Insight] D12 — aws-emulator protocol + SigV4 ground truth
Routing ONLY from SigV4 scope/X-Amz-Target/Host (unsigned POST '/' -> AmbiguousService). SQS JSON proto ALWAYS POST root '/'; path-style -> QueueDoesNotExist; QueueUrl verbatim inside every body, transport stays AWS_ENDPOINT_URL. DDB JSON1.0 PAY_PER_REQUEST, poll ACTIVE; ConditionalCheckFailed 400 __type never retried. S3 path-style incl binary Uint8Array. Signatures NOT enforced but canonical headers MUST be LOWERCASE keys: mixed-case -> silent messageless SerializationException. Errors JSON __type; BucketNotEmpty 409 XML.

### [Dead End] D13 = D17 — cz-js realm gaps + KvStore semantics
No global URL/TextDecoder in cz-js realms (kills SigV4) -> ship pure-JS splitUrl()/utf8Decode() shared ESM running on cz-js AND Node. Absolute ESM specifiers fail from temp dirs -> CzResolver /-prefix disk branch (+.mjs/.js tries). kv whole-object set() stores but get() NULL -> persist via setField/getField primitives, probe-proven not theory. KvStore: earliest_expiry_ms CONSERVATIVE LOWER bound, never upward; ttl_count EXACT +1/-1 with post-purge resync; LIVE-key touch counts HIT even if answer absent; purge_expired runs INSIDE get/has/keys/stats so assert counts only direct-call or post-read; recount_total_bytes oracle proves byte deltas exact over 200 mutations.

### [Insight] D14 = D22 — Oracle discipline + ring/TTF fixture facts
Generate expected vectors/rasters from Node oracles BEFORE embedding (it caught wrong hand base64); alpha-sum ink probes prove coverage before pixel-identity tests. ring 0.17.14: SHA256/SHA512, only SHA1 legacy; digest()->Digest .as_ref(); hmac::sign(&Key,data) NOT tag(); Key::new(Algorithm value); SecureRandom trait for fill; deprecated constant-time eq -> allow(deprecated) at ONE site. Opt<Value> sees Some(undefined-value) on explicit undefined -> guard is_undefined()||is_null(). Minimal TTF: glyf endPtsOfContours BEFORE instruction length else BLANK rasters; cmap fmt4 idDelta map chars to INTENDED glyf index.

### [Dead End] D15 — cz-json tape zero-span scalars break passthrough
KIND_BOOL/KIND_NULL pushed span (0,0) -> raw slice EMPTY -> serialize scalar passthrough emits NOTHING -> invalid JSON; container spans byte-exact so whole-doc looks fine until parent dirtied. Fix: render scalars from tagged tape.scalar(), strings copy raw escaped span. Method: A/B HEAD-vs-patched binary on SAME script isolates pre-existing bugs; temp kind/span probe then DELETE.

### [Principle] D16 — Lazy CoW transparency + non-reentrant edits-mutex recursion
Materialize ONLY while WHOLE doc is_clean(), else Proxy handles: dirty doc plus materialized GRANDchildren silently detaches nested writes. is_clean() fast-path BEFORE locking; under-guard refs not Vec clones; ONE last-wins view per dirty node; DROP MutexGuard BEFORE recursing since child subtree re-lock of the same std mutex deadlocks. Last-wins dedupe serializes key exactly ONCE; array views compare parsed usize not string allocs.

### [Principle] D18 — Font glyph cache: hits and misses blend identically
Persistent swash ScaleContext + bounded cache keyed (glyph_id,f32 size bits), FIFO cap 256 -> zero-alloc repeat draw_text ONLY when cached and fresh paths share ONE blend_alpha routine; separate compositing drifts pixels and fails pixel-exact E2E. Assert hit-vs-fresh BYTE equality not visual. Deref &mut *guard once per string; re-locking same Mutex panics; one guard per STRING not per glyph.

### [Principle] D24 — rquickjs Vec->Uint8Array ownership move beats new_copy
ArrayBuffer::new takes the Vec allocation pointer directly (ManuallyDrop + JS_NewArrayBuffer realloc/free callback bound to that buffer's capacity) so payload NEVER memcpy'd a second time like TypedArray::<u8>::new_copy (JS_NewArrayBufferCopy); wrapping via TypedArray::from_arraybuffer only creates the view. An EMPTY Vec has no allocation -> JS_NewArrayBuffer would get null ptr -> separate new_copy(&[]) trivial branch. Prove JS-owns-the-buffer: mutate through JS view then native re-read via with_borrowed_bytes (ffi-move test). JsString.to_cstring() borrow -> as_str().as_bytes().to_vec() THEN move into helper; Opt<Value> strict flag reads as_ref().and_then(as_bool()).unwrap_or(false).

### [Insight] D25 — Native codec inside borrowed slice + QuickJS view gotchas
Building a JS string INSIDE with_borrowed_bytes is legal: allocation != JS execution and the JS argument stays rooted on the stack, so valid UTF-8 needs NO intermediate Rust String copy (strict from_utf8 -> JsString::from_str directly; lossy U+FFFD fallback). QuickJS `new Uint8Array(dataView)` treats a DataView ARRAY-LIKE -> yields empty view; byteSource must resolve input.buffer for DataView/typed views with byteOffset+byteLength kept relative. Lossy U+FFFD replacement counts differ from hand char loops -> measure expected lengths with node Buffer.byteLength oracle before asserting multibyte totals (never guess).

### [Principle] D26 — Combined loop-drain scope + rustls config memoization
One ctx.with per event-loop pass may chain fetch->tls->httpCloses->http->timers ONLY if jobs are pumped AFTER the scope exits (RefCell rule); the follow-up rescan MUST SKIP timers or an interval fires twice in one iteration. Runner test helpers that execute module glue must bind the same runner callbacks (else native callbacks silently never run). Cache the rustls default ClientConfig in a OnceLock ONLY for default options; CA-env tests build uncached configs (build_client_config_uncached) and compare via Arc::ptr_eq under the shared ca_env_lock. Streaming body Content-Length hint takes &HeaderMap (unit-testable without building a Body); pre-size ONLY when buffer empty, else reserve_exact(remaining.saturating_sub(seen)).

### [Insight] D27 — Span-backed head: retract the fold separator first
Extending a previous header value span in-place for obs-fold continuation MUST truncate the just-emitted `\r\n` field separator BEFORE appending `" " + trim`, else the grown span swallows CRLF and borrowed values drift byte-for-byte (`Some("one\r\n folded")` vs legacy `"one folded"` — caught instantly by the legacy join fixture as exact oracle). ends_with(b"\r\n") guard makes repeated folds idempotent; restore a missing separator when the NEXT non-continuation field begins so `raw` stays a cleanly normalized head. Raw layout only needs span correctness, but tidy separators keep from_utf8(&head.raw) spot assertions meaningful.

### [Dead End] D28 — Structural greps scan raw text; comments trip banned-literal asserts
/refs lightweight harnesses assert structural contracts with regexes over RAW file text, so explaining an API removal in a comment using the banned literal itself (e.g. `HashMap<String, Vec<String>>`) FAILS the check even when code is fully clean. Rephrase without the literal pattern ("no name->values map / no value vectors"). grep -n on the offending file pinpoints the comment line immediately; reword, don't re-add complexity to dodge the regex.

### [Principle] D29 — Linear-cursor + vectored-TX test/wire rules
Linear input cursor (flat Vec<u8> + read_pos, compact ONLY on full drain or when append cannot fit capacity): assert parser cursor state ONLY from literal fixture byte lengths — Vec growth capacity is unspecified, so capacity-heuristic assertions are nondeterministic (use explicit buf = Vec::with_capacity(32) in compaction-rule tests); mid-request FULL consumption resets cursor+buffer to (0,0) via consume(), making stale NeedMore-state asserts lie. Zero-body-copy TX: never pass a leading zero-length IoSlice element into write_vectored, retry via ONE global byte offset mapped onto (head,body) slices (works for both writev coalescing and std's default sequential per-buffer fallback on TLS sessions), and flush ONCE at the end => wire bytes identical to combined encode()+write_all_eof_free. Obs-fold span growth keeps dup-join Cow::Owned only for genuine duplicates; single values borrow Cow::Borrowed even across folds after separator retraction.
