package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ViewportTest {
    private fun vp() = Viewport(1000f, 500f)

    @Test
    fun identityMapsLinearly() {
        val v = vp()
        assertTrue(v.isIdentity)
        assertEquals(0.25f, v.toContentX(250f), 1e-6f)
        assertEquals(0.5f, v.toContentY(250f), 1e-6f)
    }

    @Test
    fun zoomKeepsTheFocusPointFixed() {
        val v = vp()
        val before = v.toContentX(700f)
        v.zoomBy(2f, 700f, 300f)
        assertEquals(2f, v.scale, 1e-6f)
        assertEquals(before, v.toContentX(700f), 1e-5f)
        assertEquals(0.6f, v.toContentY(300f), 1e-5f)
    }

    @Test
    fun scaleIsClampedToOneAndFour() {
        val v = vp()
        v.zoomBy(10f, 0f, 0f)
        assertEquals(Viewport.MAX_SCALE, v.scale, 1e-6f)
        v.zoomBy(0.01f, 500f, 250f)
        assertEquals(1f, v.scale, 1e-6f)
        assertEquals(0f, v.panX, 1e-6f)
        assertEquals(0f, v.panY, 1e-6f)
    }

    @Test
    fun panNeverShowsBeyondTheContent() {
        val v = vp()
        v.zoomBy(2f, 500f, 250f)
        v.panBy(10_000f, 10_000f)
        assertEquals(0f, v.panX, 1e-6f)
        assertEquals(0f, v.toContentX(0f), 1e-6f)
        v.panBy(-10_000f, -10_000f)
        assertEquals(-1000f, v.panX, 1e-6f) // width * (1 - scale)
        assertEquals(1f, v.toContentX(1000f), 1e-6f)
        assertEquals(1f, v.toContentY(500f), 1e-6f)
    }

    @Test
    fun panWhileNotZoomedDoesNothing() {
        val v = vp()
        v.panBy(50f, 50f)
        assertEquals(0f, v.panX, 1e-6f)
        assertEquals(0f, v.panY, 1e-6f)
    }

    @Test
    fun touchesLandOnTheRightContentWhenZoomedAndPanned() {
        val v = vp()
        v.zoomBy(4f, 0f, 0f) // top-left corner zoomed: shows the first quarter
        assertEquals(0.25f, v.toContentX(1000f), 1e-5f)
        v.panBy(-500f, -250f) // scroll by half a screen of zoomed content
        // Content at view x=0 is now at zoomed offset 500 -> 500/4000.
        assertEquals(0.125f, v.toContentX(0f), 1e-5f)
        assertEquals(0.125f, v.toContentY(0f), 1e-5f)
    }

    @Test
    fun resetGoesBackToIdentity() {
        val v = vp()
        v.zoomBy(3f, 100f, 100f)
        v.panBy(-40f, -40f)
        v.reset()
        assertTrue(v.isIdentity)
        assertEquals(0f, v.panX, 1e-6f)
    }

    @Test
    fun resizeKeepsTheZoomAndReclamps() {
        val v = vp()
        v.zoomBy(2f, 1000f, 500f) // pan = (-1000, -500)
        v.setSize(500f, 250f)
        assertEquals(2f, v.scale, 1e-6f)
        assertEquals(-500f, v.panX, 1e-6f)
        assertFalse(v.isIdentity)
    }

    @Test
    fun nonsenseFactorsAreIgnored() {
        val v = vp()
        v.zoomBy(0f, 1f, 1f)
        v.zoomBy(-2f, 1f, 1f)
        v.zoomBy(Float.NaN, 1f, 1f)
        assertTrue(v.isIdentity)
    }
}
