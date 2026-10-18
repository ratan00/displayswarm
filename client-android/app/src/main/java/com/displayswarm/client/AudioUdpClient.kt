package com.displayswarm.client

import android.util.Log
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.SocketTimeoutException
import java.nio.ByteBuffer
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/**
 * The phone's end of the host's UDP audio path (host: `transport/audio_udp.rs`,
 * which documents the format). Over TCP every audio frame waited behind video
 * and behind any lost Wi-Fi packet's retransmission; over UDP a late frame is
 * simply concealed, and each datagram repeats the previous frame so a single
 * loss costs nothing.
 *
 * Set up from the session's [Wire.Message.AudioUdpOffer]: bind next to the TCP
 * socket, tell the host our port ([Wire.Message.AudioUdpAnswer]), then send an
 * authenticated probe every second. The host sends audio here while probes
 * arrive and falls back to TCP on its own when they stop, so nothing breaks if
 * UDP is blocked. Received frames go through [deliver] exactly like TCP ones;
 * the player drops the duplicates by sequence number.
 */
class AudioUdpClient(
    private val local: InetAddress?,
    private val host: InetAddress,
    private val hostPort: Int,
    key: ByteArray,
    private val sendControl: (Wire.Message) -> Unit,
    private val deliver: (Wire.Message.Audio) -> Unit,
) {
    private val crypto = Crypto(key)

    @Volatile
    private var running = false
    private var socket: DatagramSocket? = null
    private var thread: Thread? = null

    fun start() {
        val s = try {
            DatagramSocket(InetSocketAddress(local, 0)).apply {
                soTimeout = PROBE_INTERVAL_MS.toInt()
                receiveBufferSize = 256 * 1024
                trafficClass = 0xB8 // DSCP EF: ask Wi-Fi for its voice queue
            }
        } catch (e: Exception) {
            Log.w(TAG, "no UDP socket; audio stays on the session", e)
            return
        }
        socket = s
        running = true
        sendControl(Wire.Message.AudioUdpAnswer(s.localPort))
        thread = Thread({ loop(s) }, "audio-udp").also { it.start() }
        Log.i(TAG, "UDP audio: listening on ${s.localPort}, host $host:$hostPort")
    }

    fun stop() {
        running = false
        socket?.close()
        thread?.join(500)
        thread = null
        socket = null
    }

    private fun loop(s: DatagramSocket) {
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)
        val buf = ByteArray(4096)
        val packet = DatagramPacket(buf, buf.size)
        val dest = InetSocketAddress(host, hostPort)
        var counter = 0L
        var lastProbe = 0L
        var announced = false
        while (running) {
            val now = System.nanoTime() / 1_000_000
            if (now - lastProbe >= PROBE_INTERVAL_MS) {
                lastProbe = now
                try {
                    val d = crypto.seal(DIR_PHONE, counter++, byteArrayOf(KIND_PROBE))
                    s.send(DatagramPacket(d, d.size, dest))
                } catch (e: Exception) {
                    if (!running) return
                }
            }
            try {
                packet.setLength(buf.size)
                s.receive(packet)
            } catch (_: SocketTimeoutException) {
                continue
            } catch (e: Exception) {
                if (running) Log.w(TAG, "UDP audio receive failed", e)
                return
            }
            if (packet.address != host) continue
            val plain = crypto.open(DIR_HOST, buf, packet.length) ?: continue
            val frames = decodeAudio(plain) ?: continue
            if (!announced) {
                announced = true
                Log.i(TAG, "UDP audio: receiving")
            }
            frames.forEach(deliver)
        }
    }

    /** AES-256-GCM as the host seals it. */
    class Crypto(key: ByteArray) {
        private val spec = SecretKeySpec(key, "AES")
        private val cipher = Cipher.getInstance("AES/GCM/NoPadding")

        private fun nonce(dir: ByteArray, counter: Long) =
            ByteBuffer.allocate(12).put(dir).putLong(counter).array()

        fun seal(dir: ByteArray, counter: Long, plaintext: ByteArray): ByteArray {
            val header = ByteBuffer.allocate(HEADER).put(MAGIC).put(VERSION).putLong(counter).array()
            cipher.init(Cipher.ENCRYPT_MODE, spec, GCMParameterSpec(128, nonce(dir, counter)))
            cipher.updateAAD(header)
            return header + cipher.doFinal(plaintext)
        }

        /** The plaintext of `data[0, len)`, or null if it is not ours or was tampered with. */
        fun open(dir: ByteArray, data: ByteArray, len: Int = data.size): ByteArray? {
            if (len < HEADER + 16 || data[0] != MAGIC[0] || data[1] != MAGIC[1] || data[2] != VERSION) return null
            val counter = ByteBuffer.wrap(data, 3, 8).long
            return try {
                cipher.init(Cipher.DECRYPT_MODE, spec, GCMParameterSpec(128, nonce(dir, counter)))
                cipher.updateAAD(data, 0, HEADER)
                cipher.doFinal(data, HEADER, len - HEADER)
            } catch (_: Exception) {
                null
            }
        }
    }

    companion object {
        private const val TAG = "AudioUdp"
        val MAGIC = byteArrayOf('D'.code.toByte(), 'A'.code.toByte())
        const val VERSION: Byte = 1
        const val HEADER = 11
        const val KIND_AUDIO: Byte = 1
        const val KIND_PROBE: Byte = 2
        val DIR_HOST = "HOST".toByteArray(Charsets.US_ASCII)
        val DIR_PHONE = "PHON".toByteArray(Charsets.US_ASCII)
        const val PROBE_INTERVAL_MS = 1_000L

        /** Frames of an audio plaintext (oldest first), or null if it is malformed. */
        fun decodeAudio(plain: ByteArray): List<Wire.Message.Audio>? {
            if (plain.size < 2 || plain[0] != KIND_AUDIO) return null
            val b = ByteBuffer.wrap(plain, 2, plain.size - 2)
            val n = plain[1].toInt() and 0xFF
            val out = ArrayList<Wire.Message.Audio>(n)
            repeat(n) {
                if (b.remaining() < 15) return null
                val seq = b.int.toLong() and 0xFFFF_FFFFL
                val pts = b.long
                val ch = b.get().toInt() and 0xFF
                val len = b.short.toInt() and 0xFFFF
                if (b.remaining() < len) return null
                val data = ByteArray(len).also { b.get(it) }
                out += Wire.Message.Audio(Wire.MSG_AUDIO_FRAME, seq, pts, ch, data)
            }
            return out
        }
    }
}
