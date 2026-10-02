package com.displayswarm.client

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.BatteryManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.PowerManager
import android.os.SystemClock
import android.util.Log

/** Pure helpers for [BatteryReporter]. */
object BatteryMath {
    /** Battery percentage from `EXTRA_LEVEL`/`EXTRA_SCALE`; 255 when unknown. */
    fun percent(level: Int, scale: Int): Int =
        if (level < 0 || scale <= 0) 255 else (level * 100L / scale).toInt().coerceIn(0, 100)

    /** Whether `EXTRA_STATUS` means the battery is charging or full on power. */
    fun charging(status: Int): Boolean =
        status == BatteryManager.BATTERY_STATUS_CHARGING || status == BatteryManager.BATTERY_STATUS_FULL

    /** `EXTRA_TEMPERATURE` (tenths of a degree), or [Wire.TEMP_UNKNOWN] if absent. */
    fun temp(decidegrees: Int): Int =
        if (decidegrees == Int.MIN_VALUE) Wire.TEMP_UNKNOWN else decidegrees.coerceIn(-1000, 2000)

    /** A report is sent at least this often, and on change no more often than [MIN_GAP_MS]. */
    const val PERIOD_MS = 30_000L
    const val MIN_GAP_MS = 2_000L

    fun shouldSend(prev: Wire.Message.BatteryStatus?, cur: Wire.Message.BatteryStatus, sinceLastMs: Long): Boolean {
        if (prev == null || sinceLastMs >= PERIOD_MS) return true
        // Temperature alone drifts constantly: it rides along with other changes.
        val changed = prev.percent != cur.percent || prev.charging != cur.charging || prev.thermal != cur.thermal
        return changed && sinceLastMs >= MIN_GAP_MS
    }
}

/**
 * Tells the host the battery level, charging state and thermal status: on
 * change and every 30 s. The host lowers the bitrate when the battery is low
 * or the phone runs hot (if the device's battery saver is on) and shows the
 * status in its UI. Needs no permission (the battery broadcast is sticky; the
 * thermal listener is API 29+).
 */
class BatteryReporter : PhoneService {
    override val name = "battery"
    override val features = Wire.FEATURE_BATTERY

    private var ctx: PhoneServiceContext? = null
    private var thread: HandlerThread? = null
    private var handler: Handler? = null
    private var receiver: BroadcastReceiver? = null
    private var thermalListener: PowerManager.OnThermalStatusChangedListener? = null

    @Volatile
    private var thermal = Wire.THERMAL_NONE
    private var last: Wire.Message.BatteryStatus? = null
    private var lastAt = 0L

    override fun wants(msg: Wire.Message) = false

    override fun onMessage(msg: Wire.Message) {}

    override fun onSessionStart(ctx: PhoneServiceContext) {
        this.ctx = ctx
        last = null
        val t = HandlerThread("battery-report").also { it.start() }
        thread = t
        val h = Handler(t.looper)
        handler = h
        val app = ctx.appContext
        val rx = object : BroadcastReceiver() {
            override fun onReceive(c: Context, i: Intent) = report(i)
        }
        receiver = rx
        val sticky = app.registerReceiver(rx, IntentFilter(Intent.ACTION_BATTERY_CHANGED), null, h)
        if (Build.VERSION.SDK_INT >= 29) {
            val pm = app.getSystemService(Context.POWER_SERVICE) as PowerManager
            thermal = pm.currentThermalStatus
            val l = PowerManager.OnThermalStatusChangedListener { s ->
                thermal = s
                h.post { report(null) }
            }
            thermalListener = l
            pm.addThermalStatusListener(l)
        }
        h.post { report(sticky) }
        schedulePeriodic()
    }

    private fun schedulePeriodic() {
        handler?.postDelayed({
            report(null)
            schedulePeriodic()
        }, BatteryMath.PERIOD_MS)
    }

    private fun report(intent: Intent?) {
        val c = ctx ?: return
        val i = intent ?: c.appContext.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED)) ?: return
        val cur = Wire.Message.BatteryStatus(
            BatteryMath.percent(i.getIntExtra(BatteryManager.EXTRA_LEVEL, -1), i.getIntExtra(BatteryManager.EXTRA_SCALE, -1)),
            BatteryMath.charging(i.getIntExtra(BatteryManager.EXTRA_STATUS, -1)),
            thermal,
            BatteryMath.temp(i.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, Int.MIN_VALUE)),
        )
        val now = SystemClock.elapsedRealtime()
        if (!BatteryMath.shouldSend(last, cur, now - lastAt)) return
        last = cur
        lastAt = now
        c.send(cur)
    }

    override fun onSessionEnd() {
        val c = ctx ?: return
        ctx = null
        try {
            receiver?.let { c.appContext.unregisterReceiver(it) }
            if (Build.VERSION.SDK_INT >= 29) {
                val pm = c.appContext.getSystemService(Context.POWER_SERVICE) as PowerManager
                thermalListener?.let { pm.removeThermalStatusListener(it) }
            }
        } catch (e: Exception) {
            Log.w("DisplaySwarmBattery", "cleanup failed", e)
        }
        receiver = null
        thermalListener = null
        handler?.removeCallbacksAndMessages(null)
        thread?.quitSafely()
        thread = null
        handler = null
        last = null
    }
}
