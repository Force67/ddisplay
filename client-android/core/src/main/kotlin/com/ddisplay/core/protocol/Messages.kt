package com.ddisplay.core.protocol

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/**
 * ddisplay wire protocol message types. Binary WebSocket frames, first byte is
 * the type, integers little-endian. Mirrors server/src/protocol.rs, which is
 * authoritative.
 */
object MessageType {
    // Server -> client
    const val VIDEO_FRAME: Int = 0x01
    const val CURSOR_UPDATE: Int = 0x02
    const val SESSION_INFO: Int = 0x03
    const val MONITOR_LAYOUT: Int = 0x08
    const val MONITOR_FRAME: Int = 0x09

    // Client -> server
    const val MOUSE_MOVE: Int = 0x10
    const val MOUSE_BUTTON: Int = 0x11
    const val MOUSE_SCROLL: Int = 0x12
    const val KEY_EVENT: Int = 0x13
    const val CLIENT_READY: Int = 0x14
    const val PASTE_TEXT: Int = 0x15
    const val RELEASE_KEYS: Int = 0x16
    const val RELEASE_MOUSE: Int = 0x17
    const val RELEASE_ALL: Int = 0x18
    const val REQUEST_KEYFRAME: Int = 0x21
    const val CLIENT_CAPS: Int = 0x22
    const val CLIENT_STATS: Int = 0x23
    const val REQUEST_ADD_MONITOR: Int = 0x29
    const val REQUEST_REMOVE_MONITOR: Int = 0x2a

    // Bidirectional
    const val CLIPBOARD_DATA: Int = 0x20
    const val PING: Int = 0x24
}

/** Mouse button codes on the wire: 0 = left, 1 = middle, 2 = right. */
object MouseButtonCode {
    const val LEFT: Int = 0
    const val MIDDLE: Int = 1
    const val RIGHT: Int = 2
}

/** {codec, width, height, fps, bitrate}, sent by the server on connect and on change. */
@Serializable
data class SessionInfo(
    val codec: String = "",
    val width: Int = 0,
    val height: Int = 0,
    val fps: Int = 0,
    val bitrate: Int = 0,
)

/** One head and its pixel rectangle inside the shared framebuffer. */
@Serializable
data class MonitorRect(
    val id: Int,
    val x: Int,
    val y: Int,
    val width: Int,
    val height: Int,
)

/** {monitors:[{id,x,y,width,height}]}, the set of heads the session exposes. */
@Serializable
data class MonitorLayout(
    val monitors: List<MonitorRect> = emptyList(),
)

/** Decoder capabilities and native resolution the client reports on connect. */
@Serializable
data class ClientCaps(
    val codecs: List<String> = emptyList(),
    val width: Int = 0,
    val height: Int = 0,
)

/** Periodic client feedback for the server's adaptive bitrate controller. */
@Serializable
data class ClientStats(
    val received: Int = 0,
    val dropped: Int = 0,
    @SerialName("decode_ms") val decodeMs: Float = 0f,
    @SerialName("rtt_ms") val rttMs: Float = 0f,
)

/** A parsed server-to-client message. */
sealed interface ServerMessage {
    data class VideoFrame(
        val keyframe: Boolean,
        val pts: Long,
        val width: Int,
        val height: Int,
        val data: ByteArray,
    ) : ServerMessage

    data class CursorUpdate(
        val x: Int,
        val y: Int,
        val visible: Boolean,
    ) : ServerMessage

    data class Session(val info: SessionInfo) : ServerMessage

    data class Layout(val layout: MonitorLayout) : ServerMessage

    data class MonitorFrame(
        val monitorId: Int,
        val keyframe: Boolean,
        val pts: Long,
        val width: Int,
        val height: Int,
        val data: ByteArray,
    ) : ServerMessage

    data class Clipboard(val text: String) : ServerMessage

    /** Echo of a Ping we sent; the payload is the timestamp we put on the wire. */
    data class Pong(val timestampMs: Long) : ServerMessage

    data class Unknown(val type: Int) : ServerMessage
}
