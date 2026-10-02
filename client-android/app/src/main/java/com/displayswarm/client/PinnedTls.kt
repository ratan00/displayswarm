package com.displayswarm.client

import java.net.Socket
import java.security.SecureRandom
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLSocket
import javax.net.ssl.X509TrustManager

/**
 * TLS with certificate pinning instead of CAs: the host's certificate is
 * self-signed, and trust is its SHA-256 fingerprint, learned at pairing.
 */
object PinnedTls {
    /** Accepts exactly [pin]; with a null pin (first contact) accepts anything, the caller binds the pairing proof to [seen]. */
    class PinTrustManager(private val pin: ByteArray?) : X509TrustManager {
        @Volatile
        var seen: ByteArray? = null
            private set

        override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {
            val leaf = chain?.firstOrNull() ?: throw CertificateException("The host sent no certificate")
            val fp = Pairing.fingerprintOf(leaf.encoded)
            seen = fp
            if (pin != null && !MessageDigest_isEqual(pin, fp)) {
                throw CertificateException(
                    "The host's certificate changed. If you did not reinstall DisplaySwarm on the PC, someone may be impersonating it."
                )
            }
        }

        override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) =
            throw CertificateException("client certificates are not used")

        override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
    }

    private fun MessageDigest_isEqual(a: ByteArray, b: ByteArray) = java.security.MessageDigest.isEqual(a, b)

    /** Wraps a connected [socket], runs the handshake and returns it with the certificate fingerprint it used. */
    fun wrap(socket: Socket, host: String, port: Int, pin: ByteArray?): Pair<SSLSocket, ByteArray> {
        val tm = PinTrustManager(pin)
        val ctx = SSLContext.getInstance("TLS")
        ctx.init(null, arrayOf(tm), SecureRandom())
        // No hostname verification on purpose: the fingerprint is the identity.
        val ssl = ctx.socketFactory.createSocket(socket, host, port, true) as SSLSocket
        ssl.startHandshake()
        val fp = tm.seen ?: throw CertificateException("no certificate was checked")
        return ssl to fp
    }
}
