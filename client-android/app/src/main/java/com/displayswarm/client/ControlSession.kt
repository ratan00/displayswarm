package com.displayswarm.client

import android.util.Log
import kotlinx.coroutines.*
import java.util.concurrent.atomic.AtomicLong

/**
 * Session-level logic shared by the USB and TCP transports: the control
 * message exchange, clock sync, latency measurement, keyframe requests and
 * liveness.
 *
 * Everything the phone sends goes through [sendPacket], which the transport
 * points at its input send worker, so a message is never written in the middle
 * of a pointer or key packet.
 *
 * Latency is measured from the host's capture timestamp (`VideoFrame.captureUs`,
 * host monotonic clock) mapped onto the phone's monotonic clock
 * with [clock]. Nothing here reads the wall clock.
 */
class ControlSession(
    private val sendPacket: (ByteArray) -> Unit,
    private val onStatus: (String) -> Unit,
    private val onMetrics: (SessionMetrics) -> Unit,
    /** The session is over (host said BYE, or the link went silent); the argument is the final status text. */
    private val onEnded: (status: String) -> Unit,
    /** The effective role changed (HelloAck, host SetRole or a request from this phone). */
    private val onRole: (RoleState) -> Unit = {}
) : VideoDecoder.Listener {
    companion object {
        private const val TAG = "DisplaySwarmSession"

        /** Quality to ask the host for once connected; -1 = not chosen. Set from the settings. */
        @Volatile
        var preferredQuality: Int = -1

        const val PING_FAST_INTERVAL_MS = 200L
        const val PING_FAST_PHASE_MS = 2_000L
        const val PING_INTERVAL_MS = 1_000L
        const val HEARTBEAT_INTERVAL_MS = 500L
        const val STATS_INTERVAL_MS = 1_000L

        /** No bytes at all from the host for this long means the link is dead. */
        const val LIVENESS_TIMEOUT_MS = 3_000L

        /** At most one KEYFRAME_REQUEST per this interval. */
        const val KEYFRAME_MIN_INTERVAL_MS = 500L

        /** Frame header + VideoFrame fields before the codec data. */
        private const val VIDEO_OVERHEAD = Wire.HEADER_SIZE + 14
    }

    val clock = ClockSync()

    @Volatile
    private var streamInfo = ""

    @Volatile
    private var streaming = false

    /** Effective role; the host is the source of truth. */
    @Volatile
    var roleState = RoleState()
        private set

    @Volatile
    private var lastRxMs = nowMs()
    private var pingId = 0L
    private var lastFrameIndex = -1L
    private val lastKeyframeRequestMs = AtomicLong(0)

    // Cumulative counters (frames_decoded / frames_dropped in CLIENT_STATS).
    private val framesDecoded = AtomicLong()
    private val framesDropped = AtomicLong()
    private val rxBytes = AtomicLong()

    // Per-window latency accumulators, microseconds.
    private val rxLat = Accumulator()
    private val decLat = Accumulator()
    private val presentLat = Accumulator()

    private class Accumulator {
        private var sum = 0L
        private var count = 0L

        @Synchronized
        fun add(us: Long) {
            sum += us
            count++
        }

        @Synchronized
        fun takeAverage(): Long {
            val avg = if (count > 0) sum / count else 0L
            sum = 0
            count = 0
            return avg
        }
    }

    private fun nowMs() = System.nanoTime() / 1_000_000

    /** Called once the HelloAck arrived; the host has not necessarily started capturing yet. */
    fun onHandshakeComplete(info: String, ackRole: Int = Wire.ROLE_MIRROR, hostFeatures: Long = 0L) {
        PhoneServices.start(hostFeatures, ::send, clock)
        if (preferredQuality in 0..3) send(Wire.Message.SetQuality(preferredQuality))
        streamInfo = info
        lastRxMs = nowMs()
        roleState = roleState.onHelloAck(ackRole)
        onRole(roleState)
        onStatus(
            if (roleState.hasVideo) "Connected, waiting for host..."
            else videolessStatus()
        )
    }

    /** Tells the host the picture quality to use from now on. */
    fun setQuality(quality: Int) {
        preferredQuality = quality
        send(Wire.Message.SetQuality(quality))
    }

    /** Asks the host to switch this phone to [role]; the host answers with a SetRole. */
    fun requestRole(role: Int) {
        val next = roleState.request(role) ?: return
        applyRole(next)
        send(Wire.Message.SetRole(role))
    }

    /** The user dismissed the chooser: keep Mirror, tell nobody. */
    fun dismissRoleChooser() {
        roleState = roleState.dismissChooser()
        onRole(roleState)
    }

    private fun applyRole(next: RoleState) {
        val hadVideo = roleState.hasVideo
        roleState = next
        if (hadVideo != next.hasVideo) {
            // Video starts or stops: forget the old stream's position.
            streaming = false
            lastFrameIndex = -1
            onStatus(if (next.hasVideo) "Waiting for video..." else videolessStatus())
        } else if (next.hasVideo && streaming) {
            onStatus(streamingStatus())
        }
        onRole(next)
        PhoneServices.onRole(next.role)
    }

    private fun videolessStatus() = "Connected - ${roleState.label} (no video)"

    /**
     * Starts the periodic senders and the liveness watchdog. Cancel the returned
     * job when the transport session ends.
     */
    fun start(scope: CoroutineScope): Job = scope.launch {
        lastRxMs = nowMs()
        launch { pingLoop() }
        launch {
            while (isActive) {
                send(Wire.Message.Heartbeat)
                delay(HEARTBEAT_INTERVAL_MS)
            }
        }
        launch { statsLoop() }
        launch { watchdog() }
    }

    private suspend fun pingLoop() {
        val begin = nowMs()
        while (currentCoroutineContext().isActive) {
            send(Wire.Message.Ping(pingId++ and 0xFFFFFFFFL, PhoneClock.nowUs()))
            // Converge quickly at the start, then settle to one per second.
            delay(if (nowMs() - begin < PING_FAST_PHASE_MS) PING_FAST_INTERVAL_MS else PING_INTERVAL_MS)
        }
    }

    private suspend fun statsLoop() {
        var lastDecoded = 0L
        var lastBytes = 0L
        var lastTickMs = nowMs()
        while (currentCoroutineContext().isActive) {
            delay(STATS_INTERVAL_MS)
            val now = nowMs()
            val elapsedMs = (now - lastTickMs).coerceAtLeast(1)
            lastTickMs = now

            val decoded = framesDecoded.get()
            val bytes = rxBytes.get()
            val fps = ((decoded - lastDecoded) * 1000 / elapsedMs).toInt()
            val kbps = (bytes - lastBytes) * 8 / elapsedMs // bits per ms == kbit/s
            lastDecoded = decoded
            lastBytes = bytes

            val rxUs = rxLat.takeAverage()
            val decUs = decLat.takeAverage()
            val presentUs = presentLat.takeAverage()

            send(
                Wire.Message.StatsMsg(
                    Wire.Stats(
                        framesDecoded = decoded,
                        framesDropped = framesDropped.get(),
                        decodeLatencyUs = decUs,
                        presentLatencyUs = presentUs,
                        rxKbps = kbps
                    )
                )
            )
            onMetrics(
                SessionMetrics(
                    fps = fps,
                    rxMs = rxUs / 1000,
                    decMs = decUs / 1000,
                    presentMs = presentUs / 1000,
                    mbps = kbps / 1000.0,
                    rttMs = clock.rttUs / 1000.0,
                    synced = clock.hasSync
                )
            )
        }
    }

    private suspend fun watchdog() {
        while (currentCoroutineContext().isActive) {
            delay(250)
            if (nowMs() - lastRxMs > LIVENESS_TIMEOUT_MS) {
                Log.w(TAG, "No data from host for ${LIVENESS_TIMEOUT_MS}ms; treating link as lost")
                onEnded("Disconnected (host not responding)")
                return
            }
        }
    }

    private fun send(msg: Wire.Message) = sendPacket(msg.encode())

    /** Asks the host for an IDR frame. Rate-limited so a burst of errors sends one request. */
    override fun onKeyframeNeeded(reason: String) {
        if (!roleState.hasVideo) return // no video expected in this role
        val now = nowMs()
        val last = lastKeyframeRequestMs.get()
        if (last != 0L && now - last < KEYFRAME_MIN_INTERVAL_MS) return
        if (!lastKeyframeRequestMs.compareAndSet(last, now)) return
        Log.i(TAG, "Requesting keyframe: $reason")
        send(Wire.Message.KeyframeRequest)
    }

    /** Encoded BYE(NORMAL), for the transport to send as its last packet. */
    /** The session is over: stop its services. Transports call this on teardown. */
    fun close() = PhoneServices.stop()

    fun byePacket(): ByteArray =
        Wire.Message.Bye(Wire.BYE_NORMAL, "client disconnect").encode()

    /** Handles any message read from the host. Video still needs forwarding by the caller. */
    fun onMessage(msg: Wire.Message) {
        lastRxMs = nowMs()
        when (msg) {
            is Wire.Message.VideoFrame -> onVideo(msg)
            else -> onControl(msg)
        }
    }

    private fun onVideo(frame: Wire.Message.VideoFrame) {
        rxBytes.addAndGet(frame.data.size + VIDEO_OVERHEAD.toLong())
        if (!roleState.hasVideo) return // a straggler from before a switch to a no-video role
        if (!streaming) {
            // The first frame is proof enough, even if HOST_STATE was missed.
            streaming = true
            onStatus(streamingStatus())
        }
        if (frame.frameType == Wire.FRAME_TYPE_CONFIG) return

        // One frame can be split into several packets sharing an index (config +
        // key), so the same index again is normal; anything else but +1 is a gap.
        val idx = frame.frameIndex
        val prev = lastFrameIndex
        if (prev >= 0 && idx != prev && idx != ((prev + 1) and 0xFFFFFFFFL)) {
            onKeyframeNeeded("frame gap $prev -> $idx")
        }
        lastFrameIndex = idx

        if (clock.hasSync) {
            rxLat.add(latencyUs(frame.captureUs, PhoneClock.nowUs()))
        }
    }

    private fun streamingStatus() = "Streaming $streamInfo - ${roleState.label}".trim()

    private fun latencyUs(hostTsUs: Long, phoneNowUs: Long): Long =
        (phoneNowUs - clock.hostToPhoneUs(hostTsUs)).coerceAtLeast(0)

    private fun onControl(msg: Wire.Message) {
        when (msg) {
            is Wire.Message.Pong ->
                clock.addSample(msg.tPingUs, msg.tRecvUs, msg.tReplyUs, PhoneClock.nowUs())
            is Wire.Message.Ping -> {
                // The reply's clocks are the phone's own monotonic clock.
                val recv = PhoneClock.nowUs()
                send(Wire.Message.Pong(msg.id, msg.tSendUs, recv, PhoneClock.nowUs()))
            }
            Wire.Message.Heartbeat -> {} // liveness is recorded for every message
            is Wire.Message.HostState -> onHostState(msg)
            is Wire.Message.Bye -> {
                val detail = if (msg.text.isNotEmpty()) ": ${msg.text}" else ""
                val status = when (msg.reason) {
                    Wire.BYE_SERVER_STOPPING -> "Host stopped the server"
                    Wire.BYE_VERSION_MISMATCH -> "Version mismatch$detail"
                    Wire.BYE_PROTOCOL_ERROR -> "Protocol error$detail"
                    Wire.BYE_ERROR -> "Host error$detail"
                    else -> "Host closed the session"
                }
                Log.i(TAG, "BYE from host: reason=${msg.reason} text=${msg.text}")
                onEnded(status)
            }
            is Wire.Message.SetRole -> {
                Log.i(TAG, "Host role now ${msg.role}")
                if (RoleState.isValid(msg.role)) applyRole(roleState.onHostSetRole(msg.role))
            }
            is Wire.Message.SetBitrate -> Log.i(TAG, "Host bitrate now ${msg.kbps} kbps")
            is Wire.Message.Unknown -> Log.d(TAG, "Skipping unknown message $msg")
            // Audio, clipboard, files, ...: the session's services (PhoneServices).
            else -> if (!PhoneServices.dispatch(msg)) Log.d(TAG, "Ignoring $msg")
        }
    }

    private fun onHostState(msg: Wire.Message.HostState) {
        when (msg.state) {
            Wire.HOST_STATE_AWAITING_PERMISSION -> {
                streaming = false
                onStatus("Waiting for permission on the host...")
            }
            Wire.HOST_STATE_STREAMING -> {
                if (!roleState.hasVideo) return
                streaming = true
                onStatus(streamingStatus())
            }
            Wire.HOST_STATE_CAPTURE_FAILED -> {
                streaming = false
                onStatus("Host capture failed" + if (msg.detail.isNotEmpty()) ": ${msg.detail}" else "")
            }
            else -> Log.w(TAG, "Unknown host state ${msg.state}: ${msg.detail}")
        }
    }

    // ---- VideoDecoder.Listener -------------------------------------------

    override fun onFrameDecoded(ptsUs: Long, nowUs: Long) {
        framesDecoded.incrementAndGet()
        if (clock.hasSync) decLat.add(latencyUs(ptsUs, nowUs))
    }

    override fun onFramePresented(ptsUs: Long, nowUs: Long) {
        if (clock.hasSync) presentLat.add(latencyUs(ptsUs, nowUs))
    }

    override fun onFrameDropped() {
        framesDropped.incrementAndGet()
    }
}
