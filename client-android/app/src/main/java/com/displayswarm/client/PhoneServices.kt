package com.displayswarm.client

import android.content.Context
import android.util.Log

/**
 * A feature beside video and input that lives for one session: audio playback
 * and the mic (Phase 5), clipboard and battery reporting (Phase 8).
 * The phone-side mirror of the host's `services` module.
 *
 * Contract:
 * - Services are registered once with [PhoneServices.install] (MainActivity).
 * - [features] are OR-ed into the Hello; a service only runs when the host's
 *   HelloAck also carries at least one of its bits.
 * - [onSessionStart] is called after the handshake, [onSessionEnd] when the
 *   session is over (either side, any reason). A service must release
 *   everything there (audio tracks, recorders, listeners).
 * - Every host message [ControlSession] does not handle itself is offered to
 *   each running service; the first whose [wants] is true gets [onMessage].
 *   It runs on the transport's receive thread: do not block.
 */
interface PhoneService {
    val name: String

    /** `Wire.FEATURE_*` bits this service offers. */
    val features: Long

    fun wants(msg: Wire.Message): Boolean

    fun onMessage(msg: Wire.Message)

    fun onSessionStart(ctx: PhoneServiceContext)

    fun onSessionEnd()

    /** The effective role changed (see [RoleState]). */
    fun onRole(role: Int) {}
}

/** What a running service can use. */
class PhoneServiceContext(
    val appContext: Context,
    /** `Wire.FEATURE_*` bits both sides agreed on. */
    val features: Long,
    /** Sends a message to the host through the session's send queue. */
    val send: (Wire.Message) -> Unit,
    /** Host-to-phone clock mapping of the session, if there is one. */
    val clock: ClockSync? = null,
)

object PhoneServices {
    private const val TAG = "DisplaySwarmServices"

    @Volatile
    private var all: List<PhoneService> = emptyList()

    @Volatile
    private var appContext: Context? = null

    @Volatile
    private var running: List<PhoneService> = emptyList()

    @Volatile
    private var sender: ((Wire.Message) -> Unit)? = null

    fun install(context: Context, services: List<PhoneService>) {
        appContext = context.applicationContext
        all = services
    }

    /** Bits to OR into the Hello. */
    fun features(): Long = all.fold(0L) { acc, s -> acc or s.features }

    @Synchronized
    fun start(hostFeatures: Long, send: (Wire.Message) -> Unit, clock: ClockSync? = null) {
        stop()
        val ctx = appContext ?: return
        val agreed = hostFeatures and features()
        running = all.filter { it.features and agreed != 0L }
        val pctx = PhoneServiceContext(ctx, agreed, send, clock)
        sender = send
        running.forEach {
            try {
                it.onSessionStart(pctx)
            } catch (e: Exception) {
                Log.w(TAG, "${it.name} failed to start", e)
            }
        }
        if (running.isNotEmpty()) Log.i(TAG, "Services: ${running.joinToString { it.name }}")
        announce()
    }

    /**
     * Tells the host which optional services the user has switched on. The
     * phone offers them all; the host creates or removes its virtual devices
     * from this. Call after a switch changes.
     */
    fun announce() {
        val ctx = appContext ?: return
        val send = sender ?: return
        var bits = 0
        if (AudioSettings.playbackEnabled(ctx)) bits = bits or Wire.SERVICE_AUDIO_OUT
        if (AudioSettings.micEnabled(ctx) && AudioSettings.hasMicPermission(ctx)) bits = bits or Wire.SERVICE_MIC
        send(Wire.Message.ServiceState(bits))
    }

    @Synchronized
    fun stop() {
        running.forEach {
            try {
                it.onSessionEnd()
            } catch (e: Exception) {
                Log.w(TAG, "${it.name} failed to stop", e)
            }
        }
        running = emptyList()
        sender = null
    }

    /** Returns whether a running service took [msg]. */
    fun dispatch(msg: Wire.Message): Boolean {
        val s = running.firstOrNull { it.wants(msg) } ?: return false
        s.onMessage(msg)
        return true
    }

    fun onRole(role: Int) = running.forEach { it.onRole(role) }
}
