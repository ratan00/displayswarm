package com.displayswarm.client

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test
import java.math.BigInteger

class Spake2Test {
    private fun hex(b: ByteArray) = Pairing.toHex(b)
    private fun big(h: String) = BigInteger(h, 16)

    /** RFC 9382 Appendix B, first vector (A='server', B='client'; the RFC uses no AAD). */
    @Test
    fun rfc9382Vector() {
        val w = big("2ee57912099d31560b3a44b1184b9b4866e904c49d12ac5042c97dca461b1a5f")
        val x = big("43dd0fd7215bdcb482879fca3220c6a968e66d70b1356cac18bb26c84a78d729")
        val y = big("dcb60106f276b02606d8ef0a328c02e4b629f84f89786af5befb0bc75b6e66be")
        val a = Spake2.startAWith(w, x, "server".toByteArray(), "client".toByteArray())
        assertEquals(
            "04a56fa807caaa53a4d28dbb9853b9815c61a411118a6fe516a8798434751470f9010153ac33d0d5f2047ffdb1a3e42c9b4e6be662766e1eeb4116988ede5f912c",
            hex(a.share)
        )
        val pB = Spake2.shareB(w, y)
        assertEquals(
            "0406557e482bd03097ad0cbaa5df82115460d951e3451962f1eaf4367a420676d09857ccbc522686c83d1852abfa8ed6e4a1155cf8f1543ceca528afb591a1e0b7",
            hex(pB)
        )
        val k = Spake2.keyB(a.share, w, y)
        assertEquals(
            "0412af7e89717850671913e6b469ace67bd90a4df8ce45c2af19010175e37eed69f75897996d539356e2fa6a406d528501f907e04d97515fbe83db277b715d3325",
            hex(Spake2.encode(k))
        )
        val keys = Spake2.derive("server".toByteArray(), "client".toByteArray(), a.share, pB, k, w, ByteArray(0))
        assertEquals("0e0672dc86f8e45565d338b0540abe69", hex(keys.ke))
        assertEquals("00c12546835755c86d8c0db7851ae86f", hex(keys.kcA))
        assertEquals("a9fa3406c3b781b93d804485430ca27a", hex(keys.kcB))
        assertEquals("58ad4aa88e0b60d5061eb6b5dd93e80d9c4f00d127c65b3b35b1b5281fee38f0", hex(keys.confirmA()))
        assertEquals("d3e2e547f1ae04f2dbdbf0fc4b79f8ecff2dff314b5d32fe9fcef2fb26dc459b", hex(keys.confirmB()))
    }

    /** Shared with `host/src/transport/spake2.rs`. */
    @Test
    fun passwordScalarMatchesTheHost() {
        assertEquals(
            "50da975e3f2ea678ac9d6751c48ed13349df4a60c8abfa3c5569f33766eafa6b",
            Spake2.passwordScalar("123456").toString(16).padStart(64, '0')
        )
    }

    @Test
    fun theSamePinAgreesAndAnotherDoesNot() {
        val ida = Spake2.identityA("p")
        val idb = Spake2.identityB(ByteArray(32) { 7 })
        val w = Spake2.passwordScalar("123456")
        val a = Spake2.startA(w, ida, idb)
        val y = BigInteger("1234567890abcdef", 16)
        val pB = Spake2.shareB(w, y)
        val mine = a.finish(pB)!!
        val hostK = Spake2.keyB(a.share, w, y)
        val host = Spake2.derive(ida, idb, a.share, pB, hostK, w, Spake2.AAD)
        assertArrayEquals(host.confirmA(), mine.confirmA())
        val otherW = Spake2.passwordScalar("654321")
        val wrongHost = Spake2.derive(ida, idb, a.share, Spake2.shareB(otherW, y), Spake2.keyB(a.share, otherW, y), otherW, Spake2.AAD)
        assertFalse(wrongHost.confirmA().contentEquals(a.finish(Spake2.shareB(otherW, y))!!.confirmA()))
    }

    @Test
    fun invalidPointsAreRefused() {
        assertNull(Spake2.decode(ByteArray(65) { 4 }))
        assertNull(Spake2.decode(ByteArray(33)))
        assertNotNull(Spake2.decode(Spake2.shareB(BigInteger.ONE, BigInteger.TEN)))
    }
}
