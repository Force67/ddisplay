package com.ddisplay.app.session

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch

/**
 * Bridges the Android clipboard and a [RemoteSession]. Server-pushed text is
 * written to the local clipboard; [pushLocalToServer] sends the local clipboard
 * to the server on demand (call it when the UI regains focus).
 *
 * Echo prevention mirrors the native client's clipboard_last_set logic: the last
 * value written from the server is remembered and never sent straight back.
 */
class ClipboardSync(
    context: Context,
    private val session: RemoteSession,
    scope: CoroutineScope,
) {
    private val appContext = context.applicationContext
    private val clipboard =
        appContext.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager

    @Volatile private var lastSetFromServer: String? = null

    init {
        // setPrimaryClip touches the framework clipboard; keep it on the main thread.
        scope.launch(Dispatchers.Main) {
            session.clipboardFromServer.collect { text ->
                // Record before writing so pushLocalToServer sees it even if the
                // user copies immediately after.
                lastSetFromServer = text
                clipboard?.setPrimaryClip(ClipData.newPlainText("ddisplay", text))
            }
        }
    }

    fun pushLocalToServer() {
        val clip = clipboard?.primaryClip ?: return
        if (clip.itemCount == 0) return
        val text = clip.getItemAt(0).coerceToText(appContext)?.toString() ?: return
        if (text.isEmpty() || text == lastSetFromServer) return
        session.sendClipboard(text)
    }
}
