package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class RoleStateTest {
    @Test
    fun unsetAckShowsChooserAndStaysMirror() {
        val s = RoleState().onHelloAck(Wire.ROLE_UNSET)
        assertEquals(Wire.ROLE_MIRROR, s.role)
        assertTrue(s.chooserPending)
    }

    @Test
    fun rememberedRoleIsAdoptedWithoutChooser() {
        val s = RoleState().onHelloAck(Wire.ROLE_EXTEND)
        assertEquals(Wire.ROLE_EXTEND, s.role)
        assertFalse(s.chooserPending)
    }

    @Test
    fun garbageAckFallsBackToMirror() {
        val s = RoleState().onHelloAck(77)
        assertEquals(Wire.ROLE_MIRROR, s.role)
        assertFalse(s.chooserPending)
    }

    @Test
    fun hostSetRoleChangesRoleAndClosesChooser() {
        val s = RoleState().onHelloAck(Wire.ROLE_UNSET).onHostSetRole(Wire.ROLE_TABLET)
        assertEquals(Wire.ROLE_TABLET, s.role)
        assertFalse(s.chooserPending)
    }

    @Test
    fun hostSetRoleWithUnknownValueIsIgnored() {
        val before = RoleState(Wire.ROLE_EXTEND)
        assertEquals(before, before.onHostSetRole(Wire.ROLE_UNSET))
        assertEquals(before, before.onHostSetRole(9))
    }

    @Test
    fun requestValidatesAndAppliesRole() {
        val s = RoleState().onHelloAck(Wire.ROLE_UNSET)
        assertNull(s.request(Wire.ROLE_UNSET))
        val next = s.request(Wire.ROLE_INPUT_PAD)!!
        assertEquals(Wire.ROLE_INPUT_PAD, next.role)
        assertFalse(next.chooserPending)
    }

    @Test
    fun dismissKeepsMirror() {
        val s = RoleState().onHelloAck(Wire.ROLE_UNSET).dismissChooser()
        assertEquals(Wire.ROLE_MIRROR, s.role)
        assertFalse(s.chooserPending)
    }

    @Test
    fun onlyTabletAndInputPadHaveNoVideo() {
        for (r in RoleState.ALL_ROLES) {
            assertEquals(r != Wire.ROLE_TABLET && r != Wire.ROLE_INPUT_PAD, RoleState.hasVideo(r))
        }
        assertEquals(6, RoleState.ALL_ROLES.size)
    }
}
