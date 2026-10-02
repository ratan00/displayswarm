package com.displayswarm.client

import android.content.Intent
import android.content.pm.ActivityInfo
import android.content.res.Configuration
import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.media.MediaCodecList
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.KeyEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch
import androidx.compose.ui.platform.ComposeView
import androidx.compose.runtime.LaunchedEffect

/**
 * The single screen: the video SurfaceView, the input overlay above it, and a
 * ComposeView on top that holds every piece of UI (see [ClientUi]). Touches the
 * Compose layer does not use fall through to the overlay.
 */
class MainActivity : AppCompatActivity(), SurfaceHolder.Callback, ClientActions {

    private companion object {
        const val TAG = "DisplaySwarmActivity"
        const val RECONNECT_DELAY_MS = 3_000L
    }

    private lateinit var videoSurface: SurfaceView
    private lateinit var touchOverlay: InputOverlayView
    private lateinit var txtNoVideoHint: TextView

    private lateinit var settings: AppSettings
    private lateinit var ui: ClientUiState

    private var videoDecoder: VideoDecoder? = null
    private var networkClient: NetworkClient? = null
    private var aoaManager: AoaManager? = null
    private var inputManager: InputManager? = null
    private var isSurfaceReady = false

    /** Live TCP session, so a recreated decoder can be re-attached to it. */
    private var tcpSession: ControlSession? = null

    /** Live USB session, for role requests. */
    private var aoaSession: ControlSession? = null

    private fun activeSession(): ControlSession? = aoaSession ?: tcpSession

    // Cache display metrics for consistent use
    private var displayWidth = 0
    private var displayHeight = 0
    private var displayRefreshMhz = 60_000L

    private val main = Handler(Looper.getMainLooper())

    /** The last network target, for auto-reconnect. Null after a USB connection or a user disconnect. */
    private var lastTarget: HostEntry? = null
    private var userDisconnected = true
    private var reconnectPending: Runnable? = null
    /** One automatic retry after an established session drops; a failed attempt is never retried. */
    private var reconnectsLeft = 0

    /**
     * The wireless feature installs this to take over connecting to a discovered
     * host (pairing, TLS, ...). Return true when it handled the request; the
     * default plain-TCP path is used otherwise.
     */
    var hostConnector: ((HostEntry) -> Boolean)? = null

    /** Replaces the "Wi-Fi hosts" list on the connect screen. Safe from any thread. */
    fun setWifiHosts(hosts: List<HostEntry>) {
        runOnUiThread { ui.updateHosts(hosts) }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Session services (audio, clipboard, files, ...): see PhoneServices.
        PhoneServices.install(this, listOf(AudioPlayer(this), MicCapture(this), ClipboardSync(), BatteryReporter()))
        settings = AppSettings.from(this)
        ui = ClientUiState(settings)
        ControlSession.preferredQuality = settings.videoQuality
        // The foreground-service notification is what keeps audio alive in the background; ask once.
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) != android.content.pm.PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 6)
        }
        applyKeepScreenOn()
        lifecycleScope.launch {
            wifiHosts.hosts.collect { found ->
                setWifiHosts(found.map { HostEntry(it.name, it.host, it.port, idPrefix = it.idPrefix) })
            }
        }
        // A display should light up when it is plugged in, like a real monitor,
        // even if the phone is locked. The activity is shown above the keyguard
        // WITHOUT dismissing it: the phone itself stays locked, only the host's
        // desktop is visible, and leaving the app returns to the lock screen.
        // Without this, an accessory attach on a locked phone starts the activity
        // behind the keyguard, its surface is never created, and the OS freezes
        // the process a minute later.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O_MR1) {
            setShowWhenLocked(true)
            setTurnScreenOn(true)
        } else {
            @Suppress("DEPRECATION")
            window.addFlags(WindowManager.LayoutParams.FLAG_SHOW_WHEN_LOCKED or WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON)
        }
        hideSystemUI()
        applyOrientationPolicy()

        setContentView(R.layout.activity_main)

        videoSurface = findViewById(R.id.videoSurface)
        touchOverlay = findViewById(R.id.touchOverlay)
        txtNoVideoHint = findViewById(R.id.txtNoVideoHint)
        val shortcutBar = ShortcutBarView.attach(findViewById(R.id.rootContainer), ui, this)
        SettingsBridge.ui = ui
        SettingsBridge.actions = this
        findViewById<ComposeView>(R.id.composeView).setContent {
            LaunchedEffect(ui.settingsOpen, ui.chooserOpen) { overlayClosed() }
            LaunchedEffect(ui.shortcutsOpen, ui.connected) { shortcutBar.visibility = if (ui.shortcutsOpen && ui.connected) View.VISIBLE else View.GONE }
            LaunchedEffect(ui.latchedMeta) { shortcutBar.refresh() }
            LaunchedEffect(ui.settingsOpen) {
                if (ui.settingsOpen) {
                    ui.settingsOpen = false
                    startActivity(Intent(this@MainActivity, SettingsActivity::class.java))
                }
            }
            ClientUi(ui, this@MainActivity)
        }
        showRole(RoleState(lastRole()))

        readPanel()
        displayRefreshMhz = requestHighestRefreshRate()

        videoSurface.holder.addCallback(this)

        inputManager = InputManager(
            view = touchOverlay,
            onThreeFingerTap = { runOnUiThread { ui.toolbarOpen = !ui.toolbarOpen } },
            // ONE transport path for every input packet: pointer, stylus, mouse,
            // wheel and key all arrive here. Keyboard support deliberately does
            // not add a second callback, because two paths is how a keystroke
            // ends up being sent twice.
            onPacketReady = { packet ->
                // Send to exactly ONE transport (see the history in git: feeding
                // both doubled the pointer speed on the host).
                // Nothing goes out unless a session is really up: after a drop the
                // last frame stays on screen, and touches on it must not reach
                // a host that is gone (or come back as a burst on reconnect).
                if (activeSession() != null && ui.connected) {
                    when {
                        aoaManager?.isConnected() == true -> aoaManager?.queueInput(packet)
                        else -> networkClient?.queueInput(packet)
                    }
                }
            }
        ).also {
            it.onViewportChanged = { runOnUiThread { applyViewport() } }
            it.onStickyConsumed = { runOnUiThread { ui.latchedMeta = 0 } }
        }
        inputManager?.attach()
        applyInputSettings()

        initAoaManager()
        refreshUsbStatus()

        handleUsbIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleUsbIntent(intent)
    }

    override fun onResume() {
        super.onResume()
        refreshUsbStatus()
        wifiHosts.startDiscovery()
        // Re-asserting the overlay's focus on resume is what makes a hardware or
        // Bluetooth keyboard work straight after unlocking the phone.
        if (!touchOverlay.hasFocus()) touchOverlay.requestFocus()
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        val oldW = displayWidth
        val oldH = displayHeight
        readPanel()
        if ((oldW != displayWidth || oldH != displayHeight) && activeSession() != null) sendPanelSize()
    }

    /** The panel size the host was last told about (in the Hello or a Resize). */
    private var panelSentW = 0
    private var panelSentH = 0

    private fun sendPanelSize() {
        val rotation = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) display?.rotation ?: 0 else 0
        panelSentW = displayWidth
        panelSentH = displayHeight
        inputManager?.send(
            Wire.Message.Resize(displayWidth, displayHeight, resources.displayMetrics.densityDpi, rotation)
        )
    }

    // ---- Panel size, orientation ------------------------------------------

    /**
     * The FULL physical panel. With rotation locked it is normalised to
     * landscape (the activity starts over a portrait keyguard, where the raw
     * metrics would say portrait and the host would encode a sideways stream);
     * unlocked, it follows the current rotation and a change is sent as Resize.
     */
    private fun readPanel() {
        val (panelW, panelH) = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val b = windowManager.maximumWindowMetrics.bounds
            b.width() to b.height()
        } else {
            val real = android.util.DisplayMetrics()
            @Suppress("DEPRECATION")
            windowManager.defaultDisplay.getRealMetrics(real)
            real.widthPixels to real.heightPixels
        }
        if (settings.rotationLock) {
            displayWidth = maxOf(panelW, panelH)
            displayHeight = minOf(panelW, panelH)
        } else {
            displayWidth = panelW
            displayHeight = panelH
        }
    }

    /**
     * Asks for the panel's fastest mode at its current resolution (a 90 Hz panel often runs at
     * 60 Hz by default) and returns the refresh rate in mHz to report to the host, which can then
     * stream above 60 fps. Android may still pick a lower rate; the app keeps working either way.
     */
    private fun requestHighestRefreshRate(): Long {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return 60_000L
        val d = display ?: return 60_000L
        val current = d.mode
        val best = d.supportedModes
            .filter { it.physicalWidth == current.physicalWidth && it.physicalHeight == current.physicalHeight }
            .maxByOrNull { it.refreshRate } ?: current
        if (best.modeId != current.modeId) {
            window.attributes = window.attributes.apply { preferredDisplayModeId = best.modeId }
        }
        return (best.refreshRate * 1000f).toLong().takeIf { it > 0 } ?: 60_000L
    }

    private fun applyOrientationPolicy() {
        requestedOrientation = if (settings.rotationLock) {
            ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        } else {
            ActivityInfo.SCREEN_ORIENTATION_FULL_SENSOR
        }
    }

    private fun applyKeepScreenOn() {
        if (settings.keepScreenOn) window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        else window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    }

    private fun applyViewport() {
        val vp = inputManager?.viewport ?: return
        for (v in listOf<View>(videoSurface)) {
            v.pivotX = 0f
            v.pivotY = 0f
            v.scaleX = vp.scale
            v.scaleY = vp.scale
            v.translationX = vp.panX
            v.translationY = vp.panY
        }
        ui.zoomed = !vp.isIdentity
    }

    /** Pushes the settings that the input layer and window depend on. */
    private fun applyInputSettings() {
        val im = inputManager ?: return
        im.palmFilter.enabled = settings.palmRejection
        im.strokePrediction = settings.strokePrediction
        im.penMapping = settings.penMapping
        im.setTouchpadConfig(settings.pointerSpeed, settings.naturalScroll, resources.displayMetrics.density)
        // The input pad is a touchpad by definition; the pen role is always direct.
        val role = ui.role.role
        im.touchMode = if (role == Wire.ROLE_INPUT_PAD) TouchMode.TOUCHPAD else ui.touchMode
        im.localZoom = ui.zoomEnabled && im.touchMode == TouchMode.DIRECT
        im.gestures.viewWidth = touchOverlay.width.toFloat().coerceAtLeast(1f)
        im.gestures.viewHeight = touchOverlay.height.toFloat().coerceAtLeast(1f)
    }

    private fun overlayClosed() {
        if (!ui.settingsOpen && !ui.chooserOpen && ui.connected) touchOverlay.requestFocus()
        hideSystemUI()
    }

    // ---- ClientActions ------------------------------------------------------

    override fun connectHost(host: HostEntry) {
        if (hostConnector?.invoke(host) == true) return
        settings.lastHost = host.address
        connectTcp(host)
    }

    override fun connectUsb() {
        if (aoaManager?.findConnectedAccessory() != null) {
            userDisconnected = false
            lastTarget = null
            connectUsbAoa(null)
        } else {
            ui.status = "No USB cable with the host detected"
        }
    }

    override fun disconnect() {
        userDisconnected = true
        cancelReconnect()
        disconnect(graceful = true)
    }

    override fun requestRole(role: Int) {
        activeSession()?.requestRole(role)
    }

    override fun dismissChooser() {
        activeSession()?.dismissRoleChooser()
    }

    override fun toggleKeyboard() {
        keyboardVisible = !keyboardVisible
        inputManager?.setSoftKeyboardEnabled(keyboardVisible)
    }

    private var keyboardVisible = false
    private var sessionServiceOn = false

    override fun setTouchMode(mode: TouchMode) {
        ui.touchMode = mode
        settings.touchMode = mode
        applyInputSettings()
    }

    override fun setZoomEnabled(on: Boolean) {
        ui.zoomEnabled = on
        if (!on) resetZoom()
        applyInputSettings()
    }

    override fun resetZoom() {
        inputManager?.viewport?.reset()
        applyViewport()
    }

    override fun setRotationLock(on: Boolean) {
        ui.rotationLock = on
        settings.rotationLock = on
        applyOrientationPolicy()
    }

    override fun setVideoQuality(quality: Int) {
        ui.videoQuality = quality
        settings.videoQuality = quality
        activeSession()?.setQuality(quality) ?: run { ControlSession.preferredQuality = quality }
    }

    override fun setAudio(on: Boolean) {
        ui.audioOut = on
        settings.audioOut = on
        PhoneServices.announce()
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        PhoneServices.announce() // the mic may now be usable
    }

    override fun setMic(on: Boolean) {
        ui.mic = on
        // Also asks for RECORD_AUDIO: without it the mic is never offered.
        AudioSettings.setMicEnabled(this, on, this)
        PhoneServices.announce()
    }

    override fun setLatchedMeta(meta: Int) {
        ui.latchedMeta = meta
        inputManager?.stickyMeta = meta
    }

    override fun sendMessages(messages: List<Wire.Message>) {
        messages.forEach { inputManager?.send(it) }
    }

    override fun settingsChanged() {
        applyKeepScreenOn()
        applyInputSettings()
    }

    // ---- USB --------------------------------------------------------------

    private fun handleUsbIntent(intent: Intent?) {
        if (intent == null) return
        if (intent.action == SessionService.ACTION_DISCONNECT) {
            disconnect()
            return
        }
        if (UsbManager.ACTION_USB_ACCESSORY_ATTACHED == intent.action) {
            val accessory: UsbAccessory? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY, UsbAccessory::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY)
            }
            if (accessory != null) {
                userDisconnected = false
                lastTarget = null
                connectUsbAoa(accessory)
            }
        }
    }

    private fun refreshUsbStatus() {
        ui.usbStatus = if (aoaManager?.findConnectedAccessory() != null) {
            "Cable connected to the host"
        } else {
            "No cable detected. Plug the phone into the PC."
        }
    }

    private fun initAoaManager() {
        aoaManager = AoaManager(
            context = this,
            hello = buildHello(),
            videoDecoder = videoDecoder,
            onFrameReceived = { frameType, payload, timestampUs ->
                feedDecoder(frameType, payload, timestampUs)
            },
            onStatusChanged = { status -> runOnUiThread { showStatus(status) } },
            onMetricsUpdated = { metrics -> runOnUiThread { ui.metrics = metrics.toHudString() } },
            onRoleChanged = { state -> runOnUiThread { showRole(state) } },
            onSessionChanged = { session ->
                aoaSession = session
                if (session == null) runOnUiThread { ui.chooserOpen = false }
            }
        )
    }

    // ---- Roles ------------------------------------------------------------

    private fun lastRole(): Int {
        val saved = getSharedPreferences("displayswarm", MODE_PRIVATE).getInt("last_role", Wire.ROLE_MIRROR)
        return if (RoleState.isValid(saved)) saved else Wire.ROLE_MIRROR
    }

    /** Updates the UI, the no-video surface and the remembered role; opens the chooser if the host asks. */
    private fun showRole(state: RoleState) {
        ui.role = state
        txtNoVideoHint.text = RoleState.hint(state.role)
        txtNoVideoHint.visibility = if (state.hasVideo) View.GONE else View.VISIBLE
        // Local zoom is for looking at a mirrored screen; on the other roles the
        // fingers are real multitouch unless the user turns zoom on.
        ui.zoomEnabled = state.role == Wire.ROLE_MIRROR || state.role == Wire.ROLE_MIRROR_WINDOW
        if (!state.hasVideo) resetZoom()
        applyInputSettings()
        if (activeSession() != null) {
            // Remembered for display before the next connect only.
            getSharedPreferences("displayswarm", MODE_PRIVATE).edit().putInt("last_role", state.role).apply()
        }
        if (state.chooserPending) {
            ui.chooserFirst = true
            ui.chooserOpen = true
        } else if (ui.chooserOpen && ui.chooserFirst) {
            ui.chooserOpen = false // the host (or another path) settled the role
        }
    }

    // ---- Connecting -----------------------------------------------------------

    private fun connectUsbAoa(accessory: UsbAccessory?) {
        // NOTE: deliberately does NOT refuse to connect when the decoder is not
        // ready. A SurfaceView's surface is created asynchronously, so the AOA
        // attach broadcast can arrive before `surfaceCreated` runs. Refusing here
        // would drop the only chance to connect (the attach event does not
        // repeat). The decoder self-heals in VideoDecoder once the surface exists.
        if (!ensureDecoderReady()) {
            Log.i(TAG, "Connecting to USB before the video surface exists; decoder will attach later")
        }
        aoaManager?.setVideoDecoder(videoDecoder)
        aoaManager?.connect(accessory)
    }

    /** Wi-Fi discovery, remembered hosts and pairing (see WifiDiscovery.kt). */
    private val wifiHosts by lazy { WifiHosts(this) }

    private fun connectTcp(host: HostEntry) {
        // Ensure video decoder is ready before connecting, otherwise every frame
        // arrives with nowhere to go and the screen stays black with no error.
        if (!ensureDecoderReady()) {
            ui.status = "Video surface not ready yet, try again"
            return
        }
        cancelReconnect()
        userDisconnected = false
        reconnectsLeft = 0 // set again once this attempt is established
        lastTarget = host
        networkClient?.stop()
        networkClient = NetworkClient(
            host = host.address.trim(),
            port = host.port,
            hello = buildHello(),
            onFrameReceived = { frameType, payload, timestampUs ->
                feedDecoder(frameType, payload, timestampUs)
            },
            onStatusChanged = { status -> runOnUiThread { showStatus(status) } },
            onMetricsUpdated = { metrics -> runOnUiThread { ui.metrics = metrics.toHudString() } },
            onSessionChanged = { session ->
                tcpSession = session
                videoDecoder?.listener = session
                if (session == null) runOnUiThread { ui.chooserOpen = false }
            },
            onRoleChanged = { state -> runOnUiThread { showRole(state) } },
            // TLS + pairing, unless the dev switch asks for plain TCP.
            security = if (wifiHosts.plainTcpForDev) null
            else wifiHosts.securityFor(host.address.trim(), host.port, host.name, host.idPrefix) { hint -> PairingDialog.askPin(this, hint) }
        ).also { it.start() }
    }

    private fun cancelReconnect() {
        reconnectPending?.let { main.removeCallbacks(it) }
        reconnectPending = null
    }

    /** Retries the last network target after an unexpected drop, while auto-reconnect is on. */
    private fun scheduleReconnect() {
        val target = lastTarget ?: return
        if (userDisconnected || !settings.autoReconnect || reconnectPending != null || reconnectsLeft <= 0) return
        reconnectsLeft--
        val r = Runnable {
            reconnectPending = null
            if (!userDisconnected && networkClient == null && lastTarget != null) connectTcp(target)
        }
        reconnectPending = r
        main.postDelayed(r, RECONNECT_DELAY_MS)
    }

    /** What this phone tells the host about itself in the protocol v2 Hello. */
    private fun buildHello(): Wire.Hello {
        val version = try {
            packageManager.getPackageInfo(packageName, 0).versionName
        } catch (_: Exception) {
            null
        }
        panelSentW = displayWidth
        panelSentH = displayHeight
        return Wire.Hello(
            width = displayWidth,
            height = displayHeight,
            refreshMhz = displayRefreshMhz,
            densityDpi = resources.displayMetrics.densityDpi,
            codecs = Wire.CODEC_H264 or (if (hasHevcDecoder()) Wire.CODEC_HEVC else 0),
            features = Wire.FEATURE_TOUCH or Wire.FEATURE_STYLUS or Wire.FEATURE_KEYBOARD or PhoneServices.features(),
            maxTouchPoints = 10,
            deviceId = DeviceIdentity.id(this),
            deviceName = "${Build.MANUFACTURER} ${Build.MODEL}",
            appVersion = version ?: "dev"
        )
    }

    private fun hasHevcDecoder(): Boolean = try {
        MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.any { info ->
            !info.isEncoder && info.supportedTypes.any { it.equals("video/hevc", ignoreCase = true) }
        }
    } catch (_: Exception) {
        false
    }

    private fun feedDecoder(frameType: Int, payload: ByteArray, timestampUs: Long) {
        videoDecoder?.feedNalUnit(
            payload, timestampUs,
            isConfig = (frameType == Wire.FRAME_TYPE_CONFIG),
            isKey = (frameType == Wire.FRAME_TYPE_KEY)
        )
    }

    /** A status that means a network attempt is still under way (not finished, not failed). */
    private fun isTransportBusy(status: String) =
        status.startsWith("Connecting") || status.startsWith("Securing") || status.startsWith("Pairing")

    /** Shows a transport status; the connect screen is up until a session is really established. */
    private fun showStatus(status: String) {
        ui.status = status
        val connected = status.startsWith("Connected") || status.startsWith("Waiting") ||
            status.startsWith("Streaming") || status.startsWith("Host capture failed") ||
            status.startsWith("Requesting")
        ui.connected = connected
        if (connected) {
            cancelReconnect()
            reconnectsLeft = 1
            // The phone may have rotated between the Hello and now (USB starts while the app is launching).
            if ((panelSentW != displayWidth || panelSentH != displayHeight) && activeSession() != null) sendPanelSize()
            // Keep the process (and the host's audio) alive with the app minimised.
            if (!sessionServiceOn) {
                sessionServiceOn = true
                SessionService.start(this, "Connected to the host \u2013 audio keeps playing in the background")
            }
        } else if (!isTransportBusy(status)) {
            if (sessionServiceOn) {
                sessionServiceOn = false
                SessionService.stop(this)
            }
            ui.metrics = ""
            txtNoVideoHint.visibility = View.GONE // nothing to be an input surface for
            ui.toolbarOpen = false
            ui.chooserOpen = false
            if (networkClient != null && lastTarget != null) {
                networkClient = null // the transport already finished; allow a fresh connect
                scheduleReconnect()
            }
            refreshUsbStatus()
        }
    }

    /**
     * Ensures the VideoDecoder is created and configured with the current display dimensions.
     * Returns false when the SurfaceView has not produced a surface yet.
     */
    private fun ensureDecoderReady(): Boolean {
        val decoder = videoDecoder
        if (decoder == null) {
            Log.w(TAG, "Cannot stream: video surface is not ready yet")
            ui.status = "Waiting for video surface..."
            return false
        }
        if (!decoder.isConfigured()) {
            decoder.configure(displayWidth, displayHeight)
        }
        return true
    }

    /**
     * [graceful] tells the host (BYE) before closing; onDestroy cannot wait for
     * that, so it passes false. The final status text comes from the transport.
     */
    private fun disconnect(graceful: Boolean = true) {
        if (graceful) {
            aoaManager?.disconnectGracefully()
            networkClient?.stopGracefully()
        } else {
            aoaManager?.disconnect()
            networkClient?.stop()
        }
        networkClient = null
        tcpSession = null
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        isSurfaceReady = true
        val existing = videoDecoder
        if (existing == null) {
            videoDecoder = VideoDecoder(holder.surface).apply {
                listener = tcpSession
                configure(displayWidth, displayHeight)
            }
        } else {
            // The surface was recreated (resume, rotation, or a previous release).
            // Re-point the existing decoder; a codec bound to the dead surface
            // fails permanently with "The surface has been released".
            existing.setSurface(holder.surface)
            existing.configure(displayWidth, displayHeight)
        }
        aoaManager?.setVideoDecoder(videoDecoder)

        if (aoaManager?.isConnected() == false && aoaManager?.findConnectedAccessory() != null) {
            connectUsbAoa(null)
        }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        // Deliberately NOT reconfiguring the decoder here. The decoded picture's
        // size comes from the stream (the SPS), not from the surface, and the
        // SurfaceView scales it to fill the view.
        Log.d(TAG, "Surface changed: ${width}x${height} (panel ${displayWidth}x${displayHeight})")
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        isSurfaceReady = false
        aoaManager?.setVideoDecoder(null)
        // Release the decoder entirely rather than re-pointing it: its control
        // executor is shut down by release(), and a new decoder is built in
        // surfaceCreated().
        videoDecoder?.release()
        videoDecoder = null
    }

    private fun hideSystemUI() {
        @Suppress("DEPRECATION")
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                or View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                or View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_FULLSCREEN
        )
    }

    override fun onPause() {
        super.onPause()
        wifiHosts.stopDiscovery()
    }

    override fun onDestroy() {
        super.onDestroy()
        SettingsBridge.ui = null
        SettingsBridge.actions = null
        SessionService.stop(this)
        userDisconnected = true
        cancelReconnect()
        disconnect(graceful = false)
        aoaManager?.release()
        aoaManager = null
        videoDecoder?.release()
    }
}
