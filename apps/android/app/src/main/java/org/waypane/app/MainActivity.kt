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
