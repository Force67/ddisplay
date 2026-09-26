package com.ddisplay.app.session

import android.content.res.Resources
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import com.ddisplay.app.decode.CodecCaps
import com.ddisplay.app.decode.DecoderEvents
import com.ddisplay.app.decode.VideoDecoder
import com.ddisplay.app.input.InputSink
import com.ddisplay.app.net.SessionClient
import com.ddisplay.app.net.SessionEvent
import com.ddisplay.core.protocol.ClientCaps
import com.ddisplay.core.protocol.ClientStats
import com.ddisplay.core.protocol.ProtocolCodec
import com.ddisplay.core.protocol.ServerMessage
import com.ddisplay.core.protocol.SessionInfo
import java.util.concurrent.atomic.LongAdder
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/**
 * The single object the UI talks to. Owns the transport handshake, the decoder
 * lifecycle, the once-per-second ping + stats loop and the derived state flows.
 * Mirrors the behaviour of client-native/src/main.rs.
 *
 * All message parsing, decoder access and per-second counters are confined to
 * one consumer coroutine on [Dispatchers.Default], so nothing runs on the main
 * thread and the decoder is never touched concurrently. Input encoding is cheap
 * and goes straight to the transport from the caller's thread.
 */
class RemoteSession(
    private val client: SessionClient,
    private val decoderFactory: () -> VideoDecoder,
    scope: CoroutineScope,
) : InputSink {

    private val _state = MutableStateFlow<SessionState>(SessionState.Idle)
    val state: StateFlow<SessionState> = _state.asStateFlow()

    private val _stats = MutableStateFlow(SessionStats())
    val stats: StateFlow<SessionStats> = _stats.asStateFlow()

    private val _remoteSize = MutableStateFlow(RemoteSize(0, 0))
    val remoteSize: StateFlow<RemoteSize> = _remoteSize.asStateFlow()

    private val _cursor = MutableStateFlow(RemoteCursor(0, 0, false))
    val cursor: StateFlow<RemoteCursor> = _cursor.asStateFlow()

    private val _clipboard = MutableSharedFlow<String>(
        extraBufferCapacity = 8,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val clipboardFromServer: SharedFlow<String> = _clipboard.asSharedFlow()

    private val sessionJob = SupervisorJob(scope.coroutineContext[Job])
    private val sessionScope = CoroutineScope(scope.coroutineContext + sessionJob)

    // Low-rate control commands merged into the single consumer.
    private val control = Channel<Ev>(capacity = 32, onBufferOverflow = BufferOverflow.DROP_OLDEST)

    // Consumer-confined state (touched only inside handle()).
    private var decoder: VideoDecoder? = null
    private var surface: Surface? = null
    private var codec: String? = null
    private var configuredCodec: String? = null
    private var configuredWidth = 0
    private var configuredHeight = 0
    private var sessionWidth = 0
    private var sessionHeight = 0
    private var lastFrameWidth = 0
    private var lastFrameHeight = 0

    // Per-interval counters, reset each tick (consumer-confined).
    private var received = 0
    private var bytes = 0L
    private var lastRttMs = 0f
    private var lastTickMs = SystemClock.elapsedRealtime()

    // Fed from decoder callbacks, so these need atomics.
    private val decodeSumMicros = LongAdder()
    private val decodeCount = LongAdder()
    private val dropped = LongAdder()

    private val displayWidth = Resources.getSystem().displayMetrics.widthPixels
    private val displayHeight = Resources.getSystem().displayMetrics.heightPixels

    private val decoderEvents = object : DecoderEvents {
        override fun onNeedsKeyframe() {
            control.trySend(Ev.RequestKeyframe)
        }

        override fun onDecodedFrame(decodeMs: Float) {
            decodeSumMicros.add((decodeMs * 1000f).toLong())
            decodeCount.increment()
        }

        override fun onFrameDropped() {
            dropped.increment()
        }
    }

    init {
        sessionScope.launch(Dispatchers.Default) {
            try {
                val transport: Flow<Ev> = client.events.map { Ev.Transport(it) }
                val controls: Flow<Ev> = control.receiveAsFlow()
                merge(transport, controls).collect { handle(it) }
            } finally {
                decoder?.close()
                decoder = null
            }
        }
        sessionScope.launch {
            while (isActive) {
                delay(1000)
                control.trySend(Ev.Tick)
            }
        }
    }

    fun connect(hostPort: String) {
        if (!sessionScope.isActive) return
        _state.value = SessionState.Connecting
        client.connect(toWsUrl(hostPort))
    }

    fun disconnect() {
        client.send(ProtocolCodec.encodeReleaseAll()) // best effort, before we tear down
        client.disconnect()
        _state.value = SessionState.Disconnected(null)
        sessionScope.cancel() // consumer's finally closes the decoder on its own thread
    }

    fun setSurface(surface: Surface?) {
        control.trySend(Ev.SetSurface(surface))
    }

    fun requestKeyframe() {
        control.trySend(Ev.RequestKeyframe)
    }

    fun sendClipboard(text: String) {
        client.send(ProtocolCodec.encodeClipboardData(text))
    }

    override fun mouseMove(x: Int, y: Int) {
        client.send(ProtocolCodec.encodeMouseMove(x, y))
    }

    override fun mouseButton(button: Int, pressed: Boolean, x: Int, y: Int) {
        client.send(ProtocolCodec.encodeMouseButton(button, pressed, x, y))
    }

    override fun scroll(dx: Int, dy: Int, x: Int, y: Int) {
        client.send(ProtocolCodec.encodeMouseScroll(dx, dy, x, y))
    }

    override fun key(jsKeyCode: Int, pressed: Boolean) {
        client.send(ProtocolCodec.encodeKeyEvent(jsKeyCode, pressed))
    }

    override fun paste(text: String) {
        client.send(ProtocolCodec.encodePasteText(text))
    }

    override fun releaseAll() {
        client.send(ProtocolCodec.encodeReleaseAll())
    }

    // --- consumer ---

    private suspend fun handle(ev: Ev) {
        when (ev) {
            is Ev.Transport -> onTransport(ev.event)
            is Ev.SetSurface -> onSurface(ev.surface)
            Ev.RequestKeyframe -> client.send(ProtocolCodec.encodeRequestKeyframe(HEAD_PRIMARY))
            Ev.Tick -> onTick()
        }
    }

    private fun onTransport(event: SessionEvent) {
        when (event) {
            SessionEvent.Connected -> onConnected()
            is SessionEvent.Disconnected -> _state.value = SessionState.Disconnected(event.reason)
            is SessionEvent.BinaryMessage -> onBinary(event.data)
        }
    }

    private fun onConnected() {
        _state.value = SessionState.Connecting // stays Connecting until SessionInfo arrives
        client.send(ProtocolCodec.encodeClientReady())
        client.send(
            ProtocolCodec.encodeClientCaps(
                ClientCaps(
                    codecs = CodecCaps.supportedCodecs(),
                    width = displayWidth,
                    height = displayHeight,
                ),
            ),
        )
        client.send(ProtocolCodec.encodeRequestKeyframe(HEAD_PRIMARY))
    }

    private fun onBinary(data: ByteArray) {
        bytes += data.size // every wire byte feeds the mbps stat
        when (val msg = ProtocolCodec.parseServerMessage(data) ?: return) {
            is ServerMessage.VideoFrame -> {
                received++
                lastFrameWidth = msg.width
                lastFrameHeight = msg.height
                _remoteSize.value = RemoteSize(msg.width, msg.height)
                decoder?.submit(msg.data, msg.keyframe, msg.pts)
            }

            is ServerMessage.Session -> onSessionInfo(msg.info)

            is ServerMessage.CursorUpdate ->
                _cursor.value = RemoteCursor(msg.x, msg.y, msg.visible)

            is ServerMessage.Clipboard -> _clipboard.tryEmit(msg.text)

            is ServerMessage.Pong -> {
                val now = SystemClock.elapsedRealtime()
                if (now >= msg.timestampMs) lastRttMs = (now - msg.timestampMs).toFloat()
            }

            // Single-head client: layout, extra heads and unknowns are ignored.
            is ServerMessage.Layout,
            is ServerMessage.MonitorFrame,
            is ServerMessage.Unknown,
            -> Unit
        }
    }

    private fun onSessionInfo(info: SessionInfo) {
        _state.value = SessionState.Connected(info)
        sessionWidth = info.width
        sessionHeight = info.height
        if (info.width > 0 && info.height > 0) {
            _remoteSize.value = RemoteSize(info.width, info.height)
        }
        if (info.codec.isNotEmpty() && info.codec != codec) {
            codec = info.codec
            if (configuredCodec != null && configuredCodec != info.codec) {
                decoder?.close()
                decoder = null
                configuredCodec = null
            }
        }
        tryConfigure()
    }

    private fun onSurface(newSurface: Surface?) {
        surface = newSurface
        if (newSurface == null) {
            decoder?.close()
            decoder = null
            configuredCodec = null
        } else {
            tryConfigure()
        }
    }

    /** Build the decoder once both a Surface and a codec are known; a fresh decoder needs a keyframe. */
    private fun tryConfigure() {
        val s = surface ?: return
        val c = codec ?: return
        if (c.isEmpty()) return
        val w = if (sessionWidth > 0) sessionWidth else lastFrameWidth
        val h = if (sessionHeight > 0) sessionHeight else lastFrameHeight
        if (w <= 0 || h <= 0) return
        if (decoder != null && configuredCodec == c && configuredWidth == w && configuredHeight == h) return

        decoder?.close()
        decoder = null
        configuredCodec = null
        val d = decoderFactory()
        try {
            d.configure(c, w, h, s, decoderEvents)
        } catch (e: Exception) {
            // e.g. the Surface was released under us; a new Surface or SessionInfo retries.
            Log.w(TAG, "decoder configure failed", e)
            d.close()
            return
        }
        decoder = d
        configuredCodec = c
        configuredWidth = w
        configuredHeight = h
        client.send(ProtocolCodec.encodeRequestKeyframe(HEAD_PRIMARY))
    }

    private fun onTick() {
        val now = SystemClock.elapsedRealtime()
        val secs = (now - lastTickMs).coerceAtLeast(1L).toFloat() / 1000f
        lastTickMs = now

        val count = decodeCount.sumThenReset()
        val droppedNow = dropped.sumThenReset().toInt()
        val sumMicros = decodeSumMicros.sumThenReset()
        val decodeMs = if (count > 0) sumMicros.toFloat() / count / 1000f else 0f

        client.send(ProtocolCodec.encodePing(now))
        client.send(
            ProtocolCodec.encodeClientStats(
                ClientStats(received, droppedNow, decodeMs, lastRttMs),
            ),
        )

        _stats.value = SessionStats(
            rttMs = lastRttMs,
            fps = received / secs,
            mbps = bytes * 8f / 1_000_000f / secs,
            decodeMs = decodeMs,
            dropped = droppedNow,
        )

        received = 0
        bytes = 0L
    }

    private sealed interface Ev {
        data class Transport(val event: SessionEvent) : Ev
        data class SetSurface(val surface: Surface?) : Ev
        data object RequestKeyframe : Ev
        data object Tick : Ev
    }

    private companion object {
        const val TAG = "RemoteSession"
        const val DEFAULT_PORT = 9550
        const val HEAD_PRIMARY: Int = 0

        fun toWsUrl(input: String): String {
            val trimmed = input.trim()
            val scheme = if (trimmed.startsWith("wss://")) "wss" else "ws"
            val authority = trimmed
                .removePrefix("ws://")
                .removePrefix("wss://")
                .substringBefore('/') // we always target the /ws endpoint
            val withPort = if (authority.contains(':')) authority else "$authority:$DEFAULT_PORT"
            return "$scheme://$withPort/ws"
        }
    }
}

sealed interface SessionState {
    data object Idle : SessionState
    data object Connecting : SessionState
    data class Connected(val info: SessionInfo) : SessionState
    data class Disconnected(val reason: String?) : SessionState
}

data class SessionStats(
    val rttMs: Float = 0f,
    val fps: Float = 0f,
    val mbps: Float = 0f,
    val decodeMs: Float = 0f,
    val dropped: Int = 0,
)

data class RemoteSize(val width: Int, val height: Int)

data class RemoteCursor(val x: Int, val y: Int, val visible: Boolean)
