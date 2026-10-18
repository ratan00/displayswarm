package com.displayswarm.client

import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTimestamp
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.os.Process
import android.util.Log
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.TimeUnit

/** Per-phone audio switches. The settings UI reads and writes these. */
object AudioSettings {
    // The toolbar's switches live in AppSettings; this reads the same keys, so
    // there is one source of truth (a separate prefs file here made the
    // switches do nothing).
    private fun app(c: Context) = AppSettings.from(c)

    /**
     * Play the host's audio (default on). The phone always offers audio; the
     * host is told about a change at once ([PhoneServices.announce]).
     */
    fun playbackEnabled(c: Context) = app(c).audioOut

    fun setPlaybackEnabled(c: Context, on: Boolean) {
        app(c).audioOut = on
    }

    /** Send the microphone to the host (default off). Same timing as [playbackEnabled]. */
    fun micEnabled(c: Context) = app(c).mic

    /** Turning the mic on asks for the RECORD_AUDIO permission when [activity] is given. */
    fun setMicEnabled(c: Context, on: Boolean, activity: Activity? = null) {
        app(c).mic = on
        if (on && activity != null && !hasMicPermission(c)) {
            activity.requestPermissions(arrayOf(android.Manifest.permission.RECORD_AUDIO), 5)
        }
    }

    fun hasMicPermission(c: Context) =
        c.checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    /** The device can encode Opus with MediaCodec (API 29). */
    val micSupported get() = Build.VERSION.SDK_INT >= 29
}

/**
 * Plays the host's audio: Opus `AudioFrame`s are decoded with MediaCodec into a
 * small jitter buffer ([JitterController]) and written to a low-latency
 * `AudioTrack`. Two threads: one decodes (and conceals lost packets with
 * silence), one paces playback by blocking on the track.
 */
class AudioPlayer(private val context: Context) : PhoneService {
    override val name = "audio-player"

    override val features = Wire.FEATURE_AUDIO_OUT

    /** [ptsUs] is the host-clock time of the first sample, or [NO_PTS] (concealment silence). */
    private class Chunk(val pcm: ShortArray, val samples: Int, val ptsUs: Long = NO_PTS)

    private val rx = ArrayBlockingQueue<Wire.Message.Audio>(64)
    private val lock = Object()
    private val queue = ArrayDeque<Chunk>()
    private var queuedMs = 0
    private val ctl = JitterController()

    @Volatile
    private var running = false
    private var decodeThread: Thread? = null
    private var playThread: Thread? = null

    @Volatile
    private var lastPacketMs = 0L

    // The audio switch, cached: it was read from SharedPreferences (with a new
    // AppSettings each time) twice per 10 ms packet.
    @Volatile
    private var playbackOn = true
    private val prefs = context.applicationContext.getSharedPreferences(AppSettings.PREFS_NAME, Context.MODE_PRIVATE)
    private val prefsListener = android.content.SharedPreferences.OnSharedPreferenceChangeListener { _, key ->
        if (key == AppSettings.KEY_AUDIO_OUT) playbackOn = AudioSettings.playbackEnabled(context)
    }

    // -- multi-phone sync (see SyncAlign): 0 = off, play as soon as possible.
    @Volatile
    private var syncDelayUs = 0L
    private var clock: ClockSync? = null
    private var send: ((Wire.Message) -> Unit)? = null
    private val transit = TransitWindow()

    /** Smoothed time from writing a frame to the track until it is audible. */
    @Volatile
    private var outputPipelineUs = FALLBACK_OUTPUT_LATENCY_US

    override fun wants(msg: Wire.Message) =
        (msg is Wire.Message.Audio && msg.type == Wire.MSG_AUDIO_FRAME) || msg is Wire.Message.AudioSync

    override fun onMessage(msg: Wire.Message) {
        if (msg is Wire.Message.AudioSync) {
            syncDelayUs = msg.delayUs
            Log.i(TAG, "audio sync delay ${msg.delayUs / 1000} ms")
            return
        }
        val a = msg as? Wire.Message.Audio ?: return
        if (!playbackOn) return
        lastPacketMs = System.nanoTime() / 1_000_000
        clock?.takeIf { it.hasSync }?.let { transit.add(PhoneClock.nowUs() - it.hostToPhoneUs(a.ptsUs)) }
        // A full queue means decoding is stuck: keep the newest audio.
        while (!rx.offer(a)) rx.poll()
    }

    override fun onSessionStart(ctx: PhoneServiceContext) {
        running = true
        playbackOn = AudioSettings.playbackEnabled(context)
        prefs.registerOnSharedPreferenceChangeListener(prefsListener)
        clock = ctx.clock
        send = ctx.send
        syncDelayUs = 0
        transit.clear()
        outputPipelineUs = FALLBACK_OUTPUT_LATENCY_US
        rx.clear()
        synchronized(lock) { queue.clear(); queuedMs = 0 }
        decodeThread = Thread({ decodeLoop() }, "audio-decode").also { it.start() }
    }

    override fun onSessionEnd() {
        prefs.unregisterOnSharedPreferenceChangeListener(prefsListener)
        running = false
        decodeThread?.interrupt()
        synchronized(lock) { lock.notifyAll() }
        playThread?.interrupt()
        decodeThread?.join(500)
        playThread?.join(500)
        decodeThread = null
        playThread = null
    }

    private fun decodeLoop() {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
        // libopus when the native library loaded, MediaCodec otherwise.
        var native = 0L
        var codec: MediaCodec? = null
        val pcmBuf = ShortArray(OpusNative.MAX_FRAME_SAMPLES * 2)
        val seq = SeqTracker()
        var channels = 0
        var lastSamples = 480
        try {
            while (running) {
                val p = rx.poll(100, TimeUnit.MILLISECONDS) ?: continue
                if (native == 0L && codec == null) {
                    channels = p.channels
                    if (OpusNative.available) native = OpusNative.decoderCreate(channels)
                    if (native == 0L) codec = startDecoder(channels) ?: return
                    Log.i(TAG, "audio decoder: ${if (native != 0L) "libopus" else "MediaCodec"}, $channels ch")
                    startPlayback(channels)
                }
                if (p.channels != channels) continue
                when (val v = seq.classify(p.seq)) {
                    is SeqVerdict.Stale -> continue
                    is SeqVerdict.Gap -> repeat(v.missing) {
                        if (native != 0L) {
                            // Opus packet loss concealment: a plausible continuation, not a click of silence.
                            val n = OpusNative.decode(native, null, 0, pcmBuf, lastSamples)
                            if (n > 0) enqueue(Chunk(pcmBuf.copyOf(n * channels), n))
                        } else {
                            enqueue(Chunk(ShortArray(lastSamples * channels), lastSamples))
                        }
                    }
                    is SeqVerdict.InOrder -> {}
                }
                if (native != 0L) {
                    val n = OpusNative.decode(native, p.data, p.data.size, pcmBuf, OpusNative.MAX_FRAME_SAMPLES)
                    if (n > 0) {
                        enqueue(Chunk(pcmBuf.copyOf(n * channels), n, p.ptsUs))
                        lastSamples = n
                    }
                } else {
                    decode(codec!!, p, channels)?.let { lastSamples = it }
                }
            }
        } catch (e: InterruptedException) {
            // session over
        } catch (e: Exception) {
            Log.w(TAG, "audio decode stopped", e)
        } finally {
            if (native != 0L) OpusNative.decoderDestroy(native)
            try {
                codec?.stop()
            } catch (_: Exception) {
            }
            codec?.release()
        }
    }

    private fun startDecoder(channels: Int): MediaCodec? = try {
        val fmt = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, SAMPLE_RATE, channels)
        fmt.setByteBuffer("csd-0", java.nio.ByteBuffer.wrap(OpusCsd.head(channels)))
        fmt.setByteBuffer("csd-1", java.nio.ByteBuffer.wrap(OpusCsd.nanos(0)))
        fmt.setByteBuffer("csd-2", java.nio.ByteBuffer.wrap(OpusCsd.nanos(80_000_000L)))
        MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS).also {
            it.configure(fmt, null, null, 0)
            it.start()
        }
    } catch (e: Exception) {
        Log.w(TAG, "no Opus decoder", e)
        null
    }

    /** Decodes one packet, queues its PCM and returns samples per channel of the last chunk. */
    private fun decode(codec: MediaCodec, p: Wire.Message.Audio, channels: Int): Int? {
        val i = codec.dequeueInputBuffer(10_000)
        if (i < 0) return null
        val buf = codec.getInputBuffer(i) ?: return null
        buf.clear()
        buf.put(p.data)
        codec.queueInputBuffer(i, 0, p.data.size, p.ptsUs, 0)
        var last: Int? = null
        val info = MediaCodec.BufferInfo()
        var timeout = 3_000L // the first output of a packet may lag a moment; later ones are ready
        while (true) {
            val o = codec.dequeueOutputBuffer(info, timeout)
            timeout = 0
            if (o >= 0) {
                if (info.size > 0) {
                    val out = codec.getOutputBuffer(o)!!
                    val bytes = ByteArray(info.size)
                    out.position(info.offset)
                    out.get(bytes)
                    val pcm = PcmUtil.toShorts(bytes)
                    val samples = pcm.size / channels
                    // The decoder passes the packet's pts through; distrust it if it is far off.
                    val pts = if (Math.abs(info.presentationTimeUs - p.ptsUs) < 1_000_000L) info.presentationTimeUs else p.ptsUs
                    enqueue(Chunk(pcm, samples, pts))
                    last = samples
                }
                codec.releaseOutputBuffer(o, false)
            } else if (o != MediaCodec.INFO_OUTPUT_FORMAT_CHANGED && o != MediaCodec.INFO_OUTPUT_BUFFERS_CHANGED) {
                break
            }
        }
        return last
    }

    private fun enqueue(c: Chunk) {
        synchronized(lock) {
            queue.addLast(c)
            queuedMs += PcmUtil.durationMs(c.samples)
            lock.notifyAll()
        }
    }

    private fun startPlayback(channels: Int) {
        playThread = Thread({ playLoop(channels) }, "audio-play").also { it.start() }
    }

    private fun playLoop(channels: Int) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
        val track = buildTrack(channels)
        try {
            track.play()
            // The app's own jitter buffer absorbs the network; the track's buffer
            // only has to cover the mixer's burst. Start at two bursts and grow
            // one burst at a time on real underruns (cut ~40 ms of output delay).
            val burst = framesPerBurst()
            var bufFrames = (burst * 2).coerceAtMost(track.bufferCapacityInFrames)
            track.bufferSizeInFrames = bufFrames
            var underruns = track.underrunCount
            var audible = true
            var written = 0L // frames per channel handed to the track
            var lastReportMs = 0L
            val ts = AudioTimestamp()
            var paused = false
            while (running) {
                val chunk = nextChunk()
                if (chunk == null) {
                    // The host stops sending during silence: let the audio
                    // hardware sleep instead of playing an underrun.
                    if (!paused && System.nanoTime() / 1_000_000 - lastPacketMs > IDLE_PAUSE_MS) {
                        track.pause()
                        paused = true
                    }
                    continue
                }
                if (paused) {
                    track.play()
                    paused = false
                    underruns = track.underrunCount
                }
                val u = track.underrunCount
                if (u > underruns) {
                    underruns = u
                    if (synchronized(lock) { queuedMs } > 0 && bufFrames < track.bufferCapacityInFrames) {
                        bufFrames = (bufFrames + burst).coerceAtMost(track.bufferCapacityInFrames)
                        track.bufferSizeInFrames = bufFrames
                        Log.i(TAG, "output buffer raised to $bufFrames frames after an underrun")
                    }
                }
                // Muting keeps the track fed, so pacing and the jitter buffer
                // carry on and unmuting is instant.
                val on = playbackOn
                if (on != audible) {
                    audible = on
                    track.setVolume(if (on) 1f else 0f)
                }
                var pcm = chunk.pcm
                val nowUs = PhoneClock.nowUs()
                val audibleAt = audibleAtUs(track, ts, written, nowUs)
                outputPipelineUs = (outputPipelineUs * 15 + (audibleAt - nowUs).coerceIn(0, 500_000)) / 16
                val clk = clock
                val delayUs = syncDelayUs
                if (delayUs > 0 && clk != null && clk.hasSync && chunk.ptsUs != NO_PTS) {
                    val adj = SyncAlign.adjust(audibleAt, clk.hostToPhoneUs(chunk.ptsUs + delayUs), chunk.samples, SAMPLE_RATE)
                    if (adj.silenceSamples > 0) {
                        track.write(ShortArray(adj.silenceSamples * channels), 0, adj.silenceSamples * channels, AudioTrack.WRITE_BLOCKING)
                        written += adj.silenceSamples
                    }
                    if (adj.dropSamples > 0) pcm = pcm.copyOfRange(adj.dropSamples * channels, pcm.size)
                }
                track.write(pcm, 0, pcm.size, AudioTrack.WRITE_BLOCKING)
                written += pcm.size / channels
                synchronized(lock) { ctl.onPlayed(PcmUtil.durationMs(chunk.samples)) }
                val nowMs = nowUs / 1000
                if (nowMs - lastReportMs >= REPORT_INTERVAL_MS) {
                    lastReportMs = nowMs
                    reportLatency()
                }
            }
        } catch (e: Exception) {
            if (running) Log.w(TAG, "audio playback stopped", e)
        } finally {
            try {
                track.stop()
            } catch (_: Exception) {
            }
            track.release()
        }
    }

    /**
     * When the frame with index [written] (per channel, counted from the start
     * of the track) will be heard, on the phone clock. Uses the track's own
     * timestamp (includes the audio HAL and speaker path) when there is one.
     */
    private fun audibleAtUs(track: AudioTrack, ts: AudioTimestamp, written: Long, nowUs: Long): Long {
        if (track.getTimestamp(ts)) {
            return ts.nanoTime / 1000 + (written - ts.framePosition) * 1_000_000L / SAMPLE_RATE
        }
        val queued = (written - (track.playbackHeadPosition.toLong() and 0xFFFF_FFFFL)).coerceAtLeast(0)
        return nowUs + queued * 1_000_000L / SAMPLE_RATE + FALLBACK_OUTPUT_LATENCY_US
    }

    /**
     * Tells the host how long this phone's audio takes from the host's
     * timestamp to the speaker when played as soon as possible: transit (high
     * percentile), decode, the jitter buffer and the output path.
     */
    private fun reportLatency() {
        val transitUs = transit.percentile() ?: return
        val jitterUs = synchronized(lock) { ctl.targetMs } * 1000L
        val total = transitUs + DECODE_ALLOWANCE_US + jitterUs + outputPipelineUs
        Log.i(
            TAG,
            "latency ${total / 1000} ms = transit ${transitUs / 1000} + decode ${DECODE_ALLOWANCE_US / 1000} " +
                "+ jitter ${jitterUs / 1000} + output ${outputPipelineUs / 1000}",
        )
        send?.invoke(Wire.Message.AudioLatency(total.coerceIn(0, 5_000_000L)))
    }

    /** Waits for the next chunk that may be played, or null after a short timeout. */
    private fun nextChunk(): Chunk? = synchronized(lock) {
        // With sync on the queue legitimately holds the shared delay's worth of audio.
        val excess = syncDelayUs.let { d ->
            if (d > 0) maxOf(0, queuedMs - (d / 1000).toInt() - SYNC_QUEUE_SLACK_MS) else ctl.excessMs(queuedMs)
        }
        var dropped = 0
        while (dropped < excess && queue.size > 1) {
            val c = queue.removeFirst()
            val ms = PcmUtil.durationMs(c.samples)
            queuedMs -= ms
            dropped += ms
        }
        if (ctl.shouldPlay(queuedMs)) {
            val c = queue.removeFirstOrNull()
            if (c != null) {
                queuedMs -= PcmUtil.durationMs(c.samples)
                return c
            }
            val silence = System.nanoTime() / 1_000_000 - lastPacketMs
            ctl.onUnderrun(silence)
        }
        // Idle (nothing from the host for a while): wake rarely; enqueue() notifies anyway.
        lock.wait(if (System.nanoTime() / 1_000_000 - lastPacketMs > IDLE_PAUSE_MS) 250 else 20)
        null
    }

    /** The mixer's burst size (frames per callback), from the platform; 240 if unknown. */
    private fun framesPerBurst(): Int {
        val am = context.getSystemService(Context.AUDIO_SERVICE) as android.media.AudioManager
        return am.getProperty(android.media.AudioManager.PROPERTY_OUTPUT_FRAMES_PER_BUFFER)?.toIntOrNull()?.takeIf { it > 0 } ?: 240
    }

    private fun buildTrack(channels: Int): AudioTrack {
        val mask = if (channels == 1) AudioFormat.CHANNEL_OUT_MONO else AudioFormat.CHANNEL_OUT_STEREO
        val fmt = AudioFormat.Builder()
            .setSampleRate(SAMPLE_RATE)
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setChannelMask(mask)
            .build()
        val min = AudioTrack.getMinBufferSize(SAMPLE_RATE, mask, AudioFormat.ENCODING_PCM_16BIT)
        return AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                    .build(),
            )
            .setAudioFormat(fmt)
            // Capacity only (room to grow after underruns); the size in use is set in playLoop.
            .setBufferSizeInBytes(maxOf(min, SAMPLE_RATE / 10 * channels * 2))
            .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            .setTransferMode(AudioTrack.MODE_STREAM)
            .build()
    }

    companion object {
        private const val TAG = "AudioPlayer"
        const val SAMPLE_RATE = 48_000
        private const val NO_PTS = Long.MIN_VALUE
        private const val REPORT_INTERVAL_MS = 2_000L
        private const val FALLBACK_OUTPUT_LATENCY_US = 40_000L
        private const val DECODE_ALLOWANCE_US = 10_000L
        private const val SYNC_QUEUE_SLACK_MS = 300
        /** No audio from the host for this long: pause the track. */
        private const val IDLE_PAUSE_MS = 1_000L
    }
}
