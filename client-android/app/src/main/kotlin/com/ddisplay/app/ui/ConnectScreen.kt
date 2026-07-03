package com.ddisplay.app.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
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
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.ddisplay.app.data.RecentServers
import kotlinx.coroutines.launch

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConnectScreen(onConnect: (String) -> Unit) {
    val context = LocalContext.current
    val recentServers = remember { RecentServers(context) }
    val recent by recentServers.recent.collectAsState(initial = emptyList())
    val scope = rememberCoroutineScope()
    var address by rememberSaveable { mutableStateOf("") }

    fun connect(target: String) {
        val trimmed = target.trim()
        if (trimmed.isEmpty()) return
        scope.launch { recentServers.remember(trimmed) }
        onConnect(normalizeServerUrl(trimmed))
    }

    Scaffold(topBar = { TopAppBar(title = { Text("ddisplay") }) }) { padding ->
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
                label = { Text("Server address") },
                placeholder = { Text("192.168.1.10:9550") },
                singleLine = true,
                keyboardOptions = androidx.compose.foundation.text.KeyboardOptions(
                    keyboardType = KeyboardType.Uri,
                ),
                modifier = Modifier.fillMaxWidth(),
            )
            Button(
                onClick = { connect(address) },
                enabled = address.isNotBlank(),
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text("Connect")
            }

            if (recent.isNotEmpty()) {
                HorizontalDivider()
                Text("Recent", style = MaterialTheme.typography.titleMedium)
                LazyColumn(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    items(recent) { server ->
                        ListItem(
                            headlineContent = { Text(server) },
                            modifier = Modifier
                                .fillMaxWidth()
                                .clickable {
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

/** Prepend ws:// when the user typed a bare host:port. */
private fun normalizeServerUrl(input: String): String =
    if (input.startsWith("ws://") || input.startsWith("wss://")) input else "ws://$input"
