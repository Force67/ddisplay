package com.ddisplay.app.input

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.setValue
import com.ddisplay.core.input.LetterboxRect
import com.ddisplay.core.input.ViewTransform

const val MIN_ZOOM = 1f
const val MAX_ZOOM = 5f

/**
 * Zoom and pan applied on top of the letterboxed video, in view pixels. A
 * displayed point equals letterboxPoint * zoom + pan, matching [ViewTransform]
 * so [com.ddisplay.core.input.ViewportMath] maps touches back to the remote
 * framebuffer. SessionScreen reads it to size and place the video; the gesture
 * code writes it. Pan is clamped so the video always covers its letterbox area,
 * which forces pan to zero at 1x.
 */
@Stable
class ZoomPanState {
    var zoom by mutableFloatStateOf(MIN_ZOOM)
        private set
    var panX by mutableFloatStateOf(0f)
        private set
    var panY by mutableFloatStateOf(0f)
        private set

    val transform: ViewTransform get() = ViewTransform(zoom, panX, panY)

    val isZoomed: Boolean get() = zoom > MIN_ZOOM + 0.001f

    /** Scale by [factor] around the ([focalX], [focalY]) view point, keeping it fixed. */
    fun zoomBy(factor: Float, focalX: Float, focalY: Float, box: LetterboxRect) {
        val newZoom = (zoom * factor).coerceIn(MIN_ZOOM, MAX_ZOOM)
        val k = newZoom / zoom
        panX = focalX - (focalX - panX) * k
        panY = focalY - (focalY - panY) * k
        zoom = newZoom
        clamp(box)
    }

    fun panBy(dx: Float, dy: Float, box: LetterboxRect) {
        panX += dx
        panY += dy
        clamp(box)
    }

    fun reset() {
        zoom = MIN_ZOOM
        panX = 0f
        panY = 0f
    }

    private fun clamp(box: LetterboxRect) {
        val minX = (box.left + box.width) * (1f - zoom)
        val maxX = box.left * (1f - zoom)
        val minY = (box.top + box.height) * (1f - zoom)
        val maxY = box.top * (1f - zoom)
        panX = panX.coerceIn(minOf(minX, maxX), maxOf(minX, maxX))
        panY = panY.coerceIn(minOf(minY, maxY), maxOf(minY, maxY))
    }
}
