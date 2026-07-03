package com.ddisplay.core.protocol

import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue
import kotlinx.serialization.json.Json

private fun le16(v: Int) = byteArrayOf((v and 0xFF).toByte(), ((v shr 8) and 0xFF).toByte())

private fun le32(v: Int) = byteArrayOf(
    (v and 0xFF).toByte(),
    ((v shr 8) and 0xFF).toByte(),
    ((v shr 16) and 0xFF).toByte(),
    ((v shr 24) and 0xFF).toByte(),
)

private fun le64(v: Long) = ByteArray(8) { ((v shr (8 * it)) and 0xFF).toByte() }

private fun bytes(vararg parts: ByteArray): ByteArray {
    val out = ByteArray(parts.sumOf { it.size })
    var off = 0
    for (p in parts) {
        p.copyInto(out, off); off += p.size
    }
    return out
}

private fun b(vararg v: Int) = ByteArray(v.size) { v[it].toByte() }

class ProtocolCodecTest {

    @Test
    fun mouseMoveLayout() {
        assertContentEquals(
            bytes(b(0x10), le16(100), le16(200)),
            ProtocolCodec.encodeMouseMove(100, 200),
        )
    }

    @Test
    fun mouseButtonLayout() {
        assertContentEquals(
            bytes(b(0x11, MouseButtonCode.RIGHT, 1), le16(1920), le16(1080)),
            ProtocolCodec.encodeMouseButton(MouseButtonCode.RIGHT, true, 1920, 1080),
        )
        assertContentEquals(
            bytes(b(0x11, MouseButtonCode.LEFT, 0), le16(0), le16(0)),
            ProtocolCodec.encodeMouseButton(MouseButtonCode.LEFT, false, 0, 0),
        )
    }

    @Test
    fun mouseScrollSignedDeltas() {
        // dy > 0 scrolls down, one notch = 1. Negative dx must round-trip as i16.
        val encoded = ProtocolCodec.encodeMouseScroll(-3, 1, 10, 20)
        assertContentEquals(
            bytes(b(0x12), le16(-3 and 0xFFFF), le16(1), le16(10), le16(20)),
            encoded,
        )
    }

    @Test
    fun keyEventLayout() {
        assertContentEquals(
            bytes(b(0x13), le32(65), b(1)),
            ProtocolCodec.encodeKeyEvent(65, true),
        )
        assertContentEquals(
            bytes(b(0x13), le32(222), b(0)),
            ProtocolCodec.encodeKeyEvent(222, false),
        )
    }

    @Test
    fun noPayloadMessages() {
        assertContentEquals(b(0x14), ProtocolCodec.encodeClientReady())
        assertContentEquals(b(0x16), ProtocolCodec.encodeReleaseKeys())
        assertContentEquals(b(0x17), ProtocolCodec.encodeReleaseMouse())
        assertContentEquals(b(0x18), ProtocolCodec.encodeReleaseAll())
        assertContentEquals(b(0x29), ProtocolCodec.encodeRequestAddMonitor())
        assertContentEquals(b(0x2a), ProtocolCodec.encodeRequestRemoveMonitor())
    }

    @Test
    fun textMessages() {
        val text = "grüße"
        val utf8 = text.toByteArray(Charsets.UTF_8)
        assertContentEquals(bytes(b(0x15), utf8), ProtocolCodec.encodePasteText(text))
        assertContentEquals(bytes(b(0x20), utf8), ProtocolCodec.encodeClipboardData(text))
    }

    @Test
    fun requestKeyframe() {
        assertContentEquals(b(0x21), ProtocolCodec.encodeRequestKeyframe())
        assertContentEquals(b(0x21, 2), ProtocolCodec.encodeRequestKeyframe(2))
    }

    @Test
    fun pingLayout() {
        val ts = 0x0102030405060708L
        assertContentEquals(bytes(b(0x24), le64(ts)), ProtocolCodec.encodePing(ts))
    }

    @Test
    fun clientCapsJson() {
        val encoded = ProtocolCodec.encodeClientCaps(ClientCaps(listOf("av1", "h264"), 1920, 1080))
        assertEquals(0x22, encoded[0].toInt() and 0xFF)
        val payload = String(encoded, 1, encoded.size - 1, Charsets.UTF_8)
        assertEquals("""{"codecs":["av1","h264"],"width":1920,"height":1080}""", payload)
    }

    @Test
    fun clientStatsJson() {
        val stats = ClientStats(received = 10, dropped = 1, decodeMs = 2.5f, rttMs = 15.0f)
        val encoded = ProtocolCodec.encodeClientStats(stats)
        assertEquals(0x23, encoded[0].toInt() and 0xFF)
        val payload = String(encoded, 1, encoded.size - 1, Charsets.UTF_8)
        // Snake-case keys are the wire contract the Rust server's serde expects.
        assertTrue(payload.contains("\"decode_ms\""), payload)
        assertTrue(payload.contains("\"rtt_ms\""), payload)
        assertEquals(stats, Json.decodeFromString<ClientStats>(payload))
    }

    @Test
    fun parseVideoFrame() {
        val pts = 0x1122334455667788L
        val payload = b(0xDE, 0xAD, 0xBE, 0xEF)
        val frame = bytes(b(0x01, 1), le64(pts), le16(1280), le16(720), payload)
        val msg = ProtocolCodec.parseServerMessage(frame)
        assertTrue(msg is ServerMessage.VideoFrame)
        assertEquals(true, msg.keyframe)
        assertEquals(pts, msg.pts)
        assertEquals(1280, msg.width)
        assertEquals(720, msg.height)
        assertContentEquals(payload, msg.data)
    }

    @Test
    fun parseVideoFrameTruncatedIsNull() {
        // type + keyframe + pts + width + 1 height byte = 13; the header needs 14.
        val short = bytes(b(0x01, 1), le64(0), le16(1280), b(0))
        assertEquals(13, short.size)
        assertNull(ProtocolCodec.parseServerMessage(short))
    }

    @Test
    fun parseCursorUpdate() {
        val msg = ProtocolCodec.parseServerMessage(bytes(b(0x02), le16(400), le16(300), b(1)))
        assertTrue(msg is ServerMessage.CursorUpdate)
        assertEquals(400, msg.x)
        assertEquals(300, msg.y)
        assertTrue(msg.visible)
        assertNull(ProtocolCodec.parseServerMessage(b(0x02, 0, 0, 0, 0))) // 5 bytes, needs 6
    }

    @Test
    fun parseSessionInfo() {
        val jsonText = """{"codec":"av1","width":1920,"height":1080,"fps":60,"bitrate":8000000}"""
        val msg = ProtocolCodec.parseServerMessage(bytes(b(0x03), jsonText.toByteArray()))
        assertTrue(msg is ServerMessage.Session)
        assertEquals("av1", msg.info.codec)
        assertEquals(1920, msg.info.width)
        assertEquals(60, msg.info.fps)
        assertEquals(8000000, msg.info.bitrate)
    }

    @Test
    fun parseSessionInfoMissingBitrateDefaults() {
        val jsonText = """{"codec":"h264","width":1280,"height":720,"fps":30}"""
        val msg = ProtocolCodec.parseServerMessage(bytes(b(0x03), jsonText.toByteArray()))
        assertTrue(msg is ServerMessage.Session)
        assertEquals(0, msg.info.bitrate)
    }

    @Test
    fun parseSessionInfoMalformedIsNull() {
        assertNull(ProtocolCodec.parseServerMessage(bytes(b(0x03), "not json".toByteArray())))
    }

    @Test
    fun parseMonitorLayout() {
        val jsonText =
            """{"monitors":[{"id":0,"x":0,"y":0,"width":1920,"height":1080},{"id":1,"x":1920,"y":0,"width":2560,"height":1440}]}"""
        val msg = ProtocolCodec.parseServerMessage(bytes(b(0x08), jsonText.toByteArray()))
        assertTrue(msg is ServerMessage.Layout)
        assertEquals(2, msg.layout.monitors.size)
        assertEquals(MonitorRect(1, 1920, 0, 2560, 1440), msg.layout.monitors[1])
    }

    @Test
    fun parseMonitorFrame() {
        val pts = 42L
        val payload = b(0x01, 0x02, 0x03)
        val frame = bytes(b(0x09, 3, 0), le64(pts), le16(2560), le16(1440), payload)
        val msg = ProtocolCodec.parseServerMessage(frame)
        assertTrue(msg is ServerMessage.MonitorFrame)
        assertEquals(3, msg.monitorId)
        assertEquals(false, msg.keyframe)
        assertEquals(pts, msg.pts)
        assertEquals(2560, msg.width)
        assertEquals(1440, msg.height)
        assertContentEquals(payload, msg.data)
        assertNull(ProtocolCodec.parseServerMessage(frame.copyOfRange(0, 14))) // needs 15
    }

    @Test
    fun parseClipboard() {
        val msg = ProtocolCodec.parseServerMessage(bytes(b(0x20), "hello".toByteArray()))
        assertTrue(msg is ServerMessage.Clipboard)
        assertEquals("hello", msg.text)
        // Bare type byte decodes to empty text (from_utf8_lossy over an empty slice).
        val empty = ProtocolCodec.parseServerMessage(b(0x20))
        assertTrue(empty is ServerMessage.Clipboard)
        assertEquals("", empty.text)
    }

    @Test
    fun parsePong() {
        val ts = 987654321L
        val msg = ProtocolCodec.parseServerMessage(bytes(b(0x24), le64(ts)))
        assertTrue(msg is ServerMessage.Pong)
        assertEquals(ts, msg.timestampMs)
        // Our ping encoder produces the exact bytes the server echoes back.
        assertContentEquals(bytes(b(0x24), le64(ts)), ProtocolCodec.encodePing(ts))
        assertNull(ProtocolCodec.parseServerMessage(b(0x24, 0, 0, 0))) // 4 bytes, needs 9
    }

    @Test
    fun parseUnknownAndEmpty() {
        val msg = ProtocolCodec.parseServerMessage(b(0x7F, 1, 2, 3))
        assertTrue(msg is ServerMessage.Unknown)
        assertEquals(0x7F, msg.type)
        assertNull(ProtocolCodec.parseServerMessage(ByteArray(0)))
    }
}
