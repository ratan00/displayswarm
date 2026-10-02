package com.displayswarm.client

/** The one phone-side clock used for every measurement: monotonic microseconds. */
object PhoneClock {
    fun nowUs(): Long = System.nanoTime() / 1000
}

/**
 * NTP-style estimate of `offset = host - phone` from PING/PONG exchanges.
 *
 * The phone sends PING(t1), the host answers PONG(t1, t2 = host recv, t3 = host
 * reply) and the phone receives it at t4. Then
 * `offset = ((t2 - t1) + (t3 - t4)) / 2` and `rtt = (t4 - t1) - (t3 - t2)`.
 * The sample with the smallest rtt over the last [WINDOW] is used, because a
 * queued or delayed exchange inflates the rtt and skews the offset.
 */
class ClockSync {
    private class Sample(val offsetUs: Long, val rttUs: Long)

    private val samples = ArrayDeque<Sample>()

    @Volatile
    private var best: Sample? = null

    /** True once at least one PONG has been processed. */
    val hasSync: Boolean get() = best != null

    /** Estimated `host - phone` in microseconds, 0 before the first sample. */
    val offsetUs: Long get() = best?.offsetUs ?: 0L

    /** Round-trip time of the chosen sample in microseconds, 0 before the first sample. */
    val rttUs: Long get() = best?.rttUs ?: 0L

    @Synchronized
    fun addSample(t1: Long, t2: Long, t3: Long, t4: Long) {
        val rtt = (t4 - t1) - (t3 - t2)
        if (rtt < 0) return // clocks misbehaving or a mismatched reply
        val offset = ((t2 - t1) + (t3 - t4)) / 2
        samples.addLast(Sample(offset, rtt))
        while (samples.size > WINDOW) samples.removeFirst()
        best = samples.minBy { it.rttUs }
    }

    /** Converts a host-clock timestamp to the phone clock. */
    fun hostToPhoneUs(hostUs: Long): Long = hostUs - offsetUs

    companion object {
        const val WINDOW = 8
    }
}
