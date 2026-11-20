//! rquickjs (`ModuleDef`) builtin modules for Cardputer Zero LLRT:
//! `cardputer:display`, `cardputer:input`, `cardputer:app`, `cardputer:power`.
//!
//! Each module exports named symbols AND a `default` object containing all exports.
//! The display framebuffer is lazily opened: importing `cardputer:display` must NOT touch
//! `/dev/fb0`/`CARDPUTER_FB_PATH` until the first `flush()` call.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use rquickjs::{Array, Ctx, Function, IntoJs, Object, Result, Value};

use cz_gfx::{Canvas, FontFace};
use cz_hal::backlight::Backlight;

use cz_hal::config::LauncherConfig;
use cz_hal::fb::Framebuffer;
use cz_hal::input::{InputState, KeyEvent as HalKeyEvent, Mods};
use cz_hal::power::PowerStatus;

pub mod fs_module;
pub mod http_module;
pub mod kv_module;

// ---------------------------------------------------------------------------
// Clean `awslabs/llrt` v0.10.0-beta integration surface
// ---------------------------------------------------------------------------

// Re-export every builtin `rquickjs::module::ModuleDef` struct at the crate root so an LLRT
// fork (`llrt_modules`) can register them directly via `llrt_utils::module::ModuleInfo` without
// reaching into submodules.
pub use fs_module::FsModule;
pub use http_module::HttpModule;
pub use kv_module::KvModule;

/// Every Cardputer module specifier the LLRT bridge exposes (display, input, app, power, kv).
/// HTTP/FS ride the Node-style specifiers listed in [`MODULE_NAMES`].
pub const CARDPUTER_MODULE_NAMES: &[&str] = &[
    "cardputer:display",
    "cardputer:input",
    "cardputer:app",
    "cardputer:power",
    "cardputer:kv",
];

/// True when at least one `node:http` server is currently listening. Embedders (e.g. `llrt_core`
/// on Tokio) poll this to decide whether the HTTP/SSE pump must keep running.
pub fn has_active_http_servers() -> bool {
    http_module::listening_count() > 0
}

/// Drive one HTTP dispatch pass for embedders that run their own event loop (LLRT's Tokio loop
/// alongside QuickJS): fire pending client-"close" notifications (SSE cleanup), pull queued
/// requests toward JavaScript through the `node:http` glue installed by [`HttpModule`], and give
/// freshly queued engine requests one extra pass.
///
/// Returns `true` while HTTP work remains — a live listener or engine-queued requests — and
/// `Ok(false)` when everything is closed/drain-free OR when the glue was never installed (no
/// `node:http` import in this context yet).
///
/// IMPORTANT: QuickJS *promise jobs* created by JS handlers during the pass are deliberately NOT
/// executed here. rquickjs re-enters the runtime `RefCell` if job pumping happens inside an
/// active context scope, so embedders must call `Runtime::execute_pending_job()` OUTSIDE any
/// `Context::with()` scope after this returns (mirrors the standalone cz-js runner design).
pub fn pump_llrt_http<'js>(ctx: &Ctx<'js>) -> Result<bool> {
    /// Invoke a global glue counter function; missing/non-function or thrown errors count 0.
    fn call_glue_counter(ctx: &Ctx<'_>, name: &str) -> i64 {
        match ctx.globals().get::<_, Function>(name) {
            Ok(f) => f.call(()).unwrap_or(0),
            Err(_) => 0,
        }
    }
    // No node:http glue yet in this context: nothing to pump.
    if ctx.globals().get::<_, Function>("__czHttpDrainLoop").is_err() {
        return Ok(false);
    }
    call_glue_counter(ctx, "__czHttpCheckCloses");
    call_glue_counter(ctx, "__czHttpDrainLoop");
    // Requests queued during dispatch deserve one extra pass (same shape as the runner loop).
    if http_module::has_pending_requests() {
        call_glue_counter(ctx, "__czHttpDrainLoop");
    }
    Ok(has_active_http_servers() || http_module::has_pending_requests())
}

/// Every builtin specifier `cz-js` can resolve without a filesystem lookup.
pub const MODULE_NAMES: &[&str] = &[
    "cardputer:display",
    "cardputer:input",
    "cardputer:app",
    "cardputer:power",
    "cardputer:kv",
    "node:http",
    "http",
    "node:fs",
    "fs",
];

/// Which builtin implementation answers a resolved specifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Display,
    Input,
    App,
    Power,
    Http,
    Fs,
    Kv,
}

/// Map a bare/aliased specifier to its builtin kind (`None` means: use the file loader).
/// `"http"`/`"fs"` are Node-style aliases of `"node:http"`/`"node:fs"`.
pub fn resolve_builtin(name: &str) -> Option<ModuleKind> {
    match name {
        "cardputer:display" => Some(ModuleKind::Display),
        "cardputer:input" => Some(ModuleKind::Input),
        "cardputer:app" => Some(ModuleKind::App),
        "cardputer:power" => Some(ModuleKind::Power),
        "node:http" | "http" => Some(ModuleKind::Http),
        "node:fs" | "fs" => Some(ModuleKind::Fs),
        "cardputer:kv" => Some(ModuleKind::Kv),
        _ => None,
    }
}

/// Declare a builtin module instance for `name` in this context (caller then evaluates it).
pub fn declare_builtin<'js>(
    ctx: &rquickjs::Ctx<'js>,
    kind: ModuleKind,
    name: &str,
) -> rquickjs::Result<rquickjs::Module<'js, rquickjs::module::Declared>> {
    match kind {
        ModuleKind::Display => rquickjs::Module::declare_def::<DisplayModule, _>(ctx.clone(), name),
        ModuleKind::Input => rquickjs::Module::declare_def::<InputModule, _>(ctx.clone(), name),
        ModuleKind::App => rquickjs::Module::declare_def::<AppModule, _>(ctx.clone(), name),
        ModuleKind::Power => rquickjs::Module::declare_def::<PowerModule, _>(ctx.clone(), name),
        ModuleKind::Http => rquickjs::Module::declare_def::<http_module::HttpModule, _>(ctx.clone(), name),
        ModuleKind::Fs => rquickjs::Module::declare_def::<fs_module::FsModule, _>(ctx.clone(), name),
        ModuleKind::Kv => rquickjs::Module::declare_def::<kv_module::KvModule, _>(ctx.clone(), name),
    }
}

/// Servers currently listening (event-loop liveness).
pub fn http_listening_count() -> usize {
    http_module::listening_count()
}

/// Engine-queued requests not yet pulled toward JavaScript.
pub fn http_has_pending() -> bool {
    http_module::has_pending_requests()
}

/// Idle pause for event-loop passes (~5ms, keeps CardputerZero CPU usage low while waiting).
pub fn http_idle_pause() {
    std::thread::sleep(http_module::IDLE_SLEEP);
}

// ---------------------------------------------------------------------------
// Process-wide lazily initialized state
// ---------------------------------------------------------------------------

fn canvas() -> &'static Mutex<Canvas> {
    static CANVAS: OnceLock<Mutex<Canvas>> = OnceLock::new();
    CANVAS.get_or_init(|| Mutex::new(Canvas::new()))
}

/// Lazily opened framebuffer: only `flush()` opens the resolved device path.
fn framebuffer() -> &'static Mutex<Option<Framebuffer>> {
    static FB: OnceLock<Mutex<Option<Framebuffer>>> = OnceLock::new();
    FB.get_or_init(|| Mutex::new(None))
}

fn backlight_state() -> &'static Mutex<Option<Backlight>> {
    static BL: OnceLock<Mutex<Option<Backlight>>> = OnceLock::new();
    BL.get_or_init(|| Mutex::new(None))
}

fn input_state() -> &'static Mutex<Option<InputState>> {
    static INP: OnceLock<Mutex<Option<InputState>>> = OnceLock::new();
    INP.get_or_init(|| Mutex::new(None))
}

fn loaded_font() -> &'static Mutex<Option<FontFace>> {
    static FONT: OnceLock<Mutex<Option<FontFace>>> = OnceLock::new();
    FONT.get_or_init(|| Mutex::new(None))
}

/// Shared quit flag driven by `requestQuit()` and SIGINT/SIGTERM handlers.
static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn quit_signal_handler(_sig: libc::c_int) {
    QUIT.store(true, Ordering::SeqCst);
}

/// Install SIGINT/SIGTERM handlers that set the shared quit flag. Returns true when both
/// dispositions were accepted.
pub fn install_signal_handlers() -> bool {
    let handler = quit_signal_handler as *const () as libc::sighandler_t;
    let sigint = unsafe { libc::signal(libc::SIGINT, handler) };
    let sigterm = unsafe { libc::signal(libc::SIGTERM, handler) };
    sigint != libc::SIG_ERR && sigterm != libc::SIG_ERR
}

/// Rust-side view of the shared quit flag (`app.requestQuit()` / SIGINT / SIGTERM).
pub fn should_quit() -> bool {
    QUIT.load(Ordering::SeqCst)
}

/// Rust-side equivalent of `app.requestQuit()` (sets the shared quit flag). The runner event
/// loop returns as soon as this is set, matching Ctrl-C / SIGTERM shutdown semantics.
pub fn request_quit() {
    QUIT.store(true, Ordering::SeqCst);
}

/// Clear the shared quit flag (used by the runner after a handled shutdown and by tests).
pub fn reset_quit_flag() {
    QUIT.store(false, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// JS value helpers
// ---------------------------------------------------------------------------

/// Clamp a JS double into 0..=255 for color channels.
fn to_u8(v: f64) -> u8 {
    if !v.is_finite() {
        return 0;
    }
    let r = v.round();
    if r < 0.0 {
        0
    } else if r > 255.0 {
        255
    } else {
        r as u8
    }
}

/// Clamp a JS double into the RGB565 u16 range (NaN-safe).
fn to_u16_color(v: f64) -> u16 {
    if !v.is_finite() {
        return 0;
    }
    let i = v.round();
    if i < 0.0 {
        0
    } else if i > 65535.0 {
        65535
    } else {
        i as u16
    }
}

/// Clamp a JS double into an i32 coordinate.
fn to_i32(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    let r = v.round();
    if r < i32::MIN as f64 {
        i32::MIN
    } else if r > i32::MAX as f64 {
        i32::MAX
    } else {
        r as i32
    }
}

fn js_null<'js>(ctx: &Ctx<'js>) -> Value<'js> {
    Value::new_null(ctx.clone())
}

fn opt_num<'js, T>(ctx: &Ctx<'js>, v: Option<T>) -> Result<Value<'js>>
where
    T: IntoJs<'js>,
{
    match v {
        Some(inner) => inner.into_js(ctx),
        None => Ok(js_null(ctx)),
    }
}

pub(crate) fn build_mods_object<'js>(ctx: &Ctx<'js>, mods: &Mods) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("shift", mods.shift)?;
    obj.set("ctrl", mods.ctrl)?;
    obj.set("alt", mods.alt)?;
    obj.set("sym", mods.sym)?;
    obj.set("fn", mods.func)?;
    Ok(obj)
}

pub(crate) fn build_key_event<'js>(ctx: &Ctx<'js>, ev: &HalKeyEvent) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("code", ev.code as f64)?;
    obj.set("key", ev.key.as_str())?;
    match &ev.text {
        Some(t) => obj.set("text", t.as_str())?,
        None => obj.set("text", js_null(ctx))?,
    }
    obj.set("pressed", ev.pressed)?;
    obj.set("repeat", ev.repeat)?;
    obj.set("mods", build_mods_object(ctx, &ev.mods)?)?;
    obj.set("timestampUs", ev.timestamp_us as f64)?;
    Ok(obj)
}

pub(crate) fn build_power_object<'js>(ctx: &Ctx<'js>, s: &PowerStatus) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("available", s.available)?;
    obj.set("present", s.present)?;
    obj.set("capacity", opt_num(ctx, s.capacity)?)?;
    obj.set("voltageMv", opt_num(ctx, s.voltage_mv)?)?;
    obj.set("currentMa", opt_num(ctx, s.current_ma)?)?;
    obj.set("tempC", opt_num(ctx, s.temp_c)?)?;
    obj.set("status", s.status.as_str())?;
    obj.set("health", opt_num(ctx, s.health.clone())?)?;
    Ok(obj)
}

pub(crate) fn build_config_object<'js>(ctx: &Ctx<'js>, cfg: &LauncherConfig) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("darkTime", opt_num(ctx, cfg.dark_time)?)?;
    obj.set("brightness", opt_num(ctx, cfg.brightness)?)?;
    obj.set("volume", opt_num(ctx, cfg.volume)?)?;
    Ok(obj)
}

pub(crate) fn build_info_object<'js>(ctx: &Ctx<'js>) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    // Device identity constants per spec (the firmware image targets CardputerZero hardware).
    obj.set("device", "M5Stack CardputerZero")?;
    obj.set("arch", "aarch64")?;
    obj.set("cpu", "cortex-a53")?;
    obj.set("displayWidth", cz_gfx::WIDTH as f64)?;
    obj.set("displayHeight", cz_gfx::HEIGHT as f64)?;
    obj.set("runtime", "cardputer-zero-llrt")?;
    Ok(obj)
}

/// Build the QuickJS memory-usage snapshot directly from the runtime behind this context.
///
/// We read the raw `JSRuntime*` via `JS_GetRuntime` and fill a `JSMemoryUsage` with
/// `JS_ComputeMemoryUsage` INSIDE the active `ctx.with()` scope. This deliberately avoids
/// calling `Runtime::memory_usage()` (which would try to re-lock the runtime `RefCell` already
/// held by the surrounding scope and panic with "RefCell already borrowed"). Only JS-tracked
/// allocations are counted, so Rust-native KV payloads never inflate these numbers.
pub(crate) fn build_memory_usage_object<'js>(ctx: &Ctx<'js>) -> Result<Object<'js>> {
    use rquickjs::qjs;
    let ctx_ptr = ctx.as_raw().as_ptr();
    let rt_ptr = unsafe { qjs::JS_GetRuntime(ctx_ptr) };
    if rt_ptr.is_null() {
        // No runtime reachable: report zeros rather than crash.
        let obj = Object::new(ctx.clone())?;
        for key in [
            "mallocSize",
            "mallocCount",
            "memoryUsedSize",
            "objCount",
            "strCount",
        ] {
            obj.set(key, 0f64)?;
        }
        return Ok(obj);
    }
    let mut usage: qjs::JSMemoryUsage = unsafe { std::mem::zeroed() };
    unsafe { qjs::JS_ComputeMemoryUsage(rt_ptr, &mut usage) };
    let obj = Object::new(ctx.clone())?;
    obj.set("mallocSize", usage.malloc_size as f64)?;
    obj.set("mallocCount", usage.malloc_count as f64)?;
    obj.set("memoryUsedSize", usage.memory_used_size as f64)?;
    obj.set("objCount", usage.obj_count as f64)?;
    obj.set("strCount", usage.str_count as f64)?;
    Ok(obj)
}

/// Poll the keyboard state (lazily constructed) returning a JS array of KeyEvent objects.
pub(crate) fn js_poll<'js>(ctx: &Ctx<'js>) -> Result<Value<'js>> {
    let mut guard = input_state().lock().expect("input lock");
    if guard.is_none() {
        *guard = Some(InputState::new());
    }
    let st = guard.as_mut().expect("input state");
    let events = st.poll()?;
    let arr = Array::new(ctx.clone())?;
    for (i, ev) in events.iter().enumerate() {
        arr.set(i, build_key_event(ctx, ev)?)?;
    }
    Ok(arr.into_value())
}

// ---------------------------------------------------------------------------
// cardputer:app
// ---------------------------------------------------------------------------

/// `cardputer:app` module definition (device info, launcher config, quit control).
pub struct AppModule;

impl rquickjs::module::ModuleDef for AppModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["info", "getConfig", "shouldQuit", "requestQuit", "installSignalHandlers", "memoryUsage", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        let info_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Object<'js>> {
            build_info_object(&ctx)
        })?;
        let get_config_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Object<'js>> {
            build_config_object(&ctx, &LauncherConfig::load())
        })?;
        let should_quit_fn =
            Function::new(ctx.clone(), || -> bool { QUIT.load(Ordering::SeqCst) })?;
        let request_quit_fn = Function::new(ctx.clone(), || -> () {
            QUIT.store(true, Ordering::SeqCst);
        })?;
        let install_fn = Function::new(ctx.clone(), || -> bool { install_signal_handlers() })?;
        let memory_usage_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Object<'js>> {
            build_memory_usage_object(&ctx)
        })?;

        let values: Vec<(&'static str, Value<'js>)> = vec![
            ("info", info_fn.into_value()),
            ("getConfig", get_config_fn.into_value()),
            ("shouldQuit", should_quit_fn.into_value()),
            ("requestQuit", request_quit_fn.into_value()),
            ("installSignalHandlers", install_fn.into_value()),
            ("memoryUsage", memory_usage_fn.into_value()),
        ];
        for (name, value) in &values {
            exports.export(*name, value.clone())?;
        }
        let default_obj = Object::new(ctx.clone())?;
        for (name, value) in &values {
            default_obj.set(*name, value.clone())?;
        }
        exports.export("default", default_obj)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// cardputer:power
// ---------------------------------------------------------------------------

/// `cardputer:power` module definition (BQ27220 battery gauge).
pub struct PowerModule;

impl rquickjs::module::ModuleDef for PowerModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        decl.declare("status")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        let status_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Object<'js>> {
            build_power_object(&ctx, &cz_hal::power::status())
        })?;
        exports.export("status", status_fn.clone())?;
        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("status", status_fn)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// cardputer:input
// ---------------------------------------------------------------------------

/// `cardputer:input` module definition (TCA8418C keyboard via evdev / fixture files).
pub struct InputModule;

fn js_is_down(code: f64) -> bool {
    let mut guard = input_state().lock().expect("input lock");
    if guard.is_none() {
        *guard = Some(InputState::new());
    }
    guard.as_ref().expect("input state").is_down(to_i32(code))
}

fn js_was_pressed(code: f64) -> bool {
    let mut guard = input_state().lock().expect("input lock");
    if guard.is_none() {
        *guard = Some(InputState::new());
    }
    guard.as_ref().expect("input state").was_pressed(to_i32(code))
}

fn js_was_released(code: f64) -> bool {
    let mut guard = input_state().lock().expect("input lock");
    if guard.is_none() {
        *guard = Some(InputState::new());
    }
    guard.as_ref().expect("input state").was_released(to_i32(code))
}

fn js_get_mods<'js>(ctx: &Ctx<'js>) -> Result<Object<'js>> {
    let mut guard = input_state().lock().expect("input lock");
    if guard.is_none() {
        *guard = Some(InputState::new());
    }
    build_mods_object(ctx, &guard.as_ref().expect("input state").mods())
}

fn js_get_sym_map<'js>(ctx: &Ctx<'js>) -> Result<Object<'js>> {
    let mut guard = input_state().lock().expect("input lock");
    if guard.is_none() {
        *guard = Some(InputState::new());
    }
    let map = guard.as_ref().expect("input state").sym_map().clone();
    let obj = Object::new(ctx.clone())?;
    for (code, sym) in map {
        obj.set(code.to_string(), sym.to_string())?;
    }
    Ok(obj)
}

fn js_set_repeat(delay_ms: f64, interval_ms: f64) -> Result<()> {
    let mut guard = input_state().lock().expect("input lock");
    if guard.is_none() {
        *guard = Some(InputState::new());
    }
    guard
        .as_mut()
        .expect("input state")
        .set_repeat(delay_ms.max(0.0) as u64, interval_ms.max(1.0) as u64);
    Ok(())
}

fn js_reset() {
    let mut guard = input_state().lock().expect("input lock");
    *guard = None;
}

impl rquickjs::module::ModuleDef for InputModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in [
            "poll",
            "isDown",
            "wasPressed",
            "wasReleased",
            "getMods",
            "getSymMap",
            "setRepeat",
            "reset",
            "default",
        ] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        let poll_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Value<'js>> {
            js_poll(&ctx)
        })?;
        let is_down_fn = Function::new(ctx.clone(), js_is_down)?;
        let was_pressed_fn = Function::new(ctx.clone(), js_was_pressed)?;
        let was_released_fn = Function::new(ctx.clone(), js_was_released)?;
        let get_mods_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Object<'js>> {
            js_get_mods(&ctx)
        })?;
        let sym_map_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Object<'js>> {
            js_get_sym_map(&ctx)
        })?;
        let set_repeat_fn = Function::new(ctx.clone(), js_set_repeat)?;
        let reset_fn = Function::new(ctx.clone(), js_reset)?;

        let values: Vec<(&'static str, Value<'js>)> = vec![
            ("poll", poll_fn.into_value()),
            ("isDown", is_down_fn.into_value()),
            ("wasPressed", was_pressed_fn.into_value()),
            ("wasReleased", was_released_fn.into_value()),
            ("getMods", get_mods_fn.into_value()),
            ("getSymMap", sym_map_fn.into_value()),
            ("setRepeat", set_repeat_fn.into_value()),
            ("reset", reset_fn.into_value()),
        ];
        for (name, value) in &values {
            exports.export(*name, value.clone())?;
        }
        let default_obj = Object::new(ctx.clone())?;
        for (name, value) in &values {
            default_obj.set(*name, value.clone())?;
        }
        exports.export("default", default_obj)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Byte-buffer helpers for cardputer:display
// ---------------------------------------------------------------------------

/// Copy bytes out of a JS `Uint8Array`/`ArrayBuffer` into an owned Vec.
pub(crate) fn bytes_from_value<'js>(value: &Value<'js>) -> Result<Vec<u8>> {
    if let Ok(ta) = rquickjs::TypedArray::<u8>::from_value(value.clone()) {
        // SAFETY: no JavaScript execution happens while this slice is held.
        let bytes = unsafe { ta.as_slice() }.to_vec();
        return Ok(bytes);
    }
    if let Some(ab) = rquickjs::ArrayBuffer::from_value(value.clone()) {
        // SAFETY: no JavaScript execution happens while this slice is held.
        if let Some(b) = unsafe { ab.as_bytes() } {
            return Ok(b.to_vec());
        }
    }
    Err(rquickjs::Error::FromJs {
        from: "value",
        to: "Uint8Array or ArrayBuffer",
        message: None,
    })
}

/// Create a `Uint8Array` view over freshly copied LE canvas bytes.
pub(crate) fn u8_array_from_vec<'js>(ctx: &Ctx<'js>, v: Vec<u8>) -> Result<Value<'js>> {
    let ta = rquickjs::TypedArray::<u8>::new_copy(ctx.clone(), &v[..])?;
    Ok(ta.into_value())
}

// ---------------------------------------------------------------------------
// cardputer:display
// ---------------------------------------------------------------------------

/// Lazily ensure a backlight controller exists (first brightness call).
fn ensure_backlight() -> Result<()> {
    let mut guard = backlight_state().lock().expect("backlight lock");
    if guard.is_none() {
        *guard = Some(Backlight::new()?);
    }
    Ok(())
}

/// Open the framebuffer lazily on first flush; writes the current canvas frame LE bytes.
fn js_flush() -> Result<()> {
    let bytes = canvas().lock().expect("canvas lock").to_le_bytes();
    let mut guard = framebuffer().lock().expect("fb lock");
    if guard.is_none() {
        *guard = Some(Framebuffer::open_default()?);
    }
    guard.as_mut().expect("fb").flush_frame(&bytes)?;
    Ok(())
}

fn js_load_font(path: &str, family: Option<String>) -> bool {
    match FontFace::from_file(Path::new(path), family.as_deref()) {
        Ok(face) => {
            *loaded_font().lock().expect("font lock") = Some(face);
            true
        }
        Err(_) => false,
    }
}

/// `cardputer:display` module definition. Importing it does NOT open the framebuffer; only
/// the first `flush()` opens the resolved device/mock path.
pub struct DisplayModule;

impl rquickjs::module::ModuleDef for DisplayModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in [
            "WIDTH",
            "HEIGHT",
            "rgb565",
            "clear",
            "setPixel",
            "getPixel",
            "fillRect",
            "drawRect",
            "drawLine",
            "drawCircle",
            "fillCircle",
            "drawText",
            "blit",
            "flush",
            "getBuffer",
            "getBrightness",
            "setBrightness",
            "sleep",
            "wake",
            "isAsleep",
            "loadFont",
            "default",
        ] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        let rgb565_fn = Function::new(
            ctx.clone(),
            |r: f64, g: f64, b: f64| -> u32 { cz_gfx::rgb565(to_u8(r), to_u8(g), to_u8(b)) as u32 },
        )?;
        let clear_fn = Function::new(ctx.clone(), |color: f64| {
            canvas().lock().expect("canvas lock").clear(to_u16_color(color));
        })?;
        let set_pixel_fn = Function::new(ctx.clone(), |x: f64, y: f64, color: f64| {
            canvas().lock().expect("canvas lock").set_pixel(to_i32(x), to_i32(y), to_u16_color(color));
        })?;
        let get_pixel_fn = Function::new(ctx.clone(), |x: f64, y: f64| -> u32 {
            canvas().lock().expect("canvas lock").get_pixel(to_i32(x), to_i32(y)) as u32
        })?;
        let fill_rect_fn = Function::new(ctx.clone(), |x: f64, y: f64, w: f64, h: f64, color: f64| {
            canvas().lock().expect("canvas lock")
                .fill_rect(to_i32(x), to_i32(y), to_i32(w), to_i32(h), to_u16_color(color));
        })?;
        let draw_rect_fn = Function::new(ctx.clone(), |x: f64, y: f64, w: f64, h: f64, color: f64| {
            canvas().lock().expect("canvas lock")
                .draw_rect(to_i32(x), to_i32(y), to_i32(w), to_i32(h), to_u16_color(color));
        })?;
        let draw_line_fn = Function::new(ctx.clone(), |x0: f64, y0: f64, x1: f64, y1: f64, color: f64| {
            canvas().lock().expect("canvas lock")
                .draw_line(to_i32(x0), to_i32(y0), to_i32(x1), to_i32(y1), to_u16_color(color));
        })?;
        let draw_circle_fn = Function::new(ctx.clone(), |cx: f64, cy: f64, r: f64, color: f64| {
            canvas().lock().expect("canvas lock")
                .draw_circle(to_i32(cx), to_i32(cy), to_i32(r), to_u16_color(color));
        })?;
        let fill_circle_fn = Function::new(ctx.clone(), |cx: f64, cy: f64, r: f64, color: f64| {
            canvas().lock().expect("canvas lock")
                .fill_circle(to_i32(cx), to_i32(cy), to_i32(r), to_u16_color(color));
        })?;
        let draw_text_fn = Function::new(
            ctx.clone(),
            |x: f64, y: f64, text: String, color: f64, scale: Option<f64>| {
                let s = scale.unwrap_or(1.0);
                let s = if s.is_finite() && s >= 0.0 { s as u32 } else { 1 };
                canvas().lock().expect("canvas lock")
                    .draw_text_builtin(to_i32(x), to_i32(y), &text, to_u16_color(color), s);
            },
        )?;
        let blit_fn = Function::new(
            ctx.clone(),
            |x: f64, y: f64, w: f64, h: f64, data: Value<'js>| -> Result<()> {
                let bytes = bytes_from_value(&data)?;
                canvas().lock().expect("canvas lock")
                    .blit_rgb565(to_i32(x), to_i32(y), to_i32(w), to_i32(h), &bytes);
                Ok(())
            },
        )?;
        let flush_fn = Function::new(ctx.clone(), || -> Result<()> { js_flush() })?;
        let get_buffer_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Value<'js>> {
            let bytes = canvas().lock().expect("canvas lock").to_le_bytes();
            u8_array_from_vec(&ctx, bytes)
        })?;
        let get_brightness_fn = Function::new(ctx.clone(), || -> Result<u32> {
            ensure_backlight()?;
            let guard = backlight_state().lock().expect("backlight lock");
            guard.as_ref().expect("backlight").get_brightness()
                .map_err(Error::from)
        })?;
        let set_brightness_fn = Function::new(ctx.clone(), |level: f64| -> Result<u32> {
            ensure_backlight()?;
            let lvl = if level.is_finite() && level > 0.0 { level.round() as u32 } else { 0u32 };
            let mut guard = backlight_state().lock().expect("backlight lock");
            let bl = guard.as_mut().expect("backlight");
            bl.set_brightness(lvl).map_err(rquickjs::Error::from)
        })?;
        let sleep_fn = Function::new(ctx.clone(), || -> Result<()> {
            ensure_backlight()?;
            let mut guard = backlight_state().lock().expect("backlight lock");
            guard.as_mut().expect("backlight").sleep().map_err(Error::from)
        })?;
        let wake_fn = Function::new(ctx.clone(), || -> Result<u32> {
            ensure_backlight()?;
            let mut guard = backlight_state().lock().expect("backlight lock");
            guard.as_mut().expect("backlight").wake().map_err(Error::from)
        })?;
        let is_asleep_fn = Function::new(ctx.clone(), || -> Result<bool> {
            ensure_backlight()?;
            let guard = backlight_state().lock().expect("backlight lock");
            Ok(guard.as_ref().expect("backlight").is_asleep())
        })?;
        let load_font_fn = Function::new(ctx.clone(), |path: String, family: Option<String>| -> bool {
            js_load_font(&path, family)
        })?;

        exports.export("WIDTH", cz_gfx::WIDTH as f64)?;
        exports.export("HEIGHT", cz_gfx::HEIGHT as f64)?;

        let values: Vec<(&'static str, Value<'js>)> = vec![
            ("rgb565", rgb565_fn.into_value()),
            ("clear", clear_fn.into_value()),
            ("setPixel", set_pixel_fn.into_value()),
            ("getPixel", get_pixel_fn.into_value()),
            ("fillRect", fill_rect_fn.into_value()),
            ("drawRect", draw_rect_fn.into_value()),
            ("drawLine", draw_line_fn.into_value()),
            ("drawCircle", draw_circle_fn.into_value()),
            ("fillCircle", fill_circle_fn.into_value()),
            ("drawText", draw_text_fn.into_value()),
            ("blit", blit_fn.into_value()),
            ("flush", flush_fn.into_value()),
            ("getBuffer", get_buffer_fn.into_value()),
            ("getBrightness", get_brightness_fn.into_value()),
            ("setBrightness", set_brightness_fn.into_value()),
            ("sleep", sleep_fn.into_value()),
            ("wake", wake_fn.into_value()),
            ("isAsleep", is_asleep_fn.into_value()),
            ("loadFont", load_font_fn.into_value()),
        ];
        for (name, value) in &values {
            exports.export(*name, value.clone())?;
        }
        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("WIDTH", cz_gfx::WIDTH as f64)?;
        default_obj.set("HEIGHT", cz_gfx::HEIGHT as f64)?;
        for (name, value) in &values {
            default_obj.set(*name, value.clone())?;
        }
        exports.export("default", default_obj)?;
        Ok(())
    }
}

use rquickjs::Error;

#[cfg(test)]
mod module_tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cz-mod-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        dir.join(name)
    }
    use rquickjs::{Context, Module, Runtime};

    /// Serialize env-var + global-state touching tests (process-wide statics).
    pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn run_module_script(declares: &[(&str, ModuleRegistrar)], source: &str) -> Result<String> {
        let rt = Runtime::new()?;
        // No loaders needed: builtin modules are pre-declared with declare_def below.
        let ctx = Context::full(&rt)?;
        let outcome = ctx.with(|ctx| -> Result<String> {
            for (name, reg) in declares {
                reg(&ctx, name)?;
            }
            let promise = Module::evaluate(ctx.clone(), "test.mjs", source)?;
            promise.finish::<()>()?;
            ctx.globals().get::<_, String>("__R").or(Ok("NO_RESULT".into()))
        });
        // Drain any pending jobs so nothing leaks between tests.
        while let Ok(true) = rt.execute_pending_job() {}
        outcome.map(|r| r.to_string())
    }

    type ModuleRegistrar = &'static dyn Fn(&Ctx<'_>, &str) -> Result<()>;

    pub(crate) const DECL_DISPLAY: ModuleRegistrar = &|ctx: &Ctx<'_>, name: &str| {
        Module::declare_def::<DisplayModule, _>(ctx.clone(), name)?;
        Ok(())
    };
    pub(crate) const DECL_INPUT: ModuleRegistrar = &|ctx: &Ctx<'_>, name: &str| {
        Module::declare_def::<InputModule, _>(ctx.clone(), name)?;
        Ok(())
    };
    pub(crate) const DECL_APP: ModuleRegistrar = &|ctx: &Ctx<'_>, name: &str| {
        Module::declare_def::<AppModule, _>(ctx.clone(), name)?;
        Ok(())
    };
    pub(crate) const DECL_POWER: ModuleRegistrar = &|ctx: &Ctx<'_>, name: &str| {
        Module::declare_def::<PowerModule, _>(ctx.clone(), name)?;
        Ok(())
    };

    #[test]
    fn display_module_lazy_import_then_flush_getbuffer() {
        let _lock = test_lock();
        // Part 1: lazy init — import + drawing must NOT open/create the framebuffer path.
        let dir = std::env::temp_dir().join(format!("cz-mod-lazy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let missing = dir.join("nogfx-dir").join("fb0");
        std::env::set_var("CARDPUTER_FB_PATH", &missing);

        let src = r#"
            import * as d from "cardputer:display";
            if (d.WIDTH !== 320 || d.HEIGHT !== 170) throw new Error("dims");
            const red = d.rgb565(255, 0, 0);
            if (red !== 0xF800) throw new Error("rgb565 red mismatch");
            d.clear(red);
            d.fillRect(4, 4, 6, 6, 0x07E0);
            if (d.getPixel(5, 5) !== 0x07E0) throw new Error("fillRect pixel missing");
            d.drawText(10, 40, "Cz0 Hi 09!", 0xFFFF, 2);
            const buf = d.getBuffer();
            if (!(buf instanceof Uint8Array) || buf.length !== 108800) {
                throw new Error("getBuffer bad len " + buf.length);
            }
            globalThis.__R = "PRE_FLUSH_OK";
        "#;
        let r = run_module_script(&[("cardputer:display", DECL_DISPLAY)], src).expect("lazy eval");
        assert_eq!(r, "PRE_FLUSH_OK");
        assert!(!missing.exists(), "import must not create/open the framebuffer path");

        // Part 2: first flush opens the resolved path lazily and writes the full frame.
        let fb = dir.join("mock-fb0");
        std::fs::write(&fb, vec![0u8; cz_hal::fb::FB_BYTES]).expect("seed fb");
        std::env::set_var("CARDPUTER_FB_PATH", &fb);
        let flush_src = r#"
            import * as d from "cardputer:display";
            d.flush();
            globalThis.__R = "FLUSH_OK";
        "#;
        let r = run_module_script(&[("cardputer:display", DECL_DISPLAY)], flush_src).expect("flush eval");
        assert_eq!(r, "FLUSH_OK");
        let file = std::fs::read(&fb).expect("read fb");
        assert_eq!(file.len(), cz_hal::fb::FB_BYTES);
        let px = |x: usize, y: usize| {
            u16::from_le_bytes([file[(y * 320 + x) * 2], file[(y * 320 + x) * 2 + 1]])
        };
        assert_eq!(px(0, 0), 0xF800, "cleared background should be red at (0,0)");
        assert_eq!(px(4, 4), 0x07E0, "fillRect region should reach flushed file");
        assert_eq!(px(10, 10), 0xF800, "outside fillRect should remain red");

        std::env::remove_var("CARDPUTER_FB_PATH");
        std::fs::remove_file(&fb).ok();
    }

    #[test]
    fn backlight_sleep_wake_cycle_via_display_module() {
        let _lock = test_lock();
        let bl = temp("backlight-dir");
        std::fs::create_dir_all(&bl).expect("bl dir");
        std::fs::write(bl.join("max_brightness"), "100\n").expect("max");
        std::fs::write(bl.join("brightness"), "70\n").expect("cur");
        std::env::set_var("CARDPUTER_BACKLIGHT_DIR", &bl);

        let src = r#"
            import * as d from "cardputer:display";
            if (d.getBrightness() !== 70) throw new Error("initial brightness != 70");
            if (d.isAsleep() !== false) throw new Error("initially asleep");
            const c = d.setBrightness(300);
            if (c !== 100) throw new Error("clamp mismatch " + c);
            d.sleep();
            if (d.isAsleep() !== true || d.getBrightness() !== 0) throw new Error("sleep state wrong");
            const w = d.wake();
            if (w !== 100) throw new Error("wake mismatch " + w);
            globalThis.__R = "BACKLIGHT_OK";
        "#;
        let r = run_module_script(&[("cardputer:display", DECL_DISPLAY)], src).expect("bl eval");
        assert_eq!(r, "BACKLIGHT_OK");
        assert_eq!(
            std::fs::read_to_string(bl.join("brightness")).expect("bl file").trim(),
            "100"
        );
        std::env::remove_var("CARDPUTER_BACKLIGHT_DIR");
    }

    #[test]
    fn input_module_poll_edges_and_symmap() {
        let _lock = test_lock();
        let ev = temp("input-event3");
        let mut bytes = Vec::new();
        for (sec, usec, typ, code, val) in [
            (1i64, 1000i64, 1u16, 30i32, 1i32),
            (1, 2000, 1, 30, 0),
            (1, 3000, 1, 42, 1),
            (1, 4000, 1, 30, 1),
            (1, 5000, 1, 42, 0),
            (1, 6000, 1, 103, 1),
        ] {
            bytes.extend_from_slice(&sec.to_le_bytes());
            bytes.extend_from_slice(&usec.to_le_bytes());
            bytes.extend_from_slice(&typ.to_le_bytes());
            bytes.extend_from_slice(&(code as u16).to_le_bytes());
            bytes.extend_from_slice(&val.to_le_bytes());
        }
        std::fs::write(&ev, &bytes).expect("write ev fixture");
        std::env::set_var("CARDPUTER_INPUT_PATH", &ev);

        let src = r#"
            import * as i from "cardputer:input";
            const sym = i.getSymMap();
            if (Object.keys(sym).length !== 32) throw new Error("symmap len " + Object.keys(sym).length);
            if (sym["26"] !== "!" || sym["89"] !== "|") throw new Error("sym entries wrong");
            const evs = i.poll();
            if (!Array.isArray(evs)) throw new Error("poll must return array");
            const texts = evs.filter(e => e.pressed).map(e => e.text);
            if (JSON.stringify(texts) !== JSON.stringify(["a", "A", null])) {
                throw new Error("texts mismatch " + JSON.stringify(texts));
            }
            const last = evs[evs.length - 1];
            if (last.code !== 103 || last.key !== "Up") throw new Error("Up event wrong");
            if (typeof last.timestampUs !== "number") throw new Error("timestampUs missing");
            if (!i.isDown(103)) throw new Error("Up should be down");
            if (!i.wasPressed(103) || !i.wasReleased(30)) throw new Error("edges wrong");
            const mods = i.getMods();
            if (mods.shift !== false || mods.ctrl !== false || mods.alt !== false
                || mods.sym !== false || mods.fn !== false) {
                throw new Error("mods all false expected after releases: " + JSON.stringify(mods));
            }
            i.reset();
            if (i.isDown(103)) throw new Error("reset should clear down state");
            globalThis.__R = "INPUT_OK";
        "#;
        let r = run_module_script(&[("cardputer:input", DECL_INPUT)], src).expect("input eval");
        assert_eq!(r, "INPUT_OK");
        std::env::remove_var("CARDPUTER_INPUT_PATH");
        std::fs::remove_file(&ev).ok();
    }

    #[test]
    fn app_module_info_config_and_quit_flow() {
        let _lock = test_lock();
        reset_quit_flag();
        std::env::set_var("CARDPUTER_CONFIG_PATH", "/refs/fixtures/config_nested.json");

        let src = r#"
            import * as a from "cardputer:app";
            const info = a.info();
            if (info.device !== "M5Stack CardputerZero" || info.arch !== "aarch64"
                || info.cpu !== "cortex-a53" || info.displayWidth !== 320
                || info.displayHeight !== 170 || info.runtime !== "cardputer-zero-llrt") {
                throw new Error("info mismatch " + JSON.stringify(info));
            }
            const cfg = a.getConfig();
            if (cfg.darkTime !== 60 || cfg.brightness !== 85 || cfg.volume !== 80) {
                throw new Error("config mismatch " + JSON.stringify(cfg));
            }
            if (a.shouldQuit() !== false) throw new Error("quit initially set");
            if (a.installSignalHandlers() !== true) throw new Error("signal handlers failed");
            a.requestQuit();
            if (a.shouldQuit() !== true) throw new Error("shouldQuit not set after requestQuit");
            globalThis.__R = "APP_OK";
        "#;
        let r = run_module_script(&[("cardputer:app", DECL_APP)], src).expect("app eval");
        assert_eq!(r, "APP_OK");
        reset_quit_flag();
        std::env::remove_var("CARDPUTER_CONFIG_PATH");
    }

    #[test]
    fn power_module_status_and_missing_fallback() {
        let _lock = test_lock();
        std::env::set_var("CARDPUTER_POWER_DIR", "/refs/fixtures/power_bq27220");
        let ok_src = r#"
            import * as p from "cardputer:power";
            const s = p.status();
            if (s.available !== true || s.present !== true || s.capacity !== 87) {
                throw new Error("power base fields wrong " + JSON.stringify(s));
            }
            if (s.voltageMv !== 4123 || s.currentMa !== -245) throw new Error("units wrong");
            if (Math.abs(s.tempC - 39.3) > 0.01) throw new Error("temp " + s.tempC);
            if (s.status !== "Discharging" || s.health !== "Good") throw new Error("strings wrong");
            globalThis.__R = "POWER_OK";
        "#;
        let r = run_module_script(&[("cardputer:power", DECL_POWER)], ok_src).expect("power eval");
        assert_eq!(r, "POWER_OK");

        std::env::set_var("CARDPUTER_POWER_DIR", "/tmp/cz-nonexistent-power-dir");
        let miss_src = r#"
            import * as p from "cardputer:power";
            const s = p.status();
            if (s.available !== false || s.present !== false || s.capacity !== null
                || s.status !== "Unavailable" || s.health !== null) {
                throw new Error("missing fallback wrong " + JSON.stringify(s));
            }
            globalThis.__R = "POWER_MISSING_OK";
        "#;
        let r = run_module_script(&[("cardputer:power", DECL_POWER)], miss_src).expect("power miss eval");
        assert_eq!(r, "POWER_MISSING_OK");
        std::env::remove_var("CARDPUTER_POWER_DIR");
    }

    #[test]
    fn default_exports_shape_across_modules() {
        let _lock = test_lock();
        let src = r#"
            import display from "cardputer:display";
            import input from "cardputer:input";
            import app from "cardputer:app";
            import power from "cardputer:power";
            const need = [
                [display, ["rgb565", "clear", "flush", "getBuffer", "loadFont", "WIDTH", "HEIGHT"]],
                [input, ["poll", "isDown", "wasPressed", "wasReleased", "getMods", "getSymMap", "setRepeat", "reset"]],
                [app, ["info", "getConfig", "shouldQuit", "requestQuit", "installSignalHandlers"]],
                [power, ["status"]],
            ];
            for (const [ns, names] of need) {
                if (typeof ns !== "object") throw new Error("default not object");
                for (const n of names) {
                    if (!(n in ns)) throw new Error("missing default member " + n);
                }
            }
            globalThis.__R = "DEFAULTS_OK";
        "#;
        let r = run_module_script(
            &[
                ("cardputer:display", DECL_DISPLAY),
                ("cardputer:input", DECL_INPUT),
                ("cardputer:app", DECL_APP),
                ("cardputer:power", DECL_POWER),
            ],
            src,
        )
        .expect("defaults eval");
        assert_eq!(r, "DEFAULTS_OK");
    }

    #[test]
    fn cardputer_module_names_cover_every_cardputer_builtin_surface() {
        // Public const surface for the LLRT fork integration.
        assert_eq!(CARDPUTER_MODULE_NAMES.len(), 5);
        for n in [
            "cardputer:display",
            "cardputer:input",
            "cardputer:app",
            "cardputer:power",
            "cardputer:kv",
        ] {
            assert!(CARDPUTER_MODULE_NAMES.contains(&n), "missing {n}");
            assert!(resolve_builtin(n).is_some(), "{n} must resolve to a builtin kind");
        }
        // Compile-time proof each root-re-exported struct is usable as a ModuleDef.
        let rt = Runtime::new().expect("rt");
        let ctx = Context::full(&rt).expect("ctx");
        ctx.with(|c| {
            Module::declare_def::<DisplayModule, _>(c.clone(), "cardputer:display").expect("display");
            Module::declare_def::<InputModule, _>(c.clone(), "cardputer:input").expect("input");
            Module::declare_def::<AppModule, _>(c.clone(), "cardputer:app").expect("app");
            Module::declare_def::<PowerModule, _>(c.clone(), "cardputer:power").expect("power");
            Module::declare_def::<KvModule, _>(c.clone(), "cardputer:kv").expect("kv");
            Module::declare_def::<HttpModule, _>(c.clone(), "node:http").expect("http");
            Module::declare_def::<FsModule, _>(c.clone(), "node:fs").expect("fs");
        });
        while let Ok(true) = rt.execute_pending_job() {}
    }

    #[test]
    fn llrt_bridge_pump_hooks_idle_without_glue_and_after_http_import() {
        // Serialize against the HTTP socket tests: pumping the global dispatch loop while other
        // contexts hold live listeners could steal their queued requests (same re-entry rule as
        // next_active_request documents), so we take the shared HTTP test lock here.
        let _lock = http_module::http_test_lock();
        http_module::close_all_servers();
        assert!(!has_active_http_servers(), "no servers before the scenario");
        let rt = Runtime::new().expect("rt");
        let ctx = Context::full(&rt).expect("ctx");
        let outcome = ctx.with(|c| -> String {
            // Bare context (no node:http glue yet): pump is a clean no-op.
            let bare = pump_llrt_http(&c).expect("bare pump");
            // With glue installed but zero listeners: still idle, no error.
            Module::declare_def::<HttpModule, _>(c.clone(), "node:http").expect("http decl");
            let p = Module::evaluate(
                c.clone(),
                "pump-idle.mjs",
                r#"import * as h from "node:http"; globalThis.__SHAPE = typeof h.createServer === "function";"#,
            )
            .expect("eval");
            p.finish::<()>().expect("finish");
            let shaped: bool = c.globals().get("__SHAPE").unwrap_or(false);
            let after = pump_llrt_http(&c).expect("glue pump");
            format!("BARE={bare} SHAPE={shaped} AFTER={after}")
        });
        while let Ok(true) = rt.execute_pending_job() {}
        assert_eq!(outcome, "BARE=false SHAPE=true AFTER=false");
        assert!(!has_active_http_servers());
    }
}
