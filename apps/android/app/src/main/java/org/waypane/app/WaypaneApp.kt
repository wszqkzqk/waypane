package org.waypane.app

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.os.Build
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.displayCutout
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
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
import androidx.compose.material3.ScaffoldDefaults
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
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
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import kotlinx.coroutines.delay

private enum class AuthenticationMode(val label: String) {
    PrivateKey("Private key"),
    Password("Password"),
}

private tailrec fun Context.findActivity(): Activity = when (this) {
    is Activity -> this
    is ContextWrapper -> baseContext.findActivity()
    else -> throw IllegalStateException("Waypane must be hosted in an Activity")
}

@Composable
fun WaypaneApp(model: WaypaneViewModel) {
    val state = model.uiState
    val context = LocalContext.current
    val activity = context.findActivity()
    val hostView = LocalView.current
    var confirmDisconnect by remember { mutableStateOf(false) }
    val notificationPermission = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) {}

    LaunchedEffect(state.immersive) {
        val controller = WindowCompat.getInsetsController(activity.window, hostView)
        controller.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        WindowCompat.setDecorFitsSystemWindows(activity.window, !state.immersive)
        if (state.immersive) {
            controller.hide(WindowInsetsCompat.Type.systemBars())
        } else {
            controller.show(WindowInsetsCompat.Type.systemBars())
        }
    }
    LaunchedEffect(state.screen) {
        hostView.keepScreenOn = state.screen == Screen.Session
        val needsNotificationPermission = state.screen == Screen.Session &&
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        if (needsNotificationPermission) {
            notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }
    DisposableEffect(activity, hostView) {
        onDispose {
            hostView.keepScreenOn = false
            WindowCompat.setDecorFitsSystemWindows(activity.window, true)
            WindowCompat.getInsetsController(activity.window, hostView)
                .show(WindowInsetsCompat.Type.systemBars())
        }
    }

    BackHandler(enabled = state.screen == Screen.Session) {
        if (state.immersive) model.exitImmersive() else confirmDisconnect = true
    }
    when (state.screen) {
        Screen.Setup -> SetupScreen(model)
        Screen.Session -> SessionScreen(
            model = model,
            state = state,
            onRequestDisconnect = { confirmDisconnect = true },
        )
    }
    state.hostKeyPrompt?.let { prompt ->
        HostKeyDialog(prompt, onTrust = model::trustHostKey, onReject = model::rejectHostKey)
    }
    if (confirmDisconnect && state.screen == Screen.Session) {
        val connecting = state.connecting && state.activeSessions.isEmpty()
        AlertDialog(
            onDismissRequest = { confirmDisconnect = false },
            title = { Text(if (connecting) "Cancel connection attempt?" else "Disconnect from remote session?") },
            text = {
                Text(
                    if (connecting) {
                        "This stops the SSH connection attempt."
                    } else {
                        "This closes the SSH connection and all remote windows. To keep the session running while using other apps, leave Waypane with the Home or Recents button instead."
                    },
                )
            },
            confirmButton = {
                TextButton(
                    onClick = {
                        confirmDisconnect = false
                        model.returnToSetup()
                    },
                ) {
                    Text(
                        if (connecting) "Stop connecting" else "Disconnect",
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            },
            dismissButton = {
                TextButton(onClick = { confirmDisconnect = false }) {
                    Text(if (connecting) "Keep waiting" else "Keep session")
                }
            },
        )
    }
}

@Composable
private fun SettingSwitch(
    title: String,
    description: String,
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.spacedBy(16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(title)
            Text(
                description,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Switch(checked = checked, onCheckedChange = onCheckedChange)
    }
}

@Composable
private fun WaylandScaleSelector(
    selected: Int,
    onSelected: (Int) -> Unit,
    showDescription: Boolean = false,
) {
    val context = LocalContext.current
    val configuration = LocalConfiguration.current
    val automatic = remember(context, configuration) { context.defaultWaylandScale() }
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Text("Wayland display scale")
        LazyRow(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            items(WAYLAND_SCALE_CHOICES, key = { it }) { option ->
                FilterChip(
                    selected = selected == option,
                    onClick = { onSelected(option) },
                    label = {
                        Text(
                            if (option == WAYLAND_SCALE_AUTO) {
                                "Auto (${automatic}x)"
                            } else {
                                "${option}x"
                            },
                        )
                    },
                )
            }
        }
        if (showDescription) {
            Text(
                "Auto uses ceil(display density / 2): ${automatic}x on this screen. Native scale enlarges remote text and controls; pinch zoom and Fit only change presentation.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
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
    var autoResize by rememberSaveable { mutableStateOf(true) }
    var waylandScale by rememberSaveable { mutableStateOf(WAYLAND_SCALE_AUTO) }
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
                autoResize = autoResize,
                waylandScale = waylandScale,
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
        SettingSwitch(
            title = "Auto-resize remote windows",
            description = "Off preserves the remote desktop geometry. On asks applications to match the display area; their actual buffer size is still respected.",
            checked = autoResize,
            onCheckedChange = { autoResize = it },
        )
        WaylandScaleSelector(
            selected = waylandScale,
            onSelected = { waylandScale = it },
            showDescription = true,
        )
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
private fun SessionTopBar(
    title: String,
    hasWindow: Boolean,
    onBack: () -> Unit,
    onShowKeyboard: () -> Unit,
    onFit: () -> Unit,
    onOpenMenu: () -> Unit,
) {
    TopAppBar(
        navigationIcon = {
            IconButton(onClick = onBack) { Text("Back") }
        },
        title = {
            Text(
                title,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        },
        actions = {
            if (hasWindow) {
                TextButton(onClick = onShowKeyboard) { Text("Keyboard") }
                TextButton(onClick = onFit) { Text("Fit") }
                TextButton(onClick = onOpenMenu) { Text("Menu") }
            }
        },
    )
}

@Composable
private fun RailAction(label: String, onClick: () -> Unit) {
    TextButton(
        onClick = onClick,
        modifier = Modifier.fillMaxWidth().padding(horizontal = 4.dp),
        contentPadding = PaddingValues(horizontal = 4.dp, vertical = 2.dp),
    ) {
        Text(label, maxLines = 1, style = MaterialTheme.typography.labelMedium)
    }
}

@Composable
private fun SessionSideRail(
    hardwareInsets: WindowInsets,
    windows: List<AndroidWindow>,
    selected: SurfaceKey?,
    hasWindow: Boolean,
    onBack: () -> Unit,
    onSelectWindow: (SurfaceKey) -> Unit,
    onShowKeyboard: () -> Unit,
    onFit: () -> Unit,
    onOpenMenu: () -> Unit,
) {
    Column(
        modifier = Modifier
            .fillMaxHeight()
            .width(72.dp)
            .background(MaterialTheme.colorScheme.surface)
            .windowInsetsPadding(hardwareInsets.only(WindowInsetsSides.End + WindowInsetsSides.Vertical)),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        RailAction("Back", onBack)
        if (hasWindow) {
            RailAction("Keys", onShowKeyboard)
            RailAction("Fit", onFit)
            RailAction("Menu", onOpenMenu)
        }
        if (windows.size > 1) {
            LazyColumn(
                modifier = Modifier.fillMaxWidth().weight(1f).padding(top = 8.dp),
                verticalArrangement = Arrangement.spacedBy(2.dp),
            ) {
                items(windows, key = { it.key }) { window ->
                    TextButton(
                        onClick = { onSelectWindow(window.key) },
                        modifier = Modifier.fillMaxWidth().padding(horizontal = 4.dp),
                        contentPadding = PaddingValues(4.dp),
                        colors = ButtonDefaults.textButtonColors(
                            containerColor = if (window.key == selected) {
                                MaterialTheme.colorScheme.surfaceVariant
                            } else {
                                Color.Transparent
                            },
                        ),
                    ) {
                        Text(
                            "${window.key.window} ${window.displayTitle}",
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                            style = MaterialTheme.typography.labelSmall,
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun SessionScreen(
    model: WaypaneViewModel,
    state: UiState,
    onRequestDisconnect: () -> Unit,
) {
    val context = LocalContext.current
    val configuration = LocalConfiguration.current
    val landscape = configuration.orientation == Configuration.ORIENTATION_LANDSCAPE
    val selected = state.selected
    val selectedWindow = selected?.let(model.windows::get)
    val orderedWindows = model.windows.values.sortedWith(compareBy({ it.key.session }, { it.key.window }))
    val hardwareInsets = WindowInsets.systemBars.union(WindowInsets.displayCutout)
    val nativeScale = remember(state.waylandScale, configuration, context) {
        context.resolveWaylandScale(state.waylandScale)
    }
    var showWindowMenu by remember(selected) { mutableStateOf(false) }
    var surfaceView by remember(selected) { mutableStateOf<RemoteSurfaceView?>(null) }
    var viewSize by remember(selected) { mutableStateOf(IntSize.Zero) }

    LaunchedEffect(nativeScale) {
        model.applyWaylandScale(nativeScale)
    }
    if (state.autoResize && selected != null && viewSize.width > 0 && viewSize.height > 0) {
        LaunchedEffect(selected, viewSize, nativeScale) {
            delay(250)
            model.resizeWindow(selected, viewSize.width, viewSize.height)
        }
    }
    val showKeyboard: () -> Unit = { surfaceView?.showKeyboard() }
    val fit: () -> Unit = { surfaceView?.resetViewport() }
    val openMenu: () -> Unit = { showWindowMenu = true }
    Scaffold(
        topBar = {
            if (!state.immersive && !landscape) {
                SessionTopBar(
                    title = selectedWindow?.displayTitle ?: "Waypane",
                    hasWindow = selectedWindow != null,
                    onBack = onRequestDisconnect,
                    onShowKeyboard = showKeyboard,
                    onFit = fit,
                    onOpenMenu = openMenu,
                )
            }
        },
        contentWindowInsets = if (landscape) {
            WindowInsets(0, 0, 0, 0)
        } else {
            ScaffoldDefaults.contentWindowInsets
        },
        containerColor = Color.Black,
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            if (!state.immersive && !landscape && orderedWindows.size > 1) {
                LazyRow(
                    modifier = Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.surface).padding(4.dp),
                    horizontalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    items(orderedWindows, key = { it.key }) { window ->
                        FilterChip(
                            selected = window.key == selected,
                            onClick = { model.selectWindow(window.key) },
                            label = { Text(window.displayTitle, maxLines = 1, modifier = Modifier.widthIn(max = 220.dp)) },
                        )
                    }
                }
            }
            Row(Modifier.weight(1f).fillMaxWidth()) {
                Box(
                    Modifier
                        .weight(1f)
                        .fillMaxHeight()
                        .then(
                            if (landscape) {
                                Modifier.windowInsetsPadding(
                                    hardwareInsets.only(WindowInsetsSides.Start + WindowInsetsSides.Vertical),
                                )
                            } else {
                                Modifier
                            },
                        )
                        .background(Color.Black),
                ) {
                    val ratio = selectedWindow?.aspectRatio
                    if (selected != null && selectedWindow != null) {
                        key(selected) {
                            AndroidView(
                                factory = { context ->
                                    RemoteSurfaceView(
                                        context = context,
                                        key = selected,
                                        contentAspectRatio = ratio ?: 1f,
                                        nativeHandle = model::nativeHandle,
                                        onUnsupportedText = { text -> model.reportUnsupportedText(text) },
                                        onError = model::reportError,
                                        onViewSizeChanged = { width, height ->
                                            viewSize = IntSize(width, height)
                                        },
                                    ).also { surfaceView = it }
                                },
                                update = { view ->
                                    view.setDirectTouch(state.directTouch)
                                    view.updateContentAspectRatio(ratio ?: 1f)
                                    surfaceView = view
                                },
                                onRelease = { view ->
                                    if (surfaceView === view) surfaceView = null
                                },
                                modifier = Modifier.fillMaxSize(),
                            )
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
                    if (state.immersive && selectedWindow != null) {
                        Row(
                            modifier = Modifier
                                .align(Alignment.TopEnd)
                                .padding(8.dp)
                                .background(Color.Black.copy(alpha = 0.6f), MaterialTheme.shapes.small),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            TextButton(onClick = showKeyboard) {
                                Text("Keyboard", color = Color.White)
                            }
                            TextButton(onClick = openMenu) {
                                Text("Menu", color = Color.White)
                            }
                        }
                    }
                }
                if (!state.immersive && landscape) {
                    SessionSideRail(
                        hardwareInsets = hardwareInsets,
                        windows = orderedWindows,
                        selected = selected,
                        hasWindow = selectedWindow != null,
                        onBack = onRequestDisconnect,
                        onSelectWindow = model::selectWindow,
                        onShowKeyboard = showKeyboard,
                        onFit = fit,
                        onOpenMenu = openMenu,
                    )
                }
            }
            state.error?.let { message ->
                ErrorCard(
                    message = message,
                    onDismiss = model::clearError,
                    modifier = if (landscape) Modifier.navigationBarsPadding() else Modifier,
                )
            }
            if (!state.immersive && !landscape) {
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(40.dp)
                        .background(MaterialTheme.colorScheme.surface)
                        .padding(horizontal = 12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        state.status,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
        }
    }
    if (selected != null && showWindowMenu) {
        WindowOptionsDialog(
            status = state.status,
            autoResize = state.autoResize,
            directTouch = state.directTouch,
            waylandScale = state.waylandScale,
            immersive = state.immersive,
            onAutoResizeChange = model::setAutoResize,
            onDirectTouchChange = model::setDirectTouch,
            onWaylandScaleChange = model::selectWaylandScale,
            onFit = { surfaceView?.resetViewport() },
            onShowKeyboard = { surfaceView?.showKeyboard() },
            onToggleImmersive = model::toggleImmersive,
            onCloseWindow = { model.closeWindow(selected) },
            onDismiss = { showWindowMenu = false },
        )
    }
}

@Composable
private fun WindowOptionsDialog(
    status: String,
    autoResize: Boolean,
    directTouch: Boolean,
    waylandScale: Int,
    immersive: Boolean,
    onAutoResizeChange: (Boolean) -> Unit,
    onDirectTouchChange: (Boolean) -> Unit,
    onWaylandScaleChange: (Int) -> Unit,
    onFit: () -> Unit,
    onShowKeyboard: () -> Unit,
    onToggleImmersive: () -> Unit,
    onCloseWindow: () -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Window options") },
        text = {
            Column(
                modifier = Modifier.verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                if (status.isNotBlank()) {
                    Text(
                        status,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                SettingSwitch(
                    title = "Auto-resize remote windows",
                    description = "Ask the application to match the display area instead of preserving its own geometry.",
                    checked = autoResize,
                    onCheckedChange = onAutoResizeChange,
                )
                SettingSwitch(
                    title = "Direct touch input",
                    description = "Send native Wayland touch events instead of pointer clicks. Leave off for most desktop applications.",
                    checked = directTouch,
                    onCheckedChange = onDirectTouchChange,
                )
                WaylandScaleSelector(
                    selected = waylandScale,
                    onSelected = onWaylandScaleChange,
                )
                TextButton(
                    onClick = {
                        onFit()
                        onDismiss()
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text("Fit entire window")
                }
                TextButton(
                    onClick = {
                        onShowKeyboard()
                        onDismiss()
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text("Show keyboard")
                }
                TextButton(
                    onClick = {
                        onToggleImmersive()
                        onDismiss()
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text(if (immersive) "Exit fullscreen" else "Enter fullscreen")
                }
                TextButton(
                    onClick = {
                        onCloseWindow()
                        onDismiss()
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text("Close remote window", color = MaterialTheme.colorScheme.error)
                }
            }
        },
        confirmButton = {
            TextButton(onClick = onDismiss) { Text("Done") }
        },
    )
}

@Composable
private fun ErrorCard(
    message: String,
    modifier: Modifier = Modifier,
    onDismiss: () -> Unit,
) {
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.errorContainer),
        modifier = modifier.fillMaxWidth().padding(8.dp),
    ) {
        Row(Modifier.padding(12.dp), verticalAlignment = Alignment.CenterVertically) {
            SelectionContainer {
                Text(message, color = MaterialTheme.colorScheme.onErrorContainer, modifier = Modifier.weight(1f))
            }
            IconButton(onClick = onDismiss) { Text("Dismiss", color = MaterialTheme.colorScheme.onErrorContainer) }
        }
    }
}
