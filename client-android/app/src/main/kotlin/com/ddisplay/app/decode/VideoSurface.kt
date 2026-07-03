package com.ddisplay.app.decode

import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Modifier
import androidx.compose.ui.viewinterop.AndroidView

/**
 * A SurfaceView the decoder renders onto, wrapped for Compose. Reports the
 * [android.view.Surface] as it becomes available or changes size, and null when
 * it is destroyed. Sizing and letterbox/zoom are layout-driven by the caller.
 */
@Composable
fun VideoSurface(modifier: Modifier = Modifier, onSurfaceChanged: (android.view.Surface?) -> Unit) {
    val latest = rememberUpdatedState(onSurfaceChanged)
    // One stable callback added in the factory and removed in onRelease, so it
    // is never re-registered across recompositions. It reads the current lambda
    // via rememberUpdatedState.
    val holderCallback = remember {
        object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {}

            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
                latest.value(holder.surface)
            }

            override fun surfaceDestroyed(holder: SurfaceHolder) {
                latest.value(null)
            }
        }
    }
    AndroidView(
        modifier = modifier,
        factory = { context ->
            SurfaceView(context).apply { holder.addCallback(holderCallback) }
        },
        onRelease = { view -> view.holder.removeCallback(holderCallback) },
    )
}
