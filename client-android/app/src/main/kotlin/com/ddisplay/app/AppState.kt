package com.ddisplay.app

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/** The two top-level screens; there is no navigation graph, just this switch. */
sealed interface Screen {
    data object Connect : Screen

    data class Session(val serverUrl: String) : Screen
}

/** Drives the Connect <-> Session switch. Survives rotation via the activity's configChanges. */
class AppState {
    var screen: Screen by mutableStateOf(Screen.Connect)
        private set

    fun openSession(serverUrl: String) {
        screen = Screen.Session(serverUrl)
    }

    fun back() {
        screen = Screen.Connect
    }
}
