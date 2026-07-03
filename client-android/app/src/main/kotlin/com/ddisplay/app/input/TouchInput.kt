package com.ddisplay.app.input

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.input.pointer.AwaitPointerEventScope
import androidx.compose.ui.input.pointer.PointerEvent
import androidx.compose.ui.input.pointer.PointerId
import androidx.compose.ui.input.pointer.PointerInputChange
import androidx.compose.ui.input.pointer.PointerInputScope
import androidx.compose.ui.input.pointer.PointerType
import androidx.compose.ui.input.pointer.isPrimaryPressed
import androidx.compose.ui.input.pointer.isSecondaryPressed
import androidx.compose.ui.input.pointer.isTertiaryPressed
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import com.ddisplay.core.input.LetterboxRect
import com.ddisplay.core.input.RemotePoint
import com.ddisplay.core.input.ViewTransform
import com.ddisplay.core.input.ViewportMath
import com.ddisplay.core.input.VirtualCursor
import com.ddisplay.core.protocol.MouseButtonCode
import kotlinx.coroutines.withTimeoutOrNull
import kotlin.math.abs
import kotlin.math.roundToInt

enum class TouchMode { TRACKPAD, DIRECT }

private const val SENSITIVITY = 1.2f
private const val ACCEL_GAIN = 0.06f
private const val ACCEL_MAX = 2.5f
private val SCROLL_NOTCH = 42.dp

/**
 * Relative pointer for TRACKPAD mode: wraps the core [VirtualCursor] and exposes
 * its clamped position as Compose state so the crosshair overlay redraws.
 */
@Stable
class TrackpadPointer(remoteWidth: Int, remoteHeight: Int) {
    private val cursor = VirtualCursor(remoteWidth, remoteHeight, sensitivity = SENSITIVITY)

    var position: RemotePoint by mutableStateOf(cursor.position())
        private set

    fun moveBy(dx: Float, dy: Float) {
        position = cursor.moveBy(dx, dy)
    }

    fun moveTo(x: Int, y: Int) {
        position = cursor.moveTo(x, y)
    }
}

/** Inverse of [ViewportMath.viewToRemote]: a remote pixel back to a view-space point. */
fun remoteToView(
    remote: RemotePoint,
    viewW: Float,
    viewH: Float,
    remoteW: Int,
    remoteH: Int,
    transform: ViewTransform,
): Offset {
    if (remoteW <= 0 || remoteH <= 0) return Offset.Zero
    val box = ViewportMath.letterbox(viewW, viewH, remoteW, remoteH)
    val baseScale = box.width / remoteW
    val imageX = remote.x * baseScale + box.left
    val imageY = remote.y * baseScale + box.top
    return Offset(imageX * transform.zoom + transform.panX, imageY * transform.zoom + transform.panY)
}

/**
 * Routes touch, mouse and stylus gestures to [sink] as remote-framebuffer input.
 * [viewSize] and [remoteSize] are read live so the coroutine survives size
 * changes; it restarts only when [mode] or [pointer] change.
 */
fun Modifier.remoteInput(
    mode: TouchMode,
    pointer: TrackpadPointer,
    zoomPan: ZoomPanState,
    sink: InputSink,
    viewSize: () -> IntSize,
    remoteSize: () -> IntSize,
): Modifier = pointerInput(mode, pointer) {
    GestureHandler(this, mode, pointer, zoomPan, sink, viewSize, remoteSize).run()
}

private sealed interface Classification {
    data object Tap : Classification
    data class Move(val pos: Offset) : Classification
    data class Multi(val event: PointerEvent) : Classification
}

private class GestureHandler(
    private val scope: PointerInputScope,
    private val mode: TouchMode,
    private val pointer: TrackpadPointer,
    private val zoomPan: ZoomPanState,
    private val sink: InputSink,
    private val viewSize: () -> IntSize,
    private val remoteSize: () -> IntSize,
) {
    private val slop = scope.viewConfiguration.touchSlop
    private val longPressMs = scope.viewConfiguration.longPressTimeoutMillis
    private val doubleTapMs = scope.viewConfiguration.doubleTapTimeoutMillis
    private val stepPx = with(scope) { SCROLL_NOTCH.toPx() }

    private var mouseLeft = false
    private var mouseRight = false
    private var mouseMiddle = false

    // Timestamp and place of the last trackpad tap, so a quick second touch that
    // moves becomes a tap-and-drag rather than a fresh cursor move.
    private var lastTapUpTime = 0L
    private var lastTapPos = Offset.Zero

    suspend fun run() = scope.awaitPointerEventScope {
        while (true) {
            val event = awaitPointerEvent()
            val vp = viewSize()
            if (vp.width == 0 || vp.height == 0 || remoteSize().width <= 0) continue

            val device = event.changes.firstOrNull { it.type != PointerType.Touch }
            if (device != null) {
                handleDevice(event, device)
                continue
            }

            val down = event.changes.firstOrNull {
                it.type == PointerType.Touch && it.pressed && !it.previousPressed
            }
            if (down != null && event.changes.none { it.id != down.id && it.previousPressed }) {
                when (mode) {
                    TouchMode.TRACKPAD -> handleTrackpad(down)
                    TouchMode.DIRECT -> handleDirect(down)
                }
            }
        }
    }

    private suspend fun AwaitPointerEventScope.handleTrackpad(firstDown: PointerInputChange) {
        val startPos = firstDown.position
        val now = firstDown.uptimeMillis
        val doubleTap = (now - lastTapUpTime) < doubleTapMs &&
            (startPos - lastTapPos).getDistance() < slop * 2f
        var last = startPos

        val result: Classification? = withTimeoutOrNull(longPressMs) {
            var c: Classification? = null
            while (c == null) {
                val e = awaitPointerEvent()
                if (pressedTouches(e).size >= 2) {
                    c = Classification.Multi(e)
                    continue
                }
                val me = e.changes.firstOrNull { it.id == firstDown.id }
                if (me == null || !me.pressed) {
                    c = Classification.Tap
                    continue
                }
                if ((me.position - startPos).getDistance() > slop) {
                    last = me.position
                    c = Classification.Move(me.position)
                } else {
                    me.consume()
                }
            }
            c
        }

        when (result) {
            null -> {
                pressLeftAtCursor()
                dragLoop(firstDown.id, holdingLeft = true, startLast = last)
            }
            is Classification.Move -> {
                if (doubleTap) {
                    pressLeftAtCursor()
                    dragLoop(firstDown.id, holdingLeft = true, startLast = last)
                } else {
                    dragLoop(firstDown.id, holdingLeft = false, startLast = last)
                }
            }
            Classification.Tap -> {
                leftClickAtCursor()
                lastTapUpTime = now
                lastTapPos = startPos
            }
            is Classification.Multi -> handleTwoFinger(result.event)
        }
    }

    private suspend fun AwaitPointerEventScope.dragLoop(
        id: PointerId,
        holdingLeft: Boolean,
        startLast: Offset,
    ) {
        var last = startLast
        while (true) {
            val e = awaitPointerEvent()
            if (!holdingLeft && pressedTouches(e).size >= 2) {
                handleTwoFinger(e)
                return
            }
            val me = e.changes.firstOrNull { it.id == id }
            if (me == null || !me.pressed) {
                if (holdingLeft) releaseLeftAtCursor()
                return
            }
            val d = accelerate(me.position - last)
            last = me.position
            pointer.moveBy(d.x, d.y)
            val p = pointer.position
            sink.mouseMove(p.x, p.y)
            me.consume()
        }
    }

    private suspend fun AwaitPointerEventScope.handleDirect(firstDown: PointerInputChange) {
        val startPos = firstDown.position
        emitAbsMove(startPos)

        val result: Classification? = withTimeoutOrNull(longPressMs) {
            var c: Classification? = null
            while (c == null) {
                val e = awaitPointerEvent()
                if (pressedTouches(e).size >= 2) {
                    c = Classification.Multi(e)
                    continue
                }
                val me = e.changes.firstOrNull { it.id == firstDown.id }
                if (me == null || !me.pressed) {
                    c = Classification.Tap
                    continue
                }
                if ((me.position - startPos).getDistance() > slop) {
                    c = Classification.Move(me.position)
                } else {
                    me.consume()
                }
            }
            c
        }

        when (result) {
            null -> {
                val p = emitAbsMove(startPos)
                sink.mouseButton(MouseButtonCode.RIGHT, true, p.x, p.y)
                sink.mouseButton(MouseButtonCode.RIGHT, false, p.x, p.y)
                waitForRelease(firstDown.id)
            }
            Classification.Tap -> {
                val p = emitAbsMove(startPos)
                sink.mouseButton(MouseButtonCode.LEFT, true, p.x, p.y)
                sink.mouseButton(MouseButtonCode.LEFT, false, p.x, p.y)
            }
            is Classification.Move -> {
                val p = emitAbsMove(result.pos)
                sink.mouseButton(MouseButtonCode.LEFT, true, p.x, p.y)
                absDragLoop(firstDown.id)
            }
            is Classification.Multi -> handleTwoFinger(result.event)
        }
    }

    private suspend fun AwaitPointerEventScope.absDragLoop(id: PointerId) {
        while (true) {
            val e = awaitPointerEvent()
            if (pressedTouches(e).size >= 2) {
                releaseLeftAtCursor()
                handleTwoFinger(e)
                return
            }
            val me = e.changes.firstOrNull { it.id == id }
            if (me == null || !me.pressed) {
                releaseLeftAtCursor()
                return
            }
            emitAbsMove(me.position)
            me.consume()
        }
    }

    private suspend fun AwaitPointerEventScope.waitForRelease(id: PointerId) {
        while (true) {
            val e = awaitPointerEvent()
            val me = e.changes.firstOrNull { it.id == id }
            if (me == null || !me.pressed) return
            me.consume()
        }
    }

    private suspend fun AwaitPointerEventScope.handleTwoFinger(start: PointerEvent) {
        val startTime = start.changes.first().uptimeMillis
        val pts = pressedTouches(start)
        if (pts.size < 2) return
        var oldCentroid = centroid(pts)
        var oldDist = spread(pts, oldCentroid)
        var accumX = 0f
        var accumY = 0f
        var moved = false

        while (true) {
            val e = awaitPointerEvent()
            val cur = pressedTouches(e)
            if (cur.size < 2) {
                val duration = e.changes.first().uptimeMillis - startTime
                if (!moved && duration < doubleTapMs && mode == TouchMode.TRACKPAD) {
                    rightClickAtCursor()
                }
                return
            }
            val c = centroid(cur)
            val dist = spread(cur, c)
            val dc = c - oldCentroid
            if (dc.getDistance() > slop) moved = true

            val box = currentBox()
            if (oldDist > 1f && abs(dist - oldDist) > 0.5f) {
                zoomPan.zoomBy(dist / oldDist, c.x, c.y, box)
            }
            // Scroll only at 1x; once zoomed the same two-finger drag pans instead.
            if (zoomPan.isZoomed) {
                zoomPan.panBy(dc.x, dc.y, box)
            } else {
                accumX += dc.x
                accumY += dc.y
                val nx = (accumX / stepPx).toInt()
                val ny = (accumY / stepPx).toInt()
                if (nx != 0 || ny != 0) {
                    accumX -= nx * stepPx
                    accumY -= ny * stepPx
                    val p = pointer.position
                    // Natural scroll: fingers moving down reveal content above (wheel up).
                    sink.scroll(-nx, -ny, p.x, p.y)
                }
            }
            oldCentroid = c
            oldDist = dist
            cur.forEach { it.consume() }
        }
    }

    private fun handleDevice(event: PointerEvent, change: PointerInputChange) {
        val p = mapAbsolute(change.position)
        if (change.position != change.previousPosition || change.pressed != change.previousPressed) {
            sink.mouseMove(p.x, p.y)
            pointer.moveTo(p.x, p.y)
        }

        val buttons = event.buttons
        val wantLeft: Boolean
        val wantRight: Boolean
        val wantMiddle: Boolean
        if (change.type == PointerType.Mouse) {
            wantLeft = buttons.isPrimaryPressed
            wantRight = buttons.isSecondaryPressed
            wantMiddle = buttons.isTertiaryPressed
        } else {
            // Stylus/eraser: barrel button is the secondary press, tip contact is left.
            wantRight = buttons.isSecondaryPressed
            wantLeft = change.pressed && !wantRight
            wantMiddle = false
        }
        mouseLeft = applyButton(MouseButtonCode.LEFT, wantLeft, mouseLeft, p)
        mouseMiddle = applyButton(MouseButtonCode.MIDDLE, wantMiddle, mouseMiddle, p)
        mouseRight = applyButton(MouseButtonCode.RIGHT, wantRight, mouseRight, p)

        val sd = change.scrollDelta
        if (sd.x != 0f || sd.y != 0f) {
            // Compose reports wheel-up as positive y; the server wants dy>0 for down.
            val dy = -sd.y.roundToInt()
            val dx = sd.x.roundToInt()
            if (dx != 0 || dy != 0) sink.scroll(dx, dy, p.x, p.y)
        }
        change.consume()
    }

    private fun applyButton(button: Int, want: Boolean, held: Boolean, p: RemotePoint): Boolean {
        if (want && !held) sink.mouseButton(button, true, p.x, p.y)
        else if (!want && held) sink.mouseButton(button, false, p.x, p.y)
        return want
    }

    private fun mapAbsolute(pos: Offset): RemotePoint {
        val vp = viewSize()
        val rs = remoteSize()
        return ViewportMath.viewToRemote(
            pos.x, pos.y, vp.width.toFloat(), vp.height.toFloat(), rs.width, rs.height, zoomPan.transform,
        )
    }

    private fun emitAbsMove(pos: Offset): RemotePoint {
        val p = mapAbsolute(pos)
        sink.mouseMove(p.x, p.y)
        pointer.moveTo(p.x, p.y)
        return p
    }

    private fun pressLeftAtCursor() {
        val p = pointer.position
        sink.mouseMove(p.x, p.y)
        sink.mouseButton(MouseButtonCode.LEFT, true, p.x, p.y)
    }

    private fun releaseLeftAtCursor() {
        val p = pointer.position
        sink.mouseButton(MouseButtonCode.LEFT, false, p.x, p.y)
    }

    private fun leftClickAtCursor() {
        val p = pointer.position
        sink.mouseMove(p.x, p.y)
        sink.mouseButton(MouseButtonCode.LEFT, true, p.x, p.y)
        sink.mouseButton(MouseButtonCode.LEFT, false, p.x, p.y)
    }

    private fun rightClickAtCursor() {
        val p = pointer.position
        sink.mouseMove(p.x, p.y)
        sink.mouseButton(MouseButtonCode.RIGHT, true, p.x, p.y)
        sink.mouseButton(MouseButtonCode.RIGHT, false, p.x, p.y)
    }

    private fun currentBox(): LetterboxRect {
        val vp = viewSize()
        val rs = remoteSize()
        return ViewportMath.letterbox(vp.width.toFloat(), vp.height.toFloat(), rs.width, rs.height)
    }

    private fun accelerate(delta: Offset): Offset {
        val factor = (1f + delta.getDistance() * ACCEL_GAIN).coerceAtMost(ACCEL_MAX)
        return Offset(delta.x * factor, delta.y * factor)
    }

    private fun pressedTouches(event: PointerEvent): List<PointerInputChange> =
        event.changes.filter { it.type == PointerType.Touch && it.pressed }

    private fun centroid(pts: List<PointerInputChange>): Offset {
        var x = 0f
        var y = 0f
        pts.forEach { x += it.position.x; y += it.position.y }
        return Offset(x / pts.size, y / pts.size)
    }

    private fun spread(pts: List<PointerInputChange>, c: Offset): Float {
        var s = 0f
        pts.forEach { s += (it.position - c).getDistance() }
        return s / pts.size
    }
}
