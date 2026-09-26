package com.ddisplay.core.input

import kotlin.math.min

/** A remote framebuffer coordinate, already clamped to the u16 wire range. */
data class RemotePoint(val x: Int, val y: Int)

/** On-screen rectangle (view pixels) the remote framebuffer is drawn into when fit with no zoom. */
data class LetterboxRect(val left: Float, val top: Float, val width: Float, val height: Float)

/**
 * Zoom and pan layered on top of the letterboxed video, both in view space.
 * A displayed point equals letterboxPoint * zoom + pan, so identity ([zoom] 1,
 * no pan) reduces to the plain letterbox mapping the native client uses.
 */
data class ViewTransform(val zoom: Float = 1f, val panX: Float = 0f, val panY: Float = 0f)

/** Highest coordinate the wire can carry (u16). */
const val REMOTE_MAX: Int = 65535

/**
 * Coordinate mapping between a touch view and the remote framebuffer. Mirrors
 * the letterbox math in the native client's renderer/input (scale = min ratio,
 * centred, clicks in the bars clamp to the nearest edge) and adds a zoom/pan
 * transform for touch navigation.
 */
object ViewportMath {

    /** The fit-to-view letterbox rectangle for a [remoteW] x [remoteH] frame in a [viewW] x [viewH] view. */
    fun letterbox(viewW: Float, viewH: Float, remoteW: Int, remoteH: Int): LetterboxRect {
        if (remoteW <= 0 || remoteH <= 0) return LetterboxRect(0f, 0f, viewW, viewH)
        val scale = min(viewW / remoteW, viewH / remoteH)
        val w = remoteW * scale
        val h = remoteH * scale
        return LetterboxRect((viewW - w) * 0.5f, (viewH - h) * 0.5f, w, h)
    }

    /**
     * Map a touch point in view pixels to a remote framebuffer coordinate,
     * clamped to [0, remoteW-1] x [0, remoteH-1] and to the u16 wire range.
     * Points inside the letterbox bars clamp to the nearest edge.
     */
    fun viewToRemote(
        touchX: Float,
        touchY: Float,
        viewW: Float,
        viewH: Float,
        remoteW: Int,
        remoteH: Int,
        transform: ViewTransform = ViewTransform(),
    ): RemotePoint {
        if (remoteW <= 0 || remoteH <= 0) return RemotePoint(0, 0)
        val box = letterbox(viewW, viewH, remoteW, remoteH)
        val baseScale = box.width / remoteW

        // Undo the zoom/pan applied in view space, then undo the letterbox.
        val imageX = (touchX - transform.panX) / transform.zoom
        val imageY = (touchY - transform.panY) / transform.zoom
        val remoteX = (imageX - box.left) / baseScale
        val remoteY = (imageY - box.top) / baseScale

        return RemotePoint(
            x = clampRemote(remoteX, remoteW),
            y = clampRemote(remoteY, remoteH),
        )
    }

    private fun clampRemote(value: Float, remoteSize: Int): Int {
        val maxCoord = min(remoteSize - 1, REMOTE_MAX)
        val floored = if (value.isFinite()) value.toInt() else 0
        return floored.coerceIn(0, maxCoord)
    }
}
