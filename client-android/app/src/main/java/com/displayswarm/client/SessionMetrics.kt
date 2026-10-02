package com.displayswarm.client

/**
 * One second of measured stream health, shown in the HUD.
 *
 * The three latencies are averages from the host's capture timestamp, mapped to
 * the phone clock with the ping/pong offset: [rxMs] until the frame arrived,
 * [decMs] until it was decoded, [presentMs] until it was put on screen. They
 * are zero until the clock has synchronised.
 */
data class SessionMetrics(
    val fps: Int,
    val rxMs: Long,
    val decMs: Long,
    val presentMs: Long,
    val mbps: Double,
    val rttMs: Double,
    val synced: Boolean
) {
    /** End-to-end latency: the furthest stage that has been measured. */
    val latencyMs: Long get() = if (presentMs > 0) presentMs else if (decMs > 0) decMs else rxMs

    fun toHudString(): String {
        val rtt = "rtt " + if (synced) String.format(java.util.Locale.ROOT, "%.0f ms", rttMs) else "--"
        val lat = if (synced) {
            "lat $latencyMs ms (rx $rxMs / dec $decMs / present $presentMs)"
        } else {
            "lat -- ms (syncing clock)"
        }
        return "$lat · $fps fps · " + String.format(java.util.Locale.ROOT, "%.0f Mbps", mbps) + " · $rtt"
    }
}
