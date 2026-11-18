package org.waypane.app

import android.app.Application
import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import java.util.concurrent.atomic.AtomicLong

enum class Screen {
    Setup,
    Session,
}

data class ConnectionProfile(
    val host: String = "127.0.0.1",
    val port: Int = 8022,
    val user: String = "",
    val identity: String = "",
    val password: String = "",
    val acceptNewHostKey: Boolean = false,
    val compression: String = "lz4",
    val command: String = "foot",
)

data class UiState(
    val screen: Screen = Screen.Setup,
    val status: String = "",
    val error: String? = null,
    val selected: SurfaceKey? = null,
    val activeSessions: Set<Long> = emptySet(),
    val connecting: Boolean = false,
)

class WaypaneViewModel(application: Application) : AndroidViewModel(application) {
    private val handle = AtomicLong(0)
    private val json = Json { ignoreUnknownKeys = true }
    private val clipboardManager = application.getSystemService(ClipboardManager::class.java)
    private var lastClipboardText: String? = null
    private val clipboardListener = ClipboardManager.OnPrimaryClipChangedListener {
        val text = readSystemClipboard() ?: return@OnPrimaryClipChangedListener
        if (text == lastClipboardText) return@OnPrimaryClipChangedListener
        lastClipboardText = text
        val active = handle.get()
        if (active != 0L) {
            runCatching { NativeBridge.nativeSetClipboardText(active, text) }
        }
    }
    private val knownHostsPath = application.filesDir.resolve("known_hosts").apply {
        if (!exists()) createNewFile()
        setReadable(false, false)
        setReadable(true, true)
        setWritable(false, false)
        setWritable(true, true)
    }

    var uiState by mutableStateOf(UiState())
        private set
    val windows = mutableStateMapOf<SurfaceKey, AndroidWindow>()
    val logLines = mutableStateListOf<String>()

    val knownHostsFile: String
        get() = knownHostsPath.absolutePath

    init {
        lastClipboardText = readSystemClipboard()
        clipboardManager?.addPrimaryClipChangedListener(clipboardListener)
    }

    private fun readSystemClipboard(): String? {
        val clip = runCatching { clipboardManager?.primaryClip }.getOrNull() ?: return null
        if (clip.itemCount == 0) return null
        val description = clip.description ?: return null
        val textMime = description.hasMimeType(ClipDescription.MIMETYPE_TEXT_PLAIN)
        val htmlMime = description.hasMimeType(ClipDescription.MIMETYPE_TEXT_HTML)
        if (!textMime && !htmlMime) return null
        return runCatching {
            clip.getItemAt(0).coerceToText(getApplication<Application>()).toString()
        }.getOrNull()
    }

    fun nativeHandle(): Long = handle.get()

    fun connect(profile: ConnectionProfile) {
        val freshHandle = try {
            NativeBridge.nativeConnect(
                host = profile.host.trim(),
                port = profile.port,
                user = profile.user.trim(),
                identity = profile.identity,
                password = profile.password,
                knownHosts = knownHostsFile,
                acceptNewHostKey = profile.acceptNewHostKey,
                compression = profile.compression,
                command = profile.command.trim(),
            )
        } catch (error: RuntimeException) {
            reportError(error.message ?: error.toString())
            return
        }
        if (freshHandle == 0L) {
            reportError("Failed to create a native session")
            return
        }
        disconnectNative()
        check(handle.compareAndSet(0L, freshHandle)) { "session handle was already started" }
        lastClipboardText?.takeIf { it.isNotEmpty() }?.let { text ->
            runCatching { NativeBridge.nativeSetClipboardText(freshHandle, text) }
        }
        windows.clear()
        uiState = uiState.copy(
            screen = Screen.Session,
            status = "Connecting to ${profile.host}:${profile.port}…",
            error = null,
            selected = null,
            activeSessions = emptySet(),
            connecting = false,
        )
        appendLog("Connecting to ${profile.host}:${profile.port}")
        viewModelScope.launch(Dispatchers.IO) { consumeEvents(freshHandle) }
    }

    private suspend fun consumeEvents(sessionHandle: Long) {
        while (handle.get() == sessionHandle) {
            val payload = try {
                NativeBridge.nativeNextEvent(sessionHandle)
            } catch (error: RuntimeException) {
                withContext(Dispatchers.Main) {
                    if (handle.get() == sessionHandle) {
                        notifyError("Event channel failed: ${error.message}")
                    }
                }
                break
            }
            if (payload == null) {
                delay(50)
                continue
            }
            val event = try {
                json.decodeFromString(EngineEvent.serializer(), payload)
            } catch (error: Exception) {
                withContext(Dispatchers.Main) { appendLog("Ignored malformed native event") }
                continue
            }
            withContext(Dispatchers.Main) {
                if (handle.get() == sessionHandle) applyEvent(event)
            }
        }
    }

    private fun applyEvent(event: EngineEvent) {
        when (event) {
            is EngineEvent.Connected -> {
                uiState = uiState.copy(
                    status = "Connected to SSH; waiting for Wayland surfaces",
                    activeSessions = uiState.activeSessions + event.session,
                )
            }
            is EngineEvent.WindowUpsert -> {
                val window = event.toWindow()
                windows[window.key] = window
                if (uiState.selected == null) {
                    uiState = uiState.copy(selected = window.key)
                }
            }
            is EngineEvent.WindowRemoved -> {
                windows.remove(event.key)
                if (uiState.selected == event.key) {
                    uiState = uiState.copy(selected = windows.keys.sortedWith(compareBy({ it.session }, { it.window })).firstOrNull())
                }
            }
            is EngineEvent.SessionClosed -> {
                windows.keys.filter { it.session == event.session }.forEach(windows::remove)
                val active = uiState.activeSessions - event.session
                uiState = uiState.copy(
                    activeSessions = active,
                    selected = uiState.selected?.takeIf { it.session != event.session }
                        ?: windows.keys.firstOrNull(),
                    status = if (active.isEmpty()) "All remote sessions closed" else uiState.status,
                )
            }
            is EngineEvent.Status -> {
                event.message.lineSequence().filter { it.isNotBlank() }.take(32).forEach { message ->
                    appendLog(message)
                    uiState = uiState.copy(status = message)
                }
            }
            is EngineEvent.ClipboardText -> {
                if (event.text != lastClipboardText) {
                    lastClipboardText = event.text
                    clipboardManager?.setPrimaryClip(ClipData.newPlainText("Waypane", event.text))
                }
            }
            is EngineEvent.Error -> notifyError(event.message)
        }
    }

    fun selectWindow(key: SurfaceKey) {
        if (windows.containsKey(key)) {
            uiState = uiState.copy(selected = key)
        }
    }

    fun closeWindow(key: SurfaceKey) {
        if (!NativeBridge.nativeCloseWindow(handle.get(), key.session, key.window)) {
            reportError("Failed to queue window close")
        }
    }

    fun resizeWindow(key: SurfaceKey, width: Int, height: Int) {
        NativeBridge.nativeResizeWindow(handle.get(), key.session, key.window, width.coerceAtLeast(1), height.coerceAtLeast(1))
    }

    fun disconnectSession(session: Long) {
        NativeBridge.nativeDisconnectSession(handle.get(), session)
    }

    fun reportUnsupportedText(text: String) {
        val message = "Non-ASCII text '$text' requires text-input-v3 support"
        uiState = uiState.copy(status = message)
        appendLog(message)
    }

    fun clearError() {
        uiState = uiState.copy(error = null)
    }

    fun returnToSetup() {
        uiState = uiState.copy(
            screen = Screen.Setup,
            status = "",
            error = null,
            selected = null,
            activeSessions = emptySet(),
            connecting = false,
        )
        windows.clear()
        disconnectNative()
    }

    private fun notifyError(message: String?) {
        val text = message?.takeUnless { it.isBlank() } ?: "Unknown native error"
        reportError(text)
    }

    private fun reportError(message: String) {
        uiState = uiState.copy(error = message, status = message, connecting = false)
        appendLog(message)
    }

    private fun appendLog(message: String) {
        message.lineSequence().forEach { line ->
            logLines += line.take(1000)
        }
        while (logLines.size > 200) {
            logLines.removeAt(0)
        }
    }

    private fun disconnectNative() {
        val previous = handle.getAndSet(0L)
        if (previous != 0L) {
            NativeBridge.nativeDestroy(previous)
        }
    }

    override fun onCleared() {
        clipboardManager?.removePrimaryClipChangedListener(clipboardListener)
        disconnectNative()
        super.onCleared()
    }
}
