package com.displayswarm.client

import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * The pure parts of audio playback (no Android classes, so they are unit
 * tested): packet sequencing, the jitter buffer policy and the Opus codec
 * config MediaCodec needs.
 */

/** What to do with a packet, judged by its sequence number. */
sealed class SeqVerdict {
    /** The next packet expected (or the first one). */
    object InOrder : SeqVerdict()

    /** [missing] packets were skipped before this one. */
    class Gap(val missing: Int) : SeqVerdict()

    /** Older than what was already played, or a duplicate: drop it. */
    object Stale : SeqVerdict()
}

class SeqTracker {
    private var next = -1L

    fun classify(seq: Long): SeqVerdict {
        val s = seq and 0xFFFF_FFFFL
        if (next < 0) {
            next = (s + 1) and 0xFFFF_FFFFL
            return SeqVerdict.InOrder
        }
        val diff = (s - next) and 0xFFFF_FFFFL
        return when {
            diff >= 0x8000_0000L -> SeqVerdict.Stale
            diff == 0L -> { next = (s + 1) and 0xFFFF_FFFFL; SeqVerdict.InOrder }
            diff > MAX_GAP -> {
                // The host restarted its stream: resync instead of inserting seconds of silence.
                next = (s + 1) and 0xFFFF_FFFFL
                SeqVerdict.InOrder
            }
            else -> { next = (s + 1) and 0xFFFF_FFFFL; SeqVerdict.Gap(diff.toInt()) }
        }
    }

    companion object {
        const val MAX_GAP = 20L
    }
}

/**
 * Decides when playback may run. Decoded chunks are queued by their duration;
 * the player asks [shouldPlay] before each write:
 * - it starts once [targetMs] is buffered (prefill), so small arrival jitter
 *   does not cause dropouts;
 * - an underrun ends playback and re-fills, and raises the target (up to
 *   [MAX_TARGET_MS]) when it looks like jitter rather than the host having
 *   simply gone quiet; a long stretch without underruns lowers it again;
 * - if the queue grows beyond [maxMs] (the phone plays slower than the host
 *   produces), [excessMs] tells how much to drop to get back to the target.
 */
class JitterController(
    initialTargetMs: Int = 20,
    private val minTargetMs: Int = 10,
    private val maxMs: Int = 100,
) {
    var targetMs: Int = initialTargetMs.coerceIn(minTargetMs, MAX_TARGET_MS)
        private set
    private var playing = false
    private var calmMs = 0L

    /** True while a stream is in progress (prefill done, no underrun since). */
    val isPlaying get() = playing

    /** Whether to write to the track, given [bufferedMs] queued. */
    fun shouldPlay(bufferedMs: Int): Boolean {
        if (!playing && bufferedMs >= targetMs) playing = true
        return playing
    }

    /** The queue ran dry. [silenceMs] is how long the host had been sending nothing. */
    fun onUnderrun(silenceMs: Long) {
        playing = false
        calmMs = 0
        if (silenceMs < IDLE_MS) targetMs = (targetMs + STEP_MS).coerceAtMost(MAX_TARGET_MS)
    }

    /** [ms] of audio was played without trouble. */
    fun onPlayed(ms: Int) {
        calmMs += ms
        if (calmMs >= CALM_BEFORE_SHRINK_MS && targetMs > minTargetMs) {
            targetMs = (targetMs - STEP_MS).coerceAtLeast(minTargetMs)
            calmMs = 0
        }
    }

    /** Milliseconds of audio to discard from the front of the queue, 0 if none. */
    fun excessMs(bufferedMs: Int): Int = if (bufferedMs > maxMs) bufferedMs - targetMs else 0

    companion object {
        const val MAX_TARGET_MS = 80
        const val STEP_MS = 10
        /** No packets for this long: the host is idle, not the network jittery. */
        const val IDLE_MS = 300L
        const val CALM_BEFORE_SHRINK_MS = 5_000L
    }
}

/**
 * Multi-phone sync arithmetic. The host tells every phone one playout delay D;
 * the sample with host pts T must be audible at host time T + D. The player
 * compares when the next chunk *would* be audible with when it should be and
 * corrects by inserting silence (too early) or dropping samples (too late),
 * a bounded amount per chunk so it never glitches loudly.
 */
data class SyncAdjust(val silenceSamples: Int, val dropSamples: Int) {
    val isNone get() = silenceSamples == 0 && dropSamples == 0
}

object SyncAlign {
    /** Errors inside this are left alone. */
    const val TOLERANCE_US = 5_000L

    /** Largest correction applied to one chunk (the slew limit). */
    const val MAX_STEP_US = 10_000L

    /**
     * @param audibleAtUs when the chunk's first sample would be heard if written now (phone clock)
     * @param targetUs when it must be heard (phone clock)
     * @param chunkSamples samples per channel in the chunk
     */
    fun adjust(audibleAtUs: Long, targetUs: Long, chunkSamples: Int, sampleRate: Int = 48_000): SyncAdjust {
        val error = audibleAtUs - targetUs
        if (kotlin.math.abs(error) <= TOLERANCE_US) return SyncAdjust(0, 0)
        val step = minOf(kotlin.math.abs(error), MAX_STEP_US)
        val samples = (step * sampleRate / 1_000_000L).toInt()
        return if (error < 0) SyncAdjust(samples, 0) else SyncAdjust(0, minOf(samples, chunkSamples))
    }
}

/**
 * The phone's latency report: recent one-way transit times (host pts to
 * arrival), summarised by a high percentile so the shared delay is not set
 * below what the phone can really hold (too low would make it drop audio
 * forever; too high only adds a little delay).
 */
class TransitWindow(private val size: Int = 200) {
    private val values = ArrayDeque<Long>()

    val count get() = values.size

    @Synchronized
    fun add(us: Long) {
        values.addLast(us.coerceAtLeast(0))
        while (values.size > size) values.removeFirst()
    }

    /** The [percent]th percentile, or null with too few samples. */
    @Synchronized
    fun percentile(percent: Int = 90, minCount: Int = 20): Long? {
        if (values.size < minCount) return null
        val sorted = values.sorted()
        return sorted[((sorted.size - 1) * percent / 100).coerceIn(0, sorted.size - 1)]
    }

    @Synchronized
    fun clear() = values.clear()
}

/** The three codec-specific buffers MediaCodec's Opus decoder requires. */
object OpusCsd {
    /** "OpusHead" identification header (RFC 7845), pre-skip 0. */
    fun head(channels: Int, sampleRate: Int = 48_000): ByteArray {
        val b = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN)
        b.put("OpusHead".toByteArray(Charsets.US_ASCII))
        b.put(1) // version
        b.put(channels.toByte())
        b.putShort(0) // pre-skip: the host's stream has no encoder delay to trim
        b.putInt(sampleRate)
        b.putShort(0) // output gain
        b.put(0) // channel mapping family
        return b.array()
    }

    /** Pre-skip / seek pre-roll as nanoseconds in a little-endian long. */
    fun nanos(ns: Long): ByteArray = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(ns).array()
}

object PcmUtil {
    /** Little-endian 16-bit bytes to samples. */
    fun toShorts(bytes: ByteArray, len: Int = bytes.size): ShortArray {
        val out = ShortArray(len / 2)
        ByteBuffer.wrap(bytes, 0, len).order(ByteOrder.LITTLE_ENDIAN).asShortBuffer().get(out)
        return out
    }

    fun toBytes(s: ShortArray): ByteArray {
        val b = ByteBuffer.allocate(s.size * 2).order(ByteOrder.LITTLE_ENDIAN)
        b.asShortBuffer().put(s)
        return b.array()
    }

    fun durationMs(samplesPerChannel: Int, sampleRate: Int = 48_000): Int = samplesPerChannel * 1000 / sampleRate
}
