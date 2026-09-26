package com.ddisplay.app.input

import android.view.KeyEvent
import com.ddisplay.core.input.JsKeyCodes

/** A JS keyCode plus whether a US layout needs Shift held to produce the character. */
data class KeyStroke(val jsKeyCode: Int, val shift: Boolean)

/**
 * Android keyboard glue over [JsKeyCodes]. Physical keys map by position, so the
 * server's own layout decides the resulting character (same contract as the
 * native client). Typed characters that have no direct key on a US layout get a
 * synthesised Shift; anything unmappable is left for the caller to paste().
 */
object AndroidKeyMap {

    /** Map an [android.view.KeyEvent] keyCode to a JS keyCode, or null if unsupported. */
    fun androidKeyToJs(keyCode: Int): Int? = when (keyCode) {
        in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z -> JsKeyCodes.A + (keyCode - KeyEvent.KEYCODE_A)
        in KeyEvent.KEYCODE_0..KeyEvent.KEYCODE_9 -> JsKeyCodes.DIGIT_0 + (keyCode - KeyEvent.KEYCODE_0)
        in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 -> JsKeyCodes.F1 + (keyCode - KeyEvent.KEYCODE_F1)
        in KeyEvent.KEYCODE_NUMPAD_0..KeyEvent.KEYCODE_NUMPAD_9 ->
            JsKeyCodes.NUMPAD_0 + (keyCode - KeyEvent.KEYCODE_NUMPAD_0)

        KeyEvent.KEYCODE_DEL -> JsKeyCodes.BACKSPACE
        KeyEvent.KEYCODE_FORWARD_DEL -> JsKeyCodes.DELETE
        KeyEvent.KEYCODE_TAB -> JsKeyCodes.TAB
        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> JsKeyCodes.ENTER
        KeyEvent.KEYCODE_ESCAPE -> JsKeyCodes.ESCAPE
        KeyEvent.KEYCODE_SPACE -> JsKeyCodes.SPACE
        KeyEvent.KEYCODE_BREAK -> JsKeyCodes.PAUSE
        KeyEvent.KEYCODE_CAPS_LOCK -> JsKeyCodes.CAPS_LOCK
        KeyEvent.KEYCODE_INSERT -> JsKeyCodes.INSERT

        KeyEvent.KEYCODE_DPAD_LEFT -> JsKeyCodes.ARROW_LEFT
        KeyEvent.KEYCODE_DPAD_UP -> JsKeyCodes.ARROW_UP
        KeyEvent.KEYCODE_DPAD_RIGHT -> JsKeyCodes.ARROW_RIGHT
        KeyEvent.KEYCODE_DPAD_DOWN -> JsKeyCodes.ARROW_DOWN
        KeyEvent.KEYCODE_MOVE_HOME -> JsKeyCodes.HOME
        KeyEvent.KEYCODE_MOVE_END -> JsKeyCodes.END
        KeyEvent.KEYCODE_PAGE_UP -> JsKeyCodes.PAGE_UP
        KeyEvent.KEYCODE_PAGE_DOWN -> JsKeyCodes.PAGE_DOWN

        KeyEvent.KEYCODE_SHIFT_LEFT, KeyEvent.KEYCODE_SHIFT_RIGHT -> JsKeyCodes.SHIFT
        KeyEvent.KEYCODE_CTRL_LEFT, KeyEvent.KEYCODE_CTRL_RIGHT -> JsKeyCodes.CONTROL
        KeyEvent.KEYCODE_ALT_LEFT, KeyEvent.KEYCODE_ALT_RIGHT -> JsKeyCodes.ALT
        KeyEvent.KEYCODE_META_LEFT -> JsKeyCodes.META_LEFT
        KeyEvent.KEYCODE_META_RIGHT -> JsKeyCodes.META_RIGHT
        KeyEvent.KEYCODE_MENU -> JsKeyCodes.CONTEXT_MENU

        KeyEvent.KEYCODE_SEMICOLON -> JsKeyCodes.SEMICOLON
        KeyEvent.KEYCODE_EQUALS -> JsKeyCodes.EQUAL
        KeyEvent.KEYCODE_COMMA -> JsKeyCodes.COMMA
        KeyEvent.KEYCODE_MINUS -> JsKeyCodes.MINUS
        KeyEvent.KEYCODE_PERIOD -> JsKeyCodes.PERIOD
        KeyEvent.KEYCODE_SLASH -> JsKeyCodes.SLASH
        KeyEvent.KEYCODE_GRAVE -> JsKeyCodes.BACKQUOTE
        KeyEvent.KEYCODE_LEFT_BRACKET -> JsKeyCodes.BRACKET_LEFT
        KeyEvent.KEYCODE_BACKSLASH -> JsKeyCodes.BACKSLASH
        KeyEvent.KEYCODE_RIGHT_BRACKET -> JsKeyCodes.BRACKET_RIGHT
        KeyEvent.KEYCODE_APOSTROPHE -> JsKeyCodes.QUOTE

        KeyEvent.KEYCODE_NUMPAD_MULTIPLY -> JsKeyCodes.NUMPAD_MULTIPLY
        KeyEvent.KEYCODE_NUMPAD_ADD -> JsKeyCodes.NUMPAD_ADD
        KeyEvent.KEYCODE_NUMPAD_SUBTRACT -> JsKeyCodes.NUMPAD_SUBTRACT
        KeyEvent.KEYCODE_NUMPAD_DOT -> JsKeyCodes.NUMPAD_DECIMAL
        KeyEvent.KEYCODE_NUMPAD_DIVIDE -> JsKeyCodes.NUMPAD_DIVIDE
        KeyEvent.KEYCODE_NUM_LOCK -> JsKeyCodes.NUM_LOCK
        KeyEvent.KEYCODE_SCROLL_LOCK -> JsKeyCodes.SCROLL_LOCK

        else -> null
    }

    /**
     * The JS keyCode (and Shift requirement) that types [c] on a US layout, or
     * null when the character has no key there and should be pasted instead.
     */
    fun charToKeyStroke(c: Char): KeyStroke? = when (c) {
        in 'a'..'z' -> KeyStroke(JsKeyCodes.A + (c - 'a'), shift = false)
        in 'A'..'Z' -> KeyStroke(JsKeyCodes.A + (c - 'A'), shift = true)
        in '0'..'9' -> KeyStroke(JsKeyCodes.DIGIT_0 + (c - '0'), shift = false)
        ' ' -> KeyStroke(JsKeyCodes.SPACE, false)
        '\n' -> KeyStroke(JsKeyCodes.ENTER, false)
        '\t' -> KeyStroke(JsKeyCodes.TAB, false)

        ')' -> KeyStroke(JsKeyCodes.DIGIT_0, true)
        '!' -> KeyStroke(JsKeyCodes.DIGIT_1, true)
        '@' -> KeyStroke(JsKeyCodes.DIGIT_2, true)
        '#' -> KeyStroke(JsKeyCodes.DIGIT_3, true)
        '$' -> KeyStroke(JsKeyCodes.DIGIT_4, true)
        '%' -> KeyStroke(JsKeyCodes.DIGIT_5, true)
        '^' -> KeyStroke(JsKeyCodes.DIGIT_6, true)
        '&' -> KeyStroke(JsKeyCodes.DIGIT_7, true)
        '*' -> KeyStroke(JsKeyCodes.DIGIT_8, true)
        '(' -> KeyStroke(JsKeyCodes.DIGIT_9, true)

        ';' -> KeyStroke(JsKeyCodes.SEMICOLON, false)
        ':' -> KeyStroke(JsKeyCodes.SEMICOLON, true)
        '=' -> KeyStroke(JsKeyCodes.EQUAL, false)
        '+' -> KeyStroke(JsKeyCodes.EQUAL, true)
        ',' -> KeyStroke(JsKeyCodes.COMMA, false)
        '<' -> KeyStroke(JsKeyCodes.COMMA, true)
        '-' -> KeyStroke(JsKeyCodes.MINUS, false)
        '_' -> KeyStroke(JsKeyCodes.MINUS, true)
        '.' -> KeyStroke(JsKeyCodes.PERIOD, false)
        '>' -> KeyStroke(JsKeyCodes.PERIOD, true)
        '/' -> KeyStroke(JsKeyCodes.SLASH, false)
        '?' -> KeyStroke(JsKeyCodes.SLASH, true)
        '`' -> KeyStroke(JsKeyCodes.BACKQUOTE, false)
        '~' -> KeyStroke(JsKeyCodes.BACKQUOTE, true)
        '[' -> KeyStroke(JsKeyCodes.BRACKET_LEFT, false)
        '{' -> KeyStroke(JsKeyCodes.BRACKET_LEFT, true)
        '\\' -> KeyStroke(JsKeyCodes.BACKSLASH, false)
        '|' -> KeyStroke(JsKeyCodes.BACKSLASH, true)
        ']' -> KeyStroke(JsKeyCodes.BRACKET_RIGHT, false)
        '}' -> KeyStroke(JsKeyCodes.BRACKET_RIGHT, true)
        '\'' -> KeyStroke(JsKeyCodes.QUOTE, false)
        '"' -> KeyStroke(JsKeyCodes.QUOTE, true)

        else -> null
    }

    /** True for keys that produce a character; hardware modifiers/navigation keys do not. */
    fun producesText(jsKeyCode: Int): Boolean = when (jsKeyCode) {
        JsKeyCodes.SPACE -> true
        in JsKeyCodes.DIGIT_0..JsKeyCodes.DIGIT_9 -> true
        in JsKeyCodes.A..JsKeyCodes.Z -> true
        in JsKeyCodes.NUMPAD_0..JsKeyCodes.NUMPAD_DIVIDE -> true
        in JsKeyCodes.SEMICOLON..JsKeyCodes.QUOTE -> true
        else -> false
    }
}
