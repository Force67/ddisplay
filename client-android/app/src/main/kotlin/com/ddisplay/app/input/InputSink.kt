package com.ddisplay.app.input

/**
 * Where the session UI hands pointer, scroll and key gestures. The
 * implementation encodes them with com.ddisplay.core.protocol and sends them
 * through the SessionClient. Coordinates are remote framebuffer pixels; button
 * codes and JS keyCodes follow the wire protocol.
 */
interface InputSink {
    fun mouseMove(x: Int, y: Int)

    fun mouseButton(button: Int, pressed: Boolean, x: Int, y: Int)

    fun scroll(dx: Int, dy: Int, x: Int, y: Int)

    fun key(jsKeyCode: Int, pressed: Boolean)

    fun paste(text: String)

    fun releaseAll()
}
