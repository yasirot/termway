//! XKB keymap for the virtual keyboard and translation of terminal key events.
//!
//! ASCII uses a US layout on the usual evdev codes so shortcuts behave normally.
//! Other characters (IME output, pasted text) get dynamically assigned keycodes,
//! which requires re-uploading the keymap when a new one appears.

use crate::input::{Key, KeyEvent};
use std::fmt::Write;

pub const MOD_SHIFT: u32 = 1;
pub const MOD_CTRL: u32 = 4;
pub const MOD_ALT: u32 = 8;
pub const MOD_LOGO: u32 = 64;

const KEY_ESC: u32 = 1;
const KEY_BACKSPACE: u32 = 14;
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTMETA: u32 = 125;

/// (evdev code, base char, shifted char) for the US layout.
const US: &[(u32, char, char)] = &[
    (2, '1', '!'),
    (3, '2', '@'),
    (4, '3', '#'),
    (5, '4', '$'),
    (6, '5', '%'),
    (7, '6', '^'),
    (8, '7', '&'),
    (9, '8', '*'),
    (10, '9', '('),
    (11, '0', ')'),
    (12, '-', '_'),
    (13, '=', '+'),
    (16, 'q', 'Q'),
    (17, 'w', 'W'),
    (18, 'e', 'E'),
    (19, 'r', 'R'),
    (20, 't', 'T'),
    (21, 'y', 'Y'),
    (22, 'u', 'U'),
    (23, 'i', 'I'),
    (24, 'o', 'O'),
    (25, 'p', 'P'),
    (26, '[', '{'),
    (27, ']', '}'),
    (30, 'a', 'A'),
    (31, 's', 'S'),
    (32, 'd', 'D'),
    (33, 'f', 'F'),
    (34, 'g', 'G'),
    (35, 'h', 'H'),
    (36, 'j', 'J'),
    (37, 'k', 'K'),
    (38, 'l', 'L'),
    (39, ';', ':'),
    (40, '\'', '"'),
    (41, '`', '~'),
    (43, '\\', '|'),
    (44, 'z', 'Z'),
    (45, 'x', 'X'),
    (46, 'c', 'C'),
    (47, 'v', 'V'),
    (48, 'b', 'B'),
    (49, 'n', 'N'),
    (50, 'm', 'M'),
    (51, ',', '<'),
    (52, '.', '>'),
    (53, '/', '?'),
    (57, ' ', ' '),
];

/// (evdev code, keysym) for non-character keys.
const NAMED: &[(u32, &str)] = &[
    (KEY_ESC, "Escape"),
    (KEY_BACKSPACE, "BackSpace"),
    (KEY_ENTER, "Return"),
    (KEY_LEFTCTRL, "Control_L"),
    (KEY_LEFTSHIFT, "Shift_L"),
    (KEY_LEFTALT, "Alt_L"),
    (KEY_LEFTMETA, "Super_L"),
    (59, "F1"),
    (60, "F2"),
    (61, "F3"),
    (62, "F4"),
    (63, "F5"),
    (64, "F6"),
    (65, "F7"),
    (66, "F8"),
    (67, "F9"),
    (68, "F10"),
    (87, "F11"),
    (88, "F12"),
    (102, "Home"),
    (103, "Up"),
    (104, "Prior"),
    (105, "Left"),
    (106, "Right"),
    (107, "End"),
    (108, "Down"),
    (109, "Next"),
    (110, "Insert"),
    (111, "Delete"),
];

/// First evdev code used for dynamically mapped characters (beyond real keyboards).
const DYN_BASE: u32 = 0x300;
pub const DYN_SLOTS: usize = 64;

fn keysym_name(c: char) -> String {
    let named = match c {
        ' ' => "space",
        '!' => "exclam",
        '"' => "quotedbl",
        '#' => "numbersign",
        '$' => "dollar",
        '%' => "percent",
        '&' => "ampersand",
        '\'' => "apostrophe",
        '(' => "parenleft",
        ')' => "parenright",
        '*' => "asterisk",
        '+' => "plus",
        ',' => "comma",
        '-' => "minus",
        '.' => "period",
        '/' => "slash",
        ':' => "colon",
        ';' => "semicolon",
        '<' => "less",
        '=' => "equal",
        '>' => "greater",
        '?' => "question",
        '@' => "at",
        '[' => "bracketleft",
        '\\' => "backslash",
        ']' => "bracketright",
        '^' => "asciicircum",
        '_' => "underscore",
        '`' => "grave",
        '{' => "braceleft",
        '|' => "bar",
        '}' => "braceright",
        '~' => "asciitilde",
        c if c.is_ascii_alphanumeric() => return c.to_string(),
        c => return format!("U{:04X}", c as u32),
    };
    named.to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    /// evdev key code (as sent in `zwp_virtual_keyboard_v1.key`).
    pub code: u32,
    /// Real modifier mask (Shift=1, Control=4, Mod1=8, Mod4=64).
    pub mods: u32,
}

#[derive(Debug, Default)]
pub struct Keymap {
    /// Dynamically mapped characters, most recently used last.
    dynamic: Vec<(char, u32)>,
}

impl Keymap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether typing `c` needs a dynamically assigned keycode (it is not on the US layout).
    pub fn needs_dynamic(c: char) -> bool {
        Self::us(c).is_none() && !c.is_control()
    }

    fn us(c: char) -> Option<(u32, bool)> {
        US.iter().find_map(|&(code, lo, hi)| {
            if c == lo {
                Some((code, false))
            } else if c == hi {
                Some((code, true))
            } else {
                None
            }
        })
    }

    /// Makes sure every character has a keycode; returns true if the keymap changed
    /// and must be uploaded before sending the corresponding strokes.
    pub fn prepare(&mut self, chars: impl IntoIterator<Item = char>) -> bool {
        let mut changed = false;
        for c in chars {
            if !Self::needs_dynamic(c) {
                continue;
            }
            if let Some(pos) = self.dynamic.iter().position(|&(d, _)| d == c) {
                let e = self.dynamic.remove(pos);
                self.dynamic.push(e);
                continue;
            }
            let code = if self.dynamic.len() < DYN_SLOTS {
                DYN_BASE + self.dynamic.len() as u32
            } else {
                self.dynamic.remove(0).1
            };
            self.dynamic.push((c, code));
            changed = true;
        }
        changed
    }

    /// Translates a key event. Characters outside the US layout must have been passed
    /// to [`Keymap::prepare`] first.
    pub fn stroke(&self, ev: &KeyEvent) -> Option<Stroke> {
        let mut mods = 0;
        if ev.mods.shift {
            mods |= MOD_SHIFT;
        }
        if ev.mods.ctrl {
            mods |= MOD_CTRL;
        }
        if ev.mods.alt {
            mods |= MOD_ALT;
        }
        if ev.mods.logo {
            mods |= MOD_LOGO;
        }
        let code = match ev.key {
            Key::Char(c) => {
                if let Some((code, shifted)) = Self::us(c) {
                    if shifted {
                        mods |= MOD_SHIFT;
                    }
                    code
                } else {
                    self.dynamic.iter().find(|&&(d, _)| d == c)?.1
                }
            }
            Key::Enter => KEY_ENTER,
            Key::Tab => KEY_TAB,
            Key::Backspace => KEY_BACKSPACE,
            Key::Escape => KEY_ESC,
            Key::Up => 103,
            Key::Down => 108,
            Key::Left => 105,
            Key::Right => 106,
            Key::Home => 102,
            Key::End => 107,
            Key::PageUp => 104,
            Key::PageDown => 109,
            Key::Insert => 110,
            Key::Delete => 111,
            Key::F(n @ 1..=10) => 58 + n as u32,
            Key::F(11) => 87,
            Key::F(12) => 88,
            Key::F(_) => return None,
        };
        Some(Stroke { code, mods })
    }

    /// XKB keymap source (xkb_v1 text format).
    pub fn text(&self) -> String {
        let mut codes: Vec<u32> = US.iter().map(|k| k.0).chain(NAMED.iter().map(|k| k.0)).collect();
        codes.push(KEY_TAB);
        codes.extend(self.dynamic.iter().map(|d| d.1));
        codes.sort_unstable();
        codes.dedup();
        let max = codes.last().copied().unwrap_or(0) + 8;
        let mut s = String::with_capacity(8192);
        s.push_str("xkb_keymap {\nxkb_keycodes \"termway\" {\n    minimum = 8;\n");
        let _ = writeln!(s, "    maximum = {};", max.max(255));
        for c in &codes {
            let _ = writeln!(s, "    <K{c}> = {};", c + 8);
        }
        // Xwayland (24.1) crashes answering XkbGetIndicatorMap, as i3 sends at startup,
        // for a keymap without indicators.
        s.push_str(
            "    indicator 1 = \"Caps Lock\";\n\
    indicator 2 = \"Num Lock\";\n\
    indicator 3 = \"Scroll Lock\";\n\
};\n\
xkb_types \"termway\" {\n\
    virtual_modifiers Alt,Super,Meta;\n\
    type \"ONE_LEVEL\" { modifiers = none; level_name[Level1] = \"Any\"; };\n\
    type \"TWO_LEVEL\" { modifiers = Shift; map[Shift] = Level2; level_name[Level1] = \"Base\"; level_name[Level2] = \"Shift\"; };\n\
    type \"ALPHABETIC\" { modifiers = Shift+Lock; map[Shift] = Level2; map[Lock] = Level2; level_name[Level1] = \"Base\"; level_name[Level2] = \"Caps\"; };\n\
};\n\
xkb_compatibility \"termway\" {\n\
    virtual_modifiers Alt,Super,Meta;\n\
    interpret Shift_L { action = SetMods(modifiers = Shift); };\n\
    interpret Control_L { action = SetMods(modifiers = Control); };\n\
    interpret Alt_L { virtualModifier = Alt; action = SetMods(modifiers = modMapMods); };\n\
    interpret Super_L { virtualModifier = Super; action = SetMods(modifiers = modMapMods); };\n\
    indicator \"Caps Lock\" { whichModState = Locked; modifiers = Lock; };\n\
};\n\
xkb_symbols \"termway\" {\n",
        );
        for &(code, lo, hi) in US {
            if lo == hi {
                let _ = writeln!(s, "    key <K{code}> {{ [ {} ] }};", keysym_name(lo));
            } else {
                let _ = writeln!(s, "    key <K{code}> {{ [ {}, {} ] }};", keysym_name(lo), keysym_name(hi));
            }
        }
        let _ = writeln!(s, "    key <K{KEY_TAB}> {{ [ Tab, ISO_Left_Tab ] }};");
        for &(code, sym) in NAMED {
            let _ = writeln!(s, "    key <K{code}> {{ [ {sym} ] }};");
        }
        for &(c, code) in &self.dynamic {
            let _ = writeln!(s, "    key <K{code}> {{ [ {} ] }};", keysym_name(c));
        }
        let _ = writeln!(s, "    modifier_map Shift {{ <K{KEY_LEFTSHIFT}> }};");
        let _ = writeln!(s, "    modifier_map Control {{ <K{KEY_LEFTCTRL}> }};");
        let _ = writeln!(s, "    modifier_map Mod1 {{ <K{KEY_LEFTALT}> }};");
        let _ = writeln!(s, "    modifier_map Mod4 {{ <K{KEY_LEFTMETA}> }};");
        s.push_str("};\n};\n");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    fn ev(key: Key, mods: Mods) -> KeyEvent {
        KeyEvent { key, mods }
    }

    #[test]
    fn ascii_uses_us_layout() {
        let k = Keymap::new();
        assert_eq!(k.stroke(&ev(Key::Char('a'), Mods::NONE)), Some(Stroke { code: 30, mods: 0 }));
        assert_eq!(k.stroke(&ev(Key::Char('A'), Mods::NONE)), Some(Stroke { code: 30, mods: MOD_SHIFT }));
        assert_eq!(k.stroke(&ev(Key::Char('?'), Mods::NONE)), Some(Stroke { code: 53, mods: MOD_SHIFT }));
        let ctrl_alt = Mods { ctrl: true, alt: true, ..Mods::NONE };
        assert_eq!(k.stroke(&ev(Key::Char('c'), ctrl_alt)), Some(Stroke { code: 46, mods: MOD_CTRL | MOD_ALT }));
        assert_eq!(k.stroke(&ev(Key::F(12), Mods::NONE)), Some(Stroke { code: 88, mods: 0 }));
        assert_eq!(k.stroke(&ev(Key::F(5), Mods::NONE)), Some(Stroke { code: 63, mods: 0 }));
        assert_eq!(k.stroke(&ev(Key::F(13), Mods::NONE)), None);
    }

    #[test]
    fn unicode_gets_dynamic_codes() {
        let mut k = Keymap::new();
        assert_eq!(k.stroke(&ev(Key::Char('あ'), Mods::NONE)), None, "unprepared");
        assert!(k.prepare("aあい".chars()));
        assert!(!k.prepare("あい".chars()), "already mapped");
        let a = k.stroke(&ev(Key::Char('あ'), Mods::NONE)).unwrap();
        let i = k.stroke(&ev(Key::Char('い'), Mods::NONE)).unwrap();
        assert_eq!((a.code, i.code), (DYN_BASE, DYN_BASE + 1));
        let text = k.text();
        assert!(text.contains(&format!("<K{}> = {};", DYN_BASE, DYN_BASE + 8)));
        assert!(text.contains("[ U3042 ]"));
    }

    #[test]
    fn dynamic_slots_are_recycled_lru() {
        let mut k = Keymap::new();
        let chars: Vec<char> = (0..DYN_SLOTS as u32).map(|i| char::from_u32(0x4e00 + i).unwrap()).collect();
        k.prepare(chars.iter().copied());
        k.prepare(['\u{4e00}']);
        assert!(k.prepare(['★']));
        assert!(k.stroke(&ev(Key::Char('\u{4e00}'), Mods::NONE)).is_some(), "recently used survives");
        assert!(k.stroke(&ev(Key::Char('\u{4e01}'), Mods::NONE)).is_none(), "oldest recycled");
        assert_eq!(k.stroke(&ev(Key::Char('★'), Mods::NONE)).unwrap().code, DYN_BASE + 1);
    }

    #[test]
    fn keymap_text_is_balanced() {
        let mut k = Keymap::new();
        k.prepare(['é']);
        let t = k.text();
        assert_eq!(t.matches('{').count(), t.matches('}').count());
        for (code, _, _) in US {
            assert!(t.contains(&format!("<K{code}> = {};", code + 8)));
        }
        assert!(t.contains("modifier_map Mod1"));
    }
}
