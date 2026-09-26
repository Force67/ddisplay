package com.ddisplay.app.ui

import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.input.key.KeyEvent
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.dp
import com.ddisplay.app.input.AndroidKeyMap
import com.ddisplay.core.input.JsKeyCodes

private val MODIFIER_CODES = setOf(
    JsKeyCodes.SHIFT, JsKeyCodes.CONTROL, JsKeyCodes.ALT,
    JsKeyCodes.META_LEFT, JsKeyCodes.META_RIGHT,
)

/**
 * One handler for both the hidden IME field and the always-focused session root.
 * Hardware keys (real device id) are forwarded as key events with the OS's own
 * modifiers, never pasted. Soft-keyboard character keys are let through so the
 * IME commits them to the field, which routes them by character. Soft special
 * keys (Backspace, Enter, arrows) still arrive as key events and go straight
 * through; after a virtual non-modifier key any sticky modifiers are released.
 */
fun handleRemoteKeyEvent(
    event: KeyEvent,
    sticky: StickyModifiers,
    onRawKey: (jsKeyCode: Int, down: Boolean) -> Unit,
): Boolean {
    val native = event.nativeKeyEvent
    val js = AndroidKeyMap.androidKeyToJs(native.keyCode) ?: return false
    val virtual = native.deviceId <= 0
    if (virtual && AndroidKeyMap.producesText(js)) return false

    val down = event.type == KeyEventType.KeyDown
    onRawKey(js, down)
    if (virtual && !down && js !in MODIFIER_CODES) sticky.consumeAfterKey()
    return true
}

/**
 * Off-screen text field that owns the IME. Toggling [visible] shows or hides the
 * soft keyboard; committed text is delivered to [onText] (a single mappable
 * character is typed as key events by the caller, anything else is pasted).
 */
@Composable
fun KeyboardLayer(
    visible: Boolean,
    sticky: StickyModifiers,
    onText: (String) -> Unit,
    onRawKey: (Int, Boolean) -> Unit,
    onDismiss: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val focusRequester = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    var field by remember { mutableStateOf(TextFieldValue("")) }

    LaunchedEffect(visible) {
        if (visible) {
            focusRequester.requestFocus()
            keyboard?.show()
        } else {
            keyboard?.hide()
        }
    }

    BasicTextField(
        value = field,
        onValueChange = { next ->
            if (next.text.isNotEmpty()) {
                onText(next.text)
                field = TextFieldValue("")
            } else {
                field = next
            }
        },
        modifier = modifier
            .size(1.dp)
            .alpha(0f)
            .focusRequester(focusRequester)
            .onFocusChanged { if (!it.isFocused && visible) onDismiss() }
            .onPreviewKeyEvent { handleRemoteKeyEvent(it, sticky, onRawKey) },
        keyboardOptions = KeyboardOptions(
            autoCorrectEnabled = false,
            keyboardType = KeyboardType.Ascii,
            imeAction = ImeAction.None,
        ),
        cursorBrush = SolidColor(Color.Transparent),
    )
}
