package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class Phase8Test {
    private fun text(s: String) = ClipPayload.Text(s)

    private val png = byteArrayOf(0x89.toByte(), 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3)

    @Test
    fun clipboardLoopPrevention() {
        val s = ClipboardSyncState()
        s.noteRemote(text("at connect"))
        assertFalse(s.shouldSend(text("at connect")))
        assertTrue(s.shouldSend(text("new")))
        assertFalse(s.shouldSend(text("new")))
        s.noteRemote(text("from host"))
        assertFalse("no echo", s.shouldSend(text("from host")))
        assertTrue(s.shouldSend(text("new")))
    }

    @Test
    fun clipboardLimits() {
        val s = ClipboardSyncState()
        val big = ClipPayload.Png(ByteArray(ClipboardSyncState.MAX_IMAGE + 1))
        assertFalse(s.shouldSend(big))
        assertFalse(s.shouldSend(big))
        assertFalse(s.shouldSend(text("")))
        assertTrue(s.shouldSend(ClipPayload.Png(png)))
    }

    @Test
    fun clipboardWireMapping() {
        assertEquals(text("hi"), ClipboardSyncState.fromWire("text/plain;charset=utf-8", "hi".toByteArray()))
        assertNull(ClipboardSyncState.fromWire("image/png", byteArrayOf(1, 2, 3)))
        assertNull(ClipboardSyncState.fromWire("text/html", "<b>".toByteArray()))
        assertEquals(ClipPayload.Png(png), ClipboardSyncState.fromWire("image/png", png))
        assertEquals("image/png", ClipboardSyncState.toMessage(ClipPayload.Png(png)).mime)
        assertEquals(Wire.Message.Clipboard("text/plain", "é".toByteArray()), ClipboardSyncState.toMessage(text("é")))
    }

    @Test
    fun batteryMath() {
        assertEquals(50, BatteryMath.percent(50, 100))
        assertEquals(17, BatteryMath.percent(34, 200))
        assertEquals(255, BatteryMath.percent(-1, 100))
        assertEquals(Wire.TEMP_UNKNOWN, BatteryMath.temp(Int.MIN_VALUE))
        assertEquals(412, BatteryMath.temp(412))
    }

    @Test
    fun batteryReportCadence() {
        val a = Wire.Message.BatteryStatus(50, false, 0, 300)
        assertTrue(BatteryMath.shouldSend(null, a, 0))
        assertFalse("only the temperature drifted", BatteryMath.shouldSend(a, a.copy(tempDecidegrees = 310), 5_000))
        assertTrue(BatteryMath.shouldSend(a, a.copy(percent = 49), 5_000))
        assertFalse("rate limited", BatteryMath.shouldSend(a, a.copy(percent = 49), 500))
        assertTrue(BatteryMath.shouldSend(a, a, BatteryMath.PERIOD_MS))
        assertTrue(BatteryMath.shouldSend(a, a.copy(thermal = Wire.THERMAL_SEVERE), 3_000))
    }

    @Test
    fun batteryStatusRoundTripsWithNegativeTemperature() {
        val m = Wire.Message.BatteryStatus(3, true, Wire.THERMAL_NONE, -50)
        val bytes = m.encode()
        val h = Wire.Header.parse(bytes.copyOfRange(0, Wire.HEADER_SIZE))
        assertEquals(m, Wire.Message.decode(h.channel, h.type, bytes.copyOfRange(Wire.HEADER_SIZE, bytes.size)))
    }
}
