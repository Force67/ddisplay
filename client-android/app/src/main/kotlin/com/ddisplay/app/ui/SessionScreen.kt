package com.ddisplay.app.ui

import android.view.ViewTreeObserver
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.focusable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.layout.layout
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.findViewTreeLifecycleOwner
import com.ddisplay.app.R
import com.ddisplay.app.decode.VideoSurface
import com.ddisplay.app.input.AndroidKeyMap
import com.ddisplay.app.input.TouchMode
import com.ddisplay.app.input.TrackpadPointer
import com.ddisplay.app.input.ZoomPanState
import com.ddisplay.app.input.remoteInput
import com.ddisplay.app.input.remoteToView
import com.ddisplay.app.session.ClipboardSync
import com.ddisplay.app.session.RemoteSession
import com.ddisplay.app.session.SessionState
import com.ddisplay.core.input.JsKeyCodes
import com.ddisplay.core.input.LetterboxRect
import com.ddisplay.core.input.RemotePoint
import com.ddisplay.core.input.ViewportMath
import kotlinx.coroutines.delay
import kotlin.math.roundToInt

private const val BAR_HIDE_MS = 3000L

/**
 * The live session: letterboxed video with zoom/pan, a trackpad or direct touch
 * layer, on-screen keys, a stats HUD and connection overlays. [onRetry] defaults
 * to [onDisconnect]; wire it to reconnect the same address to retry in place.
 */
@Composable
fun SessionScreen(
    session: RemoteSession,
    onDisconnect: () -> Unit,
    onRetry: (() -> Unit)? = null,
) {
    val state by session.state.collectAsState()
    val stats by session.stats.collectAsState()
    val remote by session.remoteSize.collectAsState()
    val serverCursor by session.cursor.collectAsState()

    val zoomPan = remember { ZoomPanState() }
    val pointer = remember(remote.width, remote.height) {
        TrackpadPointer(remote.width.coerceAtLeast(1), remote.height.coerceAtLeast(1))
    }
    val sticky = remember(session) { StickyModifiers(session) }

    var touchMode by rememberSaveable { mutableStateOf(TouchMode.TRACKPAD) }
    var keyboardVisible by remember { mutableStateOf(false) }
    var specialKeysVisible by remember { mutableStateOf(false) }
    var statsVisible by rememberSaveable { mutableStateOf(false) }
    var barsVisible by remember { mutableStateOf(true) }
    var revealTick by remember { mutableIntStateOf(0) }
    var confirmDisconnect by remember { mutableStateOf(false) }
    var viewSize by remember { mutableStateOf(IntSize.Zero) }

    // derivedState so a pinch (continuous zoom changes) only recomposes the bar
    // when the video actually crosses into or out of the zoomed state.
    val zoomed by remember { derivedStateOf { zoomPan.isZoomed } }

    val rootFocus = remember { FocusRequester() }
    val view = LocalView.current
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val clipboard = remember(session) { ClipboardSync(context, session, scope) }

    val connecting = state is SessionState.Connecting
    val connected = state is SessionState.Connected
    val disconnectedReason = (state as? SessionState.Disconnected)?.reason
    val info = (state as? SessionState.Connected)?.info

    fun revealBars() {
        barsVisible = true
        revealTick++
    }

    fun tapKey(js: Int) {
        session.key(js, true)
        session.key(js, false)
        sticky.consumeAfterKey()
    }

    fun typeText(text: String) {
        val single = text.singleOrNull()?.let { AndroidKeyMap.charToKeyStroke(it) }
        if (single != null) {
            if (single.shift) session.key(JsKeyCodes.SHIFT, true)
            session.key(single.jsKeyCode, true)
            session.key(single.jsKeyCode, false)
            if (single.shift) session.key(JsKeyCodes.SHIFT, false)
        } else {
            session.paste(text)
        }
        sticky.consumeAfterKey()
    }

    // Keep the display awake for the whole session.
    DisposableEffect(view) {
        view.keepScreenOn = true
        onDispose { view.keepScreenOn = false }
    }

    // Release everything server-side if the app is backgrounded, so no key or
    // button stays stuck.
    DisposableEffect(view) {
        val owner = view.findViewTreeLifecycleOwner()
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_PAUSE) {
                sticky.reset()
                session.releaseAll()
            }
        }
        owner?.lifecycle?.addObserver(observer)
        onDispose { owner?.lifecycle?.removeObserver(observer) }
    }

    // Push the local clipboard to the server whenever the window regains focus,
    // so text copied in another app is available in the session.
    DisposableEffect(view, clipboard) {
        val observer = view.viewTreeObserver
        val listener = ViewTreeObserver.OnWindowFocusChangeListener { focused ->
            if (focused && session.state.value is SessionState.Connected) {
                clipboard.pushLocalToServer()
            }
        }
        observer.addOnWindowFocusChangeListener(listener)
        onDispose { observer.removeOnWindowFocusChangeListener(listener) }
    }

    LaunchedEffect(keyboardVisible) {
        if (!keyboardVisible) runCatching { rootFocus.requestFocus() }
    }

    LaunchedEffect(revealTick, barsVisible) {
        if (barsVisible) {
            delay(BAR_HIDE_MS)
            barsVisible = false
        }
    }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black)
            .clipToBounds()
            .onSizeChanged { viewSize = it }
            .focusRequester(rootFocus)
            .focusable()
            .onPreviewKeyEvent { event ->
                handleRemoteKeyEvent(event, sticky) { js, down -> session.key(js, down) }
            },
    ) {
        Box(
            modifier = Modifier.layout { measurable, constraints ->
                val vw = constraints.maxWidth.toFloat()
                val vh = constraints.maxHeight.toFloat()
                val z = zoomPan.zoom
                val box = if (remote.width > 0 && remote.height > 0) {
                    ViewportMath.letterbox(vw, vh, remote.width, remote.height)
                } else {
                    LetterboxRect(0f, 0f, vw, vh)
                }
                val w = (box.width * z).roundToInt().coerceAtLeast(1)
                val h = (box.height * z).roundToInt().coerceAtLeast(1)
                val placeable = measurable.measure(Constraints.fixed(w, h))
                layout(constraints.maxWidth, constraints.maxHeight) {
                    placeable.place(
                        (box.left * z + zoomPan.panX).roundToInt(),
                        (box.top * z + zoomPan.panY).roundToInt(),
                    )
                }
            },
        ) {
            VideoSurface(
                modifier = Modifier.fillMaxSize(),
                onSurfaceChanged = { session.setSurface(it) },
            )
        }

        if (connected) {
            Box(
                modifier = Modifier
                    .matchParentSize()
                    .remoteInput(
                        mode = touchMode,
                        pointer = pointer,
                        zoomPan = zoomPan,
                        sink = session,
                        viewSize = { viewSize },
                        remoteSize = { IntSize(remote.width, remote.height) },
                    ),
            )
        }

        CursorOverlay(
            touchMode = touchMode,
            pointer = pointer,
            serverCursorX = serverCursor.x,
            serverCursorY = serverCursor.y,
            serverCursorVisible = serverCursor.visible,
            remoteW = remote.width,
            remoteH = remote.height,
            zoomPan = zoomPan,
        )

        if (statsVisible) {
            StatsHud(
                rttMs = stats.rttMs,
                fps = stats.fps,
                mbps = stats.mbps,
                decodeMs = stats.decodeMs,
                dropped = stats.dropped,
                codec = info?.codec ?: "",
                remoteW = remote.width,
                remoteH = remote.height,
                modifier = Modifier
                    .align(Alignment.TopStart)
                    .padding(12.dp),
            )
        }

        SessionTopBar(
            visible = barsVisible,
            touchMode = touchMode,
            keyboardOn = keyboardVisible,
            specialKeysOn = specialKeysVisible,
            statsOn = statsVisible,
            zoomed = zoomed,
            onToggleTouchMode = {
                revealBars()
                touchMode = if (touchMode == TouchMode.TRACKPAD) TouchMode.DIRECT else TouchMode.TRACKPAD
            },
            onToggleKeyboard = { revealBars(); keyboardVisible = !keyboardVisible },
            onToggleSpecialKeys = { revealBars(); specialKeysVisible = !specialKeysVisible },
            onToggleStats = { revealBars(); statsVisible = !statsVisible },
            onResetZoom = { revealBars(); zoomPan.reset() },
            onClipboard = {
                revealBars()
                clipboard.pushLocalToServer()
            },
            onDisconnect = { confirmDisconnect = true },
            onReveal = { revealBars() },
            modifier = Modifier.align(Alignment.TopCenter),
        )

        if (specialKeysVisible) {
            SpecialKeysBar(
                sticky = sticky,
                onKey = { tapKey(it) },
                modifier = Modifier.align(Alignment.BottomCenter),
            )
        }

        KeyboardLayer(
            visible = keyboardVisible,
            sticky = sticky,
            onText = { typeText(it) },
            onRawKey = { js, down -> session.key(js, down) },
            onDismiss = { keyboardVisible = false },
            modifier = Modifier.align(Alignment.TopStart),
        )

        ConnectionOverlay(
            connecting = connecting,
            disconnectedReason = disconnectedReason,
            onRetry = onRetry ?: onDisconnect,
            onBack = onDisconnect,
        )
    }

    BackHandler { confirmDisconnect = true }

    if (confirmDisconnect) {
        AlertDialog(
            onDismissRequest = { confirmDisconnect = false },
            title = { Text(stringResource(R.string.disconnect_title)) },
            text = { Text(stringResource(R.string.disconnect_message)) },
            confirmButton = {
                TextButton(onClick = {
                    confirmDisconnect = false
                    onDisconnect()
                }) {
                    Text(stringResource(R.string.action_disconnect), color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = {
                TextButton(onClick = { confirmDisconnect = false }) {
                    Text(stringResource(R.string.action_cancel))
                }
            },
        )
    }
}

@Composable
private fun CursorOverlay(
    touchMode: TouchMode,
    pointer: TrackpadPointer,
    serverCursorX: Int,
    serverCursorY: Int,
    serverCursorVisible: Boolean,
    remoteW: Int,
    remoteH: Int,
    zoomPan: ZoomPanState,
) {
    if (remoteW <= 0 || remoteH <= 0) return
    Canvas(modifier = Modifier.fillMaxSize()) {
        // Reads of pointer.position and the transform happen in the draw phase so
        // cursor moves and pans repaint without recomposing the session tree.
        val t = zoomPan.transform
        if (serverCursorVisible) {
            val p = remoteToView(RemotePoint(serverCursorX, serverCursorY), size.width, size.height, remoteW, remoteH, t)
            drawCircle(Color.White.copy(alpha = 0.9f), radius = 4.dp.toPx(), center = p)
            drawCircle(Color.Black.copy(alpha = 0.7f), radius = 4.dp.toPx(), center = p, style = Stroke(width = 1.dp.toPx()))
        }
        if (touchMode == TouchMode.TRACKPAD) {
            val p = remoteToView(pointer.position, size.width, size.height, remoteW, remoteH, t)
            val arm = 9.dp.toPx()
            val line = Color.White.copy(alpha = 0.85f)
            drawLine(line, Offset(p.x - arm, p.y), Offset(p.x + arm, p.y), strokeWidth = 2.dp.toPx())
            drawLine(line, Offset(p.x, p.y - arm), Offset(p.x, p.y + arm), strokeWidth = 2.dp.toPx())
            drawCircle(Color.Black.copy(alpha = 0.6f), radius = 3.dp.toPx(), center = p)
        }
    }
}
