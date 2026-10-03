//! macOS virtual key codes and modifier flag bits, as both ends of a Mac-to-Mac session see them.
//!
//! Key codes are physical positions (`kVK_*`), so the remote's keyboard layout and input method
//! turn them into characters, as if the keyboard were plugged into that Mac. Modifier state uses
//! the `CGEventFlags` / `NSEvent.modifierFlags` bit layout, including the device-dependent bits
//! that tell left from right (`IOLLEvent.h`).

/// Device-dependent bits (`NX_DEVICE*KEYMASK`): which physical modifier is down.
pub const DEVICE_LCTL: u64 = 0x0000_0001;
pub const DEVICE_LSHIFT: u64 = 0x0000_0002;
pub const DEVICE_RSHIFT: u64 = 0x0000_0004;
pub const DEVICE_LCMD: u64 = 0x0000_0008;
pub const DEVICE_RCMD: u64 = 0x0000_0010;
pub const DEVICE_LALT: u64 = 0x0000_0020;
pub const DEVICE_RALT: u64 = 0x0000_0040;
pub const DEVICE_RCTL: u64 = 0x0000_2000;
pub const DEVICE_MASK: u64 =
    DEVICE_LCTL | DEVICE_LSHIFT | DEVICE_RSHIFT | DEVICE_LCMD | DEVICE_RCMD | DEVICE_LALT | DEVICE_RALT | DEVICE_RCTL;

/// Device-independent bits (`kCGEventFlagMask*`).
pub const CAPS_LOCK: u64 = 0x0001_0000;
pub const SHIFT: u64 = 0x0002_0000;
pub const CONTROL: u64 = 0x0004_0000;
pub const OPTION: u64 = 0x0008_0000;
pub const COMMAND: u64 = 0x0010_0000;
pub const NUMERIC_PAD: u64 = 0x0020_0000;
pub const HELP: u64 = 0x0040_0000;
pub const FN: u64 = 0x0080_0000;

/// Every bit that describes held modifiers (not the per-key Numeric Pad / Help bits).
pub const MODIFIER_MASK: u64 = DEVICE_MASK | CAPS_LOCK | SHIFT | CONTROL | OPTION | COMMAND | FN;

/// An undocumented bit macOS sets on every keyboard event; some keys (F11) need it.
pub const KEYBOARD_EVENT_BIT: u64 = 0x2000_0000;

pub const KEY_CAPS_LOCK: u16 = 57;
pub const KEY_FN: u16 = 63;
/// Highest virtual key code a keyboard produces.
pub const MAX_KEY_CODE: u16 = 0x7F;

/// A modifier key: its generic flag and its device bit (0 for fn, which has none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modifier {
    pub key_code: u16,
    pub flag: u64,
    pub device: u64,
}

/// The modifier keys, in the order they are pressed when syncing state (and released in reverse).
pub const MODIFIERS: [Modifier; 9] = [
    Modifier { key_code: 59, flag: CONTROL, device: DEVICE_LCTL },
    Modifier { key_code: 62, flag: CONTROL, device: DEVICE_RCTL },
    Modifier { key_code: 58, flag: OPTION, device: DEVICE_LALT },
    Modifier { key_code: 61, flag: OPTION, device: DEVICE_RALT },
    Modifier { key_code: 56, flag: SHIFT, device: DEVICE_LSHIFT },
    Modifier { key_code: 60, flag: SHIFT, device: DEVICE_RSHIFT },
    Modifier { key_code: 55, flag: COMMAND, device: DEVICE_LCMD },
    Modifier { key_code: 54, flag: COMMAND, device: DEVICE_RCMD },
    Modifier { key_code: KEY_FN, flag: FN, device: 0 },
];

pub fn modifier(key_code: u16) -> Option<Modifier> {
    MODIFIERS.iter().copied().find(|m| m.key_code == key_code)
}

/// Keys whose state lives in the modifier flags rather than in key down/up events.
pub fn is_modifier_or_caps(key_code: u16) -> bool {
    key_code == KEY_CAPS_LOCK || modifier(key_code).is_some()
}

/// Whether `m` is down in `flags`. A modifier family flag without any device bit (some
/// remappers and virtual keyboards send that) counts as the left key.
pub fn is_down(m: Modifier, flags: u64) -> bool {
    if m.device == 0 {
        return flags & m.flag != 0;
    }
    if flags & m.device != 0 {
        return true;
    }
    let family: u64 = MODIFIERS.iter().filter(|o| o.flag == m.flag).map(|o| o.device).fold(0, |a, b| a | b);
    let is_left = MODIFIERS.iter().find(|o| o.flag == m.flag).is_some_and(|first| first.key_code == m.key_code);
    is_left && flags & m.flag != 0 && flags & family == 0
}

/// Canonical held-modifier state: device bits for every held modifier plus their generic flags,
/// Caps Lock and fn. Anything else in `flags` (per-key bits, coalescing hints) is dropped.
pub fn normalize(flags: u64) -> u64 {
    let mut out = flags & (CAPS_LOCK | FN);
    for m in MODIFIERS {
        if m.device != 0 && is_down(m, flags) {
            out |= m.flag | m.device;
        }
    }
    out
}

/// Flags a key carries by itself, whatever modifiers are held. Measured for all 128 key codes
/// with `CGEventCreateKeyboardEvent` on a fresh private source (macOS 26.5).
pub fn intrinsic_flags(key_code: u16) -> u64 {
    match key_code {
        65 | 67 | 69 | 75 | 76 | 78 | 81..=89 | 91 | 92 => NUMERIC_PAD,
        123..=126 => NUMERIC_PAD | FN,
        114 => HELP | FN,
        64 | 71 | 79 | 80 | 96..=101 | 103 | 105..=107 | 109 | 111 | 113 | 115..=122 => FN,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_table_is_consistent() {
        for m in MODIFIERS {
            assert!(modifier(m.key_code) == Some(m));
            assert!(is_modifier_or_caps(m.key_code));
        }
        assert!(is_modifier_or_caps(KEY_CAPS_LOCK));
        assert!(!is_modifier_or_caps(0));
        assert_eq!(MODIFIERS.iter().map(|m| m.device).fold(0, |a, b| a | b), DEVICE_MASK);
    }

    #[test]
    fn left_and_right_are_told_apart() {
        let lcmd = modifier(55).unwrap();
        let rcmd = modifier(54).unwrap();
        assert!(is_down(lcmd, COMMAND | DEVICE_LCMD) && !is_down(rcmd, COMMAND | DEVICE_LCMD));
        assert!(is_down(rcmd, COMMAND | DEVICE_RCMD) && !is_down(lcmd, COMMAND | DEVICE_RCMD));
        // Family flag with no device bit: treat as the left key.
        assert!(is_down(lcmd, COMMAND) && !is_down(rcmd, COMMAND));
        let fn_key = modifier(KEY_FN).unwrap();
        assert!(is_down(fn_key, FN) && !is_down(fn_key, COMMAND));
    }

    #[test]
    fn normalize_keeps_only_held_state() {
        // Right shift plus the per-key bits an arrow key brings with it.
        let raw = SHIFT | DEVICE_RSHIFT | NUMERIC_PAD | 0x100 | KEYBOARD_EVENT_BIT;
        assert_eq!(normalize(raw), SHIFT | DEVICE_RSHIFT);
        assert_eq!(normalize(OPTION), OPTION | DEVICE_LALT);
        assert_eq!(normalize(CAPS_LOCK | FN), CAPS_LOCK | FN);
        assert_eq!(normalize(0), 0);
    }

    #[test]
    fn intrinsic_flags_match_measurements() {
        assert_eq!(intrinsic_flags(0), 0); // a
        assert_eq!(intrinsic_flags(36), 0); // return
        assert_eq!(intrinsic_flags(90), 0); // F20
        assert_eq!(intrinsic_flags(76), NUMERIC_PAD); // keypad enter
        assert_eq!(intrinsic_flags(123), NUMERIC_PAD | FN); // left arrow
        assert_eq!(intrinsic_flags(122), FN); // F1
        assert_eq!(intrinsic_flags(115), FN); // home
        assert_eq!(intrinsic_flags(117), FN); // forward delete
        assert_eq!(intrinsic_flags(114), HELP | FN);
    }
}
