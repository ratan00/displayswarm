package com.displayswarm.client

import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothHidDevice
import android.bluetooth.BluetoothHidDeviceAppSdpSettings
import android.bluetooth.BluetoothProfile
import android.content.Context
import android.os.Build
import android.util.Log
import androidx.annotation.RequiresApi
import java.util.concurrent.Executors
import kotlin.math.roundToInt

/** Pure HID report builders and the HID report descriptor. No Android types, so they are unit-testable. */
object HidReports {
    const val ID_KEYBOARD = 1
    const val ID_MOUSE = 2
    const val ID_DIGITIZER = 3

    /** Keyboard (8-byte report), relative mouse (4 bytes), absolute single-touch digitizer (5 bytes). */
    val DESCRIPTOR: ByteArray = intArrayOf(
        // Keyboard, report ID 1: modifiers, reserved, 6 key codes
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, ID_KEYBOARD,
        0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
        0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00,
        0xC0,
        // Mouse, report ID 2: 3 buttons, dx, dy, wheel
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, ID_MOUSE, 0x09, 0x01, 0xA1, 0x00,
        0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06,
        0xC0, 0xC0,
        // Digitizer (touch screen), report ID 3: tip, in-range, 6 pad bits, x, y (0..32767)
        0x05, 0x0D, 0x09, 0x04, 0xA1, 0x01, 0x85, ID_DIGITIZER, 0x09, 0x22, 0xA1, 0x00,
        0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02,
        0x09, 0x32, 0x81, 0x02,
        0x95, 0x06, 0x81, 0x03,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02,
        0xC0, 0xC0
    ).map { it.toByte() }.toByteArray()

    const val MOD_CTRL = 0x01
    const val MOD_SHIFT = 0x02
    const val MOD_ALT = 0x04
    const val MOD_GUI = 0x08

    /** Payload (without the report ID) of a keyboard report; at most 6 keys. */
    fun keyboard(modifiers: Int, keys: List<Int>): ByteArray {
        val r = ByteArray(8)
        r[0] = modifiers.toByte()
        keys.take(6).forEachIndexed { i, k -> r[2 + i] = k.toByte() }
        return r
    }

    /** Payload of a relative mouse report; deltas are clamped to -127..127. */
    fun mouse(buttons: Int, dx: Int, dy: Int, wheel: Int = 0): ByteArray = byteArrayOf(
        (buttons and 0x07).toByte(), dx.coerceIn(-127, 127).toByte(),
        dy.coerceIn(-127, 127).toByte(), wheel.coerceIn(-127, 127).toByte()
    )

    /** Payload of a digitizer report for a position given as 0..1. */
    fun digitizer(tip: Boolean, inRange: Boolean, x: Float, y: Float): ByteArray {
        val xi = (x.coerceIn(0f, 1f) * 32767).roundToInt()
        val yi = (y.coerceIn(0f, 1f) * 32767).roundToInt()
        val flags = (if (tip) 1 else 0) or (if (inRange) 2 else 0)
        return byteArrayOf(
            flags.toByte(), (xi and 0xFF).toByte(), (xi shr 8).toByte(), (yi and 0xFF).toByte(), (yi shr 8).toByte()
        )
    }

    /** HID usage (keyboard page) for an `android.view.KeyEvent` code, or 0 if unmapped. */
    fun usageFor(androidKeyCode: Int): Int = when (androidKeyCode) {
        in 29..54 -> 0x04 + (androidKeyCode - 29)      // A..Z
        7 -> 0x27                                       // 0
        in 8..16 -> 0x1E + (androidKeyCode - 8)         // 1..9
        66 -> 0x28
        111 -> 0x29
        67 -> 0x2A
        61 -> 0x2B
        62 -> 0x2C
        112 -> 0x4C                                     // forward delete
        19 -> 0x52 // arrows: up, down, left, right
        20 -> 0x51
        21 -> 0x50
        22 -> 0x4F
        else -> 0
    }

    fun modifiersFor(meta: Int): Int =
        (if (meta and Wire.KEY_FLAG_CTRL != 0) MOD_CTRL else 0) or
            (if (meta and Wire.KEY_FLAG_SHIFT != 0) MOD_SHIFT else 0) or
            (if (meta and Wire.KEY_FLAG_ALT != 0) MOD_ALT else 0) or
            (if (meta and Wire.KEY_FLAG_META != 0) MOD_GUI else 0)
}

/** One report to send: the report ID and its payload. */
class HidReport(val id: Int, val data: ByteArray) {
    override fun equals(other: Any?) = other is HidReport && id == other.id && data.contentEquals(other.data)
    override fun hashCode() = id * 31 + data.contentHashCode()
}

/**
 * Turns the app's input messages into HID reports for Bluetooth input-only
 * mode. Stateful: it tracks held keys and mouse buttons and the last position.
 * Phone pixels of relative mouse motion are passed through as HID counts.
 */
class HidTranslator {
    private val held = LinkedHashSet<Int>()
    private var buttons = 0

    fun translate(msg: Wire.Message): List<HidReport> = when (msg) {
        is Wire.Message.Key -> key(msg)
        is Wire.Message.Mouse -> mouse(msg)
        is Wire.Message.Scroll -> listOf(
            HidReport(HidReports.ID_MOUSE, HidReports.mouse(buttons, 0, 0, msg.dy.roundToInt()))
        )
        is Wire.Message.Touch -> touch(msg)
        else -> emptyList()
    }

    private fun key(k: Wire.Message.Key): List<HidReport> {
        val usage = HidReports.usageFor(k.keyCode)
        if (usage != 0) {
            if (k.action == Wire.ACTION_UP) held.remove(usage) else held.add(usage)
        }
        val mods = HidReports.modifiersFor(k.meta)
        return listOf(HidReport(HidReports.ID_KEYBOARD, HidReports.keyboard(mods, held.toList())))
    }

    private fun mouse(m: Wire.Message.Mouse): List<HidReport> {
        buttons = if (m.action == Wire.ACTION_UP || m.action == Wire.ACTION_CANCEL) 0 else m.buttons
        return if (m.relative) {
            listOf(HidReport(HidReports.ID_MOUSE, HidReports.mouse(buttons, m.x.roundToInt(), m.y.roundToInt())))
        } else {
            val down = m.action == Wire.ACTION_DOWN || m.action == Wire.ACTION_MOVE
            listOf(HidReport(HidReports.ID_DIGITIZER, HidReports.digitizer(down, true, m.x, m.y)))
        }
    }

    private fun touch(t: Wire.Message.Touch): List<HidReport> {
        val p = t.points.firstOrNull() ?: return emptyList()
        val up = t.action == Wire.ACTION_UP || t.action == Wire.ACTION_CANCEL
        return listOf(HidReport(HidReports.ID_DIGITIZER, HidReports.digitizer(!up, !up, p.x, p.y)))
    }
}

/**
 * The phone as a Bluetooth keyboard + mouse + touch digitizer (API 28+), so
 * any PC can use it with no DisplaySwarm software. Pair from the PC's Bluetooth
 * settings (the phone shows up after [start]), or call [connect] with a bonded
 * device. Needs the BLUETOOTH_CONNECT runtime permission on Android 12+.
 */
@RequiresApi(Build.VERSION_CODES.P)
@SuppressLint("MissingPermission")
class BluetoothHidInput(
    private val context: Context,
    private val onState: (String) -> Unit = {}
) {
    private val executor = Executors.newSingleThreadExecutor()
    private val translator = HidTranslator()
    private var hid: BluetoothHidDevice? = null
    private var host: BluetoothDevice? = null

    val isConnected: Boolean get() = host != null

    private val callback = object : BluetoothHidDevice.Callback() {
        override fun onAppStatusChanged(pluggedDevice: BluetoothDevice?, registered: Boolean) {
            onState(if (registered) "Bluetooth input ready: pair this phone from the PC" else "Bluetooth input off")
        }

        override fun onConnectionStateChanged(device: BluetoothDevice, state: Int) {
            if (state == BluetoothProfile.STATE_CONNECTED) host = device
            else if (state == BluetoothProfile.STATE_DISCONNECTED && host == device) host = null
            onState(
                when (state) {
                    BluetoothProfile.STATE_CONNECTED -> "Bluetooth connected to ${device.name ?: device.address}"
                    BluetoothProfile.STATE_CONNECTING -> "Bluetooth connecting..."
                    else -> "Bluetooth disconnected"
                }
            )
        }
    }

    private val listener = object : BluetoothProfile.ServiceListener {
        override fun onServiceConnected(profile: Int, proxy: BluetoothProfile) {
            val h = proxy as BluetoothHidDevice
            hid = h
            val sdp = BluetoothHidDeviceAppSdpSettings(
                "DisplaySwarm", "Phone as keyboard and touchpad", "DisplaySwarm",
                BluetoothHidDevice.SUBCLASS1_COMBO, HidReports.DESCRIPTOR
            )
            if (!h.registerApp(sdp, null, null, executor, callback)) onState("Bluetooth HID could not start")
        }

        override fun onServiceDisconnected(profile: Int) {
            hid = null
            host = null
        }
    }

    /** Registers the HID profile; false if Bluetooth is unavailable or off. */
    fun start(): Boolean {
        val adapter = BluetoothAdapter.getDefaultAdapter() ?: return false
        if (!adapter.isEnabled) return false
        return adapter.getProfileProxy(context.applicationContext, listener, BluetoothProfile.HID_DEVICE)
    }

    /** Connects to an already bonded PC. */
    fun connect(device: BluetoothDevice): Boolean = hid?.connect(device) ?: false

    /** Bonded devices, for a picker. */
    fun bondedDevices(): List<BluetoothDevice> =
        BluetoothAdapter.getDefaultAdapter()?.bondedDevices?.toList() ?: emptyList()

    /** Sends the HID equivalent of an input message; ignored when no PC is connected. */
    fun offer(msg: Wire.Message) {
        val h = hid ?: return
        val d = host ?: return
        for (r in translator.translate(msg)) h.sendReport(d, r.id, r.data)
    }

    fun stop() {
        hid?.let {
            it.unregisterApp()
            BluetoothAdapter.getDefaultAdapter()?.closeProfileProxy(BluetoothProfile.HID_DEVICE, it)
        }
        hid = null
        host = null
        executor.shutdown()
        Log.i("DisplaySwarmBtHid", "stopped")
    }

    companion object {
        /** True when this device can act as a HID device (API 28+ with Bluetooth). */
        fun isSupported(context: Context): Boolean =
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.P &&
                context.packageManager.hasSystemFeature(android.content.pm.PackageManager.FEATURE_BLUETOOTH)
    }
}
