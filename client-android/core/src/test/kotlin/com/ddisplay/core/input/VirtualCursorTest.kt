package com.ddisplay.core.input

import kotlin.test.Test
import kotlin.test.assertEquals

class VirtualCursorTest {

    @Test
    fun startsAtCentreByDefault() {
        val c = VirtualCursor(1920, 1080)
        assertEquals(RemotePoint(960, 540), c.position())
    }

    @Test
    fun relativeMoveAddsDelta() {
        val c = VirtualCursor(1920, 1080)
        assertEquals(RemotePoint(970, 560), c.moveBy(10f, 20f))
    }

    @Test
    fun sensitivityScalesDelta() {
        val c = VirtualCursor(1920, 1080, sensitivity = 2f)
        assertEquals(RemotePoint(980, 540), c.moveBy(10f, 0f))
    }

    @Test
    fun clampsToRemoteBounds() {
        val c = VirtualCursor(1920, 1080)
        assertEquals(RemotePoint(1919, 1079), c.moveBy(100_000f, 100_000f))
        assertEquals(RemotePoint(0, 0), c.moveBy(-100_000f, -100_000f))
    }

    @Test
    fun subPixelDeltasAccumulate() {
        // With sensitivity 0.4, truncating each move would never advance; the
        // fractional remainder must carry so the third small move lands +1.
        val c = VirtualCursor(1920, 1080, sensitivity = 0.4f, startX = 960, startY = 540)
        assertEquals(960, c.moveBy(1f, 0f).x)
        assertEquals(960, c.moveBy(1f, 0f).x)
        assertEquals(961, c.moveBy(1f, 0f).x)
    }

    @Test
    fun clampsToU16WireRange() {
        val c = VirtualCursor(100_000, 100_000)
        assertEquals(RemotePoint(REMOTE_MAX, REMOTE_MAX), c.moveTo(80_000, 80_000))
    }

    @Test
    fun moveToClampsToBounds() {
        val c = VirtualCursor(1920, 1080)
        assertEquals(RemotePoint(0, 0), c.moveTo(-5, -5))
        assertEquals(RemotePoint(1919, 1079), c.moveTo(999_999, 999_999))
    }
}
