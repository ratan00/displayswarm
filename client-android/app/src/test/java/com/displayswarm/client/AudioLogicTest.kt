package com.displayswarm.client

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AudioLogicTest {
    private fun gap(v: SeqVerdict) = (v as SeqVerdict.Gap).missing

    @Test
    fun seqInOrderThenGapThenStale() {
        val t = SeqTracker()
        assertTrue(t.classify(10) is SeqVerdict.InOrder)
        assertTrue(t.classify(11) is SeqVerdict.InOrder)
        assertEquals(2, gap(t.classify(14)))
        assertTrue(t.classify(12) is SeqVerdict.Stale)
        assertTrue(t.classify(14) is SeqVerdict.Stale)
        assertTrue(t.classify(15) is SeqVerdict.InOrder)
    }

    @Test
    fun seqWrapsAround() {
        val t = SeqTracker()
        t.classify(0xFFFF_FFFFL)
        assertTrue(t.classify(0) is SeqVerdict.InOrder)
        assertEquals(1, gap(t.classify(2)))
    }

    @Test
    fun seqBigJumpResyncsWithoutFillingSilence() {
        val t = SeqTracker()
        t.classify(5)
        assertTrue(t.classify(5 + 1000) is SeqVerdict.InOrder)
        assertTrue(t.classify(5 + 1001) is SeqVerdict.InOrder)
    }

    @Test
    fun jitterPrefillsBeforePlaying() {
        val c = JitterController(initialTargetMs = 30)
        assertFalse(c.shouldPlay(20))
        assertTrue(c.shouldPlay(30))
        assertTrue(c.shouldPlay(10)) // keeps playing once started
    }

    @Test
    fun underrunRefillsAndRaisesTargetOnlyWhenNotIdle() {
        val c = JitterController(initialTargetMs = 30)
        c.shouldPlay(30)
        c.onUnderrun(silenceMs = 20)
        assertFalse(c.isPlaying)
        assertEquals(40, c.targetMs)
        assertFalse(c.shouldPlay(30))
        assertTrue(c.shouldPlay(40))
        c.onUnderrun(silenceMs = 5_000) // the host just went quiet
        assertEquals(40, c.targetMs)
    }

    @Test
    fun targetIsCappedAndShrinksAfterCalmPlayback() {
        val c = JitterController(initialTargetMs = 30)
        repeat(20) { c.onUnderrun(10) }
        assertEquals(JitterController.MAX_TARGET_MS, c.targetMs)
        c.onPlayed(10_000)
        assertEquals(JitterController.MAX_TARGET_MS - 10, c.targetMs)
        repeat(20) { c.onPlayed(10_000) }
        assertEquals(10, c.targetMs)
    }

    @Test
    fun excessIsDroppedDownToTarget() {
        val c = JitterController(initialTargetMs = 30, maxMs = 100)
        assertEquals(0, c.excessMs(100))
        assertEquals(80, c.excessMs(110))
    }

    @Test
    fun opusHeadLayout() {
        val h = OpusCsd.head(2)
        assertEquals(19, h.size)
        assertEquals("OpusHead", String(h, 0, 8, Charsets.US_ASCII))
        assertEquals(1, h[8].toInt())
        assertEquals(2, h[9].toInt())
        // 48000 little-endian at offset 12
        assertArrayEquals(byteArrayOf(0x80.toByte(), 0xBB.toByte(), 0, 0), h.copyOfRange(12, 16))
    }

    @Test
    fun nanosAreLittleEndian() {
        assertArrayEquals(byteArrayOf(0x80.toByte(), 0x1D, 0x2C, 0x04, 0, 0, 0, 0), OpusCsd.nanos(70_000_000L))
    }

    @Test
    fun pcmRoundTrip() {
        val s = shortArrayOf(0, 1, -1, 32767, -32768, 1234)
        assertArrayEquals(s, PcmUtil.toShorts(PcmUtil.toBytes(s)))
        assertEquals(10, PcmUtil.durationMs(480))
    }

    @Test
    fun syncLeavesSmallErrorsAlone() {
        assertTrue(SyncAlign.adjust(1_000_000, 1_004_000, 480).isNone)
        assertTrue(SyncAlign.adjust(1_004_000, 1_000_000, 480).isNone)
    }

    @Test
    fun syncInsertsSilenceWhenEarlyUpToTheSlewLimit() {
        // 3 ms early past the tolerance: exactly the error (7 ms = 336 samples).
        val small = SyncAlign.adjust(1_000_000, 1_007_000, 480)
        assertEquals(336, small.silenceSamples)
        assertEquals(0, small.dropSamples)
        // 200 ms early: one 10 ms step only.
        assertEquals(480, SyncAlign.adjust(1_000_000, 1_200_000, 480).silenceSamples)
    }

    @Test
    fun syncDropsSamplesWhenLateUpToTheSlewLimitAndTheChunk() {
        val late = SyncAlign.adjust(1_020_000, 1_000_000, 480)
        assertEquals(0, late.silenceSamples)
        assertEquals(480, late.dropSamples)
        assertEquals("never more than the chunk", 240, SyncAlign.adjust(1_020_000, 1_000_000, 240).dropSamples)
        assertEquals(336, SyncAlign.adjust(1_007_000, 1_000_000, 480).dropSamples)
    }

    @Test
    fun syncConvergesToTheTarget() {
        // Start 120 ms early; each 10 ms chunk inserts up to 10 ms of silence.
        var audible = 1_000_000L
        val target = 1_120_000L
        var steps = 0
        while (!SyncAlign.adjust(audible, target, 480).isNone && steps < 100) {
            audible += SyncAlign.adjust(audible, target, 480).silenceSamples * 1_000_000L / 48_000
            steps++
        }
        assertTrue(steps in 10..14)
        assertTrue(Math.abs(audible - target) <= SyncAlign.TOLERANCE_US)
    }

    @Test
    fun transitWindowReportsAHighPercentileOnlyWhenWarm() {
        val w = TransitWindow()
        repeat(10) { w.add(30_000) }
        assertEquals(null, w.percentile())
        repeat(89) { w.add(30_000) }
        w.add(200_000) // 100 samples, one outlier above the 90th percentile
        assertEquals(30_000L, w.percentile(90))
        repeat(20) { w.add(90_000) }
        assertEquals(90_000L, w.percentile(90))
    }
}
