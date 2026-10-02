package com.displayswarm.client

import android.util.Log
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean

/** The Hello / HelloAck exchange, shared by the USB and network transports. */
object Handshake {
    private const val TAG = "DisplaySwarmHandshake"

    /** How long to wait for a HelloAck before re-sending the Hello. */
    const val RETRY_WINDOW_MS = 2_000L
    const val MAX_ATTEMPTS = 5

    const val NO_ANSWER =
        "No answer from the host. Is DisplaySwarm running there, and up to date?"

    /**
     * Sends [hello] and waits for the host's acknowledgement.
     *
     * The host may discard the first Hello (it flushes stale bytes when it
     * opens a session), so the Hello is re-sent every [RETRY_WINDOW_MS] up to
     * [MAX_ATTEMPTS] times from a coroutine while the calling thread blocks in
     * [Wire.FrameReader.awaitHelloAck]. When every attempt goes unanswered,
     * [closeInput] is called to unblock that read. Pass [Int.MAX_VALUE] as
     * [maxAttempts] to keep trying until the link itself closes (USB, where a
     * frozen or restarting host answers only once it claims the phone). The resend job is joined
     * before returning, so no write overlaps the later send worker.
     *
     * @throws Wire.HandshakeException when the host refuses, speaks another
     *   version or never answers; its message is meant for the user.
     */
    suspend fun perform(
        reader: Wire.FrameReader,
        hello: Wire.Hello,
        write: (ByteArray) -> Unit,
        closeInput: () -> Unit,
        maxAttempts: Int = MAX_ATTEMPTS
    ): Wire.HelloAck = coroutineScope {
        val bytes = Wire.Message.HelloMsg(hello).encode()
        val gaveUp = AtomicBoolean(false)
        val resend = launch { resendLoop(bytes, write, closeInput, gaveUp, maxAttempts) }
        val ack = try {
            reader.awaitHelloAck()
        } catch (e: IOException) {
            if (e is Wire.HandshakeException) throw e
            if (gaveUp.get()) throw Wire.HandshakeException(NO_ANSWER)
            throw e
        } finally {
            withContext(NonCancellable) { resend.cancelAndJoin() }
        }
        if (ack.status != Wire.HELLO_OK) throw Wire.HandshakeException(refusal(ack))
        ack
    }

    private suspend fun resendLoop(
        bytes: ByteArray,
        write: (ByteArray) -> Unit,
        closeInput: () -> Unit,
        gaveUp: AtomicBoolean,
        maxAttempts: Int
    ) {
        for (attempt in 1..maxAttempts) {
            try {
                write(bytes)
                Log.i(TAG, "Hello #$attempt sent")
            } catch (e: IOException) {
                Log.w(TAG, "Hello #$attempt failed: ${e.message}")
            }
            delay(RETRY_WINDOW_MS)
        }
        gaveUp.set(true)
        Log.w(TAG, "No HelloAck after $maxAttempts attempts")
        try { closeInput() } catch (_: Exception) {}
    }

    fun refusal(ack: Wire.HelloAck): String {
        val detail = ack.message.trim()
        val prefix = when (ack.status) {
            Wire.HELLO_VERSION_MISMATCH -> "Version mismatch"
            Wire.HELLO_BUSY -> "Host busy"
            Wire.HELLO_REJECTED -> "Host refused"
            else -> "Host refused (status ${ack.status})"
        }
        return if (detail.isEmpty()) prefix else "$prefix: $detail"
    }

    /** "1920x1080@60fps" plus the host name, for the status line. */
    fun streamInfo(ack: Wire.HelloAck): String {
        val base = "${ack.width}x${ack.height}@${ack.fps}fps"
        return if (ack.hostName.isEmpty()) base else "$base (${ack.hostName})"
    }
}
