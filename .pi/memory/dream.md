### [Note] Archive
D-detail lives in dream-archive.md (same labels).

### [Principle] D1 — QuickJS TLA settlement: pump jobs BETWEEN scopes
Never Promise::finish() (job spin deadlocks worker I/O; Context::with re-entry RefCell panic). Evaluate WITHOUT finish; JS glue sets globalThis settle flags; pump I/O+jobs ONLY between scopes. Pass promise.into_value() to JS glue; NEVER .then wired in Rust..

### [Dead End] D2 = D20 — File-edit & sandbox hygiene
Read verbatim before edit oldText; repair via sed re-emission. git -c safe.directory=/workspace per call; python3 absent -> node -e split-count anchors; heredoc terminator ALONE..; prove non-write via git status after anomalies before retry; heredoc terminator ALONE

### [Principle] D3 — Stage-then-commit beats in-place mutation
RAII staging BEFORE project writes; commit = same-dir temp + fs::rename; corrupt hashes keep manifests byte-intact; verify-before-extract, size caps; ONE staging pipeline reused..

### [Principle] D4 — Liveness registries: release ALL terminal paths; close servers first
Counts decrement at EVERY terminal point (header-only release leaked active=1, correct stdout + hang); counts INCLUDE listeners -> close servers FIRST then poll zero; stop-flag before join; hang -> inspect live-id/listener registry..

### [Principle] D5 = D11 (+D21+D23) — Harness discipline: standalone reproduce, verifier trust, convergence
NEVER edit .pi/verifier.json (SHA-256 locked vs /refs). Homemade multi-case harnesses lie: reproduce standalone (cargo test --exact NAME + /tmp probe) before theorizing; rc=0 authoritative. TARGET MET + porcelain=0 -> cheap git-status/HEAD confirms; rerun suite ONLY on state change. Memory rides separate chore(memory) commits keeping DONE hashes intact. Checkpoints evidence-backed only or file byte-intact; fabricated lessons poison recall..; panicked tests leak process-global registries causing cascading false fails

### [Insight] D6 = D19 — rquickjs API + zero-copy buffer idioms
Value no Display -> Coerced::<String>::from_js; globals().set(...) OUTER ? needed; throw Exception::throw_message / Error::Io(io::Error::other(..)); Canvas::as_le_bytes() raw_parts cfg(LE)-gated, to_le_bytes() delegates LE w/ BE fallback. fb mock: parent EXISTS -> lazy create+sized FB_BYTES else NotFound..

### [Principle] D7 = D8 + D9 — TLS/rustls ground truth
Prove rejection by EFFECTS (mTLS alert may follow complete_io Ok; rejected handshake emits NO Data event). webpki self-signed-leaf-as-ca CaUsedAsEndEntity PartialEq false always -> match Debug; accept ONLY byte-matching anchors. OnceLock roots per-process: CA-env tests SEPARATE subprocesses or load_system_roots_uncached serial. TLSSocket end(): Shutdown::Write only + keep reading; autoEnd -> Ok(0) -> End+Close both; destroy(): latch before shutdown(Both), try_clone wake-slot exits ms-level..

### [Insight] D10 — ureq/hyper/dyn-dispatch drift facts
Box<dyn Trait> rebind needs delegating impl (E0277); hand Read/Write on Box hits E0119 blanket conflict; manual redirects=max_redirects(0). hyper1 Empty<Bytes>/frame(); tar rejects '..'; semver strip '^'..; inherent methods shadow traits: call Trait::method(&x)

### [Insight] D12 — aws-emulator protocol + SigV4 ground truth
SQS JSON proto ALWAYS POST ROOT '/': path-style -> QueueDoesNotExist; ConditionalCheckFailed 400 __type never retried. S3 path-style binary Uint8Array OK; errors JSON __type; BucketNotEmpty 409 XML..; QueueUrl verbatim in every body, transport stays AWS_ENDPOINT_URL

### [Dead End] D13 = D17 — cz-js realm gaps + KvStore semantics
No URL/TextDecoder globals in cz-js realms (kills SigV4) -> pure-JS splitUrl()/utf8Decode() shared ESM runs cz-js AND Node. Absolute ESM specs fail from temp dirs -> CzResolver /-prefix disk branch (+.mjs/.js). kv whole-object set() stores, get() NULL -> setField/getField primitives (JSON-string payload), probe-proven. KvStore: earliest_expiry_ms CONSERVATIVE LOWER bound never upward; ttl_count EXACT + resync post-purge; purge_expired INSIDE get/has/keys/stats -> counts assert direct-call/post-read only..

### [Insight] D14 = D22 — Oracle discipline + ring/TTF fixture facts
Node oracles BEFORE embedding: memory vectors were wrong; alpha-sum ink probes prove coverage pre pixel-identity. TTF minimal: endPtsOfContours BEFORE instruction length/bytecodes else BLANK rasters; cmap fmt4 idDelta maps chars to INTENDED glyf index. ring 0.17.14 hmac::sign(&Key,data) NOT tag(); digest()->Digest .as_ref(); Key::new Algorithm by value. Opt<Value> Some(undefined-value) on explicit undefined -> is_undefined()||is_null()..

### [Dead End] D15 — cz-json tape zero-span scalars break passthrough
KIND_BOOL/KIND_NULL span (0,0): raw slice EMPTY so serialize passthrough emits NOTHING -> invalid JSON; container spans exact, whole-doc fine until parent dirtied. Render scalars from tagged tape.scalar(); strings copy raw escaped span. A/B HEAD-vs-patched SAME script isolates pre-existing bugs; temp kind/span probe then DELETE..

### [Principle] D16 — Lazy CoW transparency + non-reentrant edits-mutex recursion
Materialize ONLY while WHOLE doc is_clean(), else Proxy handles (dirty doc + materialized GRANDchildren silently detaches nested writes). is_clean() fast-path BEFORE locking; under-guard refs not Vec clones; ONE last-wins view/dirty node; DROP MutexGuard BEFORE recursing or child subtree re-lock deadlocks the same std mutex.

### [Principle] D24 — rquickjs Vec->Uint8Array ownership move beats new_copy
ArrayBuffer::new(ctx, Vec<u8>) + TypedArray::from_arraybuffer = ZERO memcpy (ManuallyDrop ptr handoff); EMPTY Vec hands a null ptr -> separate new_copy(&[]) branch. Prove JS-owns-the-buffer by mutate-through-view then native re-read via with_borrowed_bytes (ffi-move test). JsString.to_cstring() borrow -> as_str().as_bytes().to_vec() THEN move; Opt<Value> strict flag reads as_ref().and_then(as_bool()).

### [Insight] D25 — Native codec inside a borrowed slice + QuickJS view gotchas
Building a JS string INSIDE with_borrowed_bytes is legal (alloc != JS exec; the JS arg stays rooted on the stack) so valid UTF-8 needs NO Rust String copy. QuickJS `new Uint8Array(dataView)` treats DataView ARRAY-LIKE -> empty; byteSource must resolve input.buffer for DataView/typed views (byteOffset+byteLength stay relative). Lossy U+FFFD counts differ from hand loops -> measure lengths with node Buffer.byteLength oracle before asserting (never guess multibyte totals).

### [Principle] D26 — Combined loop-drain scope + rustls config memoization
One ctx.with per pass may chain fetch->tls->httpCloses->http->timers ONLY if jobs pump AFTER the scope; the follow-up rescan MUST SKIP timers (interval fires twice in one iteration) and runner test helpers must bind the glue too (else callbacks silently never run). Cache rustls default ClientConfig in OnceLock ONLY for default opts; CA-env tests use build_client_config_uncached + Arc::ptr_eq under the shared ca_env_lock. Content-Length hint takes &HeaderMap (unit-testable, no Body build); pre-size ONLY when buffer empty else reserve_exact(saturating_sub).

### [Principle] D18 — Font glyph cache: hits and misses blend identically
swash ScaleContext + bounded cache (glyph_id,f32 size bits) FIFO cap 256 = zero-alloc repeat draw_text ONLY if cached AND fresh share ONE blend_alpha routine; separate compositing drifts pixels -> pixel-exact E2E fails; assert hit-vs-fresh BYTE equality. &mut *guard deref once per string; re-lock same Mutex panics; one guard per STRING not glyph..
