package com.displayswarm.client

import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.ByteArrayInputStream

class PairingTest {
    private val fp = ByteArray(32) { 7 }

    private fun reader(vararg r: Wire.Message.PairResponse): Wire.FrameReader =
        Wire.FrameReader(ByteArrayInputStream(r.fold(ByteArray(0)) { acc, m -> acc + m.encode() }))

    private fun resp(status: Int, msg: String = "", token: ByteArray = ByteArray(0)) =
        Wire.Message.PairResponse(status, msg, token)

    private fun sent(writes: List<ByteArray>): List<Wire.Message.PairRequest> {
        val r = Wire.FrameReader(ByteArrayInputStream(writes.fold(ByteArray(0)) { a, b -> a + b }))
        return writes.map { r.next() as Wire.Message.PairRequest }
    }

    @Test
    fun proofMatchesTheHostVector() {
        // Same value asserted in host/src/transport/pairing.rs
        assertEquals(
            "32bfa3ca8a127b2f7368af0ba49477c498ddd7f02b9116fa878ae37000f86cc0",
            Pairing.toHex(Pairing.proof("123456".toByteArray(), fp, "dev1"))
        )
    }

    @Test
    fun pinPairingReturnsTheNewToken() = runBlocking {
        val writes = mutableListOf<ByteArray>()
        var got: ByteArray? = null
        val sec = NetSecurity("dev1", "Phone", pinProvider = { "123456" }, onAuthenticated = { _, t -> got = t })
        Pairing.perform(
            reader(resp(Wire.PAIR_PIN_REQUIRED), resp(Wire.PAIR_OK, "Paired", byteArrayOf(1, 2, 3))),
            { writes += it }, sec, fp
        )
        assertArrayEquals(byteArrayOf(1, 2, 3), got)
        val reqs = sent(writes)
        assertEquals(Wire.PAIR_MODE_PIN, reqs[0].mode)
        assertEquals(0, reqs[0].credential.size)
        assertArrayEquals(Pairing.proof("123456".toByteArray(), fp, "dev1"), reqs[1].credential)
    }

    @Test
    fun wrongPinAsksAgainThenLockoutIsReportedToTheUser() = runBlocking {
        var asked = 0
        val sec = NetSecurity("dev1", "Phone", pinProvider = { asked++; "000000" })
        try {
            Pairing.perform(
                reader(resp(Wire.PAIR_PIN_REQUIRED), resp(Wire.PAIR_WRONG, "2 left"), resp(Wire.PAIR_LOCKED, "later")),
                {}, sec, fp
            )
            fail()
        } catch (e: PairingException) {
            assertTrue(e.message!!.startsWith("Pairing locked"))
        }
        assertEquals(2, asked)
    }

    @Test
    fun trustedReconnectSendsOnlyTheToken() = runBlocking {
        val writes = mutableListOf<ByteArray>()
        var newToken: ByteArray? = byteArrayOf(9)
        val sec = NetSecurity("dev1", "Phone", token = byteArrayOf(5, 5), onAuthenticated = { _, t -> newToken = t })
        Pairing.perform(reader(resp(Wire.PAIR_OK)), { writes += it }, sec, fp)
        assertNull(newToken)
        val reqs = sent(writes)
        assertEquals(1, reqs.size)
        assertEquals(Wire.PAIR_MODE_TOKEN, reqs[0].mode)
    }

    @Test
    fun forgottenTokenFallsBackToAPin() = runBlocking {
        val writes = mutableListOf<ByteArray>()
        val sec = NetSecurity("dev1", "Phone", token = byteArrayOf(5), pinProvider = { "111111" })
        Pairing.perform(
            reader(resp(Wire.PAIR_UNTRUSTED), resp(Wire.PAIR_PIN_REQUIRED), resp(Wire.PAIR_OK, "", byteArrayOf(4))),
            { writes += it }, sec, fp
        )
        assertEquals(listOf(Wire.PAIR_MODE_TOKEN, Wire.PAIR_MODE_PIN, Wire.PAIR_MODE_PIN), sent(writes).map { it.mode })
    }

    @Test
    fun cancellingThePinDialogEndsPairing() = runBlocking {
        val sec = NetSecurity("dev1", "Phone", pinProvider = { null })
        try {
            Pairing.perform(reader(resp(Wire.PAIR_PIN_REQUIRED)), {}, sec, fp)
            fail()
        } catch (e: PairingException) {
            assertEquals("Pairing cancelled", e.message)
        }
    }

    @Test
    fun qrPairingUsesTheScannedSecret() = runBlocking {
        val writes = mutableListOf<ByteArray>()
        val sec = NetSecurity("dev1", "Phone", qrSecret = byteArrayOf(1, 2))
        Pairing.perform(reader(resp(Wire.PAIR_OK, "", byteArrayOf(3))), { writes += it }, sec, fp)
        val req = sent(writes).single()
        assertEquals(Wire.PAIR_MODE_QR, req.mode)
        assertArrayEquals(Pairing.proof(byteArrayOf(1, 2), fp, "dev1"), req.credential)
    }

    @Test
    fun qrPayloadParses() {
        val hex = "ab".repeat(32)
        val q = QrPairing.parse("displayswarm://pair?h=192.168.1.5&p=9999&fp=$hex&s=0011&n=my%20pc")
        assertNotNull(q)
        assertEquals("192.168.1.5", q!!.host)
        assertEquals(9999, q.port)
        assertEquals("my pc", q.hostName)
        assertArrayEquals(byteArrayOf(0, 0x11), q.secret)
        assertNull(QrPairing.parse("http://example.com"))
        assertNull(QrPairing.parse("displayswarm://pair?h=x&p=1&fp=abcd&s=00"))
    }

    @Test
    fun hostInfoFromTxtAndAutoConnect() {
        val txt = mapOf(
            "v" to "2".toByteArray(), "name" to "desk".toByteArray(),
            "id" to "abababababababab".toByteArray(), "pair" to "1".toByteArray()
        )
        val h = HostInfo.fromTxt("inst", "192.168.1.5", 9999, txt)!!
        assertEquals("desk", h.name)
        assertTrue(h.pairingRequired)
        assertNull(HostInfo.fromTxt("inst", "192.168.1.5", 9999, emptyMap()))

        val known = KnownHost("desk", "10.0.0.1", 9999, "ab".repeat(32), "0102")
        assertEquals(h to known, pickAutoConnect(listOf(h), listOf(known), true))
        assertNull(pickAutoConnect(listOf(h), listOf(known), false))
        assertNull(pickAutoConnect(listOf(h), listOf(known.copy(fingerprintHex = "cd".repeat(32))), true))
        assertFalse(known.matches(h.copy(idPrefix = "")))
    }
}
