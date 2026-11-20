//! TCA8418C keyboard input via evdev (`struct input_event`, 64-bit little-endian, 24 bytes).
//!
//! Supports character devices opened with `O_NONBLOCK` as well as regular-file fixtures read
//! sequentially from the current file offset so `poll()` drains all newly appended events.
//! Never calls `EVIOCGRAB`. Self-managed key repeat: default delay 400ms, interval 33ms
//! (50ms for scroll keys Up/Down/PageUp/PageDown unless overridden).

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::env_non_empty;

/// EV_SYN event type.
pub const EV_SYN: u16 = 0;
/// EV_KEY event type.
pub const EV_KEY: u16 = 1;
/// Size of one Linux (64-bit) `input_event` record.
pub const INPUT_EVENT_SIZE: usize = 24;

// Modifier keycodes (Linux input-event-codes.h + CardputerZero SYM/FN).
pub const KEY_LEFTSHIFT: i32 = 42;
pub const KEY_RIGHTSHIFT: i32 = 54;
pub const KEY_LEFTCTRL: i32 = 29;
pub const KEY_RIGHTCTRL: i32 = 97;
pub const KEY_LEFTALT: i32 = 56;
pub const KEY_RIGHTALT: i32 = 100;
pub const KEY_SYM: i32 = 383;
pub const KEY_FN: i32 = 389;

/// Default auto-repeat delay in milliseconds.
pub const DEFAULT_REPEAT_DELAY_MS: u64 = 400;
/// Default auto-repeat interval in milliseconds for regular keys.
pub const DEFAULT_REPEAT_INTERVAL_MS: u64 = 33;
/// Default auto-repeat interval in milliseconds for scroll keys.
pub const DEFAULT_SCROLL_INTERVAL_MS: u64 = 50;

/// Modifier key state snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub sym: bool,
    pub func: bool,
}

/// One processed keyboard event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: i32,
    pub key: String,
    /// Text produced by the key (None for control/navigation keys or when Ctrl/Alt held).
    pub text: Option<String>,
    pub pressed: bool,
    /// Synthetic repeat emitted by the self-managed repeat timer (`pressed` stays true).
    pub repeat: bool,
    pub mods: Mods,
    pub timestamp_us: i64,
}

const LETTERS: &[(i32, char)] = &[
    (30, 'A'), (48, 'B'), (46, 'C'), (32, 'D'), (18, 'E'), (33, 'F'), (34, 'G'),
    (35, 'H'), (23, 'I'), (36, 'J'), (37, 'K'), (38, 'L'), (50, 'M'), (49, 'N'),
    (24, 'O'), (25, 'P'), (16, 'Q'), (19, 'R'), (31, 'S'), (20, 'T'), (22, 'U'),
    (47, 'V'), (17, 'W'), (45, 'X'), (21, 'Y'), (44, 'Z'),
];

/// Exact built-in 32-symbol SYM mapping matching
/// `/usr/share/keymaps/tca8418_keypad_m5stack_keymap.map`.
pub fn builtin_sym_map() -> HashMap<i32, char> {
    let pairs: [(i32, char); 32] = [
        (26, '!'), (27, '@'), (39, '#'), (40, '$'), (41, '%'), (43, '^'), (51, '&'),
        (52, '*'), (53, '('), (94, ')'), (55, '~'), (69, '`'), (70, '_'), (71, '-'),
        (72, '+'), (73, '='), (74, '['), (75, ']'), (76, '{'), (77, '}'), (79, ';'),
        (80, ':'), (81, '\''), (82, '"'), (83, '<'), (85, '>'), (86, '\\'), (89, '|'),
        (90, ','), (91, '.'), (92, '/'), (93, '?'),
    ];
    let mut m = HashMap::new();
    for (code, sym) in pairs {
        m.insert(code, sym);
    }
    m
}

/// Keysym name -> char (used when a keymap file lacks the `# 0xXX -> c` comment).
fn keysym_name_to_char(name: &str) -> Option<char> {
    match name {
        "exclam" => Some('!'),
        "at" => Some('@'),
        "numbersign" => Some('#'),
        "dollar" => Some('$'),
        "percent" => Some('%'),
        "asciicircum" => Some('^'),
        "ampersand" => Some('&'),
        "asterisk" => Some('*'),
        "parenleft" => Some('('),
        "parenright" => Some(')'),
        "asciitilde" => Some('~'),
        "grave" => Some('`'),
        "underscore" => Some('_'),
        "minus" => Some('-'),
        "plus" => Some('+'),
        "equal" => Some('='),
        "bracketleft" => Some('['),
        "bracketright" => Some(']'),
        "braceleft" => Some('{'),
        "braceright" => Some('}'),
        "semicolon" => Some(';'),
        "colon" => Some(':'),
        "apostrophe" => Some('\''),
        "quotedbl" => Some('"'),
        "less" => Some('<'),
        "greater" => Some('>'),
        "backslash" => Some('\\'),
        "bar" => Some('|'),
        "comma" => Some(','),
        "period" => Some('.'),
        "slash" => Some('/'),
        "question" => Some('?'),
        _ => None,
    }
}

/// Parse a kbd-style keymap file (`keycode N = name # 0xXX -> c`) into keycode->char.
pub fn parse_keymap_file(text: &str) -> HashMap<i32, char> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with("keycode") {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        // Expect: ["keycode", "<num>", "=", "<name>", ...]
        if tokens.len() < 4 || tokens[2] != "=" {
            continue;
        }
        let code: i32 = match tokens[1].parse() {
            Ok(c) => c,
            Err(_) => continue,
        };
        let name = tokens[3];

        // Prefer the explicit "-> c" comment when present (exact char mapping).
        let mut ch: Option<char> = None;
        if let Some(pos) = line.find("->") {
            let tail = line[pos + 2..].trim();
            if let Some(last) = tail.chars().last() {
                ch = Some(last);
            }
        }
        if ch.is_none() {
            ch = keysym_name_to_char(name);
        }
        if let Some(c) = ch {
            map.insert(code, c);
        }
    }
    map
}

/// Default keymap file location on CardputerZero APPLaunch systems.
pub const DEFAULT_KEYMAP_PATH: &str = "/usr/share/keymaps/tca8418_keypad_m5stack_keymap.map";

/// Resolve the Sym keymap: env `CARDPUTER_KEYMAP_PATH` -> default map file -> built-in table.
pub fn resolve_sym_map() -> HashMap<i32, char> {
    if let Some(path) = env_non_empty("CARDPUTER_KEYMAP_PATH") {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let map = parse_keymap_file(&text);
            if !map.is_empty() {
                return map;
            }
        }
    }
    let def = Path::new(DEFAULT_KEYMAP_PATH);
    if def.exists() {
        if let Ok(text) = std::fs::read_to_string(def) {
            let map = parse_keymap_file(&text);
            if !map.is_empty() {
                return map;
            }
        }
    }
    builtin_sym_map()
}

/// Scan `/sys/class/input/event{0..31}/device/name` for the TCA8418C keyboard controller.
fn scan_tca8418c() -> Option<PathBuf> {
    for i in 0..32 {
        let name_path = format!("/sys/class/input/event{i}/device/name");
        if let Ok(raw) = std::fs::read_to_string(&name_path) {
            if raw.trim() == "tca8418c" {
                return Some(PathBuf::from(format!("/dev/input/event{i}")));
            }
        }
    }
    None
}

/// Resolve the input device path: env -> tca8418c scan -> `/dev/input/event3`.
pub fn resolve_input_path() -> PathBuf {
    if let Some(p) = env_non_empty("CARDPUTER_INPUT_PATH") {
        return PathBuf::from(p);
    }
    if let Some(p) = scan_tca8418c() {
        return p;
    }
    PathBuf::from("/dev/input/event3")
}

fn is_modifier(code: i32) -> bool {
    matches!(code, KEY_LEFTSHIFT | KEY_RIGHTSHIFT | KEY_LEFTCTRL | KEY_RIGHTCTRL | KEY_LEFTALT | KEY_RIGHTALT | KEY_SYM | KEY_FN)
}

fn special_key_name(code: i32) -> Option<&'static str> {
    match code {
        1 => Some("Esc"),
        14 => Some("Backspace"),
        15 => Some("Tab"),
        28 => Some("Enter"),
        57 => Some("Space"),
        59 => Some("F1"),
        60 => Some("F2"),
        61 => Some("F3"),
        62 => Some("F4"),
        63 => Some("F5"),
        64 => Some("F6"),
        65 => Some("F7"),
        66 => Some("F8"),
        67 => Some("F9"),
        68 => Some("F10"),
        87 => Some("F11"),
        88 => Some("F12"),
        102 => Some("Home"),
        103 => Some("Up"),
        104 => Some("PageUp"),
        105 => Some("Left"),
        106 => Some("Right"),
        107 => Some("End"),
        108 => Some("Down"),
        109 => Some("PageDown"),
        110 => Some("Insert"),
        111 => Some("Delete"),
        _ => None,
    }
}

fn letter_for(code: i32) -> Option<char> {
    LETTERS.iter().find(|(c, _)| *c == code).map(|(_, l)| *l)
}

fn digit_for(code: i32) -> Option<char> {
    // Linux keycodes 2..=11 are '1'..'9','0'.
    if (2..=11).contains(&code) {
        let d = code - 2;
        Some(if d == 9 { '0' } else { char::from_u32(u32::from(b'1') + d as u32).expect("digit") })
    } else {
        None
    }
}

/// Canonical key name for a keycode.
pub fn key_name(code: i32, sym_map: &HashMap<i32, char>) -> String {
    if let Some(l) = letter_for(code) {
        return l.to_string();
    }
    if let Some(d) = digit_for(code) {
        return d.to_string();
    }
    if let Some(n) = special_key_name(code) {
        return n.to_string();
    }
    if let Some(c) = sym_map.get(&code) {
        return c.to_string();
    }
    format!("KEY{code}")
}

/// Text produced by a keycode given the current modifier state (`None` when non-textual or
/// Ctrl/Alt is held on letters).
pub fn text_for(code: i32, mods: &Mods, sym_map: &HashMap<i32, char>) -> Option<String> {
    // Symbol-layer codes emit their mapped symbol (exact 32-entry TCA8418C table).
    if let Some(c) = sym_map.get(&code) {
        return Some(c.to_string());
    }
    if code == 57 {
        return Some(" ".to_string()); // Space
    }
    if let Some(l) = letter_for(code) {
        if mods.ctrl || mods.alt {
            return None;
        }
        return Some(if mods.shift {
            l.to_uppercase().to_string()
        } else {
            l.to_lowercase().to_string()
        });
    }
    if let Some(d) = digit_for(code) {
        return Some(d.to_string());
    }
    // Esc / arrows / F-keys / etc produce no text.
    None
}

fn monotonic_us() -> i64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as i64 * 1_000_000 + ts.tv_nsec / 1_000
}

/// A raw decoded Linux (64-bit little-endian) `input_event` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEventRaw {
    pub tv_sec: i64,
    pub tv_usec: i64,
    pub type_: u16,
    pub code: i32,
    pub value: i32,
}

/// Decode one 24-byte little-endian 64-bit `input_event`.
pub fn parse_input_event(bytes: &[u8]) -> Option<InputEventRaw> {
    if bytes.len() < INPUT_EVENT_SIZE {
        return None;
    }
    let tv_sec = i64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes"));
    let tv_usec = i64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
    let type_ = u16::from_le_bytes(bytes[16..18].try_into().expect("2 bytes"));
    let code = u16::from_le_bytes(bytes[18..20].try_into().expect("2 bytes")) as i32;
    let value = i32::from_le_bytes(bytes[20..24].try_into().expect("4 bytes"));
    Some(InputEventRaw { tv_sec, tv_usec, type_, code, value })
}

/// Self-managed keyboard state: held keys, per-poll edges, modifiers, repeat timers.
pub struct InputState {
    path: PathBuf,
    handle: Option<std::fs::File>,
    leftover: Vec<u8>,
    down: HashSet<i32>,
    pressed_edge: HashSet<i32>,
    released_edge: HashSet<i32>,
    mods: Mods,
    sym_map: HashMap<i32, char>,
    delay_ms: u64,
    interval_ms: u64,
    scroll_interval_ms: u64,
    repeat_overridden: bool,
    // keycode -> next synthetic repeat time (absolute microseconds, event-time base)
    held: HashMap<i32, i64>,
    // offset between CLOCK_MONOTONIC and the synthetic event clock
    sync_offset_us: Option<i64>,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

impl InputState {
    /// Create a state using fully resolved device path + keymap.
    pub fn new() -> Self {
        Self::with_paths(resolve_input_path(), resolve_sym_map())
    }

    pub fn with_paths(path: PathBuf, sym_map: HashMap<i32, char>) -> Self {
        Self {
            path,
            handle: None,
            leftover: Vec::new(),
            down: HashSet::new(),
            pressed_edge: HashSet::new(),
            released_edge: HashSet::new(),
            mods: Mods::default(),
            sym_map,
            delay_ms: DEFAULT_REPEAT_DELAY_MS,
            interval_ms: DEFAULT_REPEAT_INTERVAL_MS,
            scroll_interval_ms: DEFAULT_SCROLL_INTERVAL_MS,
            repeat_overridden: false,
            held: HashMap::new(),
            sync_offset_us: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn sym_map(&self) -> &HashMap<i32, char> {
        &self.sym_map
    }

    pub fn mods(&self) -> Mods {
        self.mods.clone()
    }

    pub fn is_down(&self, code: i32) -> bool {
        self.down.contains(&code)
    }

    pub fn was_pressed(&self, code: i32) -> bool {
        self.pressed_edge.contains(&code)
    }

    pub fn was_released(&self, code: i32) -> bool {
        self.released_edge.contains(&code)
    }

    /// Override the repeat configuration globally (applies to scroll keys too).
    pub fn set_repeat(&mut self, delay_ms: u64, interval_ms: u64) {
        self.delay_ms = delay_ms;
        self.interval_ms = interval_ms;
        self.repeat_overridden = true;
    }

    /// Clear all tracked state and close the cached device handle for clean re-open.
    pub fn reset(&mut self) {
        self.handle = None;
        self.leftover.clear();
        self.down.clear();
        self.pressed_edge.clear();
        self.released_edge.clear();
        self.mods = Mods::default();
        self.held.clear();
        self.sync_offset_us = None;
    }

    fn interval_for(&self, code: i32) -> u64 {
        if self.repeat_overridden {
            self.interval_ms
        } else if matches!(code, 103 | 104 | 108 | 109) {
            self.scroll_interval_ms
        } else {
            self.interval_ms
        }
    }

    fn open_handle(&mut self) -> io::Result<()> {
        if self.handle.is_some() {
            return Ok(());
        }
        let mut opts = OpenOptions::new();
        opts.read(true);
        // Evdev devices want read/write access; regular-file mocks only need read. Try both.
        opts.write(true).custom_flags(libc::O_NONBLOCK);
        let file = match opts.open(&self.path) {
            Ok(f) => f,
            Err(_) => {
                opts.write(false);
                opts.open(&self.path)?
            }
        };
        self.handle = Some(file);
        Ok(())
    }

    /// Read all currently available events and translate them into `KeyEvent`s.
    pub fn poll(&mut self) -> io::Result<Vec<KeyEvent>> {
        self.pressed_edge.clear();
        self.released_edge.clear();

        self.open_handle()?;
        let is_char_device = self
            .handle
            .as_ref()
            .map(|f| f.metadata().map(|m| m.file_type().is_char_device()).unwrap_or(false))
            .unwrap_or(false);
        // Regular files: read sequentially from the current offset (no seek) until EOF.
        if is_char_device {
            // Character devices report pending events non-blockingly from their own cursor.
            self.leftover.clear();
        }

        let mut buf = [0u8; 4096];
        let mut incoming: Vec<u8> = std::mem::take(&mut self.leftover);
        loop {
            match self.handle.as_mut().expect("handle").read(&mut buf) {
                Ok(0) => break,
                Ok(n) => incoming.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }

        let mut out: Vec<KeyEvent> = Vec::new();
        let mut last_ts: Option<i64> = None;

        let mut idx = 0usize;
        while idx + INPUT_EVENT_SIZE <= incoming.len() {
            let rec = &incoming[idx..idx + INPUT_EVENT_SIZE];
            idx += INPUT_EVENT_SIZE;
            let tv_sec = i64::from_le_bytes(rec[0..8].try_into().expect("8"));
            let tv_usec = i64::from_le_bytes(rec[8..16].try_into().expect("8"));
            let type_ = u16::from_le_bytes(rec[16..18].try_into().expect("2"));
            let code = i32::from(u16::from_le_bytes(rec[18..20].try_into().expect("2")));
            let value = i32::from_le_bytes(rec[20..24].try_into().expect("4"));
            if type_ != EV_KEY {
                continue; // Ignore EV_SYN / EV_MSC / etc.
            }
            let ts_us = tv_sec * 1_000_000 + tv_usec;
            if self.sync_offset_us.is_none() && tv_sec >= 0 {
                // Anchor the synthetic clock: real monotonic time minus fixture timestamp.
                self.sync_offset_us = Some(monotonic_us() - ts_us);
            }
            last_ts = Some(ts_us.max(last_ts.unwrap_or(ts_us)));

            if is_modifier(code) {
                // Track modifiers; never emit them as KeyEvents and never auto-repeat them.
                if value == 1 || value == 2 {
                    self.set_modifier(code, true);
                } else if value == 0 {
                    self.set_modifier(code, false);
                }
                continue;
            }

            if value == 2 {
                // Kernel auto-repeat is ignored; our own timer handles repeats instead.
                continue;
            }

            let pressed = value == 1;
            if pressed {
                self.down.insert(code);
                self.pressed_edge.insert(code);
                let interval_delay = self.delay_ms as i64 * 1_000;
                self.held.insert(code, ts_us + interval_delay);
            } else {
                self.down.remove(&code);
                self.released_edge.insert(code);
                self.held.remove(&code);
            }

            out.push(KeyEvent {
                code,
                key: key_name(code, &self.sym_map),
                text: text_for(code, &self.mods, &self.sym_map),
                pressed,
                repeat: false,
                mods: self.mods.clone(),
                timestamp_us: ts_us,
            });
        }

        self.leftover = if idx < incoming.len() {
            incoming[idx..].to_vec()
        } else {
            Vec::new()
        };

        // Emit synthetic repeats using the event clock (latest timestamp) or, when no new
        // events arrived, the monotonic clock mapped through the recorded anchor offset.
        if let Some(now) = last_ts.or_else(|| {
            self.sync_offset_us.map(|off| monotonic_us().saturating_sub(off))
        }) {
            self.advance_repeat_into(now, &mut out);
        }

        Ok(out)
    }

    fn set_modifier(&mut self, code: i32, held: bool) {
        match code {
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => self.mods.shift = held,
            KEY_LEFTCTRL | KEY_RIGHTCTRL => self.mods.ctrl = held,
            KEY_LEFTALT | KEY_RIGHTALT => self.mods.alt = held,
            KEY_SYM => self.mods.sym = held,
            KEY_FN => self.mods.func = held,
            _ => {}
        }
    }

    /// Advance repeat timers to `now_us` (absolute microseconds), emitting synthetic repeat
    /// press events for every held key whose deadline has passed.
    pub fn advance_repeat(&mut self, now_us: i64) -> Vec<KeyEvent> {
        let mut out = Vec::new();
        self.advance_repeat_into(now_us, &mut out);
        out
    }

    fn advance_repeat_into(&mut self, now_us: i64, out: &mut Vec<KeyEvent>) {
        let codes: Vec<i32> = self.held.keys().copied().collect();
        for code in codes {
            if !self.down.contains(&code) || is_modifier(code) {
                continue;
            }
            let next = match self.held.get(&code).copied() {
                Some(n) => n,
                None => continue,
            };
            if now_us >= next {
                let interval = self.interval_for(code) as i64 * 1_000;
                self.held.insert(code, now_us + interval);
                self.pressed_edge.insert(code);
                out.push(KeyEvent {
                    code,
                    key: key_name(code, &self.sym_map),
                    text: text_for(code, &self.mods, &self.sym_map),
                    pressed: true,
                    repeat: true,
                    mods: self.mods.clone(),
                    timestamp_us: now_us,
                });
            }
        }
    }

    /// Rewind a regular-file fixture handle back to the start (test helper).
    pub fn rewind_fixture(&mut self) -> io::Result<()> {
        self.leftover.clear();
        if let Some(f) = self.handle.as_mut() {
            f.seek(SeekFrom::Start(0))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn encode(events: &[(i64, i64, u16, i32, i32)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (sec, usec, typ, code, value) in events {
            buf.extend_from_slice(&sec.to_le_bytes());
            buf.extend_from_slice(&usec.to_le_bytes());
            buf.extend_from_slice(&typ.to_le_bytes());
            buf.extend_from_slice(&(*code as u16).to_le_bytes());
            buf.extend_from_slice(&value.to_le_bytes());
        }
        buf
    }

    fn temp_fixture(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cz-hal-in-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join(name);
        fs::write(&path, bytes).expect("write fixture");
        path
    }

    #[test]
    fn builtin_sym_map_has_exact_32_symbols() {
        let m = builtin_sym_map();
        assert_eq!(m.len(), 32);
        assert_eq!(m.get(&26), Some(&'!'));
        assert_eq!(m.get(&71), Some(&'-'));
        assert_eq!(m.get(&73), Some(&'='));
        assert_eq!(m.get(&93), Some(&'?'));
        assert_eq!(m.get(&89), Some(&'|'));
        assert_eq!(m.get(&81), Some(&'\''));
        assert_eq!(m.get(&55), Some(&'~'));
        assert_eq!(m.get(&94), Some(&')'));
    }

    #[test]
    fn keymap_file_parses_arrow_comments() {
        let text = "keycode 26 = exclam            # 0xb7 -> !\nkeycode 71 = minus             # 0xc4 -> -\n# comment\n";
        let map = parse_keymap_file(text);
        assert_eq!(map.get(&26), Some(&'!'));
        assert_eq!(map.get(&71), Some(&'-'));
    }

    #[test]
    fn keymap_file_falls_back_to_keysym_names() {
        let map = parse_keymap_file("keycode 52 = asterisk\nkeycode 92 = slash\n");
        assert_eq!(map.get(&52), Some(&'*'));
        assert_eq!(map.get(&92), Some(&'/'));
    }

    #[test]
    fn fixture_poll_tracks_edges_mods_and_text() {
        let ev = encode(&[
            (1, 1000, EV_KEY, 30, 1), // a down
            (1, 1100, EV_SYN, 0, 0),
            (1, 2000, EV_KEY, 30, 0), // a up
            (1, 3000, EV_KEY, 42, 1), // shift down
            (1, 4000, EV_KEY, 30, 1), // A down
            (1, 5000, EV_KEY, 30, 0),
            (1, 6000, EV_KEY, 42, 0),
            (1, 7000, EV_KEY, 29, 1), // ctrl down
            (1, 8000, EV_KEY, 46, 1), // c down (null text)
            (1, 9000, EV_KEY, 46, 0),
            (1, 9500, EV_KEY, 29, 0),
        ]);
        let path = temp_fixture("event3-basic", &ev);
        let mut st = InputState::with_paths(path.clone(), builtin_sym_map());
        let events = st.poll().expect("poll");
        let pressed: Vec<Option<String>> =
            events.iter().filter(|e| e.pressed).map(|e| e.text.clone()).collect();
        assert_eq!(pressed, vec![Some("a".into()), Some("A".into()), None]);
        assert!(st.was_released(30));
        assert!(!st.is_down(30));
        assert!(st.was_pressed(46));
        assert!(!st.mods.shift && !st.mods.ctrl);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn sym_layer_text_and_mod_snapshot_after_poll() {
        let ev = encode(&[
            (1, 1000, EV_KEY, KEY_SYM, 1),
            (1, 2000, EV_KEY, 26, 1),
            (1, 3000, EV_KEY, 71, 1),
            (1, 4000, EV_KEY, 93, 1),
        ]);
        let path = temp_fixture("event3-sym", &ev);
        let mut st = InputState::with_paths(path.clone(), builtin_sym_map());
        let events = st.poll().expect("poll");
        let texts: Vec<String> =
            events.iter().filter(|e| e.pressed).filter_map(|e| e.text.clone()).collect();
        assert_eq!(texts, vec!["!", "-", "?"]);
        let m = st.mods();
        assert!(m.sym && !m.shift && !m.ctrl);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn kernel_repeats_are_ignored() {
        let ev = encode(&[(1, 0, EV_KEY, 30, 1), (1, 50, EV_KEY, 30, 2), (1, 100, EV_KEY, 30, 2)]);
        let path = temp_fixture("event3-repeat-ign", &ev);
        let mut st = InputState::with_paths(path.clone(), builtin_sym_map());
        let events = st.poll().expect("poll");
        let non_repeat_presses = events.iter().filter(|e| e.pressed && !e.repeat).count();
        assert_eq!(non_repeat_presses, 1);
        assert_eq!(events.iter().filter(|e| e.repeat).count(), 0);
        assert!(st.is_down(30));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn repeat_fires_after_delay_and_interval_and_honors_override() {
        // Press 'a' at t=0; advance by 400ms -> first repeat; then 33ms steps.
        let ev = encode(&[(0, 0, EV_KEY, 30, 1)]);
        let path = temp_fixture("event3-repeat-timer", &ev);
        let mut st = InputState::with_paths(path.clone(), builtin_sym_map());
        let first = st.poll().expect("poll");
        assert_eq!(first.iter().filter(|e| e.repeat).count(), 0);

        let r1 = st.advance_repeat(400_000);
        assert_eq!(r1.len(), 1);
        assert!(r1[0].repeat && r1[0].pressed && r1[0].code == 30);
        assert_eq!(r1[0].text, Some("a".into()));
        let r2 = st.advance_repeat(432_000);
        assert_eq!(r2.len(), 0);
        let r3 = st.advance_repeat(433_500);
        assert_eq!(r3.len(), 1);

        // Release stops repeats.
        st.down.remove(&30);
        st.held.remove(&30);
        assert!(st.advance_repeat(9_000_000).is_empty());

        // Scroll key default interval is 50ms unless overridden.
        let ev2 = encode(&[(0, 0, EV_KEY, 108, 1)]);
        let path2 = temp_fixture("event3-scroll", &ev2);
        let mut s2 = InputState::with_paths(path2.clone(), builtin_sym_map());
        s2.poll().expect("poll");
        assert_eq!(s2.advance_repeat(399_000).len(), 0);
        assert_eq!(s2.advance_repeat(400_000).len(), 1);
        assert_eq!(s2.advance_repeat(449_000).len(), 0);
        assert_eq!(s2.advance_repeat(450_000).len(), 1);
        s2.set_repeat(200, 20);
        s2.reset();
        s2.poll().expect("re-poll after reset");
        assert_eq!(s2.advance_repeat(199_000).len(), 0);
        assert_eq!(s2.advance_repeat(200_000).len(), 1);
        assert_eq!(s2.advance_repeat(220_000).len(), 1);

        fs::remove_file(&path).ok();
        fs::remove_file(&path2).ok();
    }

    #[test]
    fn incremental_reads_pick_up_new_appended_events() {
        let path = temp_fixture("event3-append", &encode(&[(1, 0, EV_KEY, 30, 1)]));
        let mut st = InputState::with_paths(path.clone(), builtin_sym_map());
        let p1 = st.poll().expect("poll1");
        assert_eq!(p1.len(), 1);
        assert!(st.is_down(30) && !st.is_down(44));

        // Append a new release event and poll again: only the delta should be returned.
        let mut f = fs::OpenOptions::new().append(true).open(&path).expect("append");
        use std::io::Write;
        f.write_all(&encode(&[(2, 0, EV_KEY, 30, 0), (2, 1, EV_KEY, 44, 1)]))
            .expect("append events");
        drop(f);

        let p2 = st.poll().expect("poll2");
        assert_eq!(p2.len(), 2);
        assert!(!st.is_down(30) && st.is_down(44));
        assert!(st.was_released(30) && st.was_pressed(44));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn reset_clears_state_and_closes_handle() {
        let path = temp_fixture("event3-reset", &encode(&[(1, 0, EV_KEY, 103, 1), (1, 5, EV_KEY, 42, 1)]));
        let mut st = InputState::with_paths(path.clone(), builtin_sym_map());
        st.poll().expect("poll");
        assert!(st.is_down(103) && st.mods.shift);
        st.reset();
        assert!(!st.is_down(103));
        assert!(!st.mods.shift);
        assert!(!st.was_pressed(103));
        // Re-open cleanly: after reset the fixture is read from offset 0 again. The shift
        // press is a modifier and is tracked but never surfaced as a KeyEvent.
        let again = st.poll().expect("re-poll");
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].code, 103);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn key_names_and_digit_text() {
        let m = builtin_sym_map();
        assert_eq!(key_name(103, &m), "Up");
        assert_eq!(key_name(1, &m), "Esc");
        assert_eq!(key_name(59, &m), "F1");
        assert_eq!(key_name(68, &m), "F10");
        assert_eq!(key_name(88, &m), "F12");
        assert_eq!(key_name(30, &m), "A");
        let mods = Mods::default();
        assert_eq!(text_for(2, &mods, &m), Some("1".into()));
        assert_eq!(text_for(11, &mods, &m), Some("0".into()));
        assert_eq!(text_for(57, &mods, &m), Some(" ".into()));
        assert_eq!(text_for(108, &mods, &m), None);
        let shift = Mods { shift: true, ..Mods::default() };
        assert_eq!(text_for(30, &shift, &m), Some("A".into()));
        let ctrl = Mods { ctrl: true, ..Mods::default() };
        assert_eq!(text_for(46, &ctrl, &m), None);
    }

    #[test]
    fn env_var_overrides_input_and_keymap_resolution() {
        let fake_dev = temp_fixture("env-input", &encode(&[]));
        let keymap = temp_fixture(
            "env-keymap.map",
            b"keycode 26 = exclam # 0xb7 -> !\nkeycode 93 = question # 0xd5 -> ?\n",
        );
        std::env::set_var("CARDPUTER_INPUT_PATH", &fake_dev);
        std::env::set_var("CARDPUTER_KEYMAP_PATH", &keymap);
        assert_eq!(resolve_input_path(), fake_dev.clone());
        let map = resolve_sym_map();
        assert_eq!(map.get(&26), Some(&'!'));
        assert_eq!(map.get(&93), Some(&'?'));
        std::env::remove_var("CARDPUTER_INPUT_PATH");
        std::env::remove_var("CARDPUTER_KEYMAP_PATH");
        fs::remove_file(&fake_dev).ok();
        fs::remove_file(&keymap).ok();
    }

    #[test]
    fn parse_input_event_decodes_24_byte_record() {
        let rec = encode(&[(7, 8, EV_KEY, 42, 1)]);
        assert_eq!(rec.len(), INPUT_EVENT_SIZE);
        let raw = parse_input_event(&rec).expect("decode");
        assert_eq!(
            raw,
            InputEventRaw { tv_sec: 7, tv_usec: 8, type_: EV_KEY, code: 42, value: 1 }
        );
        assert!(parse_input_event(&rec[..23]).is_none());
    }
}
