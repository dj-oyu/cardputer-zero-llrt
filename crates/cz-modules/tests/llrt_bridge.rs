//! Black-box integration tests for the LLRT v0.10.0-beta integration surface exported by
//! `cz_modules`: root-level `ModuleDef` re-exports, `CARDPUTER_MODULE_NAMES`, `pump_llrt_http`,
//! and `has_active_http_servers`. Phase 6A adds the peripheral modules (imu, audio, ir, io).

use rquickjs::{Context, Module, Runtime};

#[test]
fn llrt_public_defs_register_all_twelve_modules_end_to_end() {
    // Every struct is importable at the crate ROOT (not just through submodules) and can be
    // declared + evaluated like any LLRT ModuleDef inside a fresh runtime.
    use cz_modules::{
        AppModule, AudioModule, CARDPUTER_MODULE_NAMES, DisplayModule, FsModule, FsPromisesModule,
        HttpModule, ImuModule, InputModule, IoModule, IrModule, JsonModule, KvModule, ModuleKind,
        PathModule, PowerModule, resolve_builtin,
    };

    assert_eq!(CARDPUTER_MODULE_NAMES.len(), 10);
    assert!(CARDPUTER_MODULE_NAMES.contains(&"cardputer:display"));
    assert!(CARDPUTER_MODULE_NAMES.contains(&"cardputer:kv"));
    assert!(CARDPUTER_MODULE_NAMES.contains(&"cardputer:json"));
    for p6 in [
        "cardputer:imu",
        "cardputer:audio",
        "cardputer:ir",
        "cardputer:io",
    ] {
        assert!(CARDPUTER_MODULE_NAMES.contains(&p6), "missing {p6}");
    }
    assert_eq!(resolve_builtin("node:http"), Some(ModuleKind::Http));
    assert_eq!(resolve_builtin("fs"), Some(ModuleKind::Fs));
    assert_eq!(resolve_builtin("cardputer:json"), Some(ModuleKind::Json));
    assert_eq!(resolve_builtin("cardputer:imu"), Some(ModuleKind::Imu));
    assert_eq!(resolve_builtin("cardputer:audio"), Some(ModuleKind::Audio));
    assert_eq!(resolve_builtin("cardputer:ir"), Some(ModuleKind::Ir));
    assert_eq!(resolve_builtin("cardputer:io"), Some(ModuleKind::Io));
    // Phase 12: Node-style path + fs/promises specifiers resolve to builtin kinds.
    assert_eq!(resolve_builtin("node:path"), Some(ModuleKind::Path));
    assert_eq!(resolve_builtin("path"), Some(ModuleKind::Path));
    assert_eq!(resolve_builtin("node:fs/promises"), Some(ModuleKind::FsPromises));
    assert_eq!(resolve_builtin("fs/promises"), Some(ModuleKind::FsPromises));

    let rt = Runtime::new().expect("runtime");
    let ctx = Context::full(&rt).expect("context");
    let out = ctx.with(|c| -> String {
        Module::declare_def::<DisplayModule, _>(c.clone(), "cardputer:display").expect("display");
        Module::declare_def::<InputModule, _>(c.clone(), "cardputer:input").expect("input");
        Module::declare_def::<AppModule, _>(c.clone(), "cardputer:app").expect("app");
        Module::declare_def::<PowerModule, _>(c.clone(), "cardputer:power").expect("power");
        Module::declare_def::<KvModule, _>(c.clone(), "cardputer:kv").expect("kv");
        Module::declare_def::<JsonModule, _>(c.clone(), "cardputer:json").expect("json");
        Module::declare_def::<ImuModule, _>(c.clone(), "cardputer:imu").expect("imu");
        Module::declare_def::<AudioModule, _>(c.clone(), "cardputer:audio").expect("audio");
        Module::declare_def::<IrModule, _>(c.clone(), "cardputer:ir").expect("ir");
        Module::declare_def::<IoModule, _>(c.clone(), "cardputer:io").expect("io");
        Module::declare_def::<HttpModule, _>(c.clone(), "node:http").expect("http");
        Module::declare_def::<FsModule, _>(c.clone(), "node:fs").expect("fs");
        Module::declare_def::<PathModule, _>(c.clone(), "node:path").expect("node:path");
        Module::declare_def::<PathModule, _>(c.clone(), "path").expect("path alias");
        Module::declare_def::<FsPromisesModule, _>(c.clone(), "node:fs/promises").expect("node:fs/promises");
        Module::declare_def::<FsPromisesModule, _>(c.clone(), "fs/promises").expect("fs/promises alias");
        let p = Module::evaluate(
            c.clone(),
            "llrt-bridge.mjs",
            r#"
            import * as display from "cardputer:display";
            import * as input from "cardputer:input";
            import * as app from "cardputer:app";
            import * as power from "cardputer:power";
            import * as kv from "cardputer:kv";
            import * as czJson from "cardputer:json";
            import * as http from "node:http";
            import * as fs from "node:fs";
            import pathP12 from "node:path";
            import fspP12 from "node:fs/promises";
            import * as imu from "cardputer:imu";
            import imuDef from "cardputer:imu";
            import * as audioMod from "cardputer:audio";
            import audioDef from "cardputer:audio";
            import * as irMod from "cardputer:ir";
            import irDef from "cardputer:ir";
            import * as ioMod from "cardputer:io";
            import ioDef from "cardputer:io";
            if (display.WIDTH !== 320 || display.HEIGHT !== 170) throw new Error("display dims");
            if (typeof input.poll !== "function") throw new Error("input.poll");
            if (typeof app.memoryUsage !== "function") throw new Error("app.memoryUsage");
            if (typeof power.status !== "function") throw new Error("power.status");
            const s = kv.open("llrt_bridge_store");
            s.set("k", 7);
            if (s.get("k") !== 7) throw new Error("kv roundtrip");
            if (typeof http.createServer !== "function" || !Array.isArray(http.METHODS)) throw new Error("http shape");
            if (typeof fs.readFileSync !== "function" || typeof fs.existsSync !== "function") throw new Error("fs shape");
            if (typeof fs.writeFileSync !== "function" || typeof fs.rmSync !== "function" || !fs.promises) throw new Error("fs phase12 shape");
            if (pathP12.sep !== "/" || pathP12.join("/a", "b") !== "/a/b") throw new Error("path phase12");
            if (typeof fspP12.readFile !== "function" || typeof fspP12.access !== "function") throw new Error("fs/promises phase12");
            if (typeof czJson.parseLazy !== "function" || typeof czJson.isLazy !== "function" || typeof czJson.lazyStats !== "function") throw new Error("json shape");
            const lazy = czJson.parseLazy('{"a":1}');
            if (!czJson.isLazy(lazy) || lazy.a !== 1) throw new Error("json lazy handle");

            // Phase 6A peripherals: every named export AND the default object must be live.
            const needP6 = [
                [imu, imuDef, ["devices", "read"]],
                [audioMod, audioDef, ["generateWav", "beep", "playWav"]],
                [irMod, irDef, ["encodeNec", "decodeNec", "sendRaw", "sendNec"]],
                [ioMod, ioDef, ["listPorts", "isReservedI2cAddr", "uartWrite", "uartRead"]],
            ];
            for (const [ns, def, names] of needP6) {
                for (const n of names) {
                    if (typeof ns[n] !== "function") throw new Error("named " + n + " missing");
                    if (typeof def[n] !== "function") throw new Error("default " + n + " missing");
                }
            }
            if (typeof imu.read().available !== "boolean") throw new Error("imu read shape");
            const wav = audioMod.generateWav(440, 40, 0.5);
            if (!(wav instanceof Uint8Array) || wav.length !== 44 + 1764 * 2) {
                throw new Error("audio wav shape " + wav.length);
            }
            const pulses = irMod.encodeNec(0x04, 0x08);
            if (!(pulses instanceof Uint32Array) || pulses.length !== 67) throw new Error("nec shape");
            const dec = irMod.decodeNec(pulses);
            if (!dec || dec.valid !== true || dec.address !== 0x04 || dec.command !== 0x08) {
                throw new Error("nec roundtrip " + JSON.stringify(dec));
            }
            const ports = ioMod.listPorts();
            if (!Array.isArray(ports.i2c) || !Array.isArray(ports.spi)
                || !Array.isArray(ports.gpio) || !Array.isArray(ports.uart)) {
                throw new Error("io ports shape " + JSON.stringify(ports));
            }
            if (ioMod.isReservedI2cAddr(0x55) !== true) throw new Error("BQ27220 guard");
            globalThis.__R = "LLRT_BRIDGE_ALL_OK";
            "#,
        )
        .expect("evaluate");
        p.finish::<()>().expect("finish");
        c.globals().get::<_, String>("__R").unwrap_or_else(|_| "NO_R".into())
    });
    while let Ok(true) = rt.execute_pending_job() {}
    assert_eq!(out, "LLRT_BRIDGE_ALL_OK");
}

#[test]
fn pump_llrt_http_and_has_active_http_servers_track_listen_then_close() {
    use cz_modules::{has_active_http_servers, http_listening_count, pump_llrt_http};
    cz_modules::http_module::close_all_servers();

    assert!(!has_active_http_servers(), "no listeners before listen");
    let rt = Runtime::new().expect("runtime");
    let ctx = Context::full(&rt).expect("context");

    // Listen phase: pump inside the scope reports liveness (server active).
    let listen_pump = ctx.with(|c| -> bool {
        Module::declare_def::<cz_modules::HttpModule, _>(c.clone(), "node:http").expect("decl");
        let p = Module::evaluate(
            c.clone(),
            "pump-live.mjs",
            r#"
            import { createServer } from "node:http";
            const srv = createServer((req, res) => { res.writeHead(200); res.end("ok"); });
            srv.listen(0, "127.0.0.1");
            globalThis.__srv = srv;
            "#,
        )
        .expect("eval");
        p.finish::<()>().expect("finish");
        pump_llrt_http(&c).expect("live pump")
    });
    while let Ok(true) = rt.execute_pending_job() {}
    assert!(listen_pump, "pump must report true while a server listens");
    assert!(has_active_http_servers());
    assert_eq!(http_listening_count(), 1);

    // Close phase: registry empties synchronously, pump goes idle again. Call close() in JS so
    // the server method keeps its `this` binding (a bare Rust handle call would lose it).
    ctx.with(|c| {
        let _: i64 = c
            .eval("globalThis.__srv.close(); 1")
            .expect("js close");
    });
    while let Ok(true) = rt.execute_pending_job() {}
    assert!(!has_active_http_servers(), "closed server must leave the registry");

    let idle_pump = ctx.with(|c| pump_llrt_http(&c)).expect("idle pump scope result");
    while let Ok(true) = rt.execute_pending_job() {}
    assert!(!idle_pump, "no listeners left -> pump reports false");
}
