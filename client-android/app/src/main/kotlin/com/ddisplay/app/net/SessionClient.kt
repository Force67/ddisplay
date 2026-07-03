package com.ddisplay.app.net

import kotlinx.coroutines.flow.Flow

/** Transport-level events surfaced to the session layer. */
sealed interface SessionEvent {
    data object Connected : SessionEvent

    data class Disconnected(val reason: String?) : SessionEvent

    data class BinaryMessage(val data: ByteArray) : SessionEvent {
        override fun equals(other: Any?): Boolean =
            this === other || (other is BinaryMessage && data.contentEquals(other.data))

        override fun hashCode(): Int = data.contentHashCode()
    }
}

/**
 * WebSocket transport to a ddisplay server. Frames are binary and framed per
 * com.ddisplay.core.protocol. The OkHttp implementation is [OkHttpSessionClient].
 */
interface SessionClient {
    val events: Flow<SessionEvent>

    fun connect(url: String)

    fun disconnect()

    fun send(data: ByteArray)
}
