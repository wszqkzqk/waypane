package org.waypane.app

import androidx.compose.runtime.Immutable
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonClassDiscriminator

@Immutable
@Serializable
data class SurfaceKey(
    val session: Long,
    val window: Int,
)

@Immutable
@Serializable
data class AndroidWindow(
    val key: SurfaceKey,
    val title: String,
    @SerialName("app_id") val appId: String,
    val width: Int,
    val height: Int,
) {
    val displayTitle: String
        get() = title.ifBlank { appId.ifBlank { "Surface ${key.window}" }.let { "$it — Waypane" } }

    val aspectRatio: Float?
        get() = if (width > 0 && height > 0) width.toFloat() / height.toFloat() else null
}

@OptIn(kotlinx.serialization.ExperimentalSerializationApi::class)
@JsonClassDiscriminator("type")
@Serializable
sealed interface EngineEvent {
    @Serializable
    @SerialName("connected")
    data class Connected(val session: Long) : EngineEvent

    @Serializable
    @SerialName("window_upsert")
    data class WindowUpsert(
        val key: SurfaceKey,
        val title: String,
        @SerialName("app_id") val appId: String,
        val width: Int,
        val height: Int,
    ) : EngineEvent {
        fun toWindow() = AndroidWindow(key, title, appId, width, height)
    }

    @Serializable
    @SerialName("window_removed")
    data class WindowRemoved(val key: SurfaceKey) : EngineEvent

    @Serializable
    @SerialName("session_closed")
    data class SessionClosed(val session: Long) : EngineEvent

    @Serializable
    @SerialName("status")
    data class Status(val message: String) : EngineEvent

    @Serializable
    @SerialName("clipboard_text")
    data class ClipboardText(val session: Long, val text: String) : EngineEvent

    @Serializable
    @SerialName("host_key_prompt")
    data class HostKeyPrompt(
        val host: String,
        val port: Int,
        val algorithm: String,
        val fingerprint: String,
        val changed: Boolean,
    ) : EngineEvent

    @Serializable
    @SerialName("error")
    data class Error(val session: Long?, val message: String) : EngineEvent
}
