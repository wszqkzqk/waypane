package org.waypane.app

import android.app.Application
import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.util.Log
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.serialization.json.Json
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.io.InputStream
import java.util.concurrent.atomic.AtomicLong

private const val MAX_PRIVATE_KEY_BYTES = 64 * 1024
private const val LOG_TAG = "Waypane"

internal fun restrictToOwner(file: File) {
    if (!file.setReadable(false, false) ||
        !file.setReadable(true, true) ||
        !file.setWritable(false, false) ||
        !file.setWritable(true, true)
    ) {
        throw IOException("Failed to restrict permissions on ${file.name}")
    }
}

internal fun importedKeyFile(application: Application): File = application.filesDir.resolve("id_ed25519")

enum class Screen {
    Setup,
    Session,
}

data class ConnectionProfile(
    val host: String = "127.0.0.1",
    val port: Int = 8022,
    val user: String = "",
    val identity: String = "",
    val passphrase: String = "",
    val password: String = "",
    val remoteSocket: String = "",
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
    val hostKeyPrompt: EngineEvent.HostKeyPrompt? = null,
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
        if (!exists() && !createNewFile()) {
            throw IOException("Failed to create known_hosts")
        }
        restrictToOwner(this)
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

    fun importedKeyPath(): String = importedKeyFile(getApplication()).takeIf(File::isFile)?.absolutePath.orEmpty()

    fun importPrivateKey(input: InputStream): String {
        val target = importedKeyFile(getApplication())
        val temporary = target.resolveSibling("${target.name}.tmp")
        try {
            FileOutputStream(temporary).use { output ->
                val buffer = ByteArray(8192)
                var total = 0
                while (true) {
                    val count = input.read(buffer)
                    if (count < 0) break
                    total += count
                    if (total > MAX_PRIVATE_KEY_BYTES) {
                        throw IOException("Private key exceeds 64 KiB")
                    }
                    output.write(buffer, 0, count)
                }
                output.fd.sync()
            }
            restrictToOwner(temporary)
            if (!temporary.renameTo(target)) {
                throw IOException("Failed to move imported private key into app storage")
            }
            restrictToOwner(target)
            return target.absolutePath
        } catch (error: Exception) {
            temporary.delete()
            throw error
        }
    }

    fun deleteImportedKey(): Boolean {
        val target = importedKeyFile(getApplication())
        return !target.exists() || target.delete()
    }

    fun connect(profile: ConnectionProfile) {
        val freshHandle = try {
            NativeBridge.nativeConnect(
                host = profile.host.trim(),
                port = profile.port,
                user = profile.user.trim(),
                identity = profile.identity,
                passphrase = profile.passphrase,
                password = profile.password,
                knownHosts = knownHostsFile,
                remoteSocket = profile.remoteSocket.trim(),
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
            connecting = true,
            hostKeyPrompt = null,
        )
        appendLog("Connecting to ${profile.host}:${profile.port}")
        viewModelScope.launch { consumeEvents(freshHandle) }
    }

    private suspend fun consumeEvents(sessionHandle: Long) {
        while (handle.get() == sessionHandle) {
            val payload = try {
                NativeBridge.nativeNextEvent(sessionHandle)
            } catch (error: RuntimeException) {
                if (handle.get() == sessionHandle) {
                    notifyError("Event channel failed: ${error.message}")
                }
                break
            }
            if (payload == null) {
                delay(25)
                continue
            }
            val event = try {
                json.decodeFromString(EngineEvent.serializer(), payload)
            } catch (_: Exception) {
                appendLog("Ignored malformed native event")
                continue
            }
            if (handle.get() == sessionHandle) applyEvent(event)
        }
    }

    private fun applyEvent(event: EngineEvent) {
        when (event) {
            is EngineEvent.Negotiated -> {
                val message = "Connected to Waypipe protocol 0x${event.version.toString(16)} (server 0x${event.remoteVersion.toString(16)}); waiting for Wayland surfaces"
                Log.i(LOG_TAG, message)
                uiState = uiState.copy(status = message)
            }
            is EngineEvent.Connected -> {
                uiState = uiState.copy(
                    status = "Connected to SSH; waiting for Wayland surfaces",
                    activeSessions = uiState.activeSessions + event.session,
                    connecting = false,
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
            is EngineEvent.HostKeyPrompt -> {
                Log.i(
                    LOG_TAG,
                    "Host key ${event.algorithm} for ${event.host}:${event.port}: ${event.fingerprint} (changed=${event.changed})",
                )
                uiState = uiState.copy(
                    hostKeyPrompt = event,
                    status = if (event.changed) {
                        "SSH host key changed for ${event.host}:${event.port}"
                    } else {
                        "Verify SSH host key for ${event.host}:${event.port}"
                    },
                )
            }
            is EngineEvent.Error -> {
                uiState = uiState.copy(hostKeyPrompt = null)
                notifyError(event.message)
            }
        }
    }

    fun trustHostKey() {
        val active = handle.get()
        if (active == 0L) return
        uiState = uiState.copy(
            hostKeyPrompt = null,
            status = uiState.hostKeyPrompt?.let { prompt ->
                if (prompt.changed) {
                    "Replacing saved SSH host key for ${prompt.host}:${prompt.port}…"
                } else {
                    "Saving SSH host key for ${prompt.host}:${prompt.port}…"
                }
            } ?: uiState.status,
        )
        if (!NativeBridge.nativeResolveHostKey(active, true)) {
            reportError("No SSH host key decision is pending")
        }
    }

    fun rejectHostKey() {
        val prompt = uiState.hostKeyPrompt
        val active = handle.get()
        if (active != 0L) {
            NativeBridge.nativeResolveHostKey(active, false)
        }
        returnToSetup()
        reportError("SSH host key rejected for ${prompt?.host ?: "host"}${prompt?.let { ":${it.port}" }.orEmpty()}")
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
        windows.clear()
        disconnectNative()
        uiState = UiState()
    }

    private fun notifyError(message: String?) {
        val text = message?.takeUnless { it.isBlank() } ?: "Unknown native error"
        reportError(text)
    }

    private fun reportError(message: String) {
        Log.w(LOG_TAG, message)
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
