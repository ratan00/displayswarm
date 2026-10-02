package com.displayswarm.client

import com.displayswarm.client.Wire.Message
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayInputStream

class ControlSessionTest {
    private val sent = ArrayList<ByteArray>()
    private val statuses = ArrayList<String>()
    private val ended = ArrayList<String>()
    private val roles = ArrayList<RoleState>()
    private val session = ControlSession(
        sendPacket = { sent += it },
        onStatus = { statuses += it },
        onMetrics = {},
        onEnded = { ended += it },
        onRole = { roles += it }
    )

    private fun decodeSent(): List<Message> = sent.map { Wire.FrameReader(ByteArrayInputStream(it)).next() }

    private fun video(index: Long, type: Int = Wire.FRAME_TYPE_DELTA) =
        Message.VideoFrame(Wire.CODEC_H264, type, index, 0, ByteArray(4))

    @Test
    fun pingIsAnsweredWithPongEchoingIdAndSendTime() {
        session.onMessage(Message.Ping(0xFFFFFFFFL, 123_456L))
        val pong = decodeSent().single() as Message.Pong
        assertEquals(0xFFFFFFFFL, pong.id)
        assertEquals(123_456L, pong.tPingUs)
        assertTrue(pong.tReplyUs >= pong.tRecvUs)
    }

    @Test
    fun byeServerStoppingEndsTheSession() {
        session.onMessage(Message.Bye(Wire.BYE_SERVER_STOPPING, ""))
        assertEquals(listOf("Host stopped the server"), ended)
    }

    @Test
    fun versionMismatchTextSurfaces() {
        session.onMessage(Message.Bye(Wire.BYE_VERSION_MISMATCH, "Update the app"))
        assertEquals(1, ended.size)
        assertTrue(ended[0], ended[0].contains("Update the app"))
        assertTrue(ended[0], ended[0].contains("Version mismatch"))
    }

    @Test
    fun awaitingPermissionShowsWaitingStatus() {
        session.onMessage(Message.HostState(Wire.HOST_STATE_AWAITING_PERMISSION, ""))
        assertEquals(1, statuses.size)
        assertTrue(statuses[0], statuses[0].startsWith("Waiting for permission"))
    }

    @Test
    fun byePacketDecodesToNormalBye() {
        val bye = Wire.FrameReader(ByteArrayInputStream(session.byePacket())).next() as Message.Bye
        assertEquals(Wire.BYE_NORMAL, bye.reason)
    }

    @Test
    fun frameIndexGapRequestsAKeyframe() {
        session.onMessage(video(1))
        session.onMessage(video(2))
        session.onMessage(video(2)) // same index again is normal
        assertTrue(sent.isEmpty())
        session.onMessage(video(6))
        assertEquals(listOf<Message>(Message.KeyframeRequest), decodeSent())
    }

    @Test
    fun unsetHandshakeReportsChooserPending() {
        session.onHandshakeComplete("info", Wire.ROLE_UNSET)
        assertTrue(roles.last().chooserPending)
        assertEquals(Wire.ROLE_MIRROR, session.roleState.role)
    }

    @Test
    fun hostSetRoleUpdatesStateAndNotifies() {
        session.onHandshakeComplete("info", Wire.ROLE_MIRROR)
        session.onMessage(Message.SetRole(Wire.ROLE_EXTEND))
        assertEquals(Wire.ROLE_EXTEND, session.roleState.role)
        assertEquals(Wire.ROLE_EXTEND, roles.last().role)
        assertTrue(sent.isEmpty()) // the host's statement is not echoed back
    }

    @Test
    fun requestRoleSendsSetRole() {
        session.onHandshakeComplete("info", Wire.ROLE_UNSET)
        session.requestRole(Wire.ROLE_PHONE_PRIMARY)
        assertEquals(listOf<Message>(Message.SetRole(Wire.ROLE_PHONE_PRIMARY)), decodeSent())
        assertEquals(Wire.ROLE_PHONE_PRIMARY, session.roleState.role)
        assertFalse(session.roleState.chooserPending)
    }

    @Test
    fun invalidRoleRequestSendsNothing() {
        session.requestRole(Wire.ROLE_UNSET)
        assertTrue(sent.isEmpty())
    }

    @Test
    fun noVideoRoleSuppressesKeyframeRequestsAndVideo() {
        session.onHandshakeComplete("info", Wire.ROLE_TABLET)
        assertTrue(statuses.last(), statuses.last().startsWith("Connected"))
        assertTrue(statuses.last(), statuses.last().contains("no video"))
        session.onKeyframeNeeded("decoder error")
        session.onMessage(video(1))
        session.onMessage(video(9))
        assertTrue(sent.isEmpty())
        // Liveness still counts: heartbeats are messages like any other.
        session.onMessage(Message.Heartbeat)
        assertTrue(ended.isEmpty())
    }

    @Test
    fun switchingToNoVideoAndBackTogglesStatus() {
        session.onHandshakeComplete("info", Wire.ROLE_MIRROR)
        session.onMessage(video(1))
        session.onMessage(Message.SetRole(Wire.ROLE_INPUT_PAD))
        assertTrue(statuses.last(), statuses.last().contains("no video"))
        session.onMessage(Message.SetRole(Wire.ROLE_EXTEND))
        assertEquals("Waiting for video...", statuses.last())
        session.onMessage(video(50)) // index reset after the switch: no gap request
        assertTrue(sent.isEmpty())
    }

    @Test
    fun keyframeRequestsAreRateLimited() {
        session.onMessage(video(1))
        session.onMessage(video(5))
        session.onMessage(video(9))
        assertEquals(1, sent.size)
    }
}
