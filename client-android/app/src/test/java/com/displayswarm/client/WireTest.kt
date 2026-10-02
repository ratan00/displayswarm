package com.displayswarm.client

import com.displayswarm.client.Wire.Message
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.File

class WireTest {
    // ---- golden vectors ------------------------------------------------------

    /** The golden messages; mirrors `samples()` in host/src/protocol/wire/tests.rs. */
    private fun samples(): List<Triple<String, ByteArray, Message>> {
        fun one(name: String, m: Message) = Triple(name, m.encode(), m)
        val video = Message.VideoFrame(
            Wire.CODEC_H264, Wire.FRAME_TYPE_KEY, 9, 123_456, ByteArray(20) { it.toByte() }
        )
        val features = Wire.FEATURE_TOUCH or Wire.FEATURE_STYLUS or Wire.FEATURE_KEYBOARD
        return listOf(
            one("hello", Message.HelloMsg(Wire.Hello(
                1600, 720, 60_000, 320, Wire.CODEC_H264 or Wire.CODEC_HEVC, features, 10,
                "a1b2", "Galaxy M06", "2.0.0"
            ))),
            one("hello_ack", Message.HelloAckMsg(Wire.HelloAck(
                Wire.HELLO_OK, 1600, 720, 60, Wire.CODEC_H264, features, Wire.ROLE_MIRROR, "laptop", ""
            ))),
            one("hello_ack_mismatch", Message.HelloAckMsg(Wire.HelloAck(
                status = Wire.HELLO_VERSION_MISMATCH, hostName = "laptop", message = "Update the DisplaySwarm app"
            ))),
            one("ping", Message.Ping(0x01020304L, 0x0A0B0C0D0E0F1011L)),
            one("pong", Message.Pong(7, 1, 2, 3)),
            one("heartbeat", Message.Heartbeat),
            one("bye", Message.Bye(Wire.BYE_SERVER_STOPPING, "stopped")),
            one("stats", Message.StatsMsg(Wire.Stats(1, 2, 3, 4, 5))),
            one("host_state", Message.HostState(Wire.HOST_STATE_STREAMING, "ok")),
            one("keyframe_request", Message.KeyframeRequest),
            one("set_bitrate", Message.SetBitrate(8000)),
            one("resize", Message.Resize(1600, 720, 320, 1)),
            one("set_role", Message.SetRole(Wire.ROLE_EXTEND)),
            one("audio_latency", Message.AudioLatency(45_000)),
            one("audio_sync", Message.AudioSync(120_000)),
            one("service_state", Message.ServiceState(Wire.SERVICE_AUDIO_OUT or Wire.SERVICE_MIC)),
            one("video_frame", Message.VideoFrame(
                video.codec, video.frameType, video.frameIndex, video.captureUs, byteArrayOf(0, 0, 0, 1, 0x65)
            )),
            Triple("video_fragmented_16", video.encode(16), video),
            one("audio_frame", Message.Audio(Wire.MSG_AUDIO_FRAME, 3, 1000, 2, byteArrayOf(1, 2, 3))),
            one("mic_frame", Message.Audio(Wire.MSG_MIC_FRAME, 4, 2000, 1, byteArrayOf(9))),
            one("touch", Message.Touch(Wire.ACTION_DOWN, 1, listOf(
                Wire.TouchPoint(0, 0.25f, 0.5f, 1.0f), Wire.TouchPoint(1, 0.75f, 0.5f, 0.5f)
            ))),
            one("pen", Message.Pen(Wire.ACTION_MOVE, Wire.PEN_TOOL_PEN, Wire.PEN_BUTTON_PRIMARY, listOf(
                Wire.PenSample(4000, 0.5f, 0.5f, 0.25f, 0.0f, 0.0f),
                Wire.PenSample(0, 0.5f, 0.75f, 0.5f, 0.25f, -0.25f)
            ))),
            one("mouse", Message.Mouse(Wire.ACTION_DOWN, Wire.MOUSE_BUTTON_PRIMARY, false, 0.5f, 0.25f)),
            one("scroll", Message.Scroll(Wire.PHASE_NONE, 0.5f, 0.5f, 0.0f, -1.0f)),
            one("pinch", Message.Pinch(Wire.PHASE_BEGIN, 0.5f, 0.5f, 1.0f, 0.0f)),
            one("key", Message.Key(Wire.ACTION_DOWN, 29, 30, Wire.KEY_FLAG_SHIFT, "A")),
            one("text", Message.Text("héllo")),
            one("clipboard", Message.Clipboard("text/plain", "hi".toByteArray())),
            one("file_offer", Message.FileOffer(1, 1024, "a.txt", "text/plain")),
            one("file_chunk", Message.FileChunk(1, 0, byteArrayOf(1, 2))),
            one("file_control", Message.FileControl(1, Wire.FILE_ACCEPT)),
            one("battery_status", Message.BatteryStatus(17, false, Wire.THERMAL_SEVERE, 412)),
            one(
                "pair_request",
                Message.PairRequest(Wire.PAIR_MODE_PIN, "a1b2", "Galaxy M06", byteArrayOf(1, 2, 3, 4))
            ),
            one("pair_response", Message.PairResponse(Wire.PAIR_OK, "ok", byteArrayOf(9, 8, 7)))
        )
    }

    private fun findVectorFile(): File {
        var dir: File? = File("").absoluteFile
        while (dir != null) {
            val f = File(dir, "protocol/v2-vectors.txt")
            if (f.isFile) return f
            dir = dir.parentFile
        }
        throw AssertionError("protocol/v2-vectors.txt not found above ${File("").absolutePath}")
    }

    private fun parseVectors(): List<Pair<String, ByteArray>> =
        findVectorFile().readLines().map { it.trim() }
            .filter { it.isNotEmpty() && !it.startsWith("#") }
            .map { line ->
                val name = line.substringBefore(':').trim()
                val digits = line.substringAfter(':').filter { !it.isWhitespace() }
                name to ByteArray(digits.length / 2) { digits.substring(it * 2, it * 2 + 2).toInt(16).toByte() }
            }

    private fun hex(b: ByteArray) = b.joinToString(" ") { "%02x".format(it) }

    /** Decodes bytes holding exactly one (possibly fragmented) message. */
    private fun decodeAll(bytes: ByteArray): Message {
        val re = Wire.Reassembler()
        var pos = 0
        while (true) {
            val h = Wire.Header.parse(bytes.copyOfRange(pos, pos + Wire.HEADER_SIZE))
            h.check()
            val end = pos + Wire.HEADER_SIZE + h.len.toInt()
            val payload = bytes.copyOfRange(pos + Wire.HEADER_SIZE, end)
            pos = end
            val full = re.push(h, payload)
            if (full != null) {
                assertEquals("trailing bytes after the message", bytes.size, pos)
                return Message.decode(h.channel, h.type, full)
            }
        }
    }

    @Test
    fun encodersMatchGoldenVectors() {
        val vectors = parseVectors()
        val samples = samples()
        assertEquals(vectors.map { it.first }, samples.map { it.first })
        for ((v, s) in vectors.zip(samples)) {
            assertEquals("${v.first}: encoding differs", hex(v.second), hex(s.second))
            assertEquals("${v.first}: golden bytes decode differently", s.third, decodeAll(v.second))
        }
    }

    // ---- header --------------------------------------------------------------

    @Test
    fun headerLayoutIsFrozen() {
        val h = Wire.Header(Wire.VERSION, Wire.CH_VIDEO, Wire.MSG_VIDEO_FRAME, Wire.FLAG_MORE, 0x01020304L)
        val bytes = h.encode()
        assertArrayEquals(byteArrayOf(0x56, 0x4D, 2, 3, 0x20, 1, 1, 2, 3, 4), bytes)
        assertEquals(h, Wire.Header.parse(bytes))
    }

    private fun assertRejected(h: Wire.Header) {
        try {
            h.check()
            fail("expected ProtocolException for $h")
        } catch (_: ProtocolException) {
        }
    }

    @Test
    fun headerCheckRejectsBadFrames() {
        val ok = Wire.Header(Wire.VERSION, Wire.CH_CONTROL, Wire.MSG_PING, 0, 12)
        ok.check()
        assertRejected(ok.copy(version = 3))
        assertRejected(ok.copy(channel = 6))
        assertRejected(ok.copy(len = Wire.MAX_FRAGMENT + 1L))
        assertRejected(ok.copy(flags = 0x02))
        try {
            Wire.Header.parse(byteArrayOf('X'.code.toByte(), 'M'.code.toByte(), 2, 0, 3, 0, 0, 0, 0, 0))
            fail("bad magic accepted")
        } catch (_: ProtocolException) {
        }
    }

    // ---- reassembly ----------------------------------------------------------

    private fun splitFrames(bytes: ByteArray): List<ByteArray> {
        val list = ArrayList<ByteArray>()
        var pos = 0
        while (pos < bytes.size) {
            val h = Wire.Header.parse(bytes.copyOfRange(pos, pos + Wire.HEADER_SIZE))
            val end = pos + Wire.HEADER_SIZE + h.len.toInt()
            list += bytes.copyOfRange(pos, end)
            pos = end
        }
        return list
    }

    @Test
    fun interleavedPingsDoNotDisturbFragmentedVideo() {
        val video = Message.VideoFrame(Wire.CODEC_H264, Wire.FRAME_TYPE_KEY, 1, 2, ByteArray(100) { it.toByte() })
        val ping = Message.Ping(1, 2)
        // 14 B of fields + 100 B of data = 114 B -> 4 fragments of 32.
        val fragments = splitFrames(video.encode(32))
        assertEquals(4, fragments.size)
        val re = Wire.Reassembler()
        val out = ArrayList<Message>()
        for (f in fragments) {
            // each fragment is followed by a ping frame
            for (frame in listOf(f, ping.encode())) {
                val h = Wire.Header.parse(frame)
                h.check()
                val full = re.push(h, frame.copyOfRange(Wire.HEADER_SIZE, frame.size))
                if (full != null) out += Message.decode(h.channel, h.type, full)
            }
        }
        assertEquals(1, out.count { it is Message.VideoFrame })
        assertEquals(4, out.count { it is Message.Ping })
        assertEquals(video, out.first { it is Message.VideoFrame })
    }

    @Test
    fun typeChangeMidMessageThrows() {
        val re = Wire.Reassembler()
        val more = Wire.Header(Wire.VERSION, Wire.CH_VIDEO, Wire.MSG_VIDEO_FRAME, Wire.FLAG_MORE, 4)
        assertEquals(null, re.push(more, ByteArray(4)))
        try {
            re.push(more.copy(type = 0x21, flags = 0), ByteArray(4))
            fail("type change accepted")
        } catch (_: ProtocolException) {
        }
    }

    @Test
    fun inputChannelIsCappedAtMaxFragment() {
        val re = Wire.Reassembler()
        val more = Wire.Header(Wire.VERSION, Wire.CH_INPUT, Wire.MSG_TEXT, Wire.FLAG_MORE, Wire.MAX_FRAGMENT.toLong())
        assertEquals(null, re.push(more, ByteArray(Wire.MAX_FRAGMENT)))
        try {
            re.push(more, ByteArray(Wire.MAX_FRAGMENT))
            fail("input message beyond the cap accepted")
        } catch (_: ProtocolException) {
        }
    }

    // ---- decode --------------------------------------------------------------

    @Test
    fun unknownTypeDecodesToUnknown() {
        val m = Message.decode(Wire.CH_CONTROL, 0x7E, byteArrayOf(1, 2, 3))
        assertEquals(Message.Unknown(Wire.CH_CONTROL, 0x7E, byteArrayOf(1, 2, 3)), m)
    }

    @Test
    fun knownTypeOnWrongChannelThrows() {
        try {
            Message.decode(Wire.CH_INPUT, Wire.MSG_PING, ByteArray(12))
            fail("wrong channel accepted")
        } catch (_: ProtocolException) {
        }
    }

    @Test
    fun shortPayloadThrows() {
        try {
            Message.decode(Wire.CH_CONTROL, Wire.MSG_PING, ByteArray(11))
            fail("short payload accepted")
        } catch (_: ProtocolException) {
        }
    }

    @Test
    fun trailingBytesAreIgnored() {
        val payload = Message.Ping(5, 6).encodePayload() + byteArrayOf(9, 9, 9)
        assertEquals(Message.Ping(5, 6), Message.decode(Wire.CH_CONTROL, Wire.MSG_PING, payload))
    }

    @Test
    fun u32FieldsStayUnsigned() {
        val m = Message.SetBitrate(0xFFFFFFFFL)
        assertEquals(m, decodeAll(m.encode()))
        val p = Message.Ping(0xFFFFFFFFL, 1)
        assertEquals(p, decodeAll(p.encode()))
    }

    @Test
    fun longTextIsTruncatedOnAUtf8Boundary() {
        for (ch in listOf("€", "é", "😀")) {
            val text = ch.repeat(70_000)
            val payload = Message.Text(text).encodePayload()
            val len = ((payload[0].toInt() and 0xFF) shl 8) or (payload[1].toInt() and 0xFF)
            assertEquals(payload.size - 2, len)
            val decoded = (Message.decode(Wire.CH_INPUT, Wire.MSG_TEXT, payload) as Message.Text).text
            assertTrue(decoded.isNotEmpty() && text.startsWith(decoded))
            val bytes = decoded.toByteArray(Charsets.UTF_8).size
            assertTrue(bytes <= 0xFFFF)
            assertEquals(len, bytes)
        }
    }
}
