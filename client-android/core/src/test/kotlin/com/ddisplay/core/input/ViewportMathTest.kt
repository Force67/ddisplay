package com.ddisplay.core.input

import kotlin.test.Test
import kotlin.test.assertEquals

class ViewportMathTest {

    // 1920x1080 remote inside a 1080x2400 portrait phone: letterbox bars top and bottom.
    private val remoteW = 1920
    private val remoteH = 1080
    private val viewW = 1080f
    private val viewH = 2400f

    @Test
    fun letterboxPortrait16by9() {
        val box = ViewportMath.letterbox(viewW, viewH, remoteW, remoteH)
        assertEquals(0f, box.left, 0.01f)
        assertEquals(896.25f, box.top, 0.01f)
        assertEquals(1080f, box.width, 0.01f)
        assertEquals(607.5f, box.height, 0.01f)
    }

    @Test
    fun centerMapsToRemoteCenter() {
        val p = ViewportMath.viewToRemote(540f, 1200f, viewW, viewH, remoteW, remoteH)
        assertEquals(RemotePoint(960, 540), p)
    }

    @Test
    fun touchInTopBarClampsToTopEdge() {
        val p = ViewportMath.viewToRemote(540f, 100f, viewW, viewH, remoteW, remoteH)
        assertEquals(RemotePoint(960, 0), p)
    }

    @Test
    fun touchInBottomBarClampsToBottomEdge() {
        val p = ViewportMath.viewToRemote(540f, 2000f, viewW, viewH, remoteW, remoteH)
        assertEquals(RemotePoint(960, remoteH - 1), p)
    }

    @Test
    fun touchLeftEdge() {
        val p = ViewportMath.viewToRemote(0f, 1200f, viewW, viewH, remoteW, remoteH)
        assertEquals(RemotePoint(0, 540), p)
    }

    @Test
    fun pillarboxWideView() {
        // 1920x1080 remote inside 2560x1080: side bars, width capped.
        val box = ViewportMath.letterbox(2560f, 1080f, remoteW, remoteH)
        assertEquals(320f, box.left, 0.01f)
        assertEquals(0f, box.top, 0.01f)
        val center = ViewportMath.viewToRemote(1280f, 540f, 2560f, 1080f, remoteW, remoteH)
        assertEquals(RemotePoint(960, 540), center)
        val leftBar = ViewportMath.viewToRemote(100f, 540f, 2560f, 1080f, remoteW, remoteH)
        assertEquals(RemotePoint(0, 540), leftBar)
    }

    @Test
    fun zoomWithoutPanScalesAboutOrigin() {
        // Square remote fully filling a square view: baseScale 1, no letterbox.
        val t = ViewTransform(zoom = 2f)
        val p = ViewportMath.viewToRemote(400f, 600f, 1000f, 1000f, 1000, 1000, t)
        assertEquals(RemotePoint(200, 300), p)
    }

    @Test
    fun zoomWithPan() {
        val t = ViewTransform(zoom = 2f, panX = 100f, panY = 100f)
        assertEquals(
            RemotePoint(200, 200),
            ViewportMath.viewToRemote(500f, 500f, 1000f, 1000f, 1000, 1000, t),
        )
        // Pans/zooms that push the touch off the framebuffer clamp to its bounds.
        assertEquals(
            RemotePoint(0, 0),
            ViewportMath.viewToRemote(50f, 50f, 1000f, 1000f, 1000, 1000, t),
        )
        assertEquals(
            RemotePoint(999, 0),
            ViewportMath.viewToRemote(2500f, 50f, 1000f, 1000f, 1000, 1000, t),
        )
    }
}
