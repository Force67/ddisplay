//! Shared JS-keyCode → keycode mapping used by all input backends.
//!
//! Clients send JavaScript `keyCode`s (position-based, layout-independent).
//! X11 wants "hardware keycodes" (= Linux evdev scancode + 8); Wayland's
//! Mutter RemoteDesktop API wants raw evdev keycodes.

/// Map a JS keyCode (position-based, layout-independent) directly to an
/// X11 hardware keycode (= Linux evdev scancode + 8).
///
/// This table is keyed on physical key position, not character value.
/// Pressing the Minus key on a German keyboard sends JS keycode 189 and
/// produces X11 keycode 20 — the server's German layout then translates
/// keycode 20 to 'ß', which is what the user expects.
pub fn js_keycode_to_x11_hw(js: u32) -> Option<u8> {
    Some(match js {
        // Editing / control
        8  => 22,  // Backspace
        9  => 23,  // Tab
        13 => 36,  // Enter
        16 => 50,  // LShift
        17 => 37,  // LCtrl
        18 => 64,  // LAlt
        19 => 127, // Pause  (evdev 119 → X11 127)
        20 => 66,  // CapsLock
        27 => 9,   // Escape
        32 => 65,  // Space
        45 => 118, // Insert
        46 => 119, // Delete

        // Navigation
        33 => 112, // PageUp
        34 => 117, // PageDown
        35 => 115, // End
        36 => 110, // Home
        37 => 113, // ArrowLeft
        38 => 111, // ArrowUp
        39 => 114, // ArrowRight
        40 => 116, // ArrowDown

        // Digit row (48–57)
        48 => 19,  // 0
        49 => 10,  // 1
        50 => 11,  // 2
        51 => 12,  // 3
        52 => 13,  // 4
        53 => 14,  // 5
        54 => 15,  // 6
        55 => 16,  // 7
        56 => 17,  // 8
        57 => 18,  // 9

        // Letters A–Z (QWERTY physical positions)
        65 => 38,  // A
        66 => 56,  // B
        67 => 54,  // C
        68 => 40,  // D
        69 => 26,  // E
        70 => 41,  // F
        71 => 42,  // G
        72 => 43,  // H
        73 => 31,  // I
        74 => 44,  // J
        75 => 45,  // K
        76 => 46,  // L
        77 => 58,  // M
        78 => 57,  // N
        79 => 32,  // O
        80 => 33,  // P
        81 => 24,  // Q
        82 => 27,  // R
        83 => 39,  // S
        84 => 28,  // T
        85 => 30,  // U
        86 => 55,  // V
        87 => 25,  // W
        88 => 53,  // X
        89 => 29,  // Y
        90 => 52,  // Z

        // Meta / context
        91 => 133, // LMeta/LSuper
        92 => 134, // RMeta/RSuper
        93 => 135, // ContextMenu

        // Numpad 0–9
        96  => 90,  // KP_0
        97  => 87,  // KP_1
        98  => 88,  // KP_2
        99  => 89,  // KP_3
        100 => 83,  // KP_4
        101 => 84,  // KP_5
        102 => 85,  // KP_6
        103 => 79,  // KP_7
        104 => 80,  // KP_8
        105 => 81,  // KP_9

        // Numpad operators
        106 => 63,  // KP_Multiply
        107 => 86,  // KP_Add
        109 => 82,  // KP_Subtract
        110 => 91,  // KP_Decimal
        111 => 106, // KP_Divide

        // F1–F12
        112 => 67,  // F1
        113 => 68,  // F2
        114 => 69,  // F3
        115 => 70,  // F4
        116 => 71,  // F5
        117 => 72,  // F6
        118 => 73,  // F7
        119 => 74,  // F8
        120 => 75,  // F9
        121 => 76,  // F10
        122 => 95,  // F11  (evdev 87 → X11 95)
        123 => 96,  // F12  (evdev 88 → X11 96)

        // Lock keys
        144 => 77,  // NumLock
        145 => 78,  // ScrollLock

        // Punctuation (layout-sensitive — position-based)
        186 => 47,  // Semicolon / ;
        187 => 21,  // Equals / =
        188 => 59,  // Comma / ,
        189 => 20,  // Minus / -
        190 => 60,  // Period / .
        191 => 61,  // Slash / /
        192 => 49,  // Backtick / `
        219 => 34,  // BracketLeft / [
        220 => 51,  // Backslash / \
        221 => 35,  // BracketRight / ]
        222 => 48,  // Quote / '

        _ => return None,
    })
}

/// Map a JS keyCode to a Linux evdev keycode (what Wayland/Mutter wants).
pub fn js_keycode_to_evdev(js: u32) -> Option<u32> {
    js_keycode_to_x11_hw(js).map(|x11| x11 as u32 - 8)
}
