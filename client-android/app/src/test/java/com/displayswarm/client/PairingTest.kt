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
import java.io.InputStream
import java.math.BigInteger

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

    /**
     * A host that runs its side of SPAKE2 with [pin] on whatever the phone sends:
     * the phone writes a request, the next read returns the answer.
     */
    private class FakeHost(val pin: String, val hostFp: ByteArray, val token: ByteArray = ByteArray(32) { 3 }) {
        private var pending = ByteArray(0)
        private var pos = 0
        private var keys: Spake2.Keys? = null
        val requests = mutableListOf<Wire.Message.PairRequest>()
        /** Replace the host's confirmation with garbage (a host that does not know the PIN). */
        var forgeConfirmation = false
        /** Answer every PIN request the way a host from before SPAKE2 did. */
        var old = false

        fun write(bytes: ByteArray) {
            val req = Wire.FrameReader(ByteArrayInputStream(bytes)).next() as Wire.Message.PairRequest
            requests += req
            val reply = answer(req)
            pending = pending.copyOfRange(pos, pending.size) + reply.encode()
            pos = 0
        }

        private fun answer(r: Wire.Message.PairRequest): Wire.Message.PairResponse {
            if (r.mode == Wire.PAIR_MODE_TOKEN) return Wire.Message.PairResponse(Wire.PAIR_UNTRUSTED, "", ByteArray(0))
            if (old) return Wire.Message.PairResponse(Wire.PAIR_WRONG, "Unknown pairing mode", ByteArray(0))
            if (r.credential.isEmpty()) return Wire.Message.PairResponse(Wire.PAIR_PIN_REQUIRED, "Enter the PIN", ByteArray(0))
            val w = Spake2.passwordScalar(pin)
            val ida = Spake2.identityA(r.deviceId)
            val idb = Spake2.identityB(hostFp)
            val body = r.credential.copyOfRange(1, r.credential.size)
            return when (r.credential[0].toInt()) {
                Wire.PAKE_SHARE -> {
                    val y = BigInteger("5eed5eed5eed5eed5eed", 16)
                    val pB = Spake2.shareB(w, y)
                    keys = Spake2.derive(ida, idb, body, pB, Spake2.keyB(body, w, y), w, Spake2.AAD)
                    Wire.Message.PairResponse(Wire.PAIR_PAKE_SHARE, "", pB)
                }
                else -> {
                    val k = keys!!
                    if (!body.contentEquals(k.confirmA())) {
                        Wire.Message.PairResponse(Wire.PAIR_WRONG, "Wrong PIN. 2 attempt(s) left.", ByteArray(0))
                    } else {
                        val cb = if (forgeConfirmation) ByteArray(32) else k.confirmB()
                        Wire.Message.PairResponse(Wire.PAIR_OK, "Paired", token + cb)
                    }
                }
            }
        }

        val input = object : InputStream() {
            override fun read(): Int = if (pos < pending.size) pending[pos++].toInt() and 0xff else -1
        }
    }

    private suspend fun pairWith(host: FakeHost, sec: NetSecurity, seenFp: ByteArray = fp) =
        Pairing.perform(Wire.FrameReader(host.input), { host.write(it) }, sec, seenFp)

    @Test
    fun pinPairingRunsSpake2AndReturnsTheNewToken() = runBlocking {
        val host = FakeHost("123456", fp)
        var got: ByteArray? = null
        pairWith(host, NetSecurity("dev1", "Phone", pinProvider = { "123456" }, onAuthenticated = { _, t -> got = t }))
        assertArrayEquals(ByteArray(32) { 3 }, got)
        assertEquals(listOf(Wire.PAIR_MODE_PAKE, Wire.PAIR_MODE_PAKE, Wire.PAIR_MODE_PAKE), host.requests.map { it.mode })
        assertEquals(0, host.requests[0].credential.size)
        assertEquals(1 + Spake2.POINT_LEN, host.requests[1].credential.size)
        // The PIN itself never goes over the wire, in any form a host could check offline.
        assertFalse(host.requests.any { it.credential.contentEquals(Pairing.proof("123456".toByteArray(), fp, "dev1")) })
    }

    @Test
    fun aWrongPinAsksAgain() = runBlocking {
        val host = FakeHost("123456", fp)
        val pins = ArrayDeque(listOf("000000", "123456"))
        var hints = mutableListOf<String>()
        var got: ByteArray? = null
        pairWith(host, NetSecurity("dev1", "Phone", pinProvider = { hints += it; pins.removeFirst() }, onAuthenticated = { _, t -> got = t }))
        assertNotNull(got)
        assertEquals(2, hints.size)
        assertTrue(hints[1].startsWith("Wrong PIN"))
    }

    @Test
    fun aHostThatCannotProveThePinIsNotTrusted() = runBlocking {
        val host = FakeHost("123456", fp).apply { forgeConfirmation = true }
        var got: ByteArray? = null
        try {
            pairWith(host, NetSecurity("dev1", "Phone", pinProvider = { "123456" }, onAuthenticated = { _, t -> got = t }))
            fail()
        } catch (e: PairingException) {
            assertTrue(e.message!!.contains("could not prove"))
        }
        assertNull(got)
    }

    @Test
    fun aPhoneThatSawAnotherCertificateCannotPair() = runBlocking {
        // A man in the middle shows the phone its own certificate.
        val host = FakeHost("123456", fp)
        val pins = ArrayDeque(listOf("123456"))
        try {
            pairWith(host, NetSecurity("dev1", "Phone", pinProvider = { pins.removeFirstOrNull() }), seenFp = ByteArray(32) { 9 })
            fail()
        } catch (e: PairingException) {
            assertEquals("Pairing cancelled", e.message)
        }
    }

    @Test
    fun forgottenTokenFallsBackToSpake2() = runBlocking {
        val host = FakeHost("111111", fp)
        pairWith(host, NetSecurity("dev1", "Phone", token = byteArrayOf(5), pinProvider = { "111111" }))
        assertEquals(
            listOf(Wire.PAIR_MODE_TOKEN, Wire.PAIR_MODE_PAKE, Wire.PAIR_MODE_PAKE, Wire.PAIR_MODE_PAKE),
            host.requests.map { it.mode }
        )
    }

    @Test
    fun anOldHostIsReportedAsNeedingAnUpdate() = runBlocking {
        val host = FakeHost("123456", fp).apply { old = true }
        try {
            pairWith(host, NetSecurity("dev1", "Phone", pinProvider = { "123456" }))
            fail()
        } catch (e: PairingException) {
            assertTrue(e.message!!.contains("Update DisplaySwarm on the PC"))
        }
    }

    @Test
    fun lockoutIsReportedToTheUser() = runBlocking {
        val sec = NetSecurity("dev1", "Phone", pinProvider = { "000000" })
        try {
            Pairing.perform(reader(resp(Wire.PAIR_LOCKED, "later")), {}, sec, fp)
            fail()
        } catch (e: PairingException) {
            assertTrue(e.message!!.startsWith("Pairing locked"))
        }
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
    fun knownHostTokensAreStoredSealedAndLegacyOnesStillRead() {
        var text: String? = """[{"name":"desk","host":"h","port":1,"fp":"${"ab".repeat(32)}","token":"0102","seen":0}]"""
        val store = KnownHostStore({ text }, { text = it }, seal = { "sealed:" + it.reversed() }, open = {
            if (it.startsWith("sealed:")) it.removePrefix("sealed:").reversed() else it
        })
        assertEquals("a token from before sealing is read as is", "0102", store.all().single().tokenHex)
        store.put(KnownHost("pc2", "h2", 2, "cd".repeat(32), "0a0b"))
        assertFalse(text!!.contains("\"0102\"") || text!!.contains("\"0a0b\""))
        assertEquals(listOf("0102", "0a0b"), store.all().map { it.tokenHex })
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
