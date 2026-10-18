package com.displayswarm.client

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** The phone side of the UDP audio format, against the bytes the host test pins (audio_udp.rs). */
class AudioUdpClientTest {
    private fun unhex(s: String) = ByteArray(s.length / 2) { s.substring(it * 2, it * 2 + 2).toInt(16).toByte() }
    private val crypto = AudioUdpClient.Crypto(ByteArray(32) { 7 })
    private val hostVec = unhex("444101000000000000000575c4a9287d81833ea2a47b02111b2b40ceaa5bbfdf04f0c453cadfd5aecc2158afb7626e")
    private val probeVec = unhex("44410100000000000000016239f6858844460fab8ce3fc72788f43e1")

    @Test
    fun opensWhatTheHostSeals() {
        val plain = crypto.open(AudioUdpClient.DIR_HOST, hostVec)!!
        val frames = AudioUdpClient.decodeAudio(plain)!!
        assertEquals(1, frames.size)
        assertEquals(9L, frames[0].seq)
        assertEquals(90_000L, frames[0].ptsUs)
        assertEquals(2, frames[0].channels)
        assertArrayEquals(byteArrayOf(1, 2, 3), frames[0].data)
    }

    @Test
    fun sealsProbesExactlyLikeTheHostExpects() {
        assertArrayEquals(probeVec, crypto.seal(AudioUdpClient.DIR_PHONE, 1, byteArrayOf(AudioUdpClient.KIND_PROBE)))
    }

    @Test
    fun rejectsTamperingTheWrongDirectionAndTheWrongKey() {
        for (i in hostVec.indices) {
            val t = hostVec.copyOf().also { it[i] = (it[i].toInt() xor 1).toByte() }
            assertNull("byte $i", crypto.open(AudioUdpClient.DIR_HOST, t))
        }
        assertNull(crypto.open(AudioUdpClient.DIR_PHONE, hostVec))
        assertNull(AudioUdpClient.Crypto(ByteArray(32) { 8 }).open(AudioUdpClient.DIR_HOST, hostVec))
    }
}
