package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ClockSyncTest {
    @Test
    fun symmetricPathRecoversOffsetAndRtt() {
        // Host clock is 5_000_000 us ahead; 1 ms each way; 0.2 ms host turnaround.
        val offset = 5_000_000L
        val t1 = 1_000_000L
        val t2 = t1 + 1_000 + offset
        val t3 = t2 + 200
        val t4 = t1 + 1_000 + 200 + 1_000
        val c = ClockSync()
        assertFalse(c.hasSync)
        c.addSample(t1, t2, t3, t4)
        assertTrue(c.hasSync)
        assertEquals(offset, c.offsetUs)
        assertEquals(2_000L, c.rttUs)
        assertEquals(1_000_000L, c.hostToPhoneUs(6_000_000L))
    }

    @Test
    fun keepsMinimumRttSampleOfWindow() {
        val c = ClockSync()
        // Slow sample with a skewed (asymmetric) offset, then a fast clean one.
        c.addSample(0, 10_000 + 100, 10_100, 20_000) // rtt 19_900
        c.addSample(100_000, 100_500, 100_500, 101_000) // rtt 1_000, offset -0? (500-0+ -500)/2
        assertEquals(1_000L, c.rttUs)
        // Later slow samples do not displace the best one while it is in the window.
        c.addSample(200_000, 250_000, 250_000, 300_000)
        assertEquals(1_000L, c.rttUs)
    }

    @Test
    fun oldBestSampleAgesOut() {
        val c = ClockSync()
        c.addSample(0, 500, 500, 1_000) // rtt 1000
        repeat(ClockSync.WINDOW) { i -> c.addSample(i * 10_000L, i * 10_000L + 1_500, i * 10_000L + 1_500, i * 10_000L + 3_000) }
        assertEquals(3_000L, c.rttUs)
    }

    @Test
    fun negativeRttIsIgnored() {
        val c = ClockSync()
        c.addSample(1_000, 0, 5_000, 2_000) // t4-t1 < t3-t2
        assertFalse(c.hasSync)
    }

    @Test
    fun hudStringFormat() {
        val m = SessionMetrics(60, 12, 25, 38, 18.0, 2.0, true)
        assertEquals("lat 38 ms (rx 12 / dec 25 / present 38) · 60 fps · 18 Mbps · rtt 2 ms", m.toHudString())
    }
}
