package com.ddisplay.app

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.remember
import androidx.lifecycle.lifecycleScope
import com.ddisplay.app.decode.MediaCodecVideoDecoder
import com.ddisplay.app.ui.ConnectScreen
import com.ddisplay.app.ui.SessionScreen
import com.ddisplay.app.ui.theme.DDisplayTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()

        val appContext = applicationContext
        // lifecycleScope survives configChanges (rotation) and cancels on real
        // destroy, tearing down any live session with it.
        val appScope = lifecycleScope

        setContent {
            DDisplayTheme {
                val state = remember {
                    AppState(appContext, appScope) { MediaCodecVideoDecoder() }
                }
                when (val screen = state.screen) {
                    is Screen.Connect -> ConnectScreen(
                        onConnect = state::connect,
                        connecting = state.connecting,
                        errorMessage = state.errorMessage,
                    )
                    is Screen.Session -> state.session?.let { session ->
                        SessionScreen(
                            session = session,
                            onDisconnect = state::disconnect,
                            onRetry = { state.connect(screen.serverAddress) },
                        )
                    }
                }
            }
        }
    }
}
