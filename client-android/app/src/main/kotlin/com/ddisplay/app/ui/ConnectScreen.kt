package com.ddisplay.app.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.ddisplay.app.R
import com.ddisplay.app.data.RecentServers
import kotlinx.coroutines.launch

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConnectScreen(
    onConnect: (String) -> Unit,
    connecting: Boolean = false,
    errorMessage: String? = null,
) {
    val context = LocalContext.current
    val recentServers = remember { RecentServers(context) }
    val recent by recentServers.recent.collectAsState(initial = emptyList())
    val scope = rememberCoroutineScope()
    var address by rememberSaveable { mutableStateOf("") }

    val valid = isValidServer(address)

    // AppState records recents and normalizes the address; the screen passes the
    // raw host:port through so recents stay free of a scheme prefix.
    fun connect(target: String) {
        val trimmed = target.trim()
        if (!isValidServer(trimmed)) return
        onConnect(trimmed)
    }

    Scaffold(topBar = { TopAppBar(title = { Text(stringResource(R.string.app_name)) }) }) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            OutlinedTextField(
                value = address,
                onValueChange = { address = it },
                label = { Text(stringResource(R.string.connect_field_label)) },
                placeholder = { Text("192.168.1.10:9550") },
                singleLine = true,
                isError = address.isNotBlank() && !valid,
                supportingText = {
                    val invalid = address.isNotBlank() && !valid
                    val message = errorMessage ?: if (invalid) stringResource(R.string.connect_invalid) else null
                    if (message != null) {
                        Text(message, color = MaterialTheme.colorScheme.error)
                    }
                },
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                enabled = !connecting,
                modifier = Modifier.fillMaxWidth(),
            )
            Button(
                onClick = { connect(address) },
                enabled = valid && !connecting,
                modifier = Modifier.fillMaxWidth(),
            ) {
                if (connecting) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(18.dp),
                        strokeWidth = 2.dp,
                        color = MaterialTheme.colorScheme.onPrimary,
                    )
                } else {
                    Text(stringResource(R.string.connect_button))
                }
            }

            if (recent.isNotEmpty()) {
                HorizontalDivider()
                Text(stringResource(R.string.connect_recent), style = MaterialTheme.typography.titleMedium)
                LazyColumn(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    items(recent) { server ->
                        ListItem(
                            headlineContent = { Text(server) },
                            trailingContent = {
                                TextButton(onClick = { scope.launch { recentServers.forget(server) } }) {
                                    Text(stringResource(R.string.connect_forget))
                                }
                            },
                            modifier = Modifier
                                .fillMaxWidth()
                                .clickable(enabled = !connecting) {
                                    address = server
                                    connect(server)
                                },
                        )
                    }
                }
            }
        }
    }
}

/** host, host:port, or a ws://|wss:// URL of either, with a port in 1..65535. */
private fun isValidServer(input: String): Boolean {
    val body = input.trim().removePrefix("ws://").removePrefix("wss://")
    if (body.isEmpty() || body.any { it.isWhitespace() }) return false
    val parts = body.split(":")
    if (parts.size > 2 || parts[0].isEmpty()) return false
    if (parts.size == 2) {
        val port = parts[1].toIntOrNull() ?: return false
        if (port !in 1..65535) return false
    }
    return true
}
