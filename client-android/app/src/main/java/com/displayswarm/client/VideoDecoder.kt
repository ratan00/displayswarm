package com.displayswarm.client

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Low-latency H.264 hardware decoder using Android MediaCodec, rendering straight
 * onto a [Surface] without a CPU copy.
 *
 * The codec runs in asynchronous mode: [MediaCodec.Callback] is delivered on a
 * dedicated [HandlerThread], so there is no polling and no dequeue timeout. A
 * NAL unit is queued the moment both it and a free input buffer exist, and every
 * decoded frame is released to the surface immediately.
 *
 * Threading contract:
 *
 *  - [controlExecutor]: single thread, runs `configure`, codec (re)builds and
 *    every `feedNalUnit`, so packets are handled strictly in arrival order. Its
 *    work never waits for the codec, so it cannot stall frame submission.
 *  - [callbackThread]: MediaCodec callbacks (free input buffer, decoded output,
 *    errors, frame rendered).
 *  - [lock] guards the codec reference, the free input-buffer indices and the
 *    pending-NAL queue. Buffer indices belong to one codec instance, so both are
 *    reset whenever the codec is replaced, and callbacks from a superseded codec
 *    are ignored.
 *
 * Overload policy: the pending queue is bounded. When it overflows, delta frames
 * that can no longer be shown in order are dropped (they cannot be decoded
 * without their predecessors anyway), decoding waits for the next keyframe, and
 * one is requested through [Listener.onKeyframeNeeded].
 */
class VideoDecoder(surface: Surface) {
    /** Receives decoder events. Called from decoder threads; must not block. */
    interface Listener {
        /** A frame's output buffer became available; [ptsUs] is the host capture timestamp. */
        fun onFrameDecoded(ptsUs: Long, nowUs: Long)

        /** A frame was put on screen; [nowUs] is on the phone monotonic clock. */
        fun onFramePresented(ptsUs: Long, nowUs: Long)

        /** A frame was discarded without being shown. */
        fun onFrameDropped()

        /** The decoder needs a fresh IDR frame (error, drop or reconfigure). */
        fun onKeyframeNeeded(reason: String)
    }

    companion object {
        private const val TAG = "DisplaySwarmDecoder"
        private const val MIME_H264 = "video/avc"
        private const val MIME_HEVC = "video/hevc"

        /**
         * The codec of an Annex-B CONFIG packet, from its first NAL header: H.264
         * starts with an SPS (type 7 in the low 5 bits), HEVC with a VPS (type 32
         * in bits 1..6). The two cannot be confused: an HEVC first byte is 0x40,
         * 0x42 or 0x44, whose low 5 bits are 0, 2 and 4.
         */
        internal fun mimeOfConfig(data: ByteArray): String? {
            var i = 0
            while (i < data.size - 3 && !(data[i] == 0.toByte() && data[i + 1] == 0.toByte() && data[i + 2] == 1.toByte())) i++
            val h = i + 3
            if (h >= data.size) return null
            val b = data[h].toInt() and 0xFF
            return when {
                (b and 0x1F) == 7 -> MIME_H264
                ((b shr 1) and 0x3F) == 32 -> MIME_HEVC
                else -> null
            }
        }

        /** NAL units allowed to wait for an input buffer before old delta frames are shed. */
        private const val MAX_PENDING = 8
    }

    /** Set by the transport; null detaches. */
    @Volatile
    var listener: Listener? = null

    private class Pending(
        val data: ByteArray,
        val ptsUs: Long,
        val isKey: Boolean,
        val isConfig: Boolean
    )

    private val configured = AtomicBoolean(false)

    /**
     * The Surface the codec renders into.
     *
     * Mutable and replaceable on purpose. A `SurfaceView`'s surface is destroyed
     * and recreated across pause/resume and configuration changes, and a
     * `MediaCodec` configured against a released surface fails permanently with
     * "The surface has been released". Holding the surface in a `val` bound at
     * construction made that unrecoverable, so the decoder is re-pointed at the
     * new surface instead.
     */
    @Volatile
    private var surface: Surface = surface

    /**
     * Set when the last configure attempt failed, so a later call knows to retry
     * even if the requested resolution has not changed. Without this, a codec that
     * failed once was never rebuilt and the screen stayed black forever.
     */
    @Volatile
    private var configureFailed = false

    /** Rate-limits the "decoder not ready" recovery attempt and its logging. */
    private var lastRecoverAttemptMs = 0L
    private var loggedNotReady = false

    private val controlExecutor = Executors.newSingleThreadExecutor { r ->
        Thread(r, "displayswarm-codec-control").apply { priority = Thread.NORM_PRIORITY }
    }

    private val callbackThread = HandlerThread("displayswarm-codec-callback", android.os.Process.THREAD_PRIORITY_URGENT_DISPLAY)
        .apply { start() }
    private val callbackHandler = Handler(callbackThread.looper)

    // ---- State guarded by [lock] -----------------------------------------
    private val lock = Any()
    private var codec: MediaCodec? = null
    private val freeInputs = ArrayDeque<Int>()
    private val pending = ArrayDeque<Pending>()

    /** True from a (re)build or an overflow until the next keyframe is queued. */
    private var waitingForKey = false
    // ----------------------------------------------------------------------

    /** The codec in use; H.264 until a CONFIG packet says otherwise. */
    @Volatile
    private var mime = MIME_H264

    private var currentWidth = 0
    private var currentHeight = 0

    /**
     * Dimensions requested by [configure], recorded synchronously so that a CONFIG
     * packet arriving before the control thread has built the codec still knows
     * what resolution to build it at.
     */
    @Volatile
    private var requestedWidth = 0

    @Volatile
    private var requestedHeight = 0

    private var renderedCount = 0L
    private var cachedCsd0: ByteArray? = null
    private var cachedCsd1: ByteArray? = null

    fun isConfigured(): Boolean = configured.get()

    /**
     * Points the decoder at a new Surface, discarding the existing codec.
     *
     * Must be called whenever the `SurfaceView` surface is (re)created. The old
     * codec is bound to the destroyed surface and every later configure would
     * fail with "The surface has been released".
     */
    fun setSurface(newSurface: Surface) {
        val changed = surface !== newSurface
        surface = newSurface
        if (changed) {
            // Force a rebuild on the next configure even if nothing else changed.
            configureFailed = true
            cachedCsd0 = null
            cachedCsd1 = null
        }
    }

    /**
     * Creates or updates the decoder. Safe to call from any thread and repeatedly;
     * returns immediately and applies the change on the control thread.
     */
    fun configure(width: Int, height: Int, csd0: ByteArray? = null, csd1: ByteArray? = null) {
        if (width <= 0 || height <= 0) {
            Log.w(TAG, "Cannot configure MediaCodec with invalid dimensions: ${width}x${height}")
            return
        }
        if (!surface.isValid) {
            Log.w(TAG, "Cannot configure MediaCodec: Surface is not valid yet")
            // Do NOT mark this a permanent failure: the surface may become valid
            // moments later, and the next configure() must be free to try again.
            return
        }

        val alignedW = width and 0x7FFFFFFE // round down to even
        val alignedH = height and 0x7FFFFFFE
        requestedWidth = alignedW
        requestedHeight = alignedH

        try {
            controlExecutor.execute {
                applyConfig(alignedW, alignedH, csd0, csd1)
            }
        } catch (e: java.util.concurrent.RejectedExecutionException) {
            Log.w(TAG, "Decoder released, ignoring configure")
        }
    }

    /** Runs on [controlExecutor]. */
    private fun applyConfig(width: Int, height: Int, csd0: ByteArray?, csd1: ByteArray?) {
        if (csd0 != null) cachedCsd0 = csd0
        if (csd1 != null) cachedCsd1 = csd1

        // Already correct for this resolution and no new CSD to apply. `configureFailed`
        // is part of this test so a previously failed codec is rebuilt.
        if (configured.get() && !configureFailed &&
            currentWidth == width && currentHeight == height && csd0 == null
        ) {
            return
        }

        try {
            startCodec(width, height)
            configureFailed = false
        } catch (e: Exception) {
            Log.e(TAG, "Failed to initialize MediaCodec: ${e.message}", e)
            configured.set(false)
            configureFailed = true
        }
    }

    /** Must be called on [controlExecutor]. Creates the codec in async mode. */
    private fun startCodec(width: Int, height: Int) {
        releaseInternal()

        val format = MediaFormat.createVideoFormat(mime, width, height).apply {
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_PRIORITY, 0) // Realtime
            cachedCsd0?.let { setByteBuffer("csd-0", ByteBuffer.wrap(it)) }
            cachedCsd1?.let { setByteBuffer("csd-1", ByteBuffer.wrap(it)) }
        }

        val mc = MediaCodec.createDecoderByType(mime)
        try {
            // The callback must be set before configure() to select async mode.
            mc.setCallback(CodecCallback(), callbackHandler)
            mc.setOnFrameRenderedListener({ _, ptsUs, systemNano ->
                listener?.onFramePresented(ptsUs, systemNano / 1000)
            }, callbackHandler)
            mc.configure(format, surface, null, 0)

            // Publish the codec before start(): input-buffer callbacks begin as
            // soon as it starts and are ignored for any codec that is not current.
            synchronized(lock) {
                codec = mc
                freeInputs.clear()
                pending.clear()
                waitingForKey = true // an IDR must come first after a (re)build
            }
            mc.start()
        } catch (e: Exception) {
            synchronized(lock) { if (codec === mc) codec = null }
            try {
                mc.release()
            } catch (_: Exception) {
            }
            throw e
        }

        currentWidth = width
        currentHeight = height
        renderedCount = 0
        configured.set(true)

        Log.i(
            TAG,
            "MediaCodec configured (async): ${width}x${height}, " +
                "hasCsd0=${cachedCsd0 != null}, hasCsd1=${cachedCsd1 != null}"
        )

        listener?.onKeyframeNeeded("decoder configured")
    }

    private inner class CodecCallback : MediaCodec.Callback() {
        override fun onInputBufferAvailable(mc: MediaCodec, index: Int) {
            synchronized(lock) {
                if (mc !== codec) return
                freeInputs.addLast(index)
                pump()
            }
        }

        override fun onOutputBufferAvailable(mc: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
            if (mc !== codec) return
            listener?.onFrameDecoded(info.presentationTimeUs, PhoneClock.nowUs())
            try {
                mc.releaseOutputBuffer(index, System.nanoTime()) // due now: show at the next vsync
            } catch (e: IllegalStateException) {
                return // codec was stopped or replaced
            }
            renderedCount++
            if (renderedCount == 1L || renderedCount % 120L == 0L) {
                Log.i(
                    TAG,
                    ">>> Rendered frame $renderedCount to SurfaceView " +
                        "(size=${info.size}, ts=${info.presentationTimeUs}) <<<"
                )
            }
        }

        override fun onOutputFormatChanged(mc: MediaCodec, format: MediaFormat) {
            Log.i(TAG, "Decoder output format changed: $format")
        }

        override fun onError(mc: MediaCodec, e: MediaCodec.CodecException) {
            if (mc !== codec) return
            Log.e(TAG, "MediaCodec error: ${e.diagnosticInfo} (recoverable=${e.isRecoverable})", e)
            recoverFromError()
        }
    }

    /** Rebuilds the codec after a failure and asks the host for a fresh keyframe. */
    private fun recoverFromError() {
        configureFailed = true
        listener?.onKeyframeNeeded("decoder error")
        try {
            controlExecutor.execute {
                if (requestedWidth > 0 && requestedHeight > 0) {
                    applyConfig(requestedWidth, requestedHeight, null, null)
                }
            }
        } catch (e: java.util.concurrent.RejectedExecutionException) {
            // Released while failing; nothing to recover.
        }
    }

    /**
     * Moves queued NAL units into free input buffers. Must hold [lock].
     */
    private fun pump() {
        val mc = codec ?: return
        while (pending.isNotEmpty() && freeInputs.isNotEmpty()) {
            val p = pending.removeFirst()
            val index = freeInputs.removeFirst()
            try {
                val buf = mc.getInputBuffer(index)
                if (buf == null || buf.capacity() < p.data.size) {
                    // Cannot hold this unit. Keep the buffer for the next one.
                    freeInputs.addFirst(index)
                    Log.w(TAG, "Input buffer too small for ${p.data.size} bytes; dropping")
                    if (!p.isConfig) dropFrame(needKeyframe = true)
                    continue
                }
                buf.clear()
                buf.put(p.data)
                mc.queueInputBuffer(
                    index, 0, p.data.size, p.ptsUs,
                    if (p.isConfig) MediaCodec.BUFFER_FLAG_CODEC_CONFIG else 0
                )
            } catch (e: IllegalStateException) {
                Log.w(TAG, "Queueing input failed: ${e.message}")
                return // codec is stopping or in error; the error callback recovers
            }
        }
    }

    /** Counts one lost frame. Must hold [lock]. */
    private fun dropFrame(needKeyframe: Boolean) {
        listener?.onFrameDropped()
        if (needKeyframe) {
            waitingForKey = true
            listener?.onKeyframeNeeded("frame dropped")
        }
    }

    /**
     * Called when [pending] outgrew [MAX_PENDING]. Discards what cannot be shown
     * in order: everything before the newest queued keyframe, or, if there is
     * none, every queued delta and then waits for the next keyframe. Must hold [lock].
     */
    private fun shedPending() {
        val lastKey = pending.indexOfLast { it.isKey }
        val keep = ArrayDeque<Pending>()
        pending.forEachIndexed { i, p ->
            when {
                p.isConfig || i >= lastKey && lastKey >= 0 -> keep.addLast(p)
                else -> listener?.onFrameDropped()
            }
        }
        pending.clear()
        pending.addAll(keep)
        if (lastKey < 0 || pending.count { !it.isConfig } > MAX_PENDING) {
            // Nothing safe to resume from; start over at the next keyframe.
            pending.removeAll { !it.isConfig }
            waitingForKey = true
        }
        listener?.onKeyframeNeeded("decoder queue overflow")
    }

    /** Splits an Annex-B byte buffer into individual NAL units. */
    private fun splitAnnexBNals(data: ByteArray): List<ByteArray> {
        val nals = ArrayList<ByteArray>()
        val offsets = ArrayList<Int>()
        var i = 0
        val len = data.size
        while (i <= len - 3) {
            if (data[i] == 0.toByte() && data[i + 1] == 0.toByte()) {
                if (data[i + 2] == 1.toByte()) {
                    offsets.add(i)
                    i += 3
                    continue
                } else if (i <= len - 4 && data[i + 2] == 0.toByte() && data[i + 3] == 1.toByte()) {
                    offsets.add(i)
                    i += 4
                    continue
                }
            }
            i++
        }

        if (offsets.isEmpty()) {
            nals.add(data)
            return nals
        }

        for (idx in 0 until offsets.size) {
            val start = offsets[idx]
            val end = if (idx + 1 < offsets.size) offsets[idx + 1] else len
            nals.add(data.copyOfRange(start, end))
        }
        return nals
    }

    /**
     * Feeds an Annex-B packet into MediaCodec. Safe to call from any thread; the
     * work is marshalled onto [controlExecutor] and never blocks.
     */
    fun feedNalUnit(
        nalData: ByteArray,
        presentationTimeUs: Long,
        isConfig: Boolean = false,
        isKey: Boolean = false
    ) {
        try {
            controlExecutor.execute {
                try {
                    if (isConfig) feedConfig(nalData, presentationTimeUs)
                    else feedFrame(nalData, presentationTimeUs, isKey)
                } catch (e: Exception) {
                    Log.e(TAG, "Error feeding NAL unit: ${e.message}", e)
                }
            }
        } catch (e: java.util.concurrent.RejectedExecutionException) {
            Log.w(TAG, "Decoder released, ignoring NAL unit")
        }
    }

    private fun feedConfig(nalData: ByteArray, presentationTimeUs: Long) {
        if (mimeOfConfig(nalData) == MIME_HEVC) {
            feedHevcConfig(nalData, presentationTimeUs)
            return
        }
        if (mime != MIME_H264) {
            // Back to H.264 (the host changed codec): the HEVC codec is useless now.
            mime = MIME_H264
            cachedCsd0 = null
            cachedCsd1 = null
            configured.set(false)
        }
        val nals = splitAnnexBNals(nalData)
        var foundSps: ByteArray? = null
        var foundPps: ByteArray? = null

        for (nal in nals) {
            val headerOffset = when {
                nal.size >= 4 && nal[0] == 0.toByte() && nal[1] == 0.toByte() &&
                    nal[2] == 0.toByte() && nal[3] == 1.toByte() -> 4

                nal.size >= 3 && nal[0] == 0.toByte() && nal[1] == 0.toByte() &&
                    nal[2] == 1.toByte() -> 3

                else -> 0
            }

            if (headerOffset < nal.size) {
                val nalType = (nal[headerOffset].toInt() and 0x1F)
                if (nalType == 7) foundSps = nal
                if (nalType == 8) foundPps = nal
            }
        }

        if (foundSps == null || foundPps == null) {
            Log.w(TAG, "CONFIG packet did not contain both SPS and PPS")
            return
        }

        Log.i(TAG, "Extracted SPS (${foundSps.size} bytes) and PPS (${foundPps.size} bytes)")

        // Reconfigure only when the codec isn't ready or the CSD actually changed.
        if (!configured.get() || cachedCsd0 == null || !cachedCsd0.contentEquals(foundSps)) {
            val w = if (requestedWidth > 0) requestedWidth else currentWidth
            val h = if (requestedHeight > 0) requestedHeight else currentHeight
            if (w <= 0 || h <= 0) {
                Log.w(TAG, "Cannot apply CSD: surface dimensions unknown yet")
                return
            }
            cachedCsd0 = foundSps
            cachedCsd1 = foundPps
            startCodec(w, h)
        } else {
            synchronized(lock) {
                pending.addLast(Pending(nalData, presentationTimeUs, isKey = false, isConfig = true))
                pump()
            }
        }
    }

    /** HEVC: VPS, SPS and PPS travel together as the one codec-specific buffer. */
    private fun feedHevcConfig(nalData: ByteArray, presentationTimeUs: Long) {
        val types = splitAnnexBNals(nalData).mapNotNull { nal ->
            val off = if (nal.size > 4 && nal[2] == 0.toByte()) 4 else 3
            if (off < nal.size) (nal[off].toInt() shr 1) and 0x3F else null
        }
        if (!types.containsAll(listOf(32, 33, 34))) {
            Log.w(TAG, "HEVC CONFIG packet without VPS, SPS and PPS")
            return
        }
        if (mime != MIME_HEVC || !configured.get() || cachedCsd0 == null || !cachedCsd0.contentEquals(nalData)) {
            val w = if (requestedWidth > 0) requestedWidth else currentWidth
            val h = if (requestedHeight > 0) requestedHeight else currentHeight
            if (w <= 0 || h <= 0) {
                Log.w(TAG, "Cannot apply CSD: surface dimensions unknown yet")
                return
            }
            mime = MIME_HEVC
            cachedCsd0 = nalData
            cachedCsd1 = null
            startCodec(w, h)
        } else {
            synchronized(lock) {
                pending.addLast(Pending(nalData, presentationTimeUs, isKey = false, isConfig = true))
                pump()
            }
        }
    }

    private fun feedFrame(nalData: ByteArray, presentationTimeUs: Long, isKey: Boolean) {
        if (codec == null || !configured.get()) {
            // Self-heal: the codec can be missing because the surface was
            // recreated or an earlier configure failed. Retry at most once every
            // second so this cannot spin, and log at most once so a 60fps stream
            // cannot flood the log with the same message.
            listener?.onFrameDropped()
            if (System.currentTimeMillis() - lastRecoverAttemptMs > 1000) {
                lastRecoverAttemptMs = System.currentTimeMillis()
                if (!loggedNotReady) {
                    loggedNotReady = true
                    Log.w(TAG, "Decoder not ready; will retry. surfaceValid=${surface.isValid}")
                }
                if (requestedWidth > 0 && requestedHeight > 0) {
                    applyConfig(requestedWidth, requestedHeight, null, null)
                }
            }
            return
        }
        if (loggedNotReady) {
            loggedNotReady = false
            Log.i(TAG, "Decoder ready again (configured=${configured.get()})")
        }

        var dropped = false
        synchronized(lock) {
            if (waitingForKey && !isKey) {
                // A delta frame cannot be decoded without its reference.
                listener?.onFrameDropped()
                dropped = true
            } else {
                if (isKey) waitingForKey = false
                pending.addLast(Pending(nalData, presentationTimeUs, isKey, isConfig = false))
                if (pending.size > MAX_PENDING) shedPending()
                pump()
            }
        }
        // Still waiting: the request made when the wait began may have been lost
        // (the session rate-limits repeats).
        if (dropped) listener?.onKeyframeNeeded("waiting for keyframe")
    }

    /** Must be called on [controlExecutor]. */
    private fun releaseInternal() {
        val mc: MediaCodec?
        synchronized(lock) {
            mc = codec
            codec = null
            freeInputs.clear()
            pending.clear()
        }
        configured.set(false)
        if (mc != null) {
            try {
                mc.stop()
                mc.release()
            } catch (e: Exception) {
                Log.w(TAG, "Error releasing codec: ${e.message}")
            }
        }
    }

    fun release() {
        try {
            controlExecutor.execute {
                releaseInternal()
                callbackThread.quitSafely()
            }
        } catch (e: java.util.concurrent.RejectedExecutionException) {
            // Already released
        }
        controlExecutor.shutdown()
    }
}
