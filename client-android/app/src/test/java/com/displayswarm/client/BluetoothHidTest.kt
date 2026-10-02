package com.displayswarm.client

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test

class BluetoothHidTest {
    @Test
    fun keyboardReportLayout() {
        val r = HidReports.keyboard(HidReports.MOD_SHIFT, listOf(0x04, 0x05))
        assertArrayEquals(byteArrayOf(2, 0, 4, 5, 0, 0, 0, 0), r)
        assertEquals(8, HidReports.keyboard(0, List(9) { 1 }).size)
    }

    @Test
    fun mouseReportClampsAndMasks() {
        assertArrayEquals(byteArrayOf(1, 127, -127, 0), HidReports.mouse(0xF9, 500, -500))
    }

    @Test
    fun digitizerReportScalesTo15Bits() {
        assertArrayEquals(byteArrayOf(3, 0xFF.toByte(), 0x7F, 0, 0), HidReports.digitizer(true, true, 1f, 0f))
        assertArrayEquals(byteArrayOf(2, 0, 0, 0, 0), HidReports.digitizer(false, true, -1f, -1f))
    }

    @Test
    fun descriptorDeclaresThreeReportIds() {
        val d = HidReports.DESCRIPTOR
        val ids = d.indices.filter { d[it] == 0x85.toByte() }.map { d[it + 1].toInt() }
        assertEquals(listOf(1, 2, 3), ids)
        // Collections are balanced.
        assertEquals(d.count { it == 0xC0.toByte() }, d.count { it == 0xA1.toByte() })
    }

    @Test
    fun keyMapping() {
        assertEquals(0x04, HidReports.usageFor(29)) // A
        assertEquals(0x1D, HidReports.usageFor(54)) // Z
        assertEquals(0x27, HidReports.usageFor(7)) // 0
        assertEquals(0x1E, HidReports.usageFor(8)) // 1
        assertEquals(0x28, HidReports.usageFor(66)) // Enter
        assertEquals(0, HidReports.usageFor(999))
        assertEquals(
            HidReports.MOD_CTRL or HidReports.MOD_ALT,
            HidReports.modifiersFor(Wire.KEY_FLAG_CTRL or Wire.KEY_FLAG_ALT)
        )
    }

    @Test
    fun translatorTracksHeldKeys() {
        val t = HidTranslator()
        val down = Wire.Message.Key(Wire.ACTION_DOWN, 29, 0, 0, "a")
        val up = Wire.Message.Key(Wire.ACTION_UP, 29, 0, 0, "a")
        assertEquals(HidReport(1, HidReports.keyboard(0, listOf(4))), t.translate(down).single())
        assertEquals(HidReport(1, HidReports.keyboard(0, emptyList())), t.translate(up).single())
    }

    @Test
    fun translatorMapsMouseTouchAndScroll() {
        val t = HidTranslator()
        val rel = Wire.Message.Mouse(Wire.ACTION_MOVE, 0, true, 5f, -3f)
        assertEquals(HidReport(2, HidReports.mouse(0, 5, -3)), t.translate(rel).single())
        val press = Wire.Message.Mouse(Wire.ACTION_DOWN, Wire.MOUSE_BUTTON_PRIMARY, true, 0f, 0f)
        assertEquals(HidReport(2, HidReports.mouse(1, 0, 0)), t.translate(press).single())
        t.translate(Wire.Message.Mouse(Wire.ACTION_UP, 0, true, 0f, 0f))
        val touch = Wire.Message.Touch(Wire.ACTION_DOWN, 0, listOf(Wire.TouchPoint(0, 0.5f, 0.5f, 1f)))
        assertEquals(HidReport(3, HidReports.digitizer(true, true, 0.5f, 0.5f)), t.translate(touch).single())
        val lift = Wire.Message.Touch(Wire.ACTION_UP, 0, listOf(Wire.TouchPoint(0, 0.5f, 0.5f, 0f)))
        assertEquals(HidReport(3, HidReports.digitizer(false, false, 0.5f, 0.5f)), t.translate(lift).single())
        val scroll = Wire.Message.Scroll(Wire.PHASE_NONE, 0f, 0f, 0f, -2f)
        assertEquals(HidReport(2, HidReports.mouse(0, 0, 0, -2)), t.translate(scroll).single())
    }
}
