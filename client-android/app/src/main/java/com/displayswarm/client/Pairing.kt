package com.displayswarm.client

import java.io.IOException
import java.net.URLDecoder
import java.security.MessageDigest
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

/** The host refused or the user gave up pairing; the message is meant for the user. */
class PairingException(message: String) : IOException(message)

/** What the QR code on the host carries: where to connect and what to trust. */
data class QrPairing(
    val host: String,
    val port: Int,
    val fingerprint: ByteArray,
    val secret: ByteArray,
    val hostName: String
) {
    companion object {
        /** Parses `displayswarm://pair?h=..&p=..&fp=<hex64>&s=<hex>&n=..`, or null if it is not one. */
        fun parse(text: String): QrPairing? {
            val t = text.trim()
            if (!t.startsWith("displayswarm://pair?")) return null
            val q = t.substringAfter('?').split('&').mapNotNull {
                val i = it.indexOf('=')
                if (i < 0) null else it.substring(0, i) to URLDecoder.decode(it.substring(i + 1), "UTF-8")
            }.toMap()
            val fp = Pairing.fromHex(q["fp"] ?: return null)?.takeIf { it.size == 32 } ?: return null
            val secret = Pairing.fromHex(q["s"] ?: return null)?.takeIf { it.isNotEmpty() } ?: return null
            val port = q["p"]?.toIntOrNull()?.takeIf { it in 1..65535 } ?: return null
            val host = q["h"]?.takeIf { it.isNotBlank() } ?: return null
            return QrPairing(host, port, fp, secret, q["n"] ?: host)
        }
    }

    override fun equals(other: Any?) = other is QrPairing && host == other.host && port == other.port &&
        fingerprint.contentEquals(other.fingerprint) && secret.contentEquals(other.secret)

    override fun hashCode() = host.hashCode() * 31 + port
}

/** How a network connection authenticates: the pinned certificate and how to (re)pair. */
class NetSecurity(
    val deviceId: String,
    val deviceName: String,
    /** The host certificate's SHA-256, or null on first contact with a host. */
    val pinnedFingerprint: ByteArray? = null,
    /** Token from an earlier pairing. */
    val token: ByteArray? = null,
    /** One-time secret from a scanned QR code. */
    val qrSecret: ByteArray? = null,
    /** Asks the user for the PIN shown on the PC; null cancels. [String] is a hint/error to show. */
    val pinProvider: suspend (hint: String) -> String? = { null },
    /** Called once the host is authenticated: its fingerprint, and a new token if just paired. */
    val onAuthenticated: (fingerprint: ByteArray, newToken: ByteArray?) -> Unit = { _, _ -> }
)

/** The pre-Hello exchange with a host, mirroring `host/src/transport/pairing.rs`. */
object Pairing {
    private val CONTEXT = "displayswarm-pair-v1".toByteArray()

    fun proof(secret: ByteArray, fingerprint: ByteArray, deviceId: String): ByteArray {
        val mac = Mac.getInstance("HmacSHA256")
        mac.init(SecretKeySpec(secret, "HmacSHA256"))
        mac.update(CONTEXT)
        mac.update(fingerprint)
        mac.update(deviceId.toByteArray(Charsets.UTF_8))
        return mac.doFinal()
    }

    fun fingerprintOf(certDer: ByteArray): ByteArray = MessageDigest.getInstance("SHA-256").digest(certDer)

    fun toHex(b: ByteArray): String = b.joinToString("") { "%02x".format(it) }

    fun fromHex(s: String): ByteArray? {
        val t = s.filter { it != ':' && !it.isWhitespace() }
        if (t.length % 2 != 0) return null
        return try {
            ByteArray(t.length / 2) { t.substring(it * 2, it * 2 + 2).toInt(16).toByte() }
        } catch (_: NumberFormatException) {
            null
        }
    }

    /**
     * Authenticates to the host over an established TLS stream. [fingerprint] is
     * the certificate the connection really used. Returns normally when the host
     * accepted the phone; throws [PairingException] with a user-facing message otherwise.
     */
    suspend fun perform(
        reader: Wire.FrameReader,
        write: (ByteArray) -> Unit,
        security: NetSecurity,
        fingerprint: ByteArray
    ) {
        fun request(mode: Int, credential: ByteArray) = write(
            Wire.Message.PairRequest(mode, security.deviceId, security.deviceName, credential).encode()
        )

        fun response(): Wire.Message.PairResponse {
            while (true) {
                val m = reader.next()
                if (m is Wire.Message.PairResponse) return m
                if (m is Wire.Message.Bye) throw PairingException(m.text.ifBlank { "Host closed the connection" })
            }
        }

        fun finish(r: Wire.Message.PairResponse) {
            security.onAuthenticated(fingerprint, r.token.takeIf { it.isNotEmpty() })
        }

        security.qrSecret?.let { secret ->
            request(Wire.PAIR_MODE_QR, proof(secret, fingerprint, security.deviceId))
            val r = response()
            if (r.status == Wire.PAIR_OK) return finish(r)
            throw PairingException(refusal(r))
        }

        security.token?.let { token ->
            request(Wire.PAIR_MODE_TOKEN, token)
            val r = response()
            if (r.status == Wire.PAIR_OK) return finish(r)
            if (r.status != Wire.PAIR_UNTRUSTED) throw PairingException(refusal(r))
            // The host forgot this phone: fall through to a PIN.
        }

        request(Wire.PAIR_MODE_PIN, ByteArray(0))
        var r = response()
        if (r.status != Wire.PAIR_PIN_REQUIRED) throw PairingException(refusal(r))
        var hint = r.message
        while (true) {
            val pin = security.pinProvider(hint)?.trim()
            if (pin.isNullOrEmpty()) throw PairingException("Pairing cancelled")
            request(Wire.PAIR_MODE_PIN, proof(pin.toByteArray(Charsets.UTF_8), fingerprint, security.deviceId))
            r = response()
            when (r.status) {
                Wire.PAIR_OK -> return finish(r)
                Wire.PAIR_WRONG -> hint = r.message
                else -> throw PairingException(refusal(r))
            }
        }
    }

    fun refusal(r: Wire.Message.PairResponse): String {
        val detail = r.message.trim()
        val prefix = when (r.status) {
            Wire.PAIR_WRONG -> "Wrong PIN"
            Wire.PAIR_LOCKED -> "Pairing locked"
            Wire.PAIR_UNTRUSTED -> "Not paired"
            Wire.PAIR_DISABLED -> "Pairing not available"
            else -> "Pairing refused (status ${r.status})"
        }
        return if (detail.isEmpty()) prefix else "$prefix: $detail"
    }
}
