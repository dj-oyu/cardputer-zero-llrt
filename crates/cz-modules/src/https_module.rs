//! Phase 8B: Node-compatible `node:https` (alias `https`) TLS SERVER on top of the `node:http`
//! engine glue. The HTTP parsing/dispatch machinery (Keep-Alive, chunked bodies, SSE streaming,
//! `close()`/`address()`, request `"close"` notifications) is reused untouched; only the listen
//! path swaps in `cz_net::https::make_tls_acceptor`, which upgrades every accepted socket through
//! the `cz-hal` `StreamAcceptor` hook — `cz-hal` itself stays strictly `libc`-only.
//!
//! Semantics:
//! - `createServer(options, requestListener?)` validates `key`/`cert` IMMEDIATELY (descriptive
//!   `Error`, never a panic) and returns the shared `CzServer` (`listen(port, host?, cb?)` with
//!   default host `"0.0.0.0"`, `address()`, `close(cb?)`, events `request|listening|close|error`).
//! - TLS options mirror Node's server shape: `key`/`cert` (PEM string or `Uint8Array`, required),
//!   optional `ca` (client CA for mTLS), `requestCert` (default `false`),
//!   `rejectUnauthorized` (default `true` when `requestCert` is set).
//! - HTTPS requests report `req.socket.encrypted === true`; plain HTTP keeps `false`.
//! - HTTPS servers share the exact registry as `node:http`, so `http_listening_count()` /
//!   `http_has_pending()` keep `cz-runner`'s event loop alive while any HTTPS server listens and
//!   exit status `0` once all are closed.
//!
//! Measured limitation: rustls' client verifier is all-or-nothing about presenting certificates;
//! the exotic `requestCert:true + rejectUnauthorized:false` combination (Node: verify-if-present)
//! therefore behaves as `requestCert && rejectUnauthorized` (a cert is mandated when both flags
//! allow it). The E2E-covered combinations (`requestCert:false`, and both-true mTLS) match Node.

use rquickjs::{Array, Ctx, Exception, Function, Object, Result, Value};

use cz_hal::http::STATUS_CODES;
use cz_hal::http_server::StreamAcceptor;
use cz_net::https::make_tls_acceptor;
use cz_net::tls::TlsServerOptions;

use crate::http_module::{ensure_glue, listen_impl, push_value_bytes, METHODS};

/// JS glue for the TLS variant: builds the shared `CzServer`, validates options eagerly, and
/// routes `listen()` through `__czListenTls` whenever `_tlsOpts` is present.
const HTTPS_SHIM: &str = r#"
globalThis.__czHttpsCreateServer = function (options, handler) {
  const opts = options === undefined || options === null ? {} : options;
  // Fail fast with a descriptive Error on missing/invalid key/cert (spec: no panics).
  globalThis.__czHttpsValidate(opts);
  const s = new globalThis.__czHttpClassServer();
  if (typeof handler === "function") {
    s.on("request", handler);
  }
  s._tlsOpts = opts;
  return s;
};
"#;

fn opt_bytes<'js>(ctx: &Ctx<'js>, opts: &Object<'js>, name: &str) -> Result<Option<Vec<u8>>> {
    let v = opts.get::<_, Value>(name)?;
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    let mut out = Vec::new();
    push_value_bytes(ctx, v, &mut out)?;
    if out.is_empty() {
        return Err(Exception::throw_message(
            ctx,
            &format!("https.createServer: '{name}' must be a non-empty string or Uint8Array"),
        ));
    }
    Ok(Some(out))
}

fn opt_bool<'js>(ctx: &Ctx<'js>, opts: &Object<'js>, name: &str, default: bool) -> Result<bool> {
    let v = opts.get::<_, Value>(name)?;
    if v.is_undefined() || v.is_null() {
        return Ok(default);
    }
    match v.as_bool() {
        Some(b) => Ok(b),
        None => Err(Exception::throw_message(
            ctx,
            &format!("https.createServer: '{name}' must be a boolean"),
        )),
    }
}

/// Convert JS TLS options into `TlsServerOptions`, throwing descriptive Errors (never panics).
fn tls_opts_from_js<'js>(ctx: &Ctx<'js>, opts: &Object<'js>) -> Result<TlsServerOptions> {
    let key = opt_bytes(ctx, opts, "key")?.ok_or_else(|| {
        Exception::throw_message(ctx, "https.createServer requires a 'key' PEM private key")
    })?;
    let cert = opt_bytes(ctx, opts, "cert")?.ok_or_else(|| {
        Exception::throw_message(ctx, "https.createServer requires a 'cert' PEM certificate chain")
    })?;
    let ca = opt_bytes(ctx, opts, "ca")?;
    let request_cert = opt_bool(ctx, opts, "requestCert", false)?;
    let reject_unauthorized = opt_bool(ctx, opts, "rejectUnauthorized", true)?;
    // See module docs: rustls mandates-or-none; Node's verify-if-present combo is approximated.
    let require_client_auth = request_cert && reject_unauthorized;
    Ok(TlsServerOptions {
        cert_pem: cert,
        key_pem: key,
        ca_pem: ca,
        require_client_auth,
    })
}

fn build_acceptor<'js>(ctx: &Ctx<'js>, opts: &Object<'js>) -> Result<StreamAcceptor> {
    let tls_opts = tls_opts_from_js(ctx, opts)?;
    make_tls_acceptor(&tls_opts)
        .map_err(|msg| Exception::throw_message(ctx, &format!("https TLS configuration error: {msg}")))
}

fn install_https_glue<'js>(ctx: &Ctx<'js>) -> Result<()> {
    ctx.globals().set(
        "__czListenTls",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, port: f64, host: String, opts: Object<'js>| -> Result<Object<'js>> {
                let acceptor = build_acceptor(&ctx, &opts)?;
                listen_impl(&ctx, port, &host, Some(acceptor))
            },
        )?,
    )?;

    // Eager option validation used by __czHttpsCreateServer: builds the rustls server config
    // (PEM parse + key/cert match + mTLS verifier setup) and discards it — errors throw in JS.
    ctx.globals().set(
        "__czHttpsValidate",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, opts: Object<'js>| -> Result<bool> {
            let _ = build_acceptor(&ctx, &opts)?;
            Ok(true)
        })?,
    )?;

    ctx.eval::<(), _>(HTTPS_SHIM)?;
    Ok(())
}

/// `node:https` / `https` module definition: named exports AND a `default` export object,
/// reusing the exact `node:http` classes so requests/responses behave identically over TLS.
pub struct HttpsModule;

impl rquickjs::module::ModuleDef for HttpsModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in [
            "createServer",
            "STATUS_CODES",
            "METHODS",
            "Server",
            "IncomingMessage",
            "ServerResponse",
            "default",
        ] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        // Reuse ALL node:http primitives (classes, dispatch, streaming, close events).
        ensure_glue(ctx)?;
        install_https_glue(ctx)?;

        let create_fn = ctx.globals().get::<_, Function>("__czHttpsCreateServer")?;
        exports.export("createServer", create_fn.clone())?;

        let codes = Object::new(ctx.clone())?;
        for (code, phrase) in STATUS_CODES {
            codes.set(code.to_string(), *phrase)?;
        }
        exports.export("STATUS_CODES", codes.clone())?;

        let methods = Array::new(ctx.clone())?;
        for (i, m) in METHODS.iter().enumerate() {
            methods.set(i, *m)?;
        }
        exports.export("METHODS", methods.clone())?;

        let server_cls = ctx.globals().get::<_, Function>("__czHttpClassServer")?;
        exports.export("Server", server_cls.clone())?;
        let incoming_cls = ctx.globals().get::<_, Function>("__czHttpClassIncoming")?;
        exports.export("IncomingMessage", incoming_cls.clone())?;
        let response_cls = ctx.globals().get::<_, Function>("__czHttpClassResponse")?;
        exports.export("ServerResponse", response_cls.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("createServer", create_fn)?;
        default_obj.set("STATUS_CODES", codes)?;
        default_obj.set("METHODS", methods)?;
        default_obj.set("Server", ctx.globals().get::<_, Function>("__czHttpClassServer")?)?;
        default_obj.set("IncomingMessage", ctx.globals().get::<_, Function>("__czHttpClassIncoming")?)?;
        default_obj.set("ServerResponse", ctx.globals().get::<_, Function>("__czHttpClassResponse")?)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}
