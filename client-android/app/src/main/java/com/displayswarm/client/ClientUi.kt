package com.displayswarm.client

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.drag
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.input.pointer.positionChange
import kotlin.math.roundToInt
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.foundation.layout.offset
import androidx.compose.material.icons.filled.DragHandle
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.horizontalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Dashboard
import androidx.compose.material.icons.filled.Keyboard
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.MicOff
import androidx.compose.material.icons.filled.Mouse
import androidx.compose.material.icons.filled.PowerSettingsNew
import androidx.compose.material.icons.filled.ScreenLockRotation
import androidx.compose.material.icons.filled.ScreenRotation
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.SwapHoriz
import androidx.compose.material.icons.filled.TouchApp
import androidx.compose.material.icons.filled.VolumeOff
import androidx.compose.material.icons.filled.VolumeUp
import androidx.compose.material.icons.filled.FitScreen
import androidx.compose.material.icons.filled.OpenInFull
import androidx.compose.material.icons.filled.Apps
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconToggleButton
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/** A host the phone can connect to over the network, for the connect screen. */
/** [idPrefix]: first hex digits of the host certificate fingerprint from discovery, to find an existing pairing when the address changed. */
data class HostEntry(val name: String, val address: String, val port: Int = 9999, val source: String = "Wi-Fi", val idPrefix: String = "")

/**
 * Everything the Compose overlays show. Mutated on the UI thread by
 * [MainActivity]. The wireless agent fills [hosts] (via [updateHosts]) and
 * receives the tap through [ClientActions.connectHost].
 */
class ClientUiState(val settings: AppSettings) {
    var connected by mutableStateOf(false)
    var status by mutableStateOf("Disconnected")
    var metrics by mutableStateOf("")
    var role by mutableStateOf(RoleState())
    var usbStatus by mutableStateOf("No USB cable detected")
    var hosts by mutableStateOf<List<HostEntry>>(emptyList())
    var manualHost by mutableStateOf(settings.lastHost.takeIf { it.isNotEmpty() && !it.contains(':') } ?: "127.0.0.1")

    var toolbarOpen by mutableStateOf(false)
    var toolbarX by mutableStateOf(settings.toolbarX)
    var toolbarY by mutableStateOf(settings.toolbarY)
    var settingsOpen by mutableStateOf(false)
    var shortcutsOpen by mutableStateOf(false)
    var macrosOpen by mutableStateOf(false)
    var chooserOpen by mutableStateOf(false)

    /** True when the chooser is the first-connect prompt, false for the mid-session menu. */
    var chooserFirst by mutableStateOf(false)

    var touchMode by mutableStateOf(settings.touchMode)
    var zoomEnabled by mutableStateOf(false)
    var zoomed by mutableStateOf(false)
    var rotationLock by mutableStateOf(settings.rotationLock)
    var audioOut by mutableStateOf(settings.audioOut)
    var videoQuality by mutableStateOf(settings.videoQuality.coerceAtLeast(0))
    var mic by mutableStateOf(settings.mic)
    var showStats by mutableStateOf(settings.showStats)
    var keepScreenOn by mutableStateOf(settings.keepScreenOn)
    var autoReconnect by mutableStateOf(settings.autoReconnect)
    var palmRejection by mutableStateOf(settings.palmRejection)
    var strokePrediction by mutableStateOf(settings.strokePrediction)
    var autoScale by mutableStateOf(settings.autoScale)
    var naturalScroll by mutableStateOf(settings.naturalScroll)
    var pointerSpeed by mutableStateOf(settings.pointerSpeed)
    var penButton1 by mutableStateOf(settings.penButton1)
    var penButton2 by mutableStateOf(settings.penButton2)
    var macros by mutableStateOf(settings.macros)

    /** Modifiers latched on the shortcut bar ([Wire.KEY_FLAG_CTRL] etc.). */
    var latchedMeta by mutableStateOf(0)

    /** Replace the list of discovered hosts (call on the UI thread; [MainActivity.setWifiHosts] posts for you). */
    fun updateHosts(list: List<HostEntry>) {
        hosts = list
    }
}

/** What the UI can ask of the app. */
interface ClientActions {
    fun connectHost(host: HostEntry)
    fun connectUsb()
    fun disconnect()
    fun requestRole(role: Int)
    fun dismissChooser()
    fun toggleKeyboard()
    fun setTouchMode(mode: TouchMode)
    fun setZoomEnabled(on: Boolean)
    fun resetZoom()
    fun setRotationLock(on: Boolean)
    fun setAudio(on: Boolean)
    fun setVideoQuality(quality: Int)
    fun setMic(on: Boolean)
    fun setLatchedMeta(meta: Int)
    fun sendMessages(messages: List<Wire.Message>)
    fun settingsChanged()
}

/** The app's palette as the Material scheme, so dialogs, fields and switches match. */
private val Scheme = darkColorScheme(
    background = Color(0xFF11111B),
    onBackground = Color(0xFFCDD6F4),
    surface = Color(0xFF1E1E2E),
    onSurface = Color(0xFFCDD6F4),
    surfaceVariant = Color(0xFF313244),
    onSurfaceVariant = Color(0xFFA6ADC8)
)

@Composable
fun ClientUi(state: ClientUiState, actions: ClientActions) {
    // Surfaces here use colours outside the scheme, and contentColorFor() falls
    // back to LocalContentColor for those: its default is black, which made
    // every plain Text black on the dark screens. Provide the light one.
    MaterialTheme(colorScheme = Scheme) {
      CompositionLocalProvider(LocalContentColor provides Scheme.onSurface) {
        Box(Modifier.fillMaxSize()) {
            if (!state.connected) {
                ConnectScreen(
                    usbStatus = state.usbStatus,
                    status = state.status,
                    hosts = state.hosts,
                    manualHost = state.manualHost,
                    onManualHostChange = { state.manualHost = it },
                    onConnect = actions::connectHost,
                    onConnectUsb = actions::connectUsb,
                    onSettings = { state.settingsOpen = true }
                )
            } else {
                if (state.showStats) StatsHud(state.status, state.metrics, Modifier.align(Alignment.TopStart))
                if (state.role.role == Wire.ROLE_INPUT_PAD || state.macrosOpen) {
                    MacroGrid(
                        state,
                        actions,
                        Modifier.align(Alignment.BottomCenter).fillMaxWidth().fillMaxHeight(if (state.role.hasVideo) 0.4f else 0.45f)
                    )
                }
                // One opener strip, on the side the toolbar was last left on.
                val onRight = state.toolbarX > LocalConfiguration.current.screenWidthDp.dp.value * LocalDensity.current.density / 2f
                EdgeHandle(state, Modifier.align(if (onRight) Alignment.CenterEnd else Alignment.CenterStart), rightEdge = onRight)
                if (state.toolbarOpen) Toolbar(state, actions, Modifier.align(Alignment.TopStart))
            }
            if (state.chooserOpen) RoleChooser(state, actions)
        }
      }
    }
}

// ---- Connect ----------------------------------------------------------------

@Composable
fun ConnectScreen(
    usbStatus: String,
    status: String,
    hosts: List<HostEntry>,
    manualHost: String,
    onManualHostChange: (String) -> Unit,
    onConnect: (HostEntry) -> Unit,
    onConnectUsb: () -> Unit,
    onSettings: () -> Unit
) {
    Surface(Modifier.fillMaxSize(), color = Color(0xFF11111B)) {
        // The padding sits inside each scrolling column: padding outside a scroll container
        // clips the content 24dp short of the screen edge, unlike other apps.
        Row(Modifier.fillMaxSize()) {
            Column(
                Modifier.weight(1f).fillMaxHeight().verticalScroll(rememberScrollState()).padding(start = 24.dp, top = 24.dp, end = 12.dp, bottom = 24.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp)
            ) {
                Text("DisplaySwarm", style = MaterialTheme.typography.headlineMedium)
                Text(status, style = MaterialTheme.typography.bodyMedium, color = Color(0xFFA6ADC8))
                Card(Modifier.fillMaxWidth()) {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text("USB", style = MaterialTheme.typography.titleMedium)
                        Text(usbStatus, style = MaterialTheme.typography.bodyMedium)
                        Button(onClick = onConnectUsb) { Text("Connect over USB") }
                    }
                }
                Card(Modifier.fillMaxWidth()) {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text("Manual address", style = MaterialTheme.typography.titleMedium)
                        OutlinedTextField(
                            value = manualHost,
                            onValueChange = onManualHostChange,
                            label = { Text("Host IP") },
                            singleLine = true
                        )
                        Button(onClick = { onConnect(HostEntry(manualHost, manualHost.trim(), 9999, "manual")) }) { Text("Connect") }
                    }
                }
                TextButton(onClick = onSettings) { Text("Settings") }
            }
            Column(Modifier.weight(1f).fillMaxHeight().padding(start = 12.dp, top = 24.dp, end = 24.dp, bottom = 24.dp)) {
                Text("Wi-Fi hosts", style = MaterialTheme.typography.titleMedium)
                if (hosts.isEmpty()) {
                    Text("No hosts found yet.", color = Color(0xFF6C7086), modifier = Modifier.padding(top = 8.dp))
                }
                LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.padding(top = 8.dp)) {
                    items(hosts) { h ->
                        Card(Modifier.fillMaxWidth().clickable { onConnect(h) }) {
                            Column(Modifier.padding(12.dp)) {
                                Text(h.name, style = MaterialTheme.typography.titleSmall)
                                Text("${h.address}:${h.port} · ${h.source}", fontSize = 12.sp, color = Color(0xFFA6ADC8))
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---- Role chooser -----------------------------------------------------------

@Composable
fun RoleChooser(state: ClientUiState, actions: ClientActions) {
    val current = state.role.role
    AlertDialog(
        onDismissRequest = { state.chooserOpen = false; actions.dismissChooser() },
        title = { Text(if (state.chooserFirst) "How should this device be used?" else "Change role") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                RoleState.ALL_ROLES.forEach { r ->
                    val selected = !state.chooserFirst && r == current
                    FilledTonalButton(
                        onClick = { state.chooserOpen = false; actions.requestRole(r) },
                        modifier = Modifier.fillMaxWidth(),
                        shape = RoundedCornerShape(12.dp)
                    ) {
                        Column(Modifier.fillMaxWidth()) {
                            Text(RoleState.label(r) + if (selected) "  (current)" else "")
                            Text(RoleState.description(r), fontSize = 12.sp, color = Color(0xFFA6ADC8))
                        }
                    }
                }
            }
        },
        confirmButton = {},
        dismissButton = {
            TextButton(onClick = { state.chooserOpen = false; actions.dismissChooser() }) {
                Text(if (state.chooserFirst) "Keep Mirror" else "Cancel")
            }
        }
    )
}

// ---- HUD --------------------------------------------------------------------

@Composable
fun StatsHud(status: String, metrics: String, modifier: Modifier = Modifier) {
    Column(modifier.padding(12.dp).background(Color(0xAA1E1E2E), RoundedCornerShape(8.dp)).padding(8.dp)) {
        Text(status, fontSize = 12.sp, color = Color.White)
        if (metrics.isNotEmpty()) Text(metrics, fontSize = 11.sp, color = Color(0xFFA6ADC8))
    }
}

// ---- Toolbar ----------------------------------------------------------------

@Composable
fun EdgeHandle(state: ClientUiState, modifier: Modifier = Modifier, rightEdge: Boolean = false) {
    // A thin strip on the toolbar's side of the screen: swipe it inwards to open the toolbar where it was left.
    Box(
        modifier
            .fillMaxHeight(0.5f)
            .width(22.dp)
            .pointerInput(rightEdge) {
                detectHorizontalDragGestures { _, dx ->
                    if (if (rightEdge) dx < -6f else dx > 6f) state.toolbarOpen = true
                }
            },
        contentAlignment = if (rightEdge) Alignment.CenterEnd else Alignment.CenterStart
    ) {
        Box(Modifier.width(4.dp).height(56.dp).background(Color(0x55FFFFFF), RoundedCornerShape(2.dp)))
    }
}

@Composable
private fun ToolButton(icon: ImageVector, label: String, checked: Boolean = false, onClick: () -> Unit) {
    IconToggleButton(checked = checked, onCheckedChange = { onClick() }) {
        Icon(icon, contentDescription = label, tint = if (checked) MaterialTheme.colorScheme.primary else Color.White)
    }
}

@Composable
fun Toolbar(state: ClientUiState, actions: ClientActions, modifier: Modifier = Modifier) {
    // Bounds come from the space ClientUi gives us (the whole immersive window), not the
    // Configuration's screen size, which leaves out the system bars and cutout.
    // The box has no pointer input, so touches outside the toolbar still fall through.
    BoxWithConstraints(modifier.fillMaxSize()) {
        var size by remember { mutableStateOf(IntSize.Zero) }
        val maxX = (constraints.maxWidth - size.width).coerceAtLeast(0).toFloat()
        val maxY = (constraints.maxHeight - size.height).coerceAtLeast(0).toFloat()
        // Unmoved: hug the left edge, vertically centred.
        val x = (if (state.toolbarX < 0f) 0f else state.toolbarX).coerceIn(0f, maxX)
        val y = (if (state.toolbarY < 0f) maxY / 2f else state.toolbarY).coerceIn(0f, maxY)
        // The grip's gesture coroutine outlives recompositions: read the live position and
        // bounds through these, not values captured on the first frame (size was still zero).
        val current by rememberUpdatedState(Offset(x, y))
        val bounds by rememberUpdatedState(Offset(maxX, maxY))
        Surface(
            Modifier.offset { IntOffset(x.roundToInt(), y.roundToInt()) }.onSizeChanged { size = it }.padding(8.dp),
            shape = RoundedCornerShape(24.dp),
            color = Color(0xE61E1E2E)
        ) {
            Column(Modifier.padding(4.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                // Grip: drag to put the toolbar anywhere on the screen; the spot is remembered.
                // Outside the scrolling list so the two never fight over a vertical drag, and it
                // follows the finger from the first touch instead of waiting out the touch slop.
                Box(
                    Modifier.size(width = 48.dp, height = 40.dp).pointerInput(Unit) {
                        awaitEachGesture {
                            val down = awaitFirstDown()
                            down.consume()
                            var pos = current
                            drag(down.id) { change ->
                                val d = change.positionChange()
                                change.consume()
                                pos = Offset((pos.x + d.x).coerceIn(0f, bounds.x), (pos.y + d.y).coerceIn(0f, bounds.y))
                                state.toolbarX = pos.x
                                state.toolbarY = pos.y
                            }
                            state.settings.toolbarX = state.toolbarX
                            state.settings.toolbarY = state.toolbarY
                        }
                    },
                    contentAlignment = Alignment.Center
                ) {
                    Icon(Icons.Filled.DragHandle, "Move toolbar", tint = Color(0xAAFFFFFF))
                }
                Column(
                    Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()),
                    horizontalAlignment = Alignment.CenterHorizontally
                ) {
                    ToolbarButtons(state, actions)
                }
            }
        }
    }
}

@Composable
private fun ToolbarButtons(state: ClientUiState, actions: ClientActions) {
    ToolButton(Icons.Filled.Close, "Close toolbar") { state.toolbarOpen = false }
    ToolButton(Icons.Filled.Keyboard, "Keyboard") { actions.toggleKeyboard() }
    ToolButton(Icons.Filled.Apps, "Shortcuts", state.shortcutsOpen) { state.shortcutsOpen = !state.shortcutsOpen }
    ToolButton(Icons.Filled.Dashboard, "Macro pad", state.macrosOpen) { state.macrosOpen = !state.macrosOpen }
    if (state.touchMode == TouchMode.TOUCHPAD) {
        ToolButton(Icons.Filled.Mouse, "Touchpad mode (tap for direct)", true) { actions.setTouchMode(TouchMode.DIRECT) }
    } else {
        ToolButton(Icons.Filled.TouchApp, "Direct mode (tap for touchpad)") { actions.setTouchMode(TouchMode.TOUCHPAD) }
    }
    ToolButton(Icons.Filled.OpenInFull, "Two-finger zoom", state.zoomEnabled) { actions.setZoomEnabled(!state.zoomEnabled) }
    if (state.zoomed) ToolButton(Icons.Filled.FitScreen, "Reset zoom") { actions.resetZoom() }
    ToolButton(
        if (state.rotationLock) Icons.Filled.ScreenLockRotation else Icons.Filled.ScreenRotation,
        "Rotation lock", state.rotationLock
    ) { actions.setRotationLock(!state.rotationLock) }
    ToolButton(if (state.audioOut) Icons.Filled.VolumeUp else Icons.Filled.VolumeOff, "Audio", state.audioOut) {
        actions.setAudio(!state.audioOut)
    }
    ToolButton(if (state.mic) Icons.Filled.Mic else Icons.Filled.MicOff, "Microphone", state.mic) {
        actions.setMic(!state.mic)
    }
    ToolButton(Icons.Filled.SwapHoriz, "Role") { state.chooserFirst = false; state.chooserOpen = true }
    ToolButton(Icons.Filled.Settings, "Settings") { state.settingsOpen = true }
    ToolButton(Icons.Filled.PowerSettingsNew, "Disconnect") { actions.disconnect() }
}

// ---- Shortcut bar -----------------------------------------------------------

// Copy/Paste/Undo/Alt+Tab/Super live in the macro pad and the modifier chips; only keys they lack are here.
internal val SHORTCUT_KEYS = listOf(
    "Esc" to "esc", "Enter" to "enter", "Tab" to "tab", "Del" to "delete",
    "←" to "left", "↑" to "up", "↓" to "down", "→" to "right"
)

// ---- Macro pad --------------------------------------------------------------

@OptIn(ExperimentalFoundationApi::class)
@Composable
fun MacroGrid(state: ClientUiState, actions: ClientActions, modifier: Modifier = Modifier) {
    var editing by remember { mutableStateOf<Int?>(null) }
    Surface(modifier, color = Color(0xCC181825)) {
        LazyVerticalGrid(
            columns = GridCells.Adaptive(96.dp),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(8.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp)
        ) {
            items(state.macros.size) { i ->
                val m = state.macros[i]
                Box(
                    Modifier
                        .height(64.dp)
                        .background(Color(0xFF313244), RoundedCornerShape(12.dp))
                        .combinedClickable(
                            onClick = { actions.sendMessages(m.messages()) },
                            onLongClick = { editing = i }
                        ),
                    contentAlignment = Alignment.Center
                ) { Text(m.label, textAlign = TextAlign.Center, fontSize = 14.sp) }
            }
            item {
                Box(
                    Modifier.height(64.dp).background(Color(0xFF232334), RoundedCornerShape(12.dp))
                        .clickable { editing = state.macros.size },
                    contentAlignment = Alignment.Center
                ) { Text("+ Add") }
            }
        }
    }
    editing?.let { idx ->
        val existing = state.macros.getOrNull(idx)
        MacroEditor(
            initial = existing,
            onSave = { m ->
                val list = state.macros.toMutableList()
                if (existing == null) list.add(m) else list[idx] = m
                state.macros = list
                state.settings.macros = list
                editing = null
            },
            onDelete = existing?.let {
                {
                    val list = state.macros.toMutableList().also { l -> l.removeAt(idx) }
                    state.macros = list
                    state.settings.macros = list
                    editing = null
                }
            },
            onCancel = { editing = null }
        )
    }
}

@Composable
private fun MacroEditor(initial: Macro?, onSave: (Macro) -> Unit, onDelete: (() -> Unit)?, onCancel: () -> Unit) {
    var label by remember { mutableStateOf(initial?.label ?: "") }
    var spec by remember { mutableStateOf(initial?.spec ?: "") }
    val macro = Macro(label.trim(), spec.trim())
    AlertDialog(
        onDismissRequest = onCancel,
        title = { Text(if (initial == null) "New button" else "Edit button") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(label, { label = it }, label = { Text("Label") }, singleLine = true)
                OutlinedTextField(
                    spec, { spec = it }, label = { Text("Keys, e.g. ctrl+shift+t, or text:hello") },
                    singleLine = true, isError = spec.isNotBlank() && macro.messages().isEmpty()
                )
            }
        },
        confirmButton = { TextButton(onClick = { onSave(macro) }, enabled = macro.isValid) { Text("Save") } },
        dismissButton = {
            Row {
                if (onDelete != null) TextButton(onClick = onDelete) { Text("Delete") }
                TextButton(onClick = onCancel) { Text("Cancel") }
            }
        }
    )
}

// ---- Settings ---------------------------------------------------------------

@Composable
private fun SwitchRow(label: String, checked: Boolean, hint: String? = null, onChange: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth().padding(vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(label)
            if (hint != null) Text(hint, fontSize = 12.sp, color = Color(0xFFA6ADC8))
        }
        Switch(checked = checked, onCheckedChange = onChange)
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ChoiceRow(label: String, value: PenButtonAction, onChange: (PenButtonAction) -> Unit) {
    Column(Modifier.padding(vertical = 4.dp)) {
        Text(label)
        // Wraps: four chips did not fit on a phone in portrait and "Eraser" ran off the screen.
        FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            PenButtonAction.entries.forEach {
                FilterChip(selected = it == value, onClick = { onChange(it) }, label = { Text(it.label) })
            }
        }
    }
}

