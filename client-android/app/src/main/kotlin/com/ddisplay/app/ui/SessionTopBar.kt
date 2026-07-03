package com.ddisplay.app.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AssistChip
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import com.ddisplay.app.R
import com.ddisplay.app.input.TouchMode

@Composable
fun SessionTopBar(
    visible: Boolean,
    touchMode: TouchMode,
    keyboardOn: Boolean,
    specialKeysOn: Boolean,
    statsOn: Boolean,
    zoomed: Boolean,
    onToggleTouchMode: () -> Unit,
    onToggleKeyboard: () -> Unit,
    onToggleSpecialKeys: () -> Unit,
    onToggleStats: () -> Unit,
    onResetZoom: () -> Unit,
    onClipboard: () -> Unit,
    onDisconnect: () -> Unit,
    onReveal: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Box(modifier.fillMaxWidth().windowInsetsPadding(WindowInsets.statusBars)) {
        AnimatedVisibility(
            visible = visible,
            enter = slideInVertically { -it } + fadeIn(),
            exit = slideOutVertically { -it } + fadeOut(),
            modifier = Modifier.align(Alignment.TopCenter),
        ) {
            Surface(
                shape = RoundedCornerShape(16.dp),
                color = MaterialTheme.colorScheme.surface.copy(alpha = 0.92f),
                tonalElevation = 4.dp,
                modifier = Modifier.padding(8.dp),
            ) {
                Row(
                    modifier = Modifier
                        .horizontalScroll(rememberScrollState())
                        .padding(horizontal = 8.dp, vertical = 4.dp),
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    val modeLabel = stringResource(
                        if (touchMode == TouchMode.TRACKPAD) R.string.mode_trackpad else R.string.mode_direct,
                    )
                    AssistChip(onClick = onToggleTouchMode, label = { Text(modeLabel) })
                    FilterChip(
                        selected = keyboardOn,
                        onClick = onToggleKeyboard,
                        label = { Text(stringResource(R.string.action_keyboard)) },
                    )
                    FilterChip(
                        selected = specialKeysOn,
                        onClick = onToggleSpecialKeys,
                        label = { Text(stringResource(R.string.action_keys)) },
                    )
                    FilterChip(
                        selected = statsOn,
                        onClick = onToggleStats,
                        label = { Text(stringResource(R.string.action_hud)) },
                    )
                    AssistChip(onClick = onClipboard, label = { Text(stringResource(R.string.action_clipboard)) })
                    if (zoomed) {
                        AssistChip(onClick = onResetZoom, label = { Text(stringResource(R.string.action_reset_zoom)) })
                    }
                    TextButton(onClick = onDisconnect) {
                        Text(
                            stringResource(R.string.action_disconnect),
                            color = MaterialTheme.colorScheme.error,
                        )
                    }
                }
            }
        }

        if (!visible) {
            Box(
                modifier = Modifier
                    .align(Alignment.TopCenter)
                    .padding(top = 4.dp)
                    .clickable(onClick = onReveal)
                    .padding(8.dp),
            ) {
                Box(
                    modifier = Modifier
                        .width(44.dp)
                        .height(5.dp)
                        .background(
                            MaterialTheme.colorScheme.onSurface.copy(alpha = 0.4f),
                            RoundedCornerShape(3.dp),
                        ),
                )
            }
        }
    }
}

@Composable
fun StatsHud(
    rttMs: Float,
    fps: Float,
    mbps: Float,
    decodeMs: Float,
    dropped: Int,
    codec: String,
    remoteW: Int,
    remoteH: Int,
    modifier: Modifier = Modifier,
) {
    Surface(
        color = Color.Black.copy(alpha = 0.55f),
        contentColor = Color.White,
        shape = RoundedCornerShape(8.dp),
        modifier = modifier,
    ) {
        Column(modifier = Modifier.padding(horizontal = 10.dp, vertical = 8.dp)) {
            HudLine(stringResource(R.string.hud_rtt, formatMs(rttMs)))
            HudLine(stringResource(R.string.hud_fps, formatOne(fps)))
            HudLine(stringResource(R.string.hud_mbps, formatOne(mbps)))
            HudLine(stringResource(R.string.hud_decode, formatMs(decodeMs)))
            HudLine(stringResource(R.string.hud_dropped, dropped))
            HudLine(stringResource(R.string.hud_codec, codec.ifEmpty { "-" }))
            HudLine(stringResource(R.string.hud_size, remoteW, remoteH))
        }
    }
}

@Composable
private fun HudLine(text: String) {
    Text(
        text = text,
        style = MaterialTheme.typography.labelSmall,
        fontFamily = FontFamily.Monospace,
    )
}

@Composable
fun ConnectionOverlay(
    connecting: Boolean,
    disconnectedReason: String?,
    onRetry: () -> Unit,
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
) {
    if (!connecting && disconnectedReason == null) return
    Box(
        modifier = modifier
            .fillMaxSize()
            .background(Color.Black.copy(alpha = 0.6f)),
        contentAlignment = Alignment.Center,
    ) {
        if (connecting) {
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                CircularProgressIndicator()
                Text(
                    stringResource(R.string.status_connecting),
                    modifier = Modifier.padding(top = 12.dp),
                    color = Color.White,
                )
            }
        } else {
            Surface(
                shape = RoundedCornerShape(16.dp),
                color = MaterialTheme.colorScheme.surface,
                tonalElevation = 6.dp,
                modifier = Modifier.padding(24.dp),
            ) {
                Column(
                    modifier = Modifier.padding(20.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Text(
                        stringResource(R.string.status_disconnected),
                        style = MaterialTheme.typography.titleMedium,
                    )
                    val reason = disconnectedReason?.takeIf { it.isNotBlank() }
                    if (reason != null) {
                        Text(reason, style = MaterialTheme.typography.bodyMedium)
                    }
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        TextButton(onClick = onBack) { Text(stringResource(R.string.action_back)) }
                        AssistChip(onClick = onRetry, label = { Text(stringResource(R.string.action_retry)) })
                    }
                }
            }
        }
    }
}

private fun formatMs(v: Float): String = String.format("%.0f", v)

private fun formatOne(v: Float): String = String.format("%.1f", v)
