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
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
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
import androidx.compose.ui.text.font.FontFamily
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

private enum class AuthenticationMode(val label: String) {
    PrivateKey("Private key"),
    Password("Password"),
}

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
    state.hostKeyPrompt?.let { prompt ->
        HostKeyDialog(prompt, onTrust = model::trustHostKey, onReject = model::rejectHostKey)
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
    var identity by rememberSaveable { mutableStateOf(model.importedKeyPath()) }
    var remoteSocket by rememberSaveable { mutableStateOf("") }
    var authentication by rememberSaveable {
        mutableStateOf(if (identity.isBlank()) AuthenticationMode.Password else AuthenticationMode.PrivateKey)
    }
    var passphrase by remember { mutableStateOf("") }
    var password by remember { mutableStateOf("") }
    var localError by remember { mutableStateOf<String?>(null) }
    var starting by remember { mutableStateOf(false) }

    val importKey = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        runCatching {
            val stream = context.contentResolver.openInputStream(uri)
                ?: error("Cannot open the selected private key")
            stream.use { input ->
                identity = model.importPrivateKey(input)
            }
            authentication = AuthenticationMode.PrivateKey
            localError = null
        }.onFailure { error ->
            localError = "Failed to import private key: ${error.message}"
        }
    }

    val portNumber = port.toIntOrNull()
    val hasAuthentication = when (authentication) {
        AuthenticationMode.PrivateKey -> identity.isNotBlank()
        AuthenticationMode.Password -> password.isNotEmpty()
    }
    val canConnect = host.isNotBlank() &&
        user.isNotBlank() &&
        command.isNotBlank() &&
        portNumber in 1..65535 &&
        hasAuthentication
    val submit: () -> Unit = {
        localError = null
        starting = true
        model.connect(
            ConnectionProfile(
                host = host,
                port = portNumber ?: 22,
                user = user,
                identity = if (authentication == AuthenticationMode.PrivateKey) identity else "",
                passphrase = if (authentication == AuthenticationMode.PrivateKey) passphrase else "",
                password = if (authentication == AuthenticationMode.Password) password else "",
                remoteSocket = remoteSocket,
                compression = compression,
                command = command,
            ),
        )
        passphrase = ""
        password = ""
        if (model.uiState.screen == Screen.Setup) starting = false
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
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(
                selected = host == "127.0.0.1" && port == "8022",
                onClick = {
                    host = "127.0.0.1"
                    port = "8022"
                },
                label = { Text("Termux") },
            )
            FilterChip(
                selected = port == "22",
                onClick = { port = "22" },
                label = { Text("Linux SSH") },
            )
        }
        OutlinedTextField(
            host,
            { host = it },
            label = { Text("SSH host") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
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
            supportingText = { Text("For Termux, use the output of 'whoami'.") },
            modifier = Modifier.fillMaxWidth(),
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            AuthenticationMode.entries.forEach { mode ->
                FilterChip(
                    selected = authentication == mode,
                    onClick = { authentication = mode },
                    label = { Text(mode.label) },
                )
            }
        }
        when (authentication) {
            AuthenticationMode.PrivateKey -> {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(onClick = { importKey.launch(arrayOf("*/*")) }) {
                        Text(if (identity.isBlank()) "Import SSH private key" else "Replace private key")
                    }
                    if (identity.isNotBlank()) {
                        OutlinedButton(
                            onClick = {
                                if (model.deleteImportedKey()) {
                                    identity = ""
                                    passphrase = ""
                                    authentication = AuthenticationMode.Password
                                    localError = null
                                } else {
                                    localError = "Failed to delete the imported private key"
                                }
                            },
                        ) {
                            Text("Delete imported key")
                        }
                    }
                }
                Text(
                    if (identity.isBlank()) {
                        "Select a private key whose public key is in ~/.ssh/authorized_keys."
                    } else {
                        "Private key stored in owner-only app storage and excluded from backups."
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                OutlinedTextField(
                    passphrase,
                    { passphrase = it },
                    label = { Text("Private key passphrase (optional)") },
                    singleLine = true,
                    visualTransformation = PasswordVisualTransformation(),
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
                    supportingText = { Text("Used only to decrypt the private key and then cleared from memory.") },
                    modifier = Modifier.fillMaxWidth(),
                )
            }
            AuthenticationMode.Password -> {
                OutlinedTextField(
                    password,
                    { password = it },
                    label = { Text("SSH password") },
                    singleLine = true,
                    visualTransformation = PasswordVisualTransformation(),
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
                    supportingText = { Text("Kept in memory only and cleared after connecting.") },
                    modifier = Modifier.fillMaxWidth(),
                )
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
        OutlinedTextField(
            remoteSocket,
            { remoteSocket = it },
            label = { Text("Remote socket path (optional)") },
            singleLine = true,
            supportingText = {
                Text("Automatic: TMPDIR, then XDG_RUNTIME_DIR, then /tmp. Manual overrides require an absolute path of at most 100 bytes.")
            },
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            command,
            { command = it },
            label = { Text("Remote application command") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
            keyboardActions = KeyboardActions(onDone = { if (canConnect) submit() }),
            supportingText = { Text("Application and arguments only, for example: env LANG=C foot. Waypipe options are managed automatically.") },
            modifier = Modifier.fillMaxWidth(),
        )
        Text(
            "Waypane runs Waypipe with --no-gpu automatically. SSH host keys are verified by SHA-256 fingerprint; new keys are saved only after your approval.",
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
            enabled = canConnect && !starting,
            onClick = submit,
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text(if (starting) "Starting…" else "Connect")
        }
    }
}

@Composable
private fun HostKeyDialog(
    prompt: EngineEvent.HostKeyPrompt,
    onTrust: () -> Unit,
    onReject: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onReject,
        title = { Text(if (prompt.changed) "SSH host key changed" else "Verify new SSH host key") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                if (prompt.changed) {
                    Text(
                        "The saved key does not match. This can indicate a man-in-the-middle attack or a server reinstall. Verify the new fingerprint with the server administrator before replacing the saved key.",
                        color = MaterialTheme.colorScheme.error,
                    )
                } else {
                    Text("This host is not in known_hosts. Verify the fingerprint before saving it.")
                }
                Text("Host: ${prompt.host}:${prompt.port}")
                Text("Key type: ${prompt.algorithm}")
                Text("SHA-256 fingerprint:")
                SelectionContainer {
                    Text(prompt.fingerprint, fontFamily = FontFamily.Monospace)
                }
            }
        },
        confirmButton = {
            TextButton(onClick = onTrust) {
                Text(if (prompt.changed) "Replace saved key" else "Trust and save")
            }
        },
        dismissButton = {
            TextButton(onClick = onReject) {
                Text("Cancel")
            }
        },
    )
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
                                RemoteSurfaceView(
                                    context = context,
                                    key = selected,
                                    nativeHandle = model::nativeHandle,
                                    onUnsupportedText = { text -> model.reportUnsupportedText(text) },
                                    onError = model::reportError,
                                )
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
                        if (state.connecting || state.activeSessions.isNotEmpty()) {
                            CircularProgressIndicator()
                            Spacer(Modifier.height(16.dp))
                        }
                        Text(
                            when {
                                state.hostKeyPrompt != null -> "Waiting for host key confirmation"
                                state.connecting -> "Connecting over SSH…"
                                state.activeSessions.isEmpty() -> "No remote windows"
                                else -> "Waiting for Wayland surfaces…"
                            },
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
            SelectionContainer {
                Text(message, color = MaterialTheme.colorScheme.onErrorContainer, modifier = Modifier.weight(1f))
            }
            IconButton(onClick = onDismiss) { Text("Dismiss", color = MaterialTheme.colorScheme.onErrorContainer) }
        }
    }
}
