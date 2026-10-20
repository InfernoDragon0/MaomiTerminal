//! Windows win32-input-mode key encoding (`CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`).
//!
//! ConPTY asks for it with `CSI ? 9001 h`. Console programs then see real key records, so
//! Shift+Enter, Ctrl+Space, Shift+Tab and Ctrl+Shift combos stay distinguishable. Pure functions.

use gpui_kit::{Keystroke, Modifiers};

const RIGHT_ALT: u16 = 0x1;
const LEFT_ALT: u16 = 0x2;
const LEFT_CTRL: u16 = 0x8;
const SHIFT: u16 = 0x10;
const ENHANCED_KEY: u16 = 0x100;

/// Records for a key press: a key-down immediately followed by the key-up (GPUI reports
/// only key-down). None when the key has no mapping, so the caller falls back to VT bytes.
pub(crate) fn encode_win32_key(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    if m.platform {
        return None;
    }
    let key = keystroke.key.as_str();
    let typed = keystroke.key_char.as_deref().filter(|t| !t.is_empty());
    // AltGr arrives as Ctrl+Alt with the composed character; plain Ctrl+Alt+letter has none.
    let alt_gr =
        m.control && m.alt && typed.is_some_and(|t| t != key && !t.starts_with(char::is_control));

    let mut state = 0;
    if m.shift {
        state |= SHIFT;
    }
    if alt_gr {
        state |= LEFT_CTRL | RIGHT_ALT;
    } else {
        if m.control {
            state |= LEFT_CTRL;
        }
        if m.alt {
            state |= LEFT_ALT;
        }
    }

    // Text only counts when no Ctrl chord replaces it.
    let text = typed.filter(|_| !m.control || alt_gr);
    let Some((vk, enhanced)) = virtual_key(key) else {
        // No virtual key (IME or other layouts' characters): send the text as character records.
        return text.map(|text| text_records(text, state));
    };
    // More than one UTF-16 unit cannot fit a single record.
    if let Some(text) = text
        && text.encode_utf16().nth(1).is_some()
    {
        return Some(text_records(text, state));
    }
    let state = if enhanced {
        state | ENHANCED_KEY
    } else {
        state
    };
    let unit = unicode_unit(key, typed, m, alt_gr);
    let mut out = Vec::with_capacity(64);
    write_record(&mut out, vk, unit, true, state);
    write_record(&mut out, vk, unit, false, state);
    Some(out)
}

/// The UTF-16 unit the key produces, 0 for none.
fn unicode_unit(key: &str, typed: Option<&str>, m: &Modifiers, alt_gr: bool) -> u16 {
    let single = || {
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii() => Some(c),
            _ => None,
        }
    };
    if m.control && !alt_gr {
        return match single() {
            Some(c @ 'a'..='z') => c as u16 - 'a' as u16 + 1,
            Some('[') => 0x1b,
            Some('\\') => 0x1c,
            Some(']') => 0x1d,
            _ => match key {
                "enter" => 13,
                "tab" => 9,
                "escape" => 27,
                "backspace" => 8,
                _ => 0,
            },
        };
    }
    match key {
        "enter" => return 13,
        "tab" => return 9,
        "escape" => return 27,
        "backspace" => return 8,
        "space" => return 32,
        _ => {}
    }
    if let Some(unit) = typed.and_then(|t| t.encode_utf16().next()) {
        return unit;
    }
    // Keystrokes without key_char (Alt+letter on some layouts): derive it from the key name.
    match single() {
        Some(c) if c.is_ascii_alphabetic() => u16::from(if m.shift {
            c.to_ascii_uppercase()
        } else {
            c.to_ascii_lowercase()
        } as u8),
        Some(c) if c.is_ascii_digit() && !m.shift => u16::from(c as u8),
        _ => 0,
    }
}

/// Character records (VK 0) for text that has no key of its own.
fn text_records(text: &str, state: u16) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in text.encode_utf16() {
        write_record(&mut out, 0, unit, true, state);
        write_record(&mut out, 0, unit, false, state);
    }
    out
}

fn write_record(out: &mut Vec<u8>, vk: u16, unit: u16, down: bool, state: u16) {
    // The scan code is 0: ConPTY rebuilds input from the virtual key and the character.
    out.extend_from_slice(format!("\x1b[{vk};0;{unit};{};{state};1_", u8::from(down)).as_bytes());
}

/// Windows virtual-key code for a GPUI key name, and whether it is an "enhanced" key.
fn virtual_key(key: &str) -> Option<(u16, bool)> {
    let named = |vk: u16| Some((vk, false));
    let enhanced = |vk: u16| Some((vk, true));
    match key {
        "enter" => named(0x0d),
        "tab" => named(0x09),
        "escape" => named(0x1b),
        "backspace" => named(0x08),
        "space" => named(0x20),
        "left" => enhanced(0x25),
        "up" => enhanced(0x26),
        "right" => enhanced(0x27),
        "down" => enhanced(0x28),
        "home" => enhanced(0x24),
        "end" => enhanced(0x23),
        "pageup" => enhanced(0x21),
        "pagedown" => enhanced(0x22),
        "insert" => enhanced(0x2d),
        "delete" => enhanced(0x2e),
        ";" => named(0xba),
        "=" => named(0xbb),
        "," => named(0xbc),
        "-" => named(0xbd),
        "." => named(0xbe),
        "/" => named(0xbf),
        "`" => named(0xc0),
        "[" => named(0xdb),
        "\\" => named(0xdc),
        "]" => named(0xdd),
        "'" => named(0xde),
        _ => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c @ 'a'..='z'), None) => named(c.to_ascii_uppercase() as u16),
                (Some(c @ '0'..='9'), None) => named(c as u16),
                (Some('f'), Some(_)) => {
                    let n: u16 = key[1..].parse().ok()?;
                    (1..=24).contains(&n).then(|| (0x70 + n - 1, false))
                }
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Option<String> {
        encode_win32_key(&Keystroke::parse(s).unwrap()).map(|b| String::from_utf8(b).unwrap())
    }

    fn typed(s: &str, ch: &str) -> Option<String> {
        let mut k = Keystroke::parse(s).unwrap();
        k.key_char = Some(ch.to_owned());
        encode_win32_key(&k).map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn shift_enter_and_shift_tab() {
        assert_eq!(
            key("shift-enter").unwrap(),
            "\x1b[13;0;13;1;16;1_\x1b[13;0;13;0;16;1_"
        );
        assert_eq!(
            key("shift-tab").unwrap(),
            "\x1b[9;0;9;1;16;1_\x1b[9;0;9;0;16;1_"
        );
    }

    #[test]
    fn control_keys() {
        assert_eq!(
            key("ctrl-c").unwrap(),
            "\x1b[67;0;3;1;8;1_\x1b[67;0;3;0;8;1_"
        );
        assert_eq!(
            key("ctrl-space").unwrap(),
            "\x1b[32;0;0;1;8;1_\x1b[32;0;0;0;8;1_"
        );
        // Ctrl+Shift+letter reaches the program too.
        assert_eq!(
            key("ctrl-shift-k").unwrap(),
            "\x1b[75;0;11;1;24;1_\x1b[75;0;11;0;24;1_"
        );
    }

    #[test]
    fn letters_carry_their_character() {
        assert_eq!(
            typed("a", "a").unwrap(),
            "\x1b[65;0;97;1;0;1_\x1b[65;0;97;0;0;1_"
        );
        assert_eq!(
            typed("shift-a", "A").unwrap(),
            "\x1b[65;0;65;1;16;1_\x1b[65;0;65;0;16;1_"
        );
        assert_eq!(
            typed("shift-1", "!").unwrap(),
            "\x1b[49;0;33;1;16;1_\x1b[49;0;33;0;16;1_"
        );
        assert_eq!(
            typed(";", ";").unwrap(),
            "\x1b[186;0;59;1;0;1_\x1b[186;0;59;0;0;1_"
        );
    }

    #[test]
    fn alt_letter() {
        assert_eq!(
            typed("alt-b", "b").unwrap(),
            "\x1b[66;0;98;1;2;1_\x1b[66;0;98;0;2;1_"
        );
        // Without key_char the character comes from the key name.
        assert_eq!(
            key("alt-b").unwrap(),
            "\x1b[66;0;98;1;2;1_\x1b[66;0;98;0;2;1_"
        );
    }

    #[test]
    fn navigation_and_function_keys() {
        assert_eq!(
            key("up").unwrap(),
            "\x1b[38;0;0;1;256;1_\x1b[38;0;0;0;256;1_"
        );
        assert_eq!(
            key("ctrl-left").unwrap(),
            "\x1b[37;0;0;1;264;1_\x1b[37;0;0;0;264;1_"
        );
        assert_eq!(
            key("delete").unwrap(),
            "\x1b[46;0;0;1;256;1_\x1b[46;0;0;0;256;1_"
        );
        assert_eq!(key("f5").unwrap(), "\x1b[116;0;0;1;0;1_\x1b[116;0;0;0;0;1_");
        assert_eq!(
            key("f24").unwrap(),
            "\x1b[135;0;0;1;0;1_\x1b[135;0;0;0;0;1_"
        );
        assert_eq!(key("f25"), None);
    }

    #[test]
    fn plain_special_keys() {
        assert_eq!(
            key("enter").unwrap(),
            "\x1b[13;0;13;1;0;1_\x1b[13;0;13;0;0;1_"
        );
        assert_eq!(
            key("backspace").unwrap(),
            "\x1b[8;0;8;1;0;1_\x1b[8;0;8;0;0;1_"
        );
        assert_eq!(
            key("escape").unwrap(),
            "\x1b[27;0;27;1;0;1_\x1b[27;0;27;0;0;1_"
        );
        assert_eq!(
            key("space").unwrap(),
            "\x1b[32;0;32;1;0;1_\x1b[32;0;32;0;0;1_"
        );
    }

    #[test]
    fn altgr_keeps_the_composed_character() {
        assert_eq!(
            typed("ctrl-alt-q", "@").unwrap(),
            "\x1b[81;0;64;1;9;1_\x1b[81;0;64;0;9;1_"
        );
        // Plain Ctrl+Alt+letter is a control chord.
        assert_eq!(
            key("ctrl-alt-c").unwrap(),
            "\x1b[67;0;3;1;10;1_\x1b[67;0;3;0;10;1_"
        );
    }

    #[test]
    fn keys_without_a_virtual_key() {
        // Characters from other layouts go out as character records.
        assert_eq!(
            typed("\u{e9}", "\u{e9}").unwrap(),
            "\x1b[0;0;233;1;0;1_\x1b[0;0;233;0;0;1_"
        );
        // Outside the BMP: one record pair per UTF-16 unit.
        assert_eq!(
            typed("\u{1f600}", "\u{1f600}").unwrap(),
            "\x1b[0;0;55357;1;0;1_\x1b[0;0;55357;0;0;1_\x1b[0;0;56832;1;0;1_\x1b[0;0;56832;0;0;1_"
        );
        assert_eq!(key("numpad5"), None);
        assert_eq!(key("cmd-a"), None);
    }
}
