package org.waypane.app

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import kotlinx.coroutines.delay
import java.io.File

@Composable
fun WaypaneApp(model: WaypaneViewModel) {
    val state = model.uiState
    BackHandler(enabled = state.screen == Screen.Session) {
        model.returnToSetup()
    }
    when (state.screen) {
        Screen.Setup -> SetupScreen(model)
        Screen.Session -> SessionScreen(model, state)
    }
}

@Composable
private fun SetupScreen(model: WaypaneViewModel) {
    val context = LocalContext.current
    var host by rememberSaveable { mutableStateOf("127.0.0.1") }
    var port by rememberSaveable { mutableStateOf("8022") }
    var user by rememberSaveable { mutableStateOf("") }
    var command by rememberSaveable { mutableStateOf("foot") }
    var compression by rememberSaveable { mutableStateOf("lz4") }
    var identity by rememberSaveable { mutableStateOf("") }
    var acceptNewHostKey by rememberSaveable { mutableStateOf(false) }
    var password by remember { mutableStateOf("") }
    var localError by remember { mutableStateOf<String?>(null) }
    var connecting by remember { mutableStateOf(false) }

    val importKey = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        runCatching {
            val target = File(context.filesDir, "id_ed25519")
            val stream = context.contentResolver.openInputStream(uri)
                ?: error("Cannot open the selected private key")
            stream.use { input ->
                target.outputStream().use { output -> input.copyTo(output) }
            }
            target.setReadable(false, false)
            target.setReadable(true, true)
            target.setWritable(false, false)
            target.setWritable(true, true)
            identity = target.absolutePath
            localError = null
        }.onFailure { error ->
            localError = "Failed to import private key: ${error.message}"
        }
    }

    val portNumber = port.toIntOrNull()
    val canConnect = host.isNotBlank() && user.isNotBlank() && command.isNotBlank() && portNumber in 1..65535
    val submit: () -> Unit = {
        localError = null
        connecting = true
        model.connect(
            ConnectionProfile(
                host = host,
                port = portNumber ?: 22,
                user = user,
                identity = identity,
                password = password,
                acceptNewHostKey = acceptNewHostKey,
                compression = compression,
                command = command,
            ),
        )
        password = ""
        connecting = false
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 24.dp, vertical = 32.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("Waypane", style = MaterialTheme.typography.displaySmall)
        Text(
            "Display Wayland applications from a headless Linux or Termux SSH host.",
            style = MaterialTheme.typography.bodyLarge,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(host, { host = it }, label = { Text("SSH host") }, singleLine = true, modifier = Modifier.fillMaxWidth())
        OutlinedTextField(
            port,
            { port = it.filter(Char::isDigit).take(5) },
            label = { Text("SSH port") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
            modifier = Modifier.widthIn(max = 220.dp),
        )
        OutlinedTextField(
            user,
            { user = it },
            label = { Text("SSH user") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Text),
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            password,
            { password = it },
            label = { Text("Password (optional, memory only)") },
            singleLine = true,
            visualTransformation = PasswordVisualTransformation(),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
            modifier = Modifier.fillMaxWidth(),
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(onClick = { importKey.launch(arrayOf("*/*")) }) {
                Text(if (identity.isBlank()) "Import SSH private key" else "SSH private key imported")
            }
            if (identity.isNotBlank()) {
                OutlinedButton(onClick = { identity = "" }) { Text("Use agent/default keys") }
            }
        }
        Text("Compression")
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            listOf("lz4", "zstd", "none").forEach { mode ->
                FilterChip(
                    selected = compression == mode,
                    onClick = { compression = mode },
                    label = { Text(mode.uppercase()) },
                )
            }
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(acceptNewHostKey, { acceptNewHostKey = it })
            Column {
                Text("Accept unknown host keys")
                Text(
                    "Security risk. Leave disabled to verify against ${model.knownHostsFile}",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        OutlinedTextField(
            command,
            { command = it },
            label = { Text("Remote command") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
            keyboardActions = KeyboardActions(onDone = { if (canConnect) submit() }),
            modifier = Modifier.fillMaxWidth(),
        )
        Text(
            "Remote applications render through Waypipe --no-gpu, so a headless server is supported.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        (localError ?: model.uiState.error)?.let { message ->
            ErrorCard(message) {
                localError = null
                model.clearError()
            }
        }
        Button(
            enabled = canConnect && !connecting,
            onClick = submit,
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text(if (connecting) "Starting…" else "Connect")
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun SessionScreen(model: WaypaneViewModel, state: UiState) {
    val configuration = LocalConfiguration.current
    val density = LocalDensity.current.density
    val selected = state.selected
    val selectedWindow = selected?.let(model.windows::get)

    Scaffold(
        topBar = {
            TopAppBar(
                navigationIcon = {
                    IconButton(onClick = model::returnToSetup) { Text("Back") }
                },
                title = {
                    Text(
                        selectedWindow?.displayTitle ?: "Waypane",
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                },
                actions = {
                    selectedWindow?.let { window ->
                        IconButton(onClick = { model.closeWindow(window.key) }) { Text("Close") }
                    }
                },
            )
        },
        containerColor = Color.Black,
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            if (model.windows.size > 1) {
                LazyRow(
                    modifier = Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.surface).padding(4.dp),
                    horizontalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    items(model.windows.values.sortedWith(compareBy({ it.key.session }, { it.key.window })), key = { it.key }) { window ->
                        FilterChip(
                            selected = window.key == selected,
                            onClick = { model.selectWindow(window.key) },
                            label = { Text(window.displayTitle, maxLines = 1, modifier = Modifier.widthIn(max = 220.dp)) },
                        )
                    }
                }
            }
            BoxWithConstraints(Modifier.weight(1f).fillMaxWidth().background(Color.Black)) {
                val ratio = selectedWindow?.aspectRatio
                if (selected != null && selectedWindow != null) {
                    key(selected) {
                    AndroidView(
                        factory = { context ->
                            RemoteSurfaceView(context, selected, model::nativeHandle) { text ->
                                model.reportUnsupportedText(text)
                            }
                        },
                        modifier = if (ratio != null) {
                            Modifier.fillMaxSize().aspectRatio(ratio, matchHeightConstraintsFirst = true)
                        } else {
                            Modifier.fillMaxSize()
                        },
                    )
                    }
                    if (configuration.smallestScreenWidthDp >= 600) {
                        LaunchedEffect(selected, maxWidth, maxHeight) {
                            delay(250)
                            model.resizeWindow(
                                selected,
                                (maxWidth.value * density).toInt().coerceAtLeast(1),
                                (maxHeight.value * density).toInt().coerceAtLeast(1),
                            )
                        }
                    }
                } else {
                    Column(
                        Modifier.fillMaxSize(),
                        horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.Center,
                    ) {
                        if (state.activeSessions.isNotEmpty()) {
                            CircularProgressIndicator()
                            Spacer(Modifier.height(16.dp))
                        }
                        Text(
                            if (state.activeSessions.isEmpty()) "No remote windows" else "Waiting for Wayland surfaces…",
                            color = Color.White,
                        )
                    }
                }
            }
            state.error?.let { message ->
                ErrorCard(message, model::clearError)
            }
            if (state.status.isNotBlank()) {
                Text(
                    state.status,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.surface).padding(8.dp),
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

@Composable
private fun ErrorCard(message: String, onDismiss: () -> Unit) {
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.errorContainer),
        modifier = Modifier.fillMaxWidth().padding(8.dp),
    ) {
        Row(Modifier.padding(12.dp), verticalAlignment = Alignment.CenterVertically) {
            Text(message, color = MaterialTheme.colorScheme.onErrorContainer, modifier = Modifier.weight(1f), maxLines = 4)
            IconButton(onClick = onDismiss) { Text("Dismiss", color = MaterialTheme.colorScheme.onErrorContainer) }
        }
    }
}
