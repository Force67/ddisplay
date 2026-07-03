package com.ddisplay.app.net

import java.net.InetAddress
import java.net.Socket
import java.util.concurrent.TimeUnit
import javax.net.SocketFactory
import kotlin.coroutines.coroutineContext
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.toByteString

/**
 * WebSocket transport over OkHttp: binary frames only, with auto-reconnect and
 * exponential backoff. Mirrors the native client's transport loop
 * (client-native/src/transport.rs).
 *
 * [scope] owns the reconnect loop; it is not cancelled here, so the same client
 * can [connect] again after [disconnect]. Callers that pass a session-scoped
 * scope get teardown for free when that scope is cancelled.
 */
class OkHttpSessionClient(
    private val scope: CoroutineScope,
    private val httpClient: OkHttpClient = defaultClient(),
) : SessionClient {

    // DROP_OLDEST bounds memory when the consumer stalls: stale video frames are
    // dropped rather than blocking the OkHttp reader thread.
    private val _events = MutableSharedFlow<SessionEvent>(
        extraBufferCapacity = 256,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    override val events: Flow<SessionEvent> = _events

    @Volatile private var webSocket: WebSocket? = null
    @Volatile private var connected = false
    private var loopJob: Job? = null

    override fun connect(url: String) {
        if (loopJob?.isActive == true) return
        loopJob = scope.launch(Dispatchers.IO) { reconnectLoop(url) }
    }

    override fun disconnect() {
        loopJob?.cancel()
        loopJob = null
        connected = false
        webSocket?.cancel()
        webSocket = null
    }

    override fun send(data: ByteArray) {
        // Drop when not connected: input queued during a reconnect is meaningless.
        if (!connected) return
        webSocket?.send(data.toByteString())
    }

    private suspend fun reconnectLoop(url: String) {
        val request = Request.Builder().url(url).build()
        var backoff = MIN_BACKOFF_MS
        while (coroutineContext[Job]?.isActive != false) {
            val closed = CompletableDeferred<Boolean>()
            val ws = httpClient.newWebSocket(request, Listener(closed))
            webSocket = ws
            val opened = try {
                closed.await()
            } catch (e: CancellationException) {
                ws.cancel()
                throw e
            }
            connected = false
            webSocket = null
            if (coroutineContext[Job]?.isActive == false) break
            if (opened) backoff = MIN_BACKOFF_MS
            delay(backoff)
            backoff = (backoff * 2).coerceAtMost(MAX_BACKOFF_MS)
        }
    }

    /** [closed] completes with whether this connection ever reached onOpen, so the loop resets backoff only after a real connection. */
    private inner class Listener(
        private val closed: CompletableDeferred<Boolean>,
    ) : WebSocketListener() {
        @Volatile private var opened = false

        override fun onOpen(webSocket: WebSocket, response: Response) {
            opened = true
            connected = true
            _events.tryEmit(SessionEvent.Connected)
        }

        override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
            _events.tryEmit(SessionEvent.BinaryMessage(bytes.toByteArray()))
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            // The server speaks binary only; ignore text frames.
        }

        override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
            webSocket.close(NORMAL_CLOSURE, null)
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            connected = false
            _events.tryEmit(SessionEvent.Disconnected(reason.ifEmpty { null }))
            closed.complete(opened)
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            connected = false
            _events.tryEmit(SessionEvent.Disconnected(t.message))
            closed.complete(opened)
        }
    }

    private companion object {
        const val NORMAL_CLOSURE = 1000
        const val MIN_BACKOFF_MS = 500L
        const val MAX_BACKOFF_MS = 10_000L

        fun defaultClient(): OkHttpClient = OkHttpClient.Builder()
            // OkHttp does not set TCP_NODELAY on its sockets (verified against
            // RealConnection in 4.12.0), so Nagle would batch small input sends.
            // The native client sets it explicitly; match that here.
            .socketFactory(NoDelaySocketFactory(SocketFactory.getDefault()))
            // Long-lived stream: a live but momentarily idle socket must not be
            // idle-timed-out.
            .readTimeout(0, TimeUnit.MILLISECONDS)
            // Detect a dead peer so the reconnect loop fires promptly.
            .pingInterval(20, TimeUnit.SECONDS)
            .build()
    }
}

private class NoDelaySocketFactory(private val delegate: SocketFactory) : SocketFactory() {
    private fun Socket.noDelay(): Socket = apply { tcpNoDelay = true }

    override fun createSocket(): Socket = delegate.createSocket().noDelay()

    override fun createSocket(host: String?, port: Int): Socket =
        delegate.createSocket(host, port).noDelay()

    override fun createSocket(host: String?, port: Int, localHost: InetAddress?, localPort: Int): Socket =
        delegate.createSocket(host, port, localHost, localPort).noDelay()

    override fun createSocket(host: InetAddress?, port: Int): Socket =
        delegate.createSocket(host, port).noDelay()

    override fun createSocket(address: InetAddress?, port: Int, localAddress: InetAddress?, localPort: Int): Socket =
        delegate.createSocket(address, port, localAddress, localPort).noDelay()
}
