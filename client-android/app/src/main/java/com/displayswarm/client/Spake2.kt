package com.displayswarm.client

import java.math.BigInteger
import java.security.MessageDigest
import java.security.SecureRandom
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

/**
 * SPAKE2 (RFC 9382), ciphersuite SPAKE2-P256-SHA256-HKDF-HMAC, mirroring
 * `host/src/transport/spake2.rs`: A is the phone, B the host.
 *
 * The P-256 arithmetic is plain affine [BigInteger] math. It is not constant
 * time, which is acceptable here: the secrets are one-shot (a fresh scalar per
 * run, a PIN that is useless once the offer is used or expires) and the
 * exchange happens once, over a local link. Both implementations are checked
 * against the RFC's test vector.
 */
object Spake2 {
    private val P = BigInteger("ffffffff00000001000000000000000000000000ffffffffffffffffffffffff", 16)
    private val A = P.subtract(BigInteger.valueOf(3))
    private val B = BigInteger("5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b", 16)
    val ORDER = BigInteger("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551", 16)
    private val G = Point(
        BigInteger("6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296", 16),
        BigInteger("4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5", 16)
    )

    /** RFC 9382 section 6, P-256. */
    private val M = decompress("02886e2f97ace46e55ba9dd7242579f2993b64e16ef3dcab95afd497333d8fa12f")
    private val N = decompress("03d8bbd6c639c62937b04d997f38c3770719c629d7014d49a24b4f98baa1292b49")

    val AAD = "displayswarm-pair-pake-v1".toByteArray()
    const val POINT_LEN = 65
    const val CONFIRM_LEN = 32

    /** An affine point; `null` stands for the point at infinity. */
    data class Point(val x: BigInteger, val y: BigInteger)

    private fun add(p: Point?, q: Point?): Point? {
        if (p == null) return q
        if (q == null) return p
        val lambda = if (p.x == q.x) {
            if (p.y.add(q.y).mod(P).signum() == 0) return null
            p.x.pow(2).multiply(BigInteger.valueOf(3)).add(A).multiply(p.y.shiftLeft(1).modInverse(P)).mod(P)
        } else {
            q.y.subtract(p.y).multiply(q.x.subtract(p.x).modInverse(P)).mod(P)
        }
        val x = lambda.pow(2).subtract(p.x).subtract(q.x).mod(P)
        val y = lambda.multiply(p.x.subtract(x)).subtract(p.y).mod(P)
        return Point(x, y)
    }

    private fun neg(p: Point?): Point? = p?.let { Point(it.x, P.subtract(it.y).mod(P)) }

    private fun mul(p: Point?, k: BigInteger): Point? {
        var r: Point? = null
        var addend = p
        var e = k.mod(ORDER)
        while (e.signum() > 0) {
            if (e.testBit(0)) r = add(r, addend)
            addend = add(addend, addend)
            e = e.shiftRight(1)
        }
        return r
    }

    private fun onCurve(p: Point): Boolean =
        p.x < P && p.y < P && p.y.pow(2).mod(P) == p.x.pow(3).add(A.multiply(p.x)).add(B).mod(P)

    private fun decompress(hex: String): Point {
        val b = Pairing.fromHex(hex)!!
        val x = BigInteger(1, b.copyOfRange(1, 33))
        val rhs = x.pow(3).add(A.multiply(x)).add(B).mod(P)
        var y = rhs.modPow(P.add(BigInteger.ONE).shiftRight(2), P) // P = 3 mod 4
        if (y.testBit(0) != (b[0].toInt() == 3)) y = P.subtract(y)
        return Point(x, y)
    }

    private fun fixed(v: BigInteger, len: Int): ByteArray {
        val raw = v.toByteArray().let { if (it.size > len) it.copyOfRange(it.size - len, it.size) else it }
        return ByteArray(len - raw.size) + raw
    }

    fun encode(p: Point?): ByteArray {
        requireNotNull(p) { "the point at infinity has no encoding" }
        return byteArrayOf(4) + fixed(p.x, 32) + fixed(p.y, 32)
    }

    /** A received point, or null unless it is a valid uncompressed point of the curve. */
    fun decode(b: ByteArray): Point? {
        if (b.size != POINT_LEN || b[0].toInt() != 4) return null
        val p = Point(BigInteger(1, b.copyOfRange(1, 33)), BigInteger(1, b.copyOfRange(33, 65)))
        return if (onCurve(p)) p else null
    }

    /** `SHA-512("displayswarm-pin-v1" || PIN) mod n`, as on the host. */
    fun passwordScalar(pin: String): BigInteger {
        val md = MessageDigest.getInstance("SHA-512")
        md.update("displayswarm-pin-v1".toByteArray())
        md.update(pin.trim().toByteArray(Charsets.UTF_8))
        return BigInteger(1, md.digest()).mod(ORDER)
    }

    fun identityA(deviceId: String) = "displayswarm-phone:$deviceId".toByteArray(Charsets.UTF_8)
    fun identityB(fingerprint: ByteArray) = "displayswarm-host:${Pairing.toHex(fingerprint)}".toByteArray()

    private fun hmac(key: ByteArray, data: ByteArray): ByteArray {
        val mac = Mac.getInstance("HmacSHA256")
        mac.init(SecretKeySpec(key, "HmacSHA256"))
        return mac.doFinal(data)
    }

    /** HKDF-SHA256 with an empty salt, 32 bytes out. */
    private fun hkdf32(ikm: ByteArray, info: ByteArray): ByteArray = hmac(hmac(ByteArray(32), ikm), info + byteArrayOf(1))

    private fun len8(part: ByteArray): ByteArray {
        val n = part.size.toLong()
        return ByteArray(8) { i -> (n ushr (8 * i)).toByte() } + part
    }

    /** The keys one run derives. */
    class Keys(val transcript: ByteArray, val ke: ByteArray, val kcA: ByteArray, val kcB: ByteArray) {
        fun confirmA(): ByteArray = hmac(kcA, transcript)
        fun confirmB(): ByteArray = hmac(kcB, transcript)
    }

    fun derive(a: ByteArray, b: ByteArray, pA: ByteArray, pB: ByteArray, k: Point?, w: BigInteger, aad: ByteArray): Keys {
        val tt = len8(a) + len8(b) + len8(pA) + len8(pB) + len8(encode(k)) + len8(fixed(w, 32))
        val hash = MessageDigest.getInstance("SHA-256").digest(tt)
        val kc = hkdf32(hash.copyOfRange(16, 32), "ConfirmationKeys".toByteArray() + aad)
        return Keys(tt, hash.copyOfRange(0, 16), kc.copyOfRange(0, 16), kc.copyOfRange(16, 32))
    }

    private fun randomScalar(rng: SecureRandom): BigInteger {
        while (true) {
            val x = BigInteger(256, rng)
            if (x.signum() > 0 && x < ORDER) return x
        }
    }

    /** The phone's side of a run. */
    class PartyA internal constructor(
        private val w: BigInteger,
        private val x: BigInteger,
        private val a: ByteArray,
        private val b: ByteArray
    ) {
        val share: ByteArray = encode(add(mul(M, w), mul(G, x)))

        /** The keys from the host's pB, or null for an invalid point. */
        fun finish(pB: ByteArray): Keys? {
            val peer = decode(pB) ?: return null
            val k = mul(add(peer, neg(mul(N, w))), x) ?: return null
            return derive(a, b, share, pB, k, w, AAD)
        }
    }

    fun startA(w: BigInteger, a: ByteArray, b: ByteArray, rng: SecureRandom = SecureRandom()) =
        PartyA(w, randomScalar(rng), a, b)

    /** For the RFC vector test: a run with a given secret. */
    internal fun startAWith(w: BigInteger, x: BigInteger, a: ByteArray, b: ByteArray) = PartyA(w, x, a, b)

    /** B's share for a given secret (tests). */
    internal fun shareB(w: BigInteger, y: BigInteger): ByteArray = encode(add(mul(N, w), mul(G, y)))

    /** B's K from pA (tests). */
    internal fun keyB(pA: ByteArray, w: BigInteger, y: BigInteger): Point? = mul(add(decode(pA), neg(mul(M, w))), y)
}
