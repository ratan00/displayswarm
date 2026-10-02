package com.displayswarm.client

import kotlin.math.abs
import kotlin.math.atan2
import kotlin.math.exp
import kotlin.math.hypot
import kotlin.math.min

/**
 * Touchpad mode: turns finger touches into pointer gestures for the host.
 *
 * Pure logic with explicit timestamps and no Android types, so it is unit
 * tested. [InputManager] feeds it the fingers of each touch event ([onTouch])
 * and calls [tick] on a ~60 Hz timer while [wantsTick] is set (scroll inertia).
 * Everything it produces goes through [send].
 *
 * Gestures:
 *  - one finger: relative cursor motion ([Wire.Message.Mouse] with `relative`,
 *    deltas in phone pixels, with a speed-dependent gain);
 *  - tap: left click; two-finger tap: right click; three-finger tap:
 *    [onThreeFingerTap] (the app's toolbar);
 *  - tap, then touch and drag: the button is held while the finger moves;
 *  - two fingers moving together: smooth scroll ([Wire.Message.Scroll], natural
 *    direction), which keeps going with [Wire.PHASE_INERTIA] after the fingers
 *    lift; two fingers moving apart or together: [Wire.Message.Pinch];
 *  - three-finger swipe: left/right switches workspace, up opens the overview
 *    (both send key combos, see [Config]).
 *
 * The phone never learns where the host's cursor is, so scroll and pinch carry
 * an estimate: the relative deltas summed and normalised to the view. The host
 * uses the position only as a hint.
 */
class GestureEngine(
    private val send: (Wire.Message) -> Unit,
    var config: Config = Config()
) {
    /** All distances are in pixels; [Config.forDensity] scales the defaults. */
    data class Config(
        val tapSlop: Float = 10f,
        val tapTimeoutMs: Long = 250,
        val multiTapTimeoutMs: Long = 300,
        /** A touch this soon after a tap, and near it, is a tap-and-drag. */
        val dragWindowMs: Long = 300,
        val dragRadius: Float = 80f,
        /** Cursor gain at low speed, and how much it grows with speed (px/ms). */
        val pointerGain: Float = 1.0f,
        val pointerAccel: Float = 0.35f,
        val pointerMaxBoost: Float = 4f,
        /** Finger travel that makes one wheel detent. */
        val pxPerDetent: Float = 30f,
        /** false: content follows the fingers reversed (classic wheel direction). */
        val naturalScroll: Boolean = true,
        /** Inertia: exponential decay time constant and the speed (detents/s) it stops at. */
        val inertiaTauMs: Float = 350f,
        val inertiaMinSpeed: Float = 0.6f,
        val inertiaMaxSpeed: Float = 60f,
        /** Pinch is chosen over scroll when the spread changes by this much more than the centroid moves. */
        val pinchDecidePx: Float = 16f,
        val swipeDistance: Float = 90f,
        val workspaceLeft: KeyCombo = KeyCombos.WORKSPACE_LEFT,
        val workspaceRight: KeyCombo = KeyCombos.WORKSPACE_RIGHT,
        val overview: KeyCombo = KeyCombos.SUPER
    ) {
        companion object {
            fun forDensity(d: Float) = Config(
                tapSlop = 10f * d, dragRadius = 80f * d, pxPerDetent = 30f * d,
                pinchDecidePx = 16f * d, swipeDistance = 90f * d
            )
        }
    }

    data class Ptr(val id: Int, val x: Float, val y: Float)

    enum class Kind { DOWN, MOVE, UP, CANCEL }

    private enum class Phase { IDLE, ONE, DRAG, TWO, THREE, DEAD }

    private enum class TwoMode { UNDECIDED, SCROLL, PINCH, DONE }

    var viewWidth = 1f
    var viewHeight = 1f
    var onThreeFingerTap: (() -> Unit)? = null

    private var phase = Phase.IDLE
    private var startMs = 0L
    private var travel = 0f // largest movement of the gesture's fingers, for tap detection

    // One finger.
    private var lastX = 0f
    private var lastY = 0f
    private var lastMoveMs = 0L
    private var moved = false
    private var dragCandidate = false
    private var lastTapUpMs = Long.MIN_VALUE / 2
    private var lastTapX = 0f
    private var lastTapY = 0f

    // Estimated host cursor, normalised.
    private var cursorX = 0.5f
    private var cursorY = 0.5f

    // Two fingers.
    private var twoMode = TwoMode.UNDECIDED
    private var twoLastCx = 0f
    private var twoLastCy = 0f
    private var twoStartCx = 0f
    private var twoStartCy = 0f
    private var startDist = 1f
    private var startAngle = 0f
    private var lastAngle = 0f
    private var angleAcc = 0f
    private var lastScale = 1f
    private val samples = ArrayDeque<Sample>()

    // Three fingers.
    private var threeStartCx = 0f
    private var threeStartCy = 0f
    private var threeLastCx = 0f
    private var threeLastCy = 0f
    private var threeDone = false

    // Inertia.
    private var inertia = false
    private var inertiaVx = 0f // detents per ms
    private var inertiaVy = 0f
    private var inertiaLastMs = 0L

    private class Sample(val t: Long, val dx: Float, val dy: Float)

    /** True while [tick] has work: keep calling it. */
    val wantsTick: Boolean get() = inertia

    /** The buttons the engine holds down right now (0 or primary, during a drag). */
    private val heldButtons: Int get() = if (phase == Phase.DRAG) Wire.MOUSE_BUTTON_PRIMARY else 0

    fun onTouch(kind: Kind, pointers: List<Ptr>, nowMs: Long) {
        if (kind == Kind.CANCEL) {
            cancel()
            return
        }
        if (kind == Kind.DOWN && inertia) stopInertia() // a touch stops the fling
        when (kind) {
            Kind.DOWN -> down(pointers, nowMs)
            Kind.MOVE -> move(pointers, nowMs)
            Kind.UP -> up(pointers, nowMs)
            Kind.CANCEL -> {}
        }
    }

    /** Abandons whatever is in progress and releases anything held. */
    fun cancel() {
        if (phase == Phase.DRAG) mouse(Wire.ACTION_UP, 0, 0f, 0f)
        if (phase == Phase.TWO) endTwo(0L, allowInertia = false)
        if (inertia) stopInertia()
        phase = Phase.IDLE
        moved = false
        dragCandidate = false
    }

    fun tick(nowMs: Long) {
        if (!inertia) return
        val dt = (nowMs - inertiaLastMs).coerceAtLeast(0L)
        if (dt == 0L) return
        inertiaLastMs = nowMs
        val decay = exp(-dt / config.inertiaTauMs)
        inertiaVx *= decay
        inertiaVy *= decay
        val speedPerSec = hypot(inertiaVx, inertiaVy) * 1000f
        if (speedPerSec < config.inertiaMinSpeed) {
            stopInertia()
            return
        }
        send(Wire.Message.Scroll(Wire.PHASE_INERTIA, cursorX, cursorY, inertiaVx * dt, inertiaVy * dt))
    }

    // ---- Touch handling ---------------------------------------------------

    private fun down(pts: List<Ptr>, now: Long) {
        when (pts.size) {
            1 -> {
                val p = pts[0]
                phase = Phase.ONE
                startMs = now
                travel = 0f
                moved = false
                pendingDx = 0f
                pendingDy = 0f
                lastX = p.x
                lastY = p.y
                lastMoveMs = now
                dragCandidate = now - lastTapUpMs <= config.dragWindowMs &&
                    hypot(p.x - lastTapX, p.y - lastTapY) <= config.dragRadius
            }
            2 -> when (phase) {
                Phase.ONE -> {
                    // The cursor may already have moved a little; a second finger
                    // still makes this a two-finger gesture.
                    phase = Phase.TWO
                    beginTwo(pts, now)
                }
                Phase.DRAG -> {} // a second finger during a drag does nothing
                else -> phase = Phase.DEAD
            }
            3 -> when (phase) {
                Phase.ONE, Phase.TWO -> {
                    if (phase == Phase.TWO) endTwo(now, allowInertia = false)
                    phase = Phase.THREE
                    threeDone = false
                    val (cx, cy) = centroid(pts)
                    threeStartCx = cx
                    threeStartCy = cy
                    threeLastCx = cx
                    threeLastCy = cy
                }
                Phase.DRAG -> {}
                else -> phase = Phase.DEAD
            }
            else -> {
                if (phase == Phase.TWO) endTwo(now, allowInertia = false)
                if (phase != Phase.DRAG) phase = Phase.DEAD
            }
        }
    }

    private fun move(pts: List<Ptr>, now: Long) {
        when (phase) {
            Phase.ONE, Phase.DRAG -> if (pts.isNotEmpty()) oneFingerMove(pts[0], now)
            Phase.TWO -> if (pts.size >= 2) twoFingerMove(pts, now)
            Phase.THREE -> if (pts.size >= 3) {
                val (cx, cy) = centroid(pts)
                threeLastCx = cx
                threeLastCy = cy
            }
            else -> {}
        }
    }

    private fun up(remaining: List<Ptr>, now: Long) {
        when (phase) {
            Phase.ONE -> {
                if (!moved && now - startMs <= config.tapTimeoutMs) {
                    click(Wire.MOUSE_BUTTON_PRIMARY)
                    lastTapUpMs = now
                    lastTapX = lastX
                    lastTapY = lastY
                }
                phase = Phase.IDLE
            }
            Phase.DRAG -> {
                mouse(Wire.ACTION_UP, 0, 0f, 0f)
                phase = Phase.IDLE
                lastTapUpMs = Long.MIN_VALUE / 2
            }
            Phase.TWO -> {
                val isTap = twoMode == TwoMode.UNDECIDED && travel <= config.tapSlop &&
                    now - startMs <= config.multiTapTimeoutMs
                endTwo(now, allowInertia = true)
                if (isTap) click(Wire.MOUSE_BUTTON_SECONDARY)
                phase = if (remaining.isEmpty()) Phase.IDLE else Phase.DEAD
            }
            Phase.THREE -> {
                if (!threeDone) {
                    threeDone = true
                    finishThree(now)
                }
                phase = if (remaining.isEmpty()) Phase.IDLE else Phase.DEAD
            }
            Phase.DEAD -> if (remaining.isEmpty()) phase = Phase.IDLE
            Phase.IDLE -> {}
        }
    }

    // ---- One finger -------------------------------------------------------

    private fun oneFingerMove(p: Ptr, now: Long) {
        val rawDx = p.x - lastX
        val rawDy = p.y - lastY
        if (!moved) {
            // Until the slop is crossed the motion is held back (so a tap does
            // not jiggle the cursor) and released with the first real move.
            travel += hypot(rawDx, rawDy)
            if (travel <= config.tapSlop) {
                lastX = p.x
                lastY = p.y
                pendingDx += rawDx
                pendingDy += rawDy
                lastMoveMs = now
                return
            }
            moved = true
            if (dragCandidate && phase != Phase.DRAG) {
                phase = Phase.DRAG
                mouse(Wire.ACTION_DOWN, Wire.MOUSE_BUTTON_PRIMARY, 0f, 0f)
            }
        }
        val dx = rawDx + pendingDx
        val dy = rawDy + pendingDy
        pendingDx = 0f
        pendingDy = 0f
        val dt = (now - lastMoveMs).coerceAtLeast(1L)
        lastMoveMs = now
        lastX = p.x
        lastY = p.y
        if (dx == 0f && dy == 0f) return
        val speed = hypot(dx, dy) / dt
        val gain = config.pointerGain * (1f + config.pointerAccel * min(speed, config.pointerMaxBoost))
        val mx = dx * gain
        val my = dy * gain
        cursorX = (cursorX + mx / viewWidth).coerceIn(0f, 1f)
        cursorY = (cursorY + my / viewHeight).coerceIn(0f, 1f)
        mouse(if (phase == Phase.DRAG) Wire.ACTION_MOVE else Wire.ACTION_HOVER_MOVE, heldButtons, mx, my)
    }

    private var pendingDx = 0f
    private var pendingDy = 0f

    private fun click(button: Int) {
        mouse(Wire.ACTION_DOWN, button, 0f, 0f)
        mouse(Wire.ACTION_UP, 0, 0f, 0f)
    }

    private fun mouse(action: Int, buttons: Int, dx: Float, dy: Float) {
        send(Wire.Message.Mouse(action, buttons, relative = true, x = dx, y = dy))
    }

    // ---- Two fingers ------------------------------------------------------

    private fun beginTwo(pts: List<Ptr>, now: Long) {
        twoMode = TwoMode.UNDECIDED
        val (cx, cy) = centroid(pts)
        twoStartCx = cx
        twoStartCy = cy
        twoLastCx = cx
        twoLastCy = cy
        startDist = spread(pts).coerceAtLeast(1f)
        startAngle = angle(pts)
        lastAngle = startAngle
        angleAcc = 0f
        lastScale = 1f
        samples.clear()
        // Travel for the tap test restarts here: the first finger's wandering
        // does not count against the two-finger tap.
        travel = 0f
        if (startMs == 0L || now - startMs > config.multiTapTimeoutMs) startMs = now
    }

    private fun twoFingerMove(pts: List<Ptr>, now: Long) {
        val (cx, cy) = centroid(pts)
        val dist = spread(pts)
        val ang = angle(pts)
        travel = maxOf(travel, hypot(cx - twoStartCx, cy - twoStartCy), abs(dist - startDist))
        if (twoMode == TwoMode.UNDECIDED) {
            val centroidMove = hypot(cx - twoStartCx, cy - twoStartCy)
            val spreadChange = abs(dist - startDist)
            when {
                spreadChange > config.pinchDecidePx && spreadChange > centroidMove -> {
                    twoMode = TwoMode.PINCH
                    send(Wire.Message.Pinch(Wire.PHASE_BEGIN, cursorX, cursorY, 1f, 0f))
                }
                centroidMove > config.tapSlop * 1.5f -> {
                    twoMode = TwoMode.SCROLL
                    // The scroll starts from the touch-down centroid.
                    twoLastCx = twoStartCx
                    twoLastCy = twoStartCy
                    scrollStep(cx, cy, now, Wire.PHASE_BEGIN)
                    return
                }
                else -> return
            }
        }
        when (twoMode) {
            TwoMode.SCROLL -> scrollStep(cx, cy, now, Wire.PHASE_UPDATE)
            TwoMode.PINCH -> {
                var d = ang - lastAngle
                if (d > Math.PI) d -= (2 * Math.PI).toFloat()
                if (d < -Math.PI) d += (2 * Math.PI).toFloat()
                angleAcc += d
                lastAngle = ang
                lastScale = dist / startDist
                send(Wire.Message.Pinch(Wire.PHASE_UPDATE, cursorX, cursorY, lastScale, angleAcc))
            }
            else -> {}
        }
    }

    private fun scrollStep(cx: Float, cy: Float, now: Long, phase: Int) {
        val fx = cx - twoLastCx
        val fy = cy - twoLastCy
        twoLastCx = cx
        twoLastCy = cy
        // Android: positive AXIS_VSCROLL scrolls up, i.e. the content moves down.
        // Natural scrolling moves the content with the fingers, so the vertical
        // sign follows the finger; horizontally positive scrolls right, i.e.
        // the content moves left, so it is opposite the finger.
        val sign = if (config.naturalScroll) 1f else -1f
        val dy = sign * fy / config.pxPerDetent
        val dx = -sign * fx / config.pxPerDetent
        samples.addLast(Sample(now, dx, dy))
        while (samples.size > 12) samples.removeFirst()
        send(Wire.Message.Scroll(phase, cursorX, cursorY, dx, dy))
    }

    /** Ends the two-finger gesture in progress (scroll, pinch or undecided). */
    private fun endTwo(now: Long, allowInertia: Boolean) {
        when (twoMode) {
            TwoMode.PINCH -> send(Wire.Message.Pinch(Wire.PHASE_END, cursorX, cursorY, lastScale, angleAcc))
            TwoMode.SCROLL -> {
                val fling = if (allowInertia) flingVelocity(now) else null
                if (fling != null) {
                    // The gesture ends and the fling carries on in its own phase.
                    send(Wire.Message.Scroll(Wire.PHASE_END, cursorX, cursorY, 0f, 0f))
                    inertia = true
                    inertiaVx = fling.first
                    inertiaVy = fling.second
                    inertiaLastMs = now
                } else {
                    send(Wire.Message.Scroll(Wire.PHASE_END, cursorX, cursorY, 0f, 0f))
                }
            }
            else -> {}
        }
        twoMode = TwoMode.DONE
        samples.clear()
    }

    /** Detents per ms over the last ~100 ms, or null when the fingers had stopped. */
    private fun flingVelocity(now: Long): Pair<Float, Float>? {
        val last = samples.lastOrNull() ?: return null
        if (now - last.t > 60) return null // paused before lifting: no fling
        val from = last.t - 100
        val recent = samples.filter { it.t >= from }
        if (recent.size < 2) return null
        val span = (last.t - recent.first().t).coerceAtLeast(16L).toFloat()
        // The first sample's motion happened before its timestamp; skip it.
        val sx = recent.drop(1).sumOf { it.dx.toDouble() }.toFloat()
        val sy = recent.drop(1).sumOf { it.dy.toDouble() }.toFloat()
        var vx = sx / span
        var vy = sy / span
        val speed = hypot(vx, vy) * 1000f
        if (speed < config.inertiaMinSpeed * 2) return null
        if (speed > config.inertiaMaxSpeed) {
            val k = config.inertiaMaxSpeed / speed
            vx *= k
            vy *= k
        }
        return vx to vy
    }

    private fun stopInertia() {
        inertia = false
        inertiaVx = 0f
        inertiaVy = 0f
        send(Wire.Message.Scroll(Wire.PHASE_END, cursorX, cursorY, 0f, 0f))
    }

    // ---- Three fingers ----------------------------------------------------

    private fun finishThree(now: Long) {
        val dx = threeLastCx - threeStartCx
        val dy = threeLastCy - threeStartCy
        val dist = hypot(dx, dy)
        if (dist <= config.tapSlop && now - startMs <= config.multiTapTimeoutMs * 2) {
            onThreeFingerTap?.invoke()
            return
        }
        if (dist < config.swipeDistance) return
        if (abs(dx) >= abs(dy)) {
            // Fingers moving right bring the next workspace in from the right.
            sendCombo(if (dx > 0) config.workspaceLeft else config.workspaceRight)
        } else if (dy < 0) {
            sendCombo(config.overview)
        }
    }

    private fun sendCombo(combo: KeyCombo) = combo.messages().forEach(send)

    // ---- Geometry ---------------------------------------------------------

    private fun centroid(pts: List<Ptr>): Pair<Float, Float> =
        (pts.sumOf { it.x.toDouble() } / pts.size).toFloat() to (pts.sumOf { it.y.toDouble() } / pts.size).toFloat()

    private fun spread(pts: List<Ptr>): Float = hypot(pts[1].x - pts[0].x, pts[1].y - pts[0].y)

    private fun angle(pts: List<Ptr>): Float = atan2(pts[1].y - pts[0].y, pts[1].x - pts[0].x)
}
