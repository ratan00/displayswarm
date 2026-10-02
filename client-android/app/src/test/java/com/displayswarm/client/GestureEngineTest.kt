package com.displayswarm.client

import com.displayswarm.client.GestureEngine.Kind
import com.displayswarm.client.GestureEngine.Ptr
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class GestureEngineTest {
    private val out = ArrayList<Wire.Message>()
    private var threeTaps = 0
    private val engine = GestureEngine({ out.add(it) }, GestureEngine.Config(pointerAccel = 0f)).also {
        it.viewWidth = 1000f
        it.viewHeight = 500f
        it.onThreeFingerTap = { threeTaps++ }
    }

    private fun p(id: Int, x: Float, y: Float) = Ptr(id, x, y)
    private fun mice() = out.filterIsInstance<Wire.Message.Mouse>()
    private fun scrolls() = out.filterIsInstance<Wire.Message.Scroll>()
    private fun pinches() = out.filterIsInstance<Wire.Message.Pinch>()
    private fun keys() = out.filterIsInstance<Wire.Message.Key>()

    @Test
    fun oneFingerMovesTheCursorRelatively() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 0)
        engine.onTouch(Kind.MOVE, listOf(p(0, 130f, 90f)), 16)
        val m = mice().single()
        assertTrue(m.relative)
        assertEquals(Wire.ACTION_HOVER_MOVE, m.action)
        assertEquals(0, m.buttons)
        assertEquals(30f, m.x, 0.001f)
        assertEquals(-10f, m.y, 0.001f)
    }

    @Test
    fun smallJitterBeforeSlopIsHeldBackThenReleased() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 0)
        engine.onTouch(Kind.MOVE, listOf(p(0, 104f, 100f)), 8)
        assertTrue(mice().isEmpty())
        engine.onTouch(Kind.MOVE, listOf(p(0, 112f, 100f)), 16)
        assertEquals(12f, mice().single().x, 0.001f) // nothing lost
    }

    @Test
    fun pointerAccelerationGrowsWithSpeed() {
        val e = GestureEngine({ out.add(it) }, GestureEngine.Config(pointerAccel = 0.5f))
        e.onTouch(Kind.DOWN, listOf(p(0, 0f, 0f)), 0)
        e.onTouch(Kind.MOVE, listOf(p(0, 20f, 0f)), 100) // 0.2 px/ms, slow
        e.onTouch(Kind.MOVE, listOf(p(0, 120f, 0f)), 110) // 10 px/ms, capped boost
        val (slow, fast) = mice().map { it.x }
        assertEquals(20f * 1.1f, slow, 0.01f)
        assertEquals(100f * (1f + 0.5f * 4f), fast, 0.01f)
    }

    @Test
    fun tapIsALeftClick() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 0)
        engine.onTouch(Kind.UP, emptyList(), 120)
        val m = mice()
        assertEquals(listOf(Wire.ACTION_DOWN, Wire.ACTION_UP), m.map { it.action })
        assertEquals(Wire.MOUSE_BUTTON_PRIMARY, m[0].buttons)
        assertEquals(0, m[1].buttons)
    }

    @Test
    fun slowTouchIsNotAClick() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 0)
        engine.onTouch(Kind.UP, emptyList(), 600)
        assertTrue(out.isEmpty())
    }

    @Test
    fun movedTouchIsNotAClick() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 0)
        engine.onTouch(Kind.MOVE, listOf(p(0, 200f, 100f)), 30)
        out.clear()
        engine.onTouch(Kind.UP, emptyList(), 60)
        assertTrue(out.isEmpty())
    }

    @Test
    fun tapThenDragHoldsTheButton() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 0)
        engine.onTouch(Kind.UP, emptyList(), 80)
        out.clear()
        engine.onTouch(Kind.DOWN, listOf(p(0, 105f, 100f)), 200)
        engine.onTouch(Kind.MOVE, listOf(p(0, 160f, 100f)), 230)
        engine.onTouch(Kind.MOVE, listOf(p(0, 200f, 100f)), 250)
        engine.onTouch(Kind.UP, emptyList(), 280)
        val m = mice()
        assertEquals(Wire.ACTION_DOWN, m[0].action)
        assertEquals(Wire.MOUSE_BUTTON_PRIMARY, m[0].buttons)
        assertEquals(Wire.ACTION_MOVE, m[1].action)
        assertEquals(Wire.MOUSE_BUTTON_PRIMARY, m[1].buttons)
        assertEquals(Wire.ACTION_MOVE, m[2].action)
        assertEquals(Wire.ACTION_UP, m.last().action)
        assertEquals(0, m.last().buttons)
    }

    @Test
    fun aSecondTapIsAnotherClick() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 0)
        engine.onTouch(Kind.UP, emptyList(), 80)
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 200)
        engine.onTouch(Kind.UP, emptyList(), 260)
        assertEquals(4, mice().size)
    }

    @Test
    fun aFarAwayTouchIsNotADragStart() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 0)
        engine.onTouch(Kind.UP, emptyList(), 80)
        out.clear()
        engine.onTouch(Kind.DOWN, listOf(p(0, 900f, 400f)), 200)
        engine.onTouch(Kind.MOVE, listOf(p(0, 950f, 400f)), 230)
        assertEquals(Wire.ACTION_HOVER_MOVE, mice().single().action)
    }

    @Test
    fun twoFingerTapIsARightClick() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 0)
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f), p(1, 200f, 100f)), 20)
        engine.onTouch(Kind.UP, listOf(p(1, 200f, 100f)), 100)
        engine.onTouch(Kind.UP, emptyList(), 110)
        val m = mice()
        assertEquals(listOf(Wire.ACTION_DOWN, Wire.ACTION_UP), m.map { it.action })
        assertEquals(Wire.MOUSE_BUTTON_SECONDARY, m[0].buttons)
    }

    private fun twoDown(now: Long) {
        engine.onTouch(Kind.DOWN, listOf(p(0, 300f, 200f)), now)
        engine.onTouch(Kind.DOWN, listOf(p(0, 300f, 200f), p(1, 400f, 200f)), now + 5)
    }

    @Test
    fun twoFingersMovingScrollNaturally() {
        twoDown(0)
        var t = 10L
        var y = 200f
        repeat(5) {
            y += 30f
            engine.onTouch(Kind.MOVE, listOf(p(0, 300f, y), p(1, 400f, y)), t)
            t += 16
        }
        val s = scrolls()
        assertEquals(Wire.PHASE_BEGIN, s.first().phase)
        assertTrue(s.drop(1).all { it.phase == Wire.PHASE_UPDATE })
        // Fingers moved down 150 px: content moves down -> positive (scroll up).
        assertEquals(150f / engine.config.pxPerDetent, s.sumOf { it.dy.toDouble() }.toFloat(), 0.01f)
        assertEquals(0f, s.sumOf { it.dx.toDouble() }.toFloat(), 0.001f)
        assertTrue(mice().isEmpty())
    }

    @Test
    fun scrollDirectionCanBeReversed() {
        val e = GestureEngine({ out.add(it) }, GestureEngine.Config(naturalScroll = false))
        e.onTouch(Kind.DOWN, listOf(p(0, 300f, 200f)), 0)
        e.onTouch(Kind.DOWN, listOf(p(0, 300f, 200f), p(1, 400f, 200f)), 5)
        e.onTouch(Kind.MOVE, listOf(p(0, 300f, 260f), p(1, 400f, 260f)), 20)
        assertTrue(scrolls().sumOf { it.dy.toDouble() } < 0)
    }

    @Test
    fun releasingAFastScrollFlingsWithInertia() {
        twoDown(0)
        var t = 10L
        var y = 200f
        repeat(6) {
            y += 20f
            engine.onTouch(Kind.MOVE, listOf(p(0, 300f, y), p(1, 400f, y)), t)
            t += 16
        }
        engine.onTouch(Kind.UP, listOf(p(1, 400f, y)), t)
        engine.onTouch(Kind.UP, emptyList(), t + 2)
        assertEquals(Wire.PHASE_END, scrolls().last().phase)
        assertTrue(engine.wantsTick)
        out.clear()
        var now = t
        repeat(10) {
            now += 16
            engine.tick(now)
        }
        val inertia = scrolls()
        assertTrue(inertia.isNotEmpty())
        assertTrue(inertia.all { it.phase == Wire.PHASE_INERTIA && it.dy > 0 })
        assertTrue(inertia.last().dy <= inertia.first().dy) // decaying
        // Eventually it stops by itself, with an END.
        repeat(400) {
            now += 16
            engine.tick(now)
        }
        assertFalse(engine.wantsTick)
        assertEquals(Wire.PHASE_END, scrolls().last().phase)
    }

    @Test
    fun pausedFingersDoNotFling() {
        twoDown(0)
        var y = 200f
        var t = 10L
        repeat(5) {
            y += 20f
            engine.onTouch(Kind.MOVE, listOf(p(0, 300f, y), p(1, 400f, y)), t)
            t += 16
        }
        engine.onTouch(Kind.UP, listOf(p(1, 400f, y)), t + 300)
        assertFalse(engine.wantsTick)
    }

    @Test
    fun aTouchStopsTheFling() {
        twoDown(0)
        var y = 200f
        var t = 10L
        repeat(6) {
            y += 20f
            engine.onTouch(Kind.MOVE, listOf(p(0, 300f, y), p(1, 400f, y)), t)
            t += 16
        }
        engine.onTouch(Kind.UP, listOf(p(1, 400f, y)), t)
        engine.onTouch(Kind.UP, emptyList(), t)
        assertTrue(engine.wantsTick)
        engine.onTouch(Kind.DOWN, listOf(p(0, 10f, 10f)), t + 20)
        assertFalse(engine.wantsTick)
    }

    @Test
    fun spreadingFingersPinch() {
        twoDown(0)
        var t = 10L
        var gap = 100f
        repeat(4) {
            gap += 40f
            engine.onTouch(Kind.MOVE, listOf(p(0, 300f, 200f), p(1, 300f + gap, 200f)), t)
            t += 16
        }
        engine.onTouch(Kind.UP, listOf(p(1, 460f, 200f)), t)
        val pc = pinches()
        assertEquals(Wire.PHASE_BEGIN, pc.first().phase)
        assertEquals(Wire.PHASE_END, pc.last().phase)
        assertEquals(260f / 100f, pc.last().scale, 0.001f)
        assertEquals(0f, pc.last().rotation, 0.001f)
        assertTrue(scrolls().isEmpty())
    }

    @Test
    fun threeFingerSwipeSwitchesWorkspace() {
        fun swipe(dx: Float, dy: Float) {
            out.clear()
            val a = { ox: Float, oy: Float -> listOf(p(0, 300f + ox, 200f + oy), p(1, 400f + ox, 200f + oy), p(2, 500f + ox, 200f + oy)) }
            engine.onTouch(Kind.DOWN, listOf(p(0, 300f, 200f)), 0)
            engine.onTouch(Kind.DOWN, listOf(p(0, 300f, 200f), p(1, 400f, 200f)), 5)
            engine.onTouch(Kind.DOWN, a(0f, 0f), 10)
            engine.onTouch(Kind.MOVE, a(dx / 2, dy / 2), 60)
            engine.onTouch(Kind.MOVE, a(dx, dy), 120)
            engine.onTouch(Kind.UP, a(dx, dy).drop(1), 140)
            engine.onTouch(Kind.UP, emptyList(), 150)
        }
        swipe(300f, 0f)
        assertEquals(engine.config.workspaceLeft.keyCode, keys().first().keyCode)
        assertEquals(engine.config.workspaceLeft.meta, keys().first().meta)
        assertEquals(listOf(Wire.ACTION_DOWN, Wire.ACTION_UP), keys().map { it.action })
        swipe(-300f, 0f)
        assertEquals(engine.config.workspaceRight.keyCode, keys().first().keyCode)
        swipe(0f, -300f)
        assertEquals(engine.config.overview.keyCode, keys().first().keyCode)
        swipe(0f, 300f)
        assertTrue(keys().isEmpty())
        swipe(20f, 0f) // too short and too long-lived for a tap
        assertTrue(keys().isEmpty())
        assertTrue(mice().isEmpty())
    }

    @Test
    fun threeFingerTapCallsBack() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 300f, 200f)), 0)
        engine.onTouch(Kind.DOWN, listOf(p(0, 300f, 200f), p(1, 400f, 200f)), 5)
        engine.onTouch(Kind.DOWN, listOf(p(0, 300f, 200f), p(1, 400f, 200f), p(2, 500f, 200f)), 10)
        engine.onTouch(Kind.UP, listOf(p(0, 300f, 200f), p(1, 400f, 200f)), 90)
        engine.onTouch(Kind.UP, listOf(p(0, 300f, 200f)), 95)
        engine.onTouch(Kind.UP, emptyList(), 100)
        assertEquals(1, threeTaps)
        assertTrue(out.isEmpty())
    }

    @Test
    fun cancelReleasesAHeldDragButton() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 0)
        engine.onTouch(Kind.UP, emptyList(), 60)
        engine.onTouch(Kind.DOWN, listOf(p(0, 100f, 100f)), 150)
        engine.onTouch(Kind.MOVE, listOf(p(0, 200f, 100f)), 170)
        out.clear()
        engine.onTouch(Kind.CANCEL, emptyList(), 180)
        assertEquals(listOf(Wire.ACTION_UP), mice().map { it.action })
        assertEquals(0, mice()[0].buttons)
    }

    @Test
    fun cancelEndsAScroll() {
        twoDown(0)
        engine.onTouch(Kind.MOVE, listOf(p(0, 300f, 260f), p(1, 400f, 260f)), 20)
        out.clear()
        engine.onTouch(Kind.CANCEL, emptyList(), 30)
        assertEquals(Wire.PHASE_END, scrolls().single().phase)
        assertFalse(engine.wantsTick)
    }

    @Test
    fun extraFingersAfterAGestureAreIgnoredUntilAllLift() {
        twoDown(0)
        engine.onTouch(Kind.UP, listOf(p(0, 300f, 200f)), 50) // was a tap: right click
        out.clear()
        engine.onTouch(Kind.MOVE, listOf(p(0, 400f, 300f)), 70) // the remaining finger wanders
        assertTrue(out.isEmpty())
    }

    @Test
    fun scrollAndPinchCarryTheEstimatedCursor() {
        engine.onTouch(Kind.DOWN, listOf(p(0, 0f, 0f)), 0)
        engine.onTouch(Kind.MOVE, listOf(p(0, 200f, 100f)), 16) // +0.2, +0.2 of 1000x500
        engine.onTouch(Kind.DOWN, listOf(p(0, 200f, 100f), p(1, 300f, 100f)), 30)
        engine.onTouch(Kind.MOVE, listOf(p(0, 200f, 160f), p(1, 300f, 160f)), 50)
        val s = scrolls().first()
        assertEquals(0.7f, s.x, 0.001f)
        assertEquals(0.7f, s.y, 0.001f)
    }
}
