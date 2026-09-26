package com.ddisplay.core.input

import kotlin.math.min

/**
 * Trackpad-style relative pointer. Finger deltas (view pixels) accumulate,
 * scaled by [sensitivity], into a remote position clamped to the framebuffer
 * and the u16 wire range. Sub-pixel remainders are kept so slow drags are not
 * quantised away.
 */
class VirtualCursor(
    remoteWidth: Int,
    remoteHeight: Int,
    var sensitivity: Float = 1f,
    startX: Int = remoteWidth / 2,
    startY: Int = remoteHeight / 2,
) {
    private val maxX: Int = min(remoteWidth - 1, REMOTE_MAX).coerceAtLeast(0)
    private val maxY: Int = min(remoteHeight - 1, REMOTE_MAX).coerceAtLeast(0)

    private var fx: Float = startX.coerceIn(0, maxX).toFloat()
    private var fy: Float = startY.coerceIn(0, maxY).toFloat()

    val x: Int get() = fx.toInt().coerceIn(0, maxX)
    val y: Int get() = fy.toInt().coerceIn(0, maxY)

    fun position(): RemotePoint = RemotePoint(x, y)

    /** Apply a relative finger movement and return the new clamped position. */
    fun moveBy(dx: Float, dy: Float): RemotePoint {
        fx = (fx + dx * sensitivity).coerceIn(0f, maxX.toFloat())
        fy = (fy + dy * sensitivity).coerceIn(0f, maxY.toFloat())
        return position()
    }

    /** Jump to an absolute remote coordinate (for example after a tap-to-locate). */
    fun moveTo(remoteX: Int, remoteY: Int): RemotePoint {
        fx = remoteX.coerceIn(0, maxX).toFloat()
        fy = remoteY.coerceIn(0, maxY).toFloat()
        return position()
    }
}
