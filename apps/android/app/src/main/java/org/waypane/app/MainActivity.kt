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

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.darkColorScheme
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.core.view.WindowCompat

private const val STATE_SESSION_ACTIVE = "waypane.session_active"

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val model = (application as WaypaneApplication).sessionModel
        if (savedInstanceState?.getBoolean(STATE_SESSION_ACTIVE) == true &&
            model.nativeHandle() == 0L
        ) {
            model.reportSessionLost(
                "The previous session ended because Android reclaimed Waypane while it was in the background. Connect again to continue.",
            )
        }
        enableEdgeToEdge()
        WindowCompat.getInsetsController(window, window.decorView).apply {
            isAppearanceLightStatusBars = false
            isAppearanceLightNavigationBars = false
        }
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(modifier = Modifier.fillMaxSize(), color = Color.Black) {
                    WaypaneApp(model)
                }
            }
        }
    }

    override fun onSaveInstanceState(outState: Bundle) {
        val model = (application as WaypaneApplication).sessionModel
        outState.putBoolean(STATE_SESSION_ACTIVE, model.uiState.screen == Screen.Session)
        super.onSaveInstanceState(outState)
    }
}
