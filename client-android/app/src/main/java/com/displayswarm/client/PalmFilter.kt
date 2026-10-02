package com.displayswarm.client

/**
 * Palm rejection: while the pen is in range (hovering or touching), and for
 * [graceMs] after it leaves, finger touches are not sent. A hand resting on the
 * glass while drawing would otherwise click and drag on the host.
 *
 * - Fingers the host already has down when the pen arrives get one CANCEL, so
 *   the host releases them instead of leaving a contact stuck.
 * - A finger that landed while the pen was active stays muted until it lifts,
 *   even after the grace period, so a palm does not wake up mid-stroke.
 *
 * Pure state, no Android types: [InputManager] feeds it pen activity and each
 * finger Touch message, and sends whatever [filterTouch] returns.
 */
class PalmFilter(private val graceMs: Long = 500) {
    var enabled = true

    private var penInRange = false
    private var penLeftAtMs = Long.MIN_VALUE / 2

    /** Finger ids muted until they lift. */
    private val muted = HashSet<Int>()

    /** Finger ids the host currently has down (sent DOWN, no UP yet). */
    private val hostDown = HashSet<Int>()

    /** The pen entered range (`inRange`) or left it (lift out of hover, exit, cancel). */
    fun onPen(inRange: Boolean, nowMs: Long) {
        if (penInRange && !inRange) penLeftAtMs = nowMs
        penInRange = inRange
    }

    fun penActive(nowMs: Long): Boolean = penInRange || nowMs - penLeftAtMs < graceMs

    /** The Touch messages to send in place of [msg] (possibly none). */
    fun filterTouch(msg: Wire.Message.Touch, nowMs: Long): List<Wire.Message.Touch> {
        if (!enabled) return listOf(track(msg))
        val out = ArrayList<Wire.Message.Touch>(2)
        if (penActive(nowMs)) {
            if (hostDown.isNotEmpty()) {
                val stuck = msg.points.filter { it.id in hostDown }
                if (stuck.isNotEmpty()) {
                    out.add(Wire.Message.Touch(Wire.ACTION_CANCEL, stuck[0].id, stuck))
                }
                hostDown.clear()
            }
            msg.points.forEach { muted.add(it.id) }
            if (msg.action == Wire.ACTION_UP) muted.remove(msg.actionId)
            if (msg.action == Wire.ACTION_CANCEL) muted.clear()
            return out
        }

        val points = msg.points.filter { it.id !in muted }
        val actionMuted = msg.actionId in muted
        when (msg.action) {
            Wire.ACTION_UP -> muted.remove(msg.actionId)
            Wire.ACTION_CANCEL -> muted.clear()
        }
        if (points.isEmpty()) return out
        // A down or up of a muted finger means nothing to the host; the others
        // just moved.
        val sent = if (actionMuted && (msg.action == Wire.ACTION_DOWN || msg.action == Wire.ACTION_UP)) {
            Wire.Message.Touch(Wire.ACTION_MOVE, points[0].id, points)
        } else {
            Wire.Message.Touch(msg.action, msg.actionId, points)
        }
        out.add(track(sent))
        return out
    }

    private fun track(msg: Wire.Message.Touch): Wire.Message.Touch {
        when (msg.action) {
            Wire.ACTION_DOWN -> hostDown.add(msg.actionId)
            Wire.ACTION_UP -> hostDown.remove(msg.actionId)
            Wire.ACTION_CANCEL -> hostDown.clear()
            // A move can carry a finger whose down the host saw earlier.
            Wire.ACTION_MOVE -> {}
        }
        return msg
    }
}
