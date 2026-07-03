package com.ddisplay.app.data

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map

private val Context.dataStore: DataStore<Preferences> by preferencesDataStore(name = "ddisplay")

/** Server addresses the user has connected to, newest first, capped and deduped. */
class RecentServers(private val context: Context) {
    private val key = stringPreferencesKey("recent_servers")

    val recent: Flow<List<String>> = context.dataStore.data.map { prefs ->
        prefs[key].toList()
    }

    suspend fun remember(address: String) {
        val trimmed = address.trim()
        if (trimmed.isEmpty()) return
        context.dataStore.edit { prefs ->
            val current = prefs[key].toList()
            prefs[key] = (listOf(trimmed) + current.filter { it != trimmed })
                .take(MAX_ENTRIES)
                .joinToString("\n")
        }
    }

    private fun String?.toList(): List<String> =
        this?.split('\n')?.filter { it.isNotBlank() } ?: emptyList()

    private companion object {
        const val MAX_ENTRIES = 8
    }
}
