package com.ddisplay.app

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.remember
import com.ddisplay.app.ui.ConnectScreen
import com.ddisplay.app.ui.SessionScreen
import com.ddisplay.app.ui.theme.DDisplayTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            DDisplayTheme {
                val state = remember { AppState() }
                when (val screen = state.screen) {
                    is Screen.Connect -> ConnectScreen(onConnect = state::openSession)
                    is Screen.Session -> SessionScreen(
                        serverUrl = screen.serverUrl,
                        onDisconnect = state::back,
                    )
                }
            }
        }
    }
}
