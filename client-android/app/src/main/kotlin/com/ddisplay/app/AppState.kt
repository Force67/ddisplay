package com.ddisplay.app

import android.content.Context
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.ddisplay.app.data.RecentServers
import com.ddisplay.app.decode.VideoDecoder
import com.ddisplay.app.net.OkHttpSessionClient
import com.ddisplay.app.session.RemoteSession
import com.ddisplay.app.session.SessionState
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch

/** The two top-level screens; there is no navigation graph, just this switch. */
sealed interface Screen {
    data object Connect : Screen

    data class Session(val serverAddress: String) : Screen
}

/**
 * Drives the Connect <-> Session switch and owns the live [RemoteSession].
 * Survives rotation via the activity's configChanges. The UI renders the
 * session from [session]'s flows; this class only manages its lifecycle.
 *
 * The Connect screen stays up until the first successful connection, showing
 * [connecting] and any [errorMessage] from a failed attempt. Once connected it
 * hands off to the session screen, which owns reconnect and disconnect from
 * then on.
 */
class AppState(
    context: Context,
    private val scope: CoroutineScope,
    private val decoderFactory: () -> VideoDecoder,
) {
    private val recentServers = RecentServers(context.applicationContext)

    var screen: Screen by mutableStateOf(Screen.Connect)
        private set

    var session: RemoteSession? by mutableStateOf(null)
        private set

    var connecting: Boolean by mutableStateOf(false)
        private set

    var errorMessage: String? by mutableStateOf(null)
        private set

    private var stateJob: Job? = null

    fun connect(hostPort: String) {
        val target = hostPort.trim()
        if (target.isEmpty()) return
        session?.disconnect()
        stateJob?.cancel()
        connecting = true
        errorMessage = null
        scope.launch { recentServers.remember(target) }
        val next = RemoteSession(
            client = OkHttpSessionClient(scope),
            decoderFactory = decoderFactory,
            scope = scope,
        )
        session = next
        stateJob = scope.launch { follow(next, target) }
        next.connect(target)
    }

    fun disconnect() {
        stateJob?.cancel()
        stateJob = null
        session?.disconnect()
        session = null
        connecting = false
        errorMessage = null
        screen = Screen.Connect
    }

    private suspend fun follow(session: RemoteSession, target: String) {
        session.state.collect { state ->
            when (state) {
                SessionState.Connecting -> {
                    connecting = true
                    errorMessage = null
                }
                is SessionState.Connected -> {
                    connecting = false
                    errorMessage = null
                    if (screen is Screen.Connect) screen = Screen.Session(target)
                }
                is SessionState.Disconnected -> {
                    connecting = false
                    errorMessage = state.reason
                }
                SessionState.Idle -> Unit
            }
        }
    }
}
