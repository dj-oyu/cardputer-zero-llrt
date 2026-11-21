### [Note] Archive
D-detail lives in dream-archive.md (same labels; D24-D29 full evidence appended this round).

### [Principle] D1 — QuickJS TLA settlement: pump jobs BETWEEN scopes
Never Promise::finish() (job spin deadlocks worker I/O; Context::with RefCell panic); evaluate WITHOUT finish, JS settle flags, pump I/O+jobs ONLY between scopes; promise.into_value() to glue, NEVER Rust .then..

### [Dead End] D2 = D20 — File-edit & sandbox hygiene
Read verbatim before edit oldText; sed re-emission repair; git -c safe.directory=/workspace per call; python3 absent -> node -e split-count anchors; heredoc terminator ALONE on own line; prove non-write via git status after anomalies..

### [Principle] D3 — Stage-then-commit beats in-place mutation
RAII staging BEFORE writes; commit same-dir temp + fs::rename; corrupt hashes keep manifests byte-intact; verify-before-extract + size caps; ONE staging pipeline reused..

### [Principle] D4 — Liveness registries: release ALL terminal paths; close servers first
Counts decrement at EVERY terminal point; counts INCLUDE listeners -> close servers FIRST then poll zero; stop-flag before join; hang -> inspect live-id/listener registry..

### [Principle] D5 = D11 (+D21+D23) — Harness discipline: standalone reproduce, verifier trust, convergence
NEVER edit .pi/verifier.json (SHA-256 locked vs /refs). Homemade multi-case harnesses lie -> reproduce standalone (--exact NAME + /tmp probe), rc=0 authoritative; panicked tests leak process-global registries -> cascading false fails; TARGET MET + porcelain=0 -> cheap git-status confirms, rerun ONLY on state change; memory rides separate chore(memory) commits; checkpoints evidence-backed only..

### [Insight] D6 = D19 — rquickjs API + zero-copy buffer idioms
Value no Display -> Coerced::<String>::from_js; globals().set needs OUTER ?; throw Exception::throw_message / Error::Io(io::Error::other); Canvas as_le_bytes cfg(LE)-gated w/ BE fallback; fb mock: parent dir EXISTS else NotFound..

### [Principle] D7 = D8 + D9 — TLS/rustls ground truth
Prove rejection by EFFECTS (mTLS alert may follow complete_io Ok; rejected handshake emits NO Data event); webpki leaf-as-ca CaUsedAsEndEntity PartialEq always false -> match Debug; OnceLock roots -> CA-env needs separate subprocesses or uncached serial; end() = Shutdown::Write + keep reading; destroy(): latch before shutdown(Both), try_clone wake-slot ms..

### [Insight] D10 — ureq/hyper/dyn-dispatch drift facts
Box<dyn Trait> rebind needs delegating impl (E0277); hand Read/Write on Box hits E0119 blanket conflict; manual redirects=max_redirects(0). hyper1 Empty<Bytes>/frame(); tar rejects '..'; semver strip '^'..; inherent methods shadow traits: call Trait::method(&x)

### [Insight] D12 — aws-emulator protocol + SigV4 ground truth
SQS JSON proto ALWAYS POST ROOT '/': path-style -> QueueDoesNotExist; ConditionalCheckFailed 400 never retried. S3 path-style binary Uint8Array OK; errors JSON __type; BucketNotEmpty 409 XML; canonical headers LOWERCASE keys else silent messageless SerializationException..

### [Dead End] D13 = D17 — cz-js realm gaps + KvStore semantics
No URL/TextDecoder in cz-js realms -> pure-JS splitUrl()/utf8Decode() shared ESM (cz-js+Node); absolute ESM specs fail from temp dirs -> /-prefix disk branch (+.mjs/.js); kv whole-object set() get() NULL -> setField/getField primitives, probe-proven; earliest_expiry_ms CONSERVATIVE lower bound; purge_expired INSIDE get/has/keys/stats -> counts assert direct-call/post-read only..

### [Insight] D14 = D22 — Oracle discipline + ring/TTF fixture facts
Node oracles BEFORE embedding vectors/rasters (hand-written were wrong). ring hmac::sign(&Key,data) NOT tag(), digest()->Digest .as_ref(). Opt<Value> explicit undefined sees Some(undefined-value) -> is_undefined()||is_null(). Minimal TTF: endPtsOfContours BEFORE instruction length; cmap fmt4 idDelta -> INTENDED glyf index..

### [Dead End] D15 — cz-json tape zero-span scalars break passthrough
KIND_BOOL/KIND_NULL span (0,0) -> EMPTY raw slice -> passthrough emits NOTHING -> invalid JSON; containers fine until parent dirtied. Render scalars from tagged tape.scalar(), strings copy escaped raw span; A/B HEAD-vs-patched SAME script isolates pre-existing bugs; temp probe then DELETE..

### [Principle] D16 — Lazy CoW transparency + non-reentrant edits-mutex recursion
Materialize ONLY while WHOLE doc is_clean() else Proxy detaches nested writes; is_clean() fast-path BEFORE locking; under-guard refs not Vec clones; ONE last-wins view per dirty node; DROP MutexGuard BEFORE recursing (same std mutex re-lock deadlocks)..

### [Principle] D24 — rquickjs Vec->Uint8Array ownership move beats new_copy
ArrayBuffer::new(ctx, Vec<u8>) + TypedArray::from_arraybuffer = ZERO memcpy; EMPTY Vec null ptr -> separate new_copy(&[]); prove JS-owns-buffer mutate-view then with_borrowed_bytes re-read; to_cstring borrow -> bytes to_vec THEN move..

### [Insight] D25 — Native codec inside a borrowed slice + QuickJS view gotchas
JS string built INSIDE with_borrowed_bytes legal (alloc != JS exec; arg rooted on stack) -> valid UTF-8 zero Rust String copy; new Uint8Array(DataView) ARRAY-LIKE -> empty, resolve input.buffer (+byteOffset/byteLength); multibyte totals need node Buffer.byteLength oracle, never guess..

### [Principle] D26 — Combined loop-drain scope + rustls config memoization
One ctx.with chains fetch->tls->httpCloses->http->timers ONLY if jobs pump AFTER the scope; follow-up rescan MUST SKIP timers (interval twice); runner helpers must bind glue too. rustls ClientConfig OnceLock default-opts ONLY; CA-env uncached + Arc::ptr_eq under ca_env_lock. CL hint takes &HeaderMap; pre-size ONLY when buffer empty else reserve_exact(saturating_sub)..

### [Principle] D18 — Font glyph cache: hits and misses blend identically
swash ScaleContext + cache (glyph_id,f32 bits) FIFO cap 256 = zero-alloc repeats ONLY when cached AND fresh share ONE blend_alpha routine; assert hit-vs-fresh BYTE equality; deref guard once per STRING not glyph, re-lock same Mutex panics..

### [Insight] D27 — Span-backed head: retract the fold separator first
obs-fold in-place value-span growth MUST truncate the just-emitted `\r\n` BEFORE " " + trim else span swallows CRLF ("one\r\n folded" vs legacy "one folded"); ends_with guard idempotent; restore separator at next field begin; legacy join fixtures = exact byte oracle..

### [Dead End] D28 — Structural greps scan raw text; comments trip banned-literal asserts
/refs structural asserts regex RAW file text -> banned literal even inside an explanatory comment FAILS clean code; reword w/o pattern ("no name->values map / no value vectors"); grep -n pinpoints fast..

### [Principle] D29 — Linear-cursor + vectored-TX test/wire rules
Cursor-state asserts ONLY from literal fixture byte lengths (Vec capacity unspecified; explicit with_capacity(32) rule tests); FULL mid-request consumption resets read_pos+buf to (0,0) so stale NeedMore asserts lie. Vectored TX: NEVER leading zero-length IoSlice, ONE global offset retry mapped on (head,body), flush ONCE = wire-identical to combined encode()+write_all (TCP writev coalesces, TLS std-default sequential)..
