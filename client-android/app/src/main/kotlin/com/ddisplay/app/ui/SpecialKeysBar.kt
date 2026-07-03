package com.ddisplay.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import com.ddisplay.app.R
import com.ddisplay.app.input.InputSink
import com.ddisplay.core.input.JsKeyCodes

/**
 * Sticky modifier keys for the on-screen keys bar. Toggling one on sends its
 * keydown and holds it; toggling off (or the next non-modifier key via
 * [consumeAfterKey]) sends the keyup. This makes combos like Ctrl+C reachable
 * one key at a time on a touch keyboard.
 */
@Stable
class StickyModifiers(private val sink: InputSink) {
    var ctrl by mutableStateOf(false)
        private set
    var alt by mutableStateOf(false)
        private set
    var shift by mutableStateOf(false)
        private set
    var meta by mutableStateOf(false)
        private set

    fun toggleCtrl() {
        ctrl = !ctrl
        sink.key(JsKeyCodes.CONTROL, ctrl)
    }

    fun toggleAlt() {
        alt = !alt
        sink.key(JsKeyCodes.ALT, alt)
    }

    fun toggleShift() {
        shift = !shift
        sink.key(JsKeyCodes.SHIFT, shift)
    }

    fun toggleMeta() {
        meta = !meta
        sink.key(JsKeyCodes.META_LEFT, meta)
    }

    /** Release every held modifier after a modified key was sent. */
    fun consumeAfterKey() {
        if (ctrl) { sink.key(JsKeyCodes.CONTROL, false); ctrl = false }
        if (alt) { sink.key(JsKeyCodes.ALT, false); alt = false }
        if (shift) { sink.key(JsKeyCodes.SHIFT, false); shift = false }
        if (meta) { sink.key(JsKeyCodes.META_LEFT, false); meta = false }
    }

    fun reset() = consumeAfterKey()
}

@Composable
fun SpecialKeysBar(
    sticky: StickyModifiers,
    onKey: (Int) -> Unit,
    modifier: Modifier = Modifier,
) {
    var showFunctionRow by remember { mutableStateOf(false) }

    Surface(
        color = MaterialTheme.colorScheme.surface,
        tonalElevation = 3.dp,
        modifier = modifier
            .fillMaxWidth()
            .imePadding()
            .windowInsetsPadding(WindowInsets.navigationBars),
    ) {
        Column(
            modifier = Modifier.padding(vertical = 6.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .horizontalScroll(rememberScrollState())
                    .padding(horizontal = 8.dp),
                horizontalArrangement = Arrangement.spacedBy(6.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                ModifierChip(stringResource(R.string.key_ctrl), sticky.ctrl, sticky::toggleCtrl)
                ModifierChip(stringResource(R.string.key_alt), sticky.alt, sticky::toggleAlt)
                ModifierChip(stringResource(R.string.key_shift), sticky.shift, sticky::toggleShift)
                ModifierChip(stringResource(R.string.key_super), sticky.meta, sticky::toggleMeta)
                Divider()
                KeyCap(stringResource(R.string.key_esc)) { onKey(JsKeyCodes.ESCAPE) }
                KeyCap(stringResource(R.string.key_tab)) { onKey(JsKeyCodes.TAB) }
                KeyCap(stringResource(R.string.key_del)) { onKey(JsKeyCodes.DELETE) }
                KeyCap("←") { onKey(JsKeyCodes.ARROW_LEFT) }
                KeyCap("↑") { onKey(JsKeyCodes.ARROW_UP) }
                KeyCap("↓") { onKey(JsKeyCodes.ARROW_DOWN) }
                KeyCap("→") { onKey(JsKeyCodes.ARROW_RIGHT) }
                KeyCap(stringResource(R.string.key_home)) { onKey(JsKeyCodes.HOME) }
                KeyCap(stringResource(R.string.key_end)) { onKey(JsKeyCodes.END) }
                KeyCap(stringResource(R.string.key_pgup)) { onKey(JsKeyCodes.PAGE_UP) }
                KeyCap(stringResource(R.string.key_pgdn)) { onKey(JsKeyCodes.PAGE_DOWN) }
                ModifierChip(stringResource(R.string.key_fn), showFunctionRow) {
                    showFunctionRow = !showFunctionRow
                }
            }
            if (showFunctionRow) {
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .horizontalScroll(rememberScrollState())
                        .padding(horizontal = 8.dp),
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    for (i in 0 until 12) {
                        KeyCap("F${i + 1}") { onKey(JsKeyCodes.F1 + i) }
                    }
                }
            }
        }
    }
}

@Composable
private fun ModifierChip(label: String, selected: Boolean, onClick: () -> Unit) {
    FilterChip(selected = selected, onClick = onClick, label = { Text(label) })
}

@Composable
private fun KeyCap(label: String, onClick: () -> Unit) {
    Surface(
        shape = RoundedCornerShape(8.dp),
        color = MaterialTheme.colorScheme.surfaceVariant,
        contentColor = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier
            .heightIn(min = 40.dp)
            .widthIn(min = 44.dp)
            .clickable(onClick = onClick),
    ) {
        Box(
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
            contentAlignment = Alignment.Center,
        ) {
            Text(label, style = MaterialTheme.typography.labelLarge)
        }
    }
}

@Composable
private fun Divider() {
    Spacer(
        modifier = Modifier
            .width(1.dp)
            .heightIn(min = 28.dp)
            .background(MaterialTheme.colorScheme.outlineVariant),
    )
}
