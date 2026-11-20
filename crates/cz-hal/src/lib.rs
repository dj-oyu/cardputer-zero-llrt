//! Cardputer Zero Hardware Abstraction Layer.
//!
//! Every device path can be overridden through environment variables so the HAL can be tested
//! deterministically against mock files/sysfs trees without root privileges or real hardware:
//!
//! - `CARDPUTER_FB_PATH` — framebuffer (`/dev/fb_lcd` -> `/dev/fb0` fallback)
//! - `CARDPUTER_BACKLIGHT_DIR` — backlight sysfs directory
//! - `CARDPUTER_INPUT_PATH` — evdev input device (`tca8418c` scan -> `/dev/input/event3`)
//! - `CARDPUTER_KEYMAP_PATH` — TCA8418C keymap file (built-in 32 symbol Sym table fallback)
//! - `CARDPUTER_CONFIG_PATH` — APPLaunch `config.json` (`$HOME/.config/cardputerzero/config.json`)
//! - `CARDPUTER_POWER_DIR` — BQ27220 battery sysfs directory
//!
//! [`http`] is transport-agnostic HTTP/1.1 protocol code (request parser, chunked decoding, the
//! 1 MiB body cap and response serialization) used by the `node:http` module in `cz-modules`.
//! [`http_server`] adds the multi-client TCP engine: one poll-parked accept thread per listening
//! socket plus one reader thread per connection, forwarding parsed requests toward JavaScript.
//!
//! No panics: hardware failures surface as `std::io::Error` / structured defaults.

pub mod backlight;
pub mod config;
pub mod fb;
pub mod http;
pub mod http_server;
pub mod input;
pub mod kv;
pub mod power;

/// Non-empty environment variable as a string, `None` when unset or empty.
pub(crate) fn env_non_empty(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}
