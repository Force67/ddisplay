package com.ddisplay.core.input

/**
 * JavaScript keyCode values the server understands. The wire keycode space is
 * position-based (layout-independent): the server maps each code to a physical
 * key, so its own layout decides the resulting character. This is exactly the
 * set in server/src/input/keymap.rs; codes outside it are dropped server-side.
 */
object JsKeyCodes {
    // Editing / control
    const val BACKSPACE = 8
    const val TAB = 9
    const val ENTER = 13
    const val SHIFT = 16
    const val CONTROL = 17
    const val ALT = 18
    const val PAUSE = 19
    const val CAPS_LOCK = 20
    const val ESCAPE = 27
    const val SPACE = 32
    const val INSERT = 45
    const val DELETE = 46

    // Navigation
    const val PAGE_UP = 33
    const val PAGE_DOWN = 34
    const val END = 35
    const val HOME = 36
    const val ARROW_LEFT = 37
    const val ARROW_UP = 38
    const val ARROW_RIGHT = 39
    const val ARROW_DOWN = 40

    // Digit row
    const val DIGIT_0 = 48
    const val DIGIT_1 = 49
    const val DIGIT_2 = 50
    const val DIGIT_3 = 51
    const val DIGIT_4 = 52
    const val DIGIT_5 = 53
    const val DIGIT_6 = 54
    const val DIGIT_7 = 55
    const val DIGIT_8 = 56
    const val DIGIT_9 = 57

    // Letters (physical QWERTY positions)
    const val A = 65
    const val B = 66
    const val C = 67
    const val D = 68
    const val E = 69
    const val F = 70
    const val G = 71
    const val H = 72
    const val I = 73
    const val J = 74
    const val K = 75
    const val L = 76
    const val M = 77
    const val N = 78
    const val O = 79
    const val P = 80
    const val Q = 81
    const val R = 82
    const val S = 83
    const val T = 84
    const val U = 85
    const val V = 86
    const val W = 87
    const val X = 88
    const val Y = 89
    const val Z = 90

    // Meta / context
    const val META_LEFT = 91
    const val META_RIGHT = 92
    const val CONTEXT_MENU = 93

    // Numpad
    const val NUMPAD_0 = 96
    const val NUMPAD_1 = 97
    const val NUMPAD_2 = 98
    const val NUMPAD_3 = 99
    const val NUMPAD_4 = 100
    const val NUMPAD_5 = 101
    const val NUMPAD_6 = 102
    const val NUMPAD_7 = 103
    const val NUMPAD_8 = 104
    const val NUMPAD_9 = 105
    const val NUMPAD_MULTIPLY = 106
    const val NUMPAD_ADD = 107
    const val NUMPAD_SUBTRACT = 109
    const val NUMPAD_DECIMAL = 110
    const val NUMPAD_DIVIDE = 111

    // Function keys
    const val F1 = 112
    const val F2 = 113
    const val F3 = 114
    const val F4 = 115
    const val F5 = 116
    const val F6 = 117
    const val F7 = 118
    const val F8 = 119
    const val F9 = 120
    const val F10 = 121
    const val F11 = 122
    const val F12 = 123

    // Lock keys
    const val NUM_LOCK = 144
    const val SCROLL_LOCK = 145

    // Punctuation (layout-sensitive, addressed by position)
    const val SEMICOLON = 186
    const val EQUAL = 187
    const val COMMA = 188
    const val MINUS = 189
    const val PERIOD = 190
    const val SLASH = 191
    const val BACKQUOTE = 192
    const val BRACKET_LEFT = 219
    const val BACKSLASH = 220
    const val BRACKET_RIGHT = 221
    const val QUOTE = 222
}
