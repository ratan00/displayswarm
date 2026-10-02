package com.displayswarm.client

import com.displayswarm.client.Wire.Message
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.EOFException

class FrameReaderTest {
    private val ack = Wire.HelloAck(Wire.HELLO_OK, 1600, 720, 60, Wire.CODEC_H264, 7, Wire.ROLE_MIRROR, "laptop", "")

    private fun reader(vararg parts: ByteArray) =
        Wire.FrameReader(ByteArrayInputStream(parts.fold(ByteArray(0)) { a, b -> a + b }))

    private fun video(index: Long, size: Int = 20) =
        Message.VideoFrame(Wire.CODEC_H264, Wire.FRAME_TYPE_DELTA, index, 5, ByteArray(size) { it.toByte() })

    @Test
    fun awaitHelloAckSkipsJunkAndStaleFrames() {
        val r = reader(
            "junk".toByteArray(), "VM".toByteArray(), byteArrayOf(0),
            video(3).encode(),
            Message.Heartbeat.encode(),
            Message.HelloAckMsg(ack).encode(),
            Message.Heartbeat.encode(),
            Message.Bye(Wire.BYE_NORMAL, "x").encode()
        )
        assertEquals(ack, r.awaitHelloAck())
        assertTrue(r.skippedBytes > 0)
        assertEquals(Message.Heartbeat, r.next())
        assertEquals(Message.Bye(Wire.BYE_NORMAL, "x"), r.next())
    }

    @Test
    fun awaitHelloAckKeepsMessagesBufferedDuringTheScan() {
        val r = reader(Message.HelloAckMsg(ack).encode(), Message.Ping(1, 2).encode(), Message.Heartbeat.encode())
        assertEquals(ack, r.awaitHelloAck())
        assertEquals(Message.Ping(1, 2), r.next())
        assertEquals(Message.Heartbeat, r.next())
    }

    @Test
    fun awaitHelloAckThrowsOnOtherVersion() {
        val bytes = Message.HelloAckMsg(ack).encode()
        bytes[2] = 3
        try {
            reader(bytes).awaitHelloAck()
            fail("version 3 accepted")
        } catch (_: Wire.HandshakeException) {
        }
    }

    @Test
    fun nextReassemblesFragmentedVideoAmongHeartbeats() {
        val v = video(7, 100)
        val all = v.encode(32)
        val frames = ArrayList<ByteArray>()
        var pos = 0
        while (pos < all.size) {
            val h = Wire.Header.parse(all.copyOfRange(pos, pos + Wire.HEADER_SIZE))
            val end = pos + Wire.HEADER_SIZE + h.len.toInt()
            frames += all.copyOfRange(pos, end)
            pos = end
        }
        val parts = ArrayList<ByteArray>()
        for (f in frames) {
            parts += f
            parts += Message.Heartbeat.encode()
        }
        val r = reader(*parts.toTypedArray())
        val got = ArrayList<Message>()
        try {
            while (true) got += r.next()
        } catch (_: EOFException) {
        }
        assertEquals(1, got.count { it == v })
        assertEquals(frames.size, got.count { it == Message.Heartbeat })
    }

    @Test
    fun nextThrowsOnGarbageAfterAValidFrame() {
        val r = reader(Message.Heartbeat.encode(), "garbage-garbage".toByteArray())
        assertEquals(Message.Heartbeat, r.next())
        try {
            r.next()
            fail("garbage accepted")
        } catch (_: ProtocolException) {
        }
    }

    @Test
    fun nextThrowsEofAtEndOfStream() {
        val r = reader(Message.Heartbeat.encode())
        r.next()
        try {
            r.next()
            fail("expected EOF")
        } catch (_: EOFException) {
        }
    }
}
