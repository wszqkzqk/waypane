// Waypane: window-level Wayland forwarding over SSH.
// Copyright (C) 2026  Zhou Qiankang <wszqkzqk@qq.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

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
    @SerialName("negotiated")
    data class Negotiated(
        @SerialName("remote_version") val remoteVersion: Long,
        val version: Long,
    ) : EngineEvent

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
