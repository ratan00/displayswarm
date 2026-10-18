package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Test

class DeviceIdentityTest {
    @Test
    fun sameAndroidIdGivesSameIdAndHidesTheRawValue() {
        val a = DeviceIdentity.derived("0123456789abcdef")
        assertEquals(a, DeviceIdentity.derived("0123456789abcdef"))
        assertNotEquals("0123456789abcdef", a)
        assertNotEquals(a, DeviceIdentity.derived("fedcba9876543210"))
    }

    @Test
    fun unusableAndroidIdsFallBack() {
        assertNull(DeviceIdentity.derived(null))
        assertNull(DeviceIdentity.derived(""))
        assertNull(DeviceIdentity.derived("9774d56d682e549c"))
    }
}
