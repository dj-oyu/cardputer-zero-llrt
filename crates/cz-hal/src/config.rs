//! APPLaunch launcher config reader (`config.json`), matching zcode/launcher_config.rs rules:
//! only top-level (depth == 1) keys are honored, integer literals and numeric strings are both
//! accepted, and malformed/missing files fall back to defaults gracefully.

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

use crate::env_non_empty;

/// Default dark_time (screen-off idle seconds) when missing or invalid.
pub const DEFAULT_DARK_TIME: i32 = 30;

#[derive(Clone, Debug, PartialEq)]
enum JsonLite {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<JsonLite>),
    Obj(Vec<(String, JsonLite)>),
}

/// Minimal recursive-descent JSON parser (no external deps) producing a lightweight tree.
fn parse_json(input: &str) -> Result<JsonLite, &'static str> {
    let bytes = input.as_bytes();
    let mut pos = 0usize;

    fn skip_ws(bytes: &[u8], pos: &mut usize) {
        while *pos < bytes.len() {
            match bytes[*pos] {
                b' ' | b'\t' | b'\r' | b'\n' => *pos += 1,
                _ => break,
            }
        }
    }

    fn parse_value(bytes: &[u8], pos: &mut usize) -> Result<JsonLite, &'static str> {
        skip_ws(bytes, pos);
        if *pos >= bytes.len() {
            return Err("unexpected end of input");
        }
        match bytes[*pos] {
            b'{' => parse_obj(bytes, pos),
            b'[' => parse_arr(bytes, pos),
            b'"' => Ok(JsonLite::Str(parse_str(bytes, pos)?)),
            b't' => expect_lit(bytes, pos, "true").map(|_| JsonLite::Bool(true)),
            b'f' => expect_lit(bytes, pos, "false").map(|_| JsonLite::Bool(false)),
            b'n' => expect_lit(bytes, pos, "null").map(|_| JsonLite::Null),
            b'-' | b'0'..=b'9' => parse_num(bytes, pos),
            _ => Err("invalid value start"),
        }
    }

    fn expect_lit(bytes: &[u8], pos: &mut usize, lit: &str) -> Result<(), &'static str> {
        if bytes.len() < *pos + lit.len() || &bytes[*pos..*pos + lit.len()] != lit.as_bytes() {
            return Err("invalid literal");
        }
        *pos += lit.len();
        Ok(())
    }

    fn parse_str(bytes: &[u8], pos: &mut usize) -> Result<String, &'static str> {
        if bytes[*pos] != b'"' {
            return Err("expected string");
        }
        *pos += 1;
        let mut out = String::new();
        while *pos < bytes.len() {
            let c = bytes[*pos];
            match c {
                b'"' => {
                    *pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    *pos += 1;
                    if *pos >= bytes.len() {
                        return Err("truncated escape");
                    }
                    let e = bytes[*pos];
                    *pos += 1;
                    match e {
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0C}'),
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'u' => {
                            if *pos + 4 > bytes.len() {
                                return Err("truncated \\u escape");
                            }
                            let hex = std::str::from_utf8(&bytes[*pos..*pos + 4])
                                .map_err(|_| "bad \\u hex")?;
                            let v = u32::from_str_radix(hex, 16).map_err(|_| "bad \\u hex")?;
                            *pos += 4;
                            out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err("bad escape"),
                    }
                }
                _ => {
                    // Multi-byte UTF-8: push byte-wise through the string collector.
                    if c >= 0x80 {
                        return Err("non-ascii strings not supported");
                    }
                    out.push(c as char);
                    *pos += 1;
                }
            }
        }
        Err("unterminated string")
    }

    fn parse_num(bytes: &[u8], pos: &mut usize) -> Result<JsonLite, &'static str> {
        let start = *pos;
        if bytes[*pos] == b'-' {
            *pos += 1;
        }
        let digits_start = *pos;
        while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
            *pos += 1;
        }
        if *pos == digits_start {
            return Err("invalid number");
        }
        if *pos < bytes.len() && bytes[*pos] == b'.' {
            *pos += 1;
            let frac_start = *pos;
            while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
                *pos += 1;
            }
            if *pos == frac_start {
                return Err("invalid fraction");
            }
        }
        if *pos < bytes.len() && (bytes[*pos] == b'e' || bytes[*pos] == b'E') {
            *pos += 1;
            if *pos < bytes.len() && (bytes[*pos] == b'+' || bytes[*pos] == b'-') {
                *pos += 1;
            }
            let exp_start = *pos;
            while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
                *pos += 1;
            }
            if *pos == exp_start {
                return Err("invalid exponent");
            }
        }
        let text = std::str::from_utf8(&bytes[start..*pos]).map_err(|_| "bad number utf8")?;
        text.parse::<f64>()
            .map(JsonLite::Num)
            .map_err(|_| "invalid number")
    }

    fn parse_obj(bytes: &[u8], pos: &mut usize) -> Result<JsonLite, &'static str> {
        *pos += 1; // consume '{'
        skip_ws(bytes, pos);
        let mut entries: Vec<(String, JsonLite)> = Vec::new();
        if *pos < bytes.len() && bytes[*pos] == b'}' {
            *pos += 1;
            return Ok(JsonLite::Obj(entries));
        }
        loop {
            let key = parse_str(bytes, pos)?;
            skip_ws(bytes, pos);
            if *pos >= bytes.len() || bytes[*pos] != b':' {
                return Err("expected ':' in object");
            }
            *pos += 1;
            let value = parse_value(bytes, pos)?;
            entries.push((key, value));
            skip_ws(bytes, pos);
            if *pos >= bytes.len() {
                return Err("unterminated object");
            }
            match bytes[*pos] {
                b',' => {
                    *pos += 1;
                    skip_ws(bytes, pos);
                    // Allow trailing comma gracefully.
                    if *pos < bytes.len() && bytes[*pos] == b'}' {
                        *pos += 1;
                        return Ok(JsonLite::Obj(entries));
                    }
                }
                b'}' => {
                    *pos += 1;
                    return Ok(JsonLite::Obj(entries));
                }
                _ => return Err("bad object separator"),
            }
        }
    }

    fn parse_arr(bytes: &[u8], pos: &mut usize) -> Result<JsonLite, &'static str> {
        *pos += 1; // consume '['
        skip_ws(bytes, pos);
        let mut items: Vec<JsonLite> = Vec::new();
        if *pos < bytes.len() && bytes[*pos] == b']' {
            *pos += 1;
            return Ok(JsonLite::Arr(items));
        }
        loop {
            let v = parse_value(bytes, pos)?;
            items.push(v);
            skip_ws(bytes, pos);
            if *pos >= bytes.len() {
                return Err("unterminated array");
            }
            match bytes[*pos] {
                b',' => {
                    *pos += 1;
                    skip_ws(bytes, pos);
                    if *pos < bytes.len() && bytes[*pos] == b']' {
                        *pos += 1;
                        return Ok(JsonLite::Arr(items));
                    }
                }
                b']' => {
                    *pos += 1;
                    return Ok(JsonLite::Arr(items));
                }
                _ => return Err("bad array separator"),
            }
        }
    }

    let value = parse_value(bytes, &mut pos)?;
    skip_ws(bytes, &mut pos);
    if pos != bytes.len() {
        return Err("trailing content after JSON value");
    }
    Ok(value)
}

/// Parsed APPLaunch launcher configuration (top-level keys only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherConfig {
    /// Screen-off idle seconds; `None` means disabled (`0`), default `Some(30)`.
    pub dark_time: Option<i32>,
    /// Startup brightness when present and > 0.
    pub brightness: Option<i32>,
    /// Startup volume (clamped to <= 100) when present and >= 0.
    pub volume: Option<i32>,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            dark_time: Some(DEFAULT_DARK_TIME),
            brightness: None,
            volume: None,
        }
    }
}

/// Extract an i32 from a top-level JSON value: integer literals or numeric strings.
fn as_i32(v: &JsonLite) -> Option<i32> {
    match v {
        JsonLite::Num(n) => {
            if n.fract() == 0.0 && n.is_finite() {
                Some(*n as i32)
            } else {
                None
            }
        }
        JsonLite::Str(s) => s.trim().parse::<i32>().ok(),
        _ => None,
    }
}

/// Top-level entry lookup (depth == 1 only; nested keys are ignored by construction).
fn top_level(entries: &[(String, JsonLite)], key: &str) -> Option<i32> {
    entries.iter().find(|(k, _)| k == key).and_then(|(_, v)| as_i32(v))
}

impl LauncherConfig {
    /// Parse launcher config from JSON text applying the spec rules exactly.
    pub fn from_json_str(text: &str) -> Self {
        let parsed = match parse_json(text) {
            Ok(JsonLite::Obj(entries)) => entries,
            _ => return Self::default(),
        };

        let dark_time = match top_level(&parsed, "dark_time") {
            None => Some(DEFAULT_DARK_TIME),
            Some(v) if v <= 0 => None,
            Some(v) => Some(v),
        };
        let brightness = match top_level(&parsed, "brightness") {
            Some(v) if v > 0 => Some(v),
            _ => None,
        };
        let volume = match top_level(&parsed, "volume") {
            Some(v) if v >= 0 => Some(v.min(100)),
            _ => None,
        };

        Self { dark_time, brightness, volume }
    }

    /// Resolve config path: `CARDPUTER_CONFIG_PATH` -> `$HOME/.config/cardputerzero/config.json`.
    pub fn resolved_path() -> PathBuf {
        if let Some(p) = env_non_empty("CARDPUTER_CONFIG_PATH") {
            return PathBuf::from(p);
        }
        let home = env_non_empty("HOME").unwrap_or_else(|| "/root".to_string());
        PathBuf::from(home).join(".config").join("cardputerzero").join("config.json")
    }

    /// Load the resolved config file; missing/malformed files fall back to defaults.
    pub fn load() -> Self {
        Self::load_at(&Self::resolved_path())
    }

    /// Load a specific path with graceful fallback to defaults.
    pub fn load_at(path: &std::path::Path) -> Self {
        let mut s = String::new();
        match fs::File::open(path).and_then(|mut f| f.read_to_string(&mut s)) {
            Ok(_) => Self::from_json_str(&s),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => Self::default(),
            Err(_) => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_cfg(name: &str, text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cz-hal-cfg-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join(name);
        fs::write(&path, text).expect("write cfg");
        path
    }

    #[test]
    fn nested_keys_are_ignored_top_level_only() {
        let text = r#"{"appstore":{"registry":{"dark_time":999,"brightness":12}},"dark_time":60,"brightness":"85","volume":80}"#;
        let cfg = LauncherConfig::from_json_str(text);
        assert_eq!(cfg.dark_time, Some(60));
        assert_eq!(cfg.brightness, Some(85));
        assert_eq!(cfg.volume, Some(80));
    }

    #[test]
    fn numeric_strings_and_int_literals_both_accepted() {
        let cfg = LauncherConfig::from_json_str(r#"{"dark_time":"80","brightness":"50","volume":"42"}"#);
        assert_eq!(cfg.dark_time, Some(80));
        assert_eq!(cfg.brightness, Some(50));
        assert_eq!(cfg.volume, Some(42));
    }

    #[test]
    fn zero_dark_time_disables_sleep() {
        let cfg = LauncherConfig::from_json_str(r#"{"dark_time":0,"brightness":50}"#);
        assert_eq!(cfg.dark_time, None);
        assert_eq!(cfg.brightness, Some(50));
        assert_eq!(cfg.volume, None);
    }

    #[test]
    fn negative_volume_rejected_volume_clamped_to_100() {
        let cfg = LauncherConfig::from_json_str(r#"{"volume":-5}"#);
        assert_eq!(cfg.volume, None);
        let cfg2 = LauncherConfig::from_json_str(r#"{"volume":250}"#);
        assert_eq!(cfg2.volume, Some(100));
        let cfg3 = LauncherConfig::from_json_str(r#"{"volume":0}"#);
        assert_eq!(cfg3.volume, Some(0));
    }

    #[test]
    fn broken_or_non_object_json_falls_back_to_defaults() {
        let cfg = LauncherConfig::from_json_str(r#"{ broken json !!! "dark_time":"#);
        assert_eq!(cfg, LauncherConfig::default());
        let arr = LauncherConfig::from_json_str(r#"[1,2,3]"#);
        assert_eq!(arr, LauncherConfig::default());
        let empty = LauncherConfig::from_json_str("");
        assert_eq!(empty, LauncherConfig::default());
    }

    #[test]
    fn invalid_value_types_treated_as_missing() {
        let cfg = LauncherConfig::from_json_str(r#"{"dark_time":"abc","brightness":0,"volume":true}"#);
        assert_eq!(cfg.dark_time, Some(DEFAULT_DARK_TIME)); // invalid -> default
        assert_eq!(cfg.brightness, None); // 0 is not > 0
        assert_eq!(cfg.volume, None); // bool invalid
    }

    #[test]
    fn load_at_missing_file_and_broken_file_yield_defaults() {
        let broken = temp_cfg("broken.json", "{ broken json !!! \"dark_time\":");
        let cfg = LauncherConfig::load_at(&broken);
        assert_eq!(cfg.dark_time, Some(30));
        assert_eq!(cfg.brightness, None);

        let nested = temp_cfg(
            "nested.json",
            "{\"appstore\":{\"dark_time\":999},\"dark_time\":60,\"brightness\":\"85\",\"volume\":80}",
        );
        let cfg2 = LauncherConfig::load_at(&nested);
        assert_eq!(cfg2.dark_time, Some(60));
        assert_eq!(cfg2.brightness, Some(85));
        assert_eq!(cfg2.volume, Some(80));

        let missing = std::env::temp_dir().join("cz-hal-cfg-nope.json");
        fs::remove_file(&missing).ok();
        assert_eq!(LauncherConfig::load_at(&missing), LauncherConfig::default());

        fs::remove_file(&broken).ok();
        fs::remove_file(&nested).ok();
    }

    #[test]
    fn env_path_override_and_home_fallback() {
        let cfgfile = temp_cfg("env-cfg.json", "{\"dark_time\":12}");
        std::env::set_var("CARDPUTER_CONFIG_PATH", &cfgfile);
        assert_eq!(LauncherConfig::resolved_path(), cfgfile);
        let cfg = LauncherConfig::load();
        assert_eq!(cfg.dark_time, Some(12));
        std::env::set_var("CARDPUTER_CONFIG_PATH", "");
        std::env::remove_var("CARDPUTER_CONFIG_PATH");
        // HOME fallback shape (do not read a real HOME file to stay deterministic).
        let saved_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", "/tmp/cz-hal-fake-home");
        assert_eq!(
            LauncherConfig::resolved_path(),
            std::path::PathBuf::from("/tmp/cz-hal-fake-home/.config/cardputerzero/config.json")
        );
        match saved_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        fs::remove_file(&cfgfile).ok();
    }

    #[test]
    fn json_parser_accepts_full_document_shapes() {
        let v = parse_json(r#"{"a":[1,-2.5,3e2,true,null,"s\n"],"b":{"c":"d"}}"#);
        assert!(matches!(v, Ok(JsonLite::Obj(_))));
        assert!(parse_json(r#"{"a":1}"#).is_ok());
        assert!(parse_json("{\"a\":1}}").is_err());
        assert!(parse_json("\"\\u0041\"").is_ok());
    }
}
