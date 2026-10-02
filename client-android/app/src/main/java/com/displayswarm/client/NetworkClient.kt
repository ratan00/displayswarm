package com.displayswarm.client

import android.util.Log
import kotlinx.coroutines.*
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.io.IOException
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.BufferOverflow

class NetworkClient(
    private val host: String,
    private val port: Int,
    private val hello: Wire.Hello,
    private val onFrameReceived: (frameType: Int, payload: ByteArray, timestampUs: Long) -> Unit,
    private val onStatusChanged: (status: String) -> Unit,
    private val onMetricsUpdated: (SessionMetrics) -> Unit,
    /** Called with the live [ControlSession] when a connection is established, and null when it ends. */
    private val onSessionChanged: (ControlSession?) -> Unit = {},
    /** The effective device role changed (HelloAck or SetRole). */
    private val onRoleChanged: (RoleState) -> Unit = {},
    /**
     * TLS with certificate pinning and pairing (the default for every network
     * connection). Null is plain TCP, for development against a host started
     * with `DISPLAYSWARM_INSECURE_TCP=1`.
     */
    private val security: NetSecurity? = null
) {
    companion object {
        private const val TAG = "DisplaySwarmNetwork"
        private const val CONNECT_TIMEOUT_MS = 5000
        private const val READ_TIMEOUT_MS = 10000

        /**
         * Total handshake budget. Covers a full portal screen-share approval
         * round-trip, which begins only after the HelloAck is sent.
         */
        private const val HANDSHAKE_TIMEOUT_MS = 60_000L
    }

    private var socket: Socket? = null
    private var outputStream: OutputStream? = null
    private val inputChannel = Channel<ByteArray>(256, onBufferOverflow = BufferOverflow.DROP_OLDEST)
    private val isRunning = AtomicBoolean(false)
    private val scope = CoroutineScope(Dispatchers.IO + SupervisorJob())

    @Volatile
    private var session: ControlSession? = null

    /** Final status text, set by whatever ended the connection first. */
    @Volatile
    private var endStatus: String? = null

    @Volatile
    private var byePacket: ByteArray? = null

    @Volatile
    private var byeSent: CompletableDeferred<Unit>? = null

    fun start() {
        if (isRunning.getAndSet(true)) return

        var sessionJob: Job? = null
        scope.launch {
            try {
                onStatusChanged("Connecting to $host:$port...")
                val sock = Socket()
                sock.tcpNoDelay = true // Disable Nagle's algorithm for minimal latency
                sock.sendBufferSize = 64 * 1024
                sock.receiveBufferSize = 1024 * 1024 // 1MB buffer for video frames
                sock.soTimeout = READ_TIMEOUT_MS
                localAddressOnSameSubnet(host)?.let { local ->
                    // A phone acting as the hotspot routes its own apps out over mobile
                    // data, so a connect to a client on the hotspot subnet fails with
                    // EINVAL. Binding to our address on that subnet picks the right route.
                    Log.i(TAG, "Binding to $local for $host")
                    sock.bind(InetSocketAddress(local, 0))
                }
                sock.connect(InetSocketAddress(host, port), CONNECT_TIMEOUT_MS)

                var stream: Socket = sock
                var hostFingerprint: ByteArray? = null
                if (security != null) {
                    onStatusChanged("Securing the connection...")
                    val (ssl, fp) = PinnedTls.wrap(sock, host, port, security.pinnedFingerprint)
                    stream = ssl
                    hostFingerprint = fp
                }
                socket = stream
                val out = BufferedOutputStream(stream.getOutputStream(), 16 * 1024)
                val reader = Wire.FrameReader(BufferedInputStream(stream.getInputStream(), 1024 * 1024))
                outputStream = out

                // The handshake has its own resend/give-up timing; no read timeout here.
                sock.soTimeout = 0
                if (security != null && hostFingerprint != null) {
                    onStatusChanged("Pairing...")
                    Pairing.perform(reader, { out.write(it); out.flush() }, security, hostFingerprint)
                }
                val ack = Handshake.perform(reader, hello, { out.write(it); out.flush() }, {
                    try { stream.shutdownInput() } catch (_: Exception) {} // not supported on TLS sockets
                    stream.close()
                })
                Log.i(
                    TAG,
                    "HelloAck received: ${ack.width}x${ack.height} @ ${ack.fps} fps host=${ack.hostName}"
                )

                // No read timeout while streaming; the session's liveness
                // watchdog ends the connection if the host goes silent.

                // Session logic (clock sync, latency, liveness, keyframe requests).
                // Its sends share the input worker so nothing interleaves mid-packet.
                val sess = ControlSession(
                    sendPacket = ::queueInput,
                    onStatus = onStatusChanged,
                    onMetrics = onMetricsUpdated,
                    onEnded = { status -> endSession(status) },
                    onRole = onRoleChanged
                )
                session = sess
                onSessionChanged(sess)
                sess.onHandshakeComplete(Handshake.streamInfo(ack), ack.role, ack.features)

                // Start input sender worker and the session's senders/watchdog
                launch { sendWorker(out) }
                sessionJob = sess.start(this)

                // Receive loop: every message goes to the session; video also to the app.
                while (isActive && isRunning.get()) {
                    val msg = reader.next()
                    sess.onMessage(msg)
                    if (msg is Wire.Message.VideoFrame) {
                        onFrameReceived(msg.frameType, msg.data, msg.captureUs)
                    }
                }
            } catch (e: PairingException) {
                Log.w(TAG, "Pairing failed: ${e.message}")
                endStatus = endStatus ?: e.message
            } catch (e: javax.net.ssl.SSLException) {
                Log.w(TAG, "TLS failed: ${e.message}")
                endStatus = endStatus ?: "Secure connection failed: ${e.cause?.message ?: e.message}"
            } catch (e: Wire.HandshakeException) {
                Log.w(TAG, "Handshake failed: ${e.message}")
                endStatus = endStatus ?: e.message
            } catch (e: ProtocolException) {
                Log.w(TAG, "Protocol error: ${e.message}")
                endStatus = endStatus ?: "Disconnected (protocol error: ${e.message})"
            } catch (e: Exception) {
                if (isRunning.get()) {
                    Log.e(TAG, "Connection error: ${e.message}")
                    endStatus = endStatus ?: "Disconnected (${e.message ?: "connection lost"})"
                }
            } finally {
                sessionJob?.cancel()
                finish()
            }
        }
    }

    /** This phone's IPv4 address on the same subnet as [host], if it has one. */
    private fun localAddressOnSameSubnet(host: String): java.net.InetAddress? = try {
        val target = java.net.InetAddress.getByName(host).address
        if (target.size != 4) null else java.net.NetworkInterface.getNetworkInterfaces().toList()
            .filter { it.isUp && !it.isLoopback }
            .flatMap { it.interfaceAddresses }
            .firstOrNull { ia ->
                val a = ia.address.address
                val bits = ia.networkPrefixLength.toInt()
                a.size == 4 && (0 until bits).all { i ->
                    ((a[i / 8].toInt() shr (7 - i % 8)) and 1) == ((target[i / 8].toInt() shr (7 - i % 8)) and 1)
                }
            }?.address
    } catch (_: Exception) {
        null
    }

    fun queueInput(packet: ByteArray) {
        if (!isRunning.get()) return
        inputChannel.trySend(packet)
    }

    private suspend fun sendWorker(out: OutputStream) = withContext(Dispatchers.IO) {
        try {
            for (packet in inputChannel) {
                if (!isActive || !isRunning.get()) break
                out.write(packet)
                out.flush()
                if (packet === byePacket) byeSent?.complete(Unit)
            }
        } catch (e: Exception) {
            Log.w(TAG, "Input sender stopped: ${e.message}")
        }
    }

    private fun cleanupSocket() {
        try {
            socket?.close()
        } catch (_: Exception) {}
        socket = null
        outputStream = null
    }

    /** Ends the session with [status] as the final text (host BYE or a silent link). */
    private fun endSession(status: String) {
        endStatus = endStatus ?: status
        cleanupSocket() // unblocks the receive loop, whose cleanup reports the status
    }

    /** Tears everything down and reports the final status exactly once. */
    private fun finish() {
        if (!isRunning.getAndSet(false)) return
        session?.close()
        session = null
        onSessionChanged(null)
        cleanupSocket()
        scope.cancel()
        onStatusChanged(endStatus ?: "Disconnected")
        endStatus = null
    }

    fun stop() {
        finish()
    }

    /**
     * Stops after telling the host: BYE(NORMAL) goes out through the send worker
     * (so it cannot land mid-packet), then the socket is closed once it has been
     * written, or after a short timeout if the link is already stuck.
     */
    fun stopGracefully() {
        val sess = session
        if (!isRunning.get() || sess == null) {
            finish()
            return
        }
        val bye = sess.byePacket()
        byePacket = bye
        byeSent = CompletableDeferred()
        endStatus = endStatus ?: "Disconnected (closed by user)"
        val sent = byeSent
        queueInput(bye)
        // `scope` is cancelled by finish(), so wait on a separate scope.
        CoroutineScope(Dispatchers.IO).launch {
            withTimeoutOrNull(500) { sent?.await() }
            finish()
        }
    }
}
