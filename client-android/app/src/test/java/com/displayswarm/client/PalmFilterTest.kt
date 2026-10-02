package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PalmFilterTest {
    private fun p(id: Int) = Wire.TouchPoint(id, 0.5f, 0.5f, 1f)
    private fun touch(action: Int, actionId: Int, vararg ids: Int) =
        Wire.Message.Touch(action, actionId, ids.map { p(it) })

    @Test
    fun fingersPassWithoutPen() {
        val f = PalmFilter()
        val down = touch(Wire.ACTION_DOWN, 0, 0)
        assertEquals(listOf(down), f.filterTouch(down, 0))
    }

    @Test
    fun fingersAreDroppedWhilePenHoversAndDuringGrace() {
        val f = PalmFilter(graceMs = 500)
        f.onPen(true, 1000)
        assertTrue(f.filterTouch(touch(Wire.ACTION_DOWN, 1, 1), 1000).isEmpty())
        f.onPen(false, 2000)
        assertTrue(f.penActive(2400))
        assertTrue(f.filterTouch(touch(Wire.ACTION_MOVE, 1, 1), 2400).isEmpty())
        assertFalse(f.penActive(2500))
    }

    @Test
    fun fingerDownBeforePenIsCancelledOnce() {
        val f = PalmFilter()
        f.filterTouch(touch(Wire.ACTION_DOWN, 0, 0), 0)
        f.onPen(true, 10)
        val out = f.filterTouch(touch(Wire.ACTION_MOVE, 0, 0), 10)
        assertEquals(listOf(touch(Wire.ACTION_CANCEL, 0, 0)), out)
        assertTrue(f.filterTouch(touch(Wire.ACTION_MOVE, 0, 0), 20).isEmpty())
    }

    @Test
    fun palmThatLandedDuringPenStaysMutedUntilLifted() {
        val f = PalmFilter(graceMs = 100)
        f.onPen(true, 0)
        f.filterTouch(touch(Wire.ACTION_DOWN, 3, 3), 0)
        f.onPen(false, 50)
        // Grace over, the palm is still down: still muted.
        assertTrue(f.filterTouch(touch(Wire.ACTION_MOVE, 3, 3), 1000).isEmpty())
        // A new finger goes through; the palm is left out of it.
        val out = f.filterTouch(touch(Wire.ACTION_DOWN, 4, 3, 4), 1000)
        assertEquals(listOf(touch(Wire.ACTION_DOWN, 4, 4)), out)
        // The palm lifting is only a move of the finger that remains.
        assertEquals(listOf(touch(Wire.ACTION_MOVE, 4, 4)), f.filterTouch(touch(Wire.ACTION_UP, 3, 3, 4), 1010))
        // Once lifted, the same id is a normal finger again.
        f.filterTouch(touch(Wire.ACTION_UP, 4, 4), 1020)
        assertEquals(listOf(touch(Wire.ACTION_DOWN, 3, 3)), f.filterTouch(touch(Wire.ACTION_DOWN, 3, 3), 1030))
    }

    @Test
    fun disabledPassesEverything() {
        val f = PalmFilter()
        f.enabled = false
        f.onPen(true, 0)
        val down = touch(Wire.ACTION_DOWN, 0, 0)
        assertEquals(listOf(down), f.filterTouch(down, 0))
    }
}
