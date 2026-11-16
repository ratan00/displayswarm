package com.displayswarm.client

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

class FileTransferTest {
    private class MemSink : FileTransferCore.Sink {
        override val out = ByteArrayOutputStream()
        var committed = false
        var aborted = false
        override fun commit(): Any? { committed = true; return "mem" }
        override fun abort() { aborted = true }
    }

    private val sent = LinkedBlockingQueue<Wire.Message>()
    private val events = LinkedBlockingQueue<FileTransferCore.Event>()
    private val sinks = mutableListOf<MemSink>()
    private val core = FileTransferCore({ sent.add(it) }, { _, _, _ -> MemSink().also { sinks += it } }, { events.add(it) }, ackTimeoutMs = 2000)

    private fun next(): Wire.Message = sent.poll(5, TimeUnit.SECONDS) ?: error("nothing sent")

    @Test
    fun aFileFromThePcIsWrittenAcknowledgedAndCommitted() {
        val id = 0x8000_0001L
        val body = ByteArray(10 * 1000) { it.toByte() }
        core.onMessage(Wire.Message.FileOffer(id, body.size.toLong(), "../report.pdf", "application/pdf"))
        assertEquals(Wire.Message.FileControl(id, Wire.FILE_ACCEPT), next())
        for (i in 0 until 10) core.onMessage(Wire.Message.FileChunk(id, i * 1000L, body.copyOfRange(i * 1000, (i + 1) * 1000)))
        assertEquals(Wire.Message.FileControl(id, Wire.FILE_ACK), next())
        core.onMessage(Wire.Message.FileControl(id, Wire.FILE_COMPLETE))
        assertArrayEquals(body, sinks.single().out.toByteArray())
        assertTrue(sinks.single().committed)
        assertEquals(FileTransferCore.Event.Received("report.pdf", "mem"), events.poll(1, TimeUnit.SECONDS))
    }

    @Test
    fun aShortOrDisorderedFileIsAbortedAndTheHostTold() {
        val id = 0x8000_0002L
        core.onMessage(Wire.Message.FileOffer(id, 10, "a", ""))
        next()
        core.onMessage(Wire.Message.FileChunk(id, 4, ByteArray(2)))
        assertEquals(Wire.Message.FileControl(id, Wire.FILE_CANCEL), next())
        assertTrue(sinks.single().aborted)
        // A phone-range id from the host is refused.
        core.onMessage(Wire.Message.FileOffer(5, 1, "x", ""))
        assertEquals(Wire.Message.FileControl(5, Wire.FILE_REJECT), next())
    }

    @Test
    fun sendingWaitsForAcksSoTheQueueStaysSmall() {
        val body = ByteArray(FileTransferCore.CHUNK * 40 + 7) { (it % 251).toByte() }
        val id = core.offer("big.bin", "application/octet-stream", body.size.toLong()) { ByteArrayInputStream(body) }
        val offer = next() as Wire.Message.FileOffer
        assertEquals(id, offer.id)
        assertTrue(id and FileTransferCore.HOST_ID_BIT == 0L)
        core.onMessage(Wire.Message.FileControl(id, Wire.FILE_ACCEPT))
        val got = ByteArrayOutputStream()
        var chunks = 0
        var maxInFlight = 0
        var acked = 0
        while (true) {
            val m = next()
            if (m is Wire.Message.FileControl) {
                assertEquals(Wire.FILE_COMPLETE, m.op)
                break
            }
            m as Wire.Message.FileChunk
            assertEquals(got.size().toLong(), m.offset)
            got.write(m.data)
            chunks++
            maxInFlight = maxOf(maxInFlight, chunks - acked)
            if (chunks % Wire.FILE_ACK_EVERY == 0) {
                // Acknowledge a little late, as a real receiver would.
                Thread.sleep(5)
                acked += Wire.FILE_ACK_EVERY
                core.onMessage(Wire.Message.FileControl(id, Wire.FILE_ACK))
            }
        }
        assertArrayEquals(body, got.toByteArray())
        assertTrue("never more than the window in flight: $maxInFlight", maxInFlight <= FileTransferCore.WINDOW)
        assertEquals(FileTransferCore.Event.Sent("big.bin"), events.poll(2, TimeUnit.SECONDS))
    }

    @Test
    fun namesFollowTheHostsRules() {
        assertEquals("passwd", FileTransferCore.safeName("../../etc/passwd"))
        assertEquals("x.dll", FileTransferCore.safeName("C:\\Windows\\x.dll"))
        assertEquals("bashrc", FileTransferCore.safeName(".bashrc"))
        assertEquals("what_.txt", FileTransferCore.safeName("what?.txt"))
        assertEquals("file", FileTransferCore.safeName(""))
    }
}
