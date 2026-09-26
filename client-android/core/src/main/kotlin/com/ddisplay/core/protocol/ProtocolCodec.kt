package com.ddisplay.core.protocol

import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json

/**
 * Encoders for client-to-server messages and a parser for server-to-client
 * messages, byte-for-byte compatible with server/src/protocol.rs.
 *
 * Parsers return null on empty or truncated input, matching the Rust `None`
 * behaviour. Malformed JSON payloads (SessionInfo, MonitorLayout) also yield
 * null. Unrecognised message types map to [ServerMessage.Unknown], like Rust.
 */
object ProtocolCodec {

    private val json = Json {
        encodeDefaults = true
        ignoreUnknownKeys = true
    }

    // --- Client -> server encoders ---

    fun encodeMouseMove(x: Int, y: Int): ByteArray =
        frame(5).put(MessageType.MOUSE_MOVE.toByte())
            .putShort(x.toShort())
            .putShort(y.toShort())
            .array()

    fun encodeMouseButton(button: Int, pressed: Boolean, x: Int, y: Int): ByteArray =
        frame(7).put(MessageType.MOUSE_BUTTON.toByte())
            .put(button.toByte())
            .put(if (pressed) 1 else 0)
            .putShort(x.toShort())
            .putShort(y.toShort())
            .array()

    fun encodeMouseScroll(dx: Int, dy: Int, x: Int, y: Int): ByteArray =
        frame(9).put(MessageType.MOUSE_SCROLL.toByte())
            .putShort(dx.toShort())
            .putShort(dy.toShort())
            .putShort(x.toShort())
            .putShort(y.toShort())
            .array()

    fun encodeKeyEvent(jsKeyCode: Int, pressed: Boolean): ByteArray =
        frame(6).put(MessageType.KEY_EVENT.toByte())
            .putInt(jsKeyCode)
            .put(if (pressed) 1 else 0)
            .array()

    fun encodeClientReady(): ByteArray = byteArrayOf(MessageType.CLIENT_READY.toByte())

    fun encodePasteText(text: String): ByteArray =
        textFrame(MessageType.PASTE_TEXT, text)

    fun encodeReleaseKeys(): ByteArray = byteArrayOf(MessageType.RELEASE_KEYS.toByte())

    fun encodeReleaseMouse(): ByteArray = byteArrayOf(MessageType.RELEASE_MOUSE.toByte())

    fun encodeReleaseAll(): ByteArray = byteArrayOf(MessageType.RELEASE_ALL.toByte())

    fun encodeClipboardData(text: String): ByteArray =
        textFrame(MessageType.CLIPBOARD_DATA, text)

    /** No head id: every head re-IDRs. With [monitorId]: only that head. */
    fun encodeRequestKeyframe(monitorId: Int? = null): ByteArray =
        if (monitorId == null) {
            byteArrayOf(MessageType.REQUEST_KEYFRAME.toByte())
        } else {
            byteArrayOf(MessageType.REQUEST_KEYFRAME.toByte(), monitorId.toByte())
        }

    fun encodeClientCaps(caps: ClientCaps): ByteArray =
        jsonFrame(MessageType.CLIENT_CAPS, json.encodeToString(caps))

    fun encodeClientStats(stats: ClientStats): ByteArray =
        jsonFrame(MessageType.CLIENT_STATS, json.encodeToString(stats))

    fun encodePing(timestampMs: Long): ByteArray =
        frame(9).put(MessageType.PING.toByte()).putLong(timestampMs).array()

    fun encodeRequestAddMonitor(): ByteArray =
        byteArrayOf(MessageType.REQUEST_ADD_MONITOR.toByte())

    fun encodeRequestRemoveMonitor(): ByteArray =
        byteArrayOf(MessageType.REQUEST_REMOVE_MONITOR.toByte())

    // --- Server -> client parser ---

    fun parseServerMessage(data: ByteArray): ServerMessage? {
        if (data.isEmpty()) return null
        return when (data[0].toInt() and 0xFF) {
            MessageType.VIDEO_FRAME -> {
                if (data.size < 14) return null
                ServerMessage.VideoFrame(
                    keyframe = data[1].toInt() != 0,
                    pts = readU64LE(data, 2),
                    width = readU16LE(data, 10),
                    height = readU16LE(data, 12),
                    data = data.copyOfRange(14, data.size),
                )
            }

            MessageType.CURSOR_UPDATE -> {
                if (data.size < 6) return null
                ServerMessage.CursorUpdate(
                    x = readU16LE(data, 1),
                    y = readU16LE(data, 3),
                    visible = data[5].toInt() != 0,
                )
            }

            MessageType.SESSION_INFO -> {
                if (data.size < 2) return null
                val info = decodeJson<SessionInfo>(data) ?: return null
                ServerMessage.Session(info)
            }

            MessageType.MONITOR_LAYOUT -> {
                if (data.size < 2) return null
                val layout = decodeJson<MonitorLayout>(data) ?: return null
                ServerMessage.Layout(layout)
            }

            MessageType.MONITOR_FRAME -> {
                if (data.size < 15) return null
                ServerMessage.MonitorFrame(
                    monitorId = data[1].toInt() and 0xFF,
                    keyframe = data[2].toInt() != 0,
                    pts = readU64LE(data, 3),
                    width = readU16LE(data, 11),
                    height = readU16LE(data, 13),
                    data = data.copyOfRange(15, data.size),
                )
            }

            MessageType.CLIPBOARD_DATA -> {
                // Rust decodes data[1..] lossily; a bare type byte yields "".
                val text = if (data.size > 1) String(data, 1, data.size - 1, Charsets.UTF_8) else ""
                ServerMessage.Clipboard(text)
            }

            MessageType.PING -> {
                if (data.size < 9) return null
                ServerMessage.Pong(readU64LE(data, 1))
            }

            else -> ServerMessage.Unknown(data[0].toInt() and 0xFF)
        }
    }

    private inline fun <reified T> decodeJson(data: ByteArray): T? =
        runCatching {
            json.decodeFromString<T>(String(data, 1, data.size - 1, Charsets.UTF_8))
        }.getOrNull()

    private fun frame(size: Int): ByteBuffer =
        ByteBuffer.allocate(size).order(ByteOrder.LITTLE_ENDIAN)

    private fun textFrame(type: Int, text: String): ByteArray {
        val bytes = text.toByteArray(Charsets.UTF_8)
        return ByteArray(1 + bytes.size).also {
            it[0] = type.toByte()
            bytes.copyInto(it, 1)
        }
    }

    private fun jsonFrame(type: Int, jsonText: String): ByteArray = textFrame(type, jsonText)

    private fun readU16LE(b: ByteArray, off: Int): Int =
        (b[off].toInt() and 0xFF) or ((b[off + 1].toInt() and 0xFF) shl 8)

    private fun readU64LE(b: ByteArray, off: Int): Long {
        var v = 0L
        for (i in 0 until 8) {
            v = v or ((b[off + i].toLong() and 0xFF) shl (8 * i))
        }
        return v
    }
}
