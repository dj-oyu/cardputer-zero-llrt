### [Note] Archive
D-detail in dream-archive.md(same labels);P14 detail SPEC_PHASE14 §5-§6+DISCOVERIES H83.

### [Principle] D1 QuickJS TLA:pump jobs BETWEEN scopes
No Promise::finish()(jobspin deadlock,withRefCell panic);eval WITHOUT finish,settle FLAG,pump ONLY between Scopes;bare await eval=SyntaxErr->async IIFE.then __Rflag drain OUTSIDE.

### [Dead End] D2=D20=D34 file-edit/sandbox hygiene
safe.directory EVERY new shell;patch verifier-COPY at FINAL assignment post-listen NOT init+print effective base BEFORE trust(D59).

### [Principle] D3 stage-then-commit
RAII staging BEFORE writes;same-dir temp+rename;verify+caps BEFORE extract;manifest byte-intact under corruption.

### [Principle] D4 liveness registries
Release at EVERY terminal point

### [Principle] D5=D11=D21=D23 harness discipline
Repro STANDALONE before fix;closed D54=D55=D57 duplicate recalls pointer-only).

### [Insight] D6=D19 rquickjs API
Value no Display->Coerced::from_js

### [Principle] D7=D8=D9 TLS/rustls ground truth
rejection by EFFECTS(alert may follow complete_io Ok;destroy latch first..archive.

### [Insight] D10 dyn-dispatch drift
Box<dyn Trait> rebind=delegating impl(E0277);impl Read/Write on Box E0119 blanket;inherent SHADOWS trait methods->call Trait::method(&x) explicitly.

### [Insight] D12 aws-emulator SigV4
SQS POST ROOT /;SigV4 headers LOWERCASE else SILENT msgless SerializationException.

### [Dead End] D13=D17 realm gaps+Kv semantics
era URL/TextDecoder gap MOOT(P10C/P11);purge INSIDE get/has/keys/stats,count post-read only..archive:30.

### [Insight] D14=D22=D32=D45=D48 oracles BEFORE embedding REAL Node vectors/fixtures
guesses WRONG repeatedly:base64url fb=="-w",fbfffe=="-__-" unpadded;hex ws skipped,first invalid TERMINATES,odd dangling dropped;utf8 日本語==9 | registries TWO shapes(npm dist-tags vs compact name/version/tarball/integrity):probe REAL fixtures,compact fallback SAME semver+canonical /pkg/-/file,err msg kept,guards both|ring hmac sign(&Key) NOT tag..archive.

### [Dead End] D15=D35 cz-json zero-span scalars
zero-span scalars->EMPTY raw slice->emits NOTHING=invalid JSON

### [Principle] D16 Lazy CoW+mutex recursion
materialize only whole-doc clean;DROP guard BEFORE recursion(re-lock=DEADLOCK).

### [Principle] D18 glyph cache identity blends
ScaleContext+cache(glyph_id,f32)FIFO256 zero-alloc;hit AND fresh share ONE alpha routine,assert bytes;deref once/STRING not glyph.

### [Principle] D24=D44 typed-array ownership/mutation
ArrayBuffer::new(ctx,Vec)=ZERO memcpy(empty nullptr->separate new_copy,JS-owns proof=mutate view re-read);double Result ??.archive:96.

### [Insight] D25 codec in borrowed slice+views
JS string in borrowed slice legal(rooted,alloc!=exec);views resolve buffer+byteOffset;Node byteLength totals oracle.

### [Principle] D26=D29=D30=D31=D36=D38=D39=D40=D41 engine-wiring family (archive)
loop-drain scope+rustls memo;accum FLOOR+terminal latch;shared classes existence-check,export BY ref.

### [Insight] D27 obs-fold retract separator first
retract emitted \r\n BEFORE fold continuation else span eats CRLF;restore next field;byte fixtures.

### [Dead End] D28=D33 structural greps RAW text
banned literal trips even in COMMENT->reword sans pattern;grep-n fast.

### [Dead End] D37 `"#` kills r#"…"# embedded shims
literal terminates raw string early(cryptic RUST err):##".."## delims;grep first.

### [Principle] D43 handshake leftovers to frame loop
handover coalesced leftovers past terminator(WS upgrade swallowed TEXT+PING->Pong lost,36s->.2s):move resp+head_end,drain cursor first EXACT size,no over-read,persist resets;probe ENTRY eprintln before guess.archive:96.

### [Insight] D42 normalizeBody BEFORE header pairs
CT undefined w/ body OK;echo-headers isolates;pre-snapshot clients need meta.archive:87.

### [Insight] D50 multicall dispatch BEFORE init
dispatch FIRST(no Runtime for zpm/npm paths),node passthrough runtime(argv0 zpm|npm or argv1 zpm);keep standalone cz-zpm bin for release-build verifiers;guard all 4 shapes+None.

### [Principle] D47 Web-class shims DUAL-SHAPE ctors(engine record vs user init)
ctor hitting engine-state on user init=>native slot undefined f64 err EVERY route;manual=BUFFERED slot-less id:-1 released:true+resolved;sweep both-shape classes(D47-class).

### [Dead End] D46=D52=D58 spawnSync x in-process server=frozen pump,HOST-only fix
ALL JS starves:blocked parent shows server Recv-Q CONSTANT full-GET-unread>=5s w/ ESTABLISHED st=01 (TCP-level only,accept never)=>socket existence NEVER proves pump;reopen HOST-only via §5.2 snippet(helper-before-block|async spawn),replica-at-HEAD canonical(ALL PASS byte-exact).NEVER sync-child+same-process server.

### [Dead End] D49 ureq feature rustls RE-ADDS webpki-roots even default-features=false
feature rustls pulls no-provider+_ring+bundled-roots dep=>LOCK gains banned pkg,rg-gates FAIL;use rustls-no-provider+gzip w/ with_parts custom connector;rule:dep-trim rescan LOCK not manifest(cargo tree -e features + rg banned-names exit1) [pattern literal lives D49 body only via docs reword D28-class].

### [Principle] D51 unpassable-by-design gate protocol prove/document/commit/STOP never tamper
repro>=2x+ruled-out by class,bypass refused

### [Insight] D53 boss verifier pins HOST-rotated
sha256 LIVE pin vs seed BEFORE cp(seeds unapplied till host swap;mismatch=instant FAIL);check active-seed label-set.

### [Insight] D56 bound registry I/O FAST(shipped)+tar facade trap
timeouts read vendored ORACLE(Resolve5s/connect10/read10 default;ZPM_TIMEOUT_MS 1..60000 loud-reject,guarded);never-answer exits ~10s/2.6s tuned vs old SIGKILL-null,FAIL tail informative | TRAP tgz facade re-export !=bundle=>hand-picked tar members FALSE mismatch,real pipeline only.
