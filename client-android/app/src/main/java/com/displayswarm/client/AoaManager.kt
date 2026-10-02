package com.displayswarm.client

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.ParcelFileDescriptor
import android.util.Log
import kotlinx.coroutines.*
import java.io.BufferedOutputStream
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.BufferOverflow

/**
 * Manages zero-setup native USB communication via Android Open Accessory (AOA 2.0).
 * Eliminates ADB and USB debugging requirements by streaming directly over bulk USB endpoints.
 */
class AoaManager(
    private val context: Context,
    private val hello: Wire.Hello,
    private var videoDecoder: VideoDecoder? = null,
    private val onFrameReceived: ((frameType: Int, payload: ByteArray, timestampUs: Long) -> Unit)? = null,
    private val onStatusChanged: (status: String) -> Unit = {},
    private val onMetricsUpdated: (SessionMetrics) -> Unit = {},
    /** The effective device role changed (HelloAck or SetRole). */
    private val onRoleChanged: (RoleState) -> Unit = {},
    /** Called with the live [ControlSession] when a connection is established, and null when it ends. */
    private val onSessionChanged: (ControlSession?) -> Unit = {}
) {
    companion object {
        private const val TAG = "DisplaySwarmAOA"
        const val ACTION_USB_PERMISSION = "com.displayswarm.client.USB_PERMISSION"
        const val ACCESSORY_MANUFACTURER = "DisplaySwarm"
        const val ACCESSORY_MODEL = "DisplaySwarmDisplay"

        /** Pause between a session ending and the next automatic attempt. */
        private const val RECONNECT_DELAY_MS = 1_500L

    }

    private val usbManager: UsbManager =
        context.getSystemService(Context.USB_SERVICE) as UsbManager

    private var accessory: UsbAccessory? = null
    private var fileDescriptor: ParcelFileDescriptor? = null
    private var inputStream: FileInputStream? = null
    private var outputStream: FileOutputStream? = null

    /** Outgoing packets of the current session; replaced for every session. */
    @Volatile
    private var inputChannel = newInputChannel()

    private fun newInputChannel() = Channel<ByteArray>(256, onBufferOverflow = BufferOverflow.DROP_OLDEST)

    @Volatile
    private var isRunning = false

    /**
     * Identifies the current session. A session's own teardown only acts while
     * it is still current, so a stale session ending late cannot close the fd
     * of the one that replaced it.
     */
    @Volatile
    private var generation = 0

    /**
     * Keep reconnecting while the accessory stays attached. Set by [connect],
     * cleared when the user disconnects. A host restart then needs nothing from
     * the user: the app is already waiting with a Hello when the new host claims
     * the phone.
     */
    @Volatile
    private var autoReconnect = false
    private val scope = CoroutineScope(Dispatchers.IO + SupervisorJob())

    /** Session logic for the live connection; null between connections. */
    @Volatile
    private var session: ControlSession? = null

    /**
     * The status shown once the connection is torn down, set by whatever ended
     * it first (host BYE, silent link, I/O error). Null means a plain disconnect.
     */
    @Volatile
    private var endStatus: String? = null

    /** Completed by the send worker once the goodbye packet has been written. */
    @Volatile
    private var byeSent: CompletableDeferred<Unit>? = null
    private var isReceiverRegistered = false

    private val usbReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            when (intent.action) {
                ACTION_USB_PERMISSION -> {
                    synchronized(this) {
                        val acc: UsbAccessory? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                            intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY, UsbAccessory::class.java)
                        } else {
                            @Suppress("DEPRECATION")
                            intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY)
                        }

                        val granted = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
                        if (granted && acc != null) {
                            Log.i(TAG, "USB permission granted for accessory: ${acc.model}")
                            openAccessoryConnection(acc)
                        } else {
                            Log.w(TAG, "USB permission denied for accessory")
                            onStatusChanged("USB AOA permission denied")
                        }
                    }
                }
                UsbManager.ACTION_USB_ACCESSORY_DETACHED -> {
                    val acc: UsbAccessory? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                        intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY, UsbAccessory::class.java)
                    } else {
                        @Suppress("DEPRECATION")
                        intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY)
                    }
                    if (acc != null && acc == accessory) {
                        Log.i(TAG, "USB accessory detached: ${acc.model}")
                        endStatus = endStatus ?: "USB Accessory Detached"
                        disconnect()
                    }
                }
            }
        }
    }

    init {
        val filter = IntentFilter().apply {
            addAction(ACTION_USB_PERMISSION)
            addAction(UsbManager.ACTION_USB_ACCESSORY_DETACHED)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            context.registerReceiver(usbReceiver, filter, Context.RECEIVER_EXPORTED)
        } else {
            context.registerReceiver(usbReceiver, filter)
        }
        isReceiverRegistered = true
    }

    fun setVideoDecoder(decoder: VideoDecoder?) {
        this.videoDecoder = decoder
        session?.let { decoder?.listener = it }
        if (decoder != null && !decoder.isConfigured() && isRunning) {
            decoder.configure(hello.width, hello.height)
        }
    }

    /**
     * Checks if any matching DisplaySwarm USB accessory is currently connected.
     */
    fun findConnectedAccessory(): UsbAccessory? {
        val list = usbManager.accessoryList ?: return null
        return list.firstOrNull { acc ->
            (acc.manufacturer.equals(ACCESSORY_MANUFACTURER, ignoreCase = true) &&
             acc.model.equals(ACCESSORY_MODEL, ignoreCase = true))
        } ?: list.firstOrNull()
    }

    /**
     * Connect to the given accessory, or automatically locate a connected accessory.
     *
     * A non-null [targetAccessory] comes from a fresh USB_ACCESSORY_ATTACHED:
     * the USB link was re-established, so any session still running is on a
     * dead fd and is replaced rather than kept.
     */
    fun connect(targetAccessory: UsbAccessory? = null): Boolean {
        autoReconnect = true
        if (isRunning) {
            if (targetAccessory == null) return true
            Log.i(TAG, "Accessory attached again; replacing the stale session")
            disconnect()
        }

        val acc = targetAccessory ?: findConnectedAccessory()
        if (acc == null) {
            Log.d(TAG, "No USB accessory found connected")
            return false
        }

        accessory = acc

        if (!usbManager.hasPermission(acc)) {
            Log.i(TAG, "Requesting permission for accessory: ${acc.model}")
            val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            } else {
                PendingIntent.FLAG_UPDATE_CURRENT
            }
            val permissionIntent = PendingIntent.getBroadcast(
                context, 0, Intent(ACTION_USB_PERMISSION), flags
            )
            usbManager.requestPermission(acc, permissionIntent)
            onStatusChanged("Requesting USB AOA Permission...")
            return true
        }

        return openAccessoryConnection(acc)
    }

    private fun openAccessoryConnection(
        acc: UsbAccessory,
        connectingStatus: String = "Connecting via USB AOA..."
    ): Boolean {
        // Claimed before anything else so a previous session's late teardown
        // (disconnect(oldGen)) can never close the fds opened below.
        val gen = ++generation
        try {
            Log.i(TAG, "Opening USB accessory ${acc.manufacturer} - ${acc.model}")
            val pfd = usbManager.openAccessory(acc)
            if (pfd == null) {
                Log.e(TAG, "Failed to open USB accessory file descriptor")
                onStatusChanged("Failed to open USB accessory")
                return false
            }

            fileDescriptor = pfd
            accessory = acc
            val fd = pfd.fileDescriptor
            val inStream = FileInputStream(fd)
            val outStream = FileOutputStream(fd)
            inputStream = inStream
            outputStream = outStream

            endStatus = null
            inputChannel = newInputChannel()
            isRunning = true
            onStatusChanged(connectingStatus)

            // Launch worker coroutines
            scope.launch { runAoaSession(inStream, outStream, inputChannel, gen) }

            return true
        } catch (e: Exception) {
            Log.e(TAG, "Error opening accessory connection: ${e.message}", e)
            onStatusChanged("USB Connection error: ${e.message}")
            disconnect()
            return false
        }
    }

    private suspend fun runAoaSession(
        rawIn: FileInputStream,
        rawOut: FileOutputStream,
        channel: Channel<ByteArray>,
        gen: Int
    ) = withContext(Dispatchers.IO) {
        val safeIn = SafeAoaInputStream(rawIn)
        val safeOut = SafeAoaOutputStream(rawOut)
        val out = BufferedOutputStream(safeOut, 16384)

        var inputJob: Job? = null
        var sessionJob: Job? = null
        try {
            // 1. Hello / HelloAck. The host flushes stale bytes when it opens a
            //    session, so the Hello is re-sent until it is acknowledged. With
            //    no host reading the pipe the first write simply blocks until one
            //    claims the phone, which is what an auto-reconnect waits on.
            val reader = Wire.FrameReader(safeIn)
            val ack = Handshake.perform(reader, hello, { out.write(it); out.flush() }, { rawIn.close() },
                maxAttempts = Int.MAX_VALUE)
            Log.i(
                TAG,
                "HelloAck received via USB AOA: ${ack.width}x${ack.height} @ ${ack.fps} fps " +
                    "host=${ack.hostName}"
            )

            // Session logic (clock sync, latency, liveness, keyframe requests). Its
            // sends share the input worker so nothing interleaves mid-packet.
            val sess = ControlSession(
                sendPacket = ::queueInput,
                onStatus = onStatusChanged,
                onMetrics = onMetricsUpdated,
                onEnded = { status -> endSession(status, gen) },
                onRole = onRoleChanged
            )
            session = sess
            onSessionChanged(sess)
            videoDecoder?.listener = sess
            sess.onHandshakeComplete(Handshake.streamInfo(ack), ack.role, ack.features)

            videoDecoder?.let {
                if (!it.isConfigured()) {
                    it.configure(hello.width, hello.height)
                }
            }

            // 3. Spawn input sender worker, then the session's senders/watchdog
            inputJob = launch { sendWorker(out, channel) }
            sessionJob = sess.start(this)

            // 4. Receive loop: every message goes to the session; video also to the decoder.
            var framesLogged = 0
            while (isActive && isRunning && gen == generation) {
                val msg = reader.next()
                sess.onMessage(msg)

                if (msg is Wire.Message.VideoFrame) {
                    if (framesLogged < 3) {
                        framesLogged++
                        Log.i(
                            TAG,
                            "Video frame #$framesLogged received: type=${msg.frameType} " +
                                "index=${msg.frameIndex} payload=${msg.data.size}B"
                        )
                    }

                    // Feed to video decoder directly or notify callback
                    val dec = videoDecoder
                    if (dec != null) {
                        dec.feedNalUnit(
                            msg.data, msg.captureUs,
                            isConfig = msg.frameType == Wire.FRAME_TYPE_CONFIG,
                            isKey = msg.frameType == Wire.FRAME_TYPE_KEY
                        )
                    } else {
                        onFrameReceived?.invoke(msg.frameType, msg.data, msg.captureUs)
                    }
                }
            }
        } catch (e: Wire.HandshakeException) {
            Log.w(TAG, "USB AOA handshake failed: ${e.message}")
            endStatus = endStatus ?: e.message
        } catch (e: ProtocolException) {
            Log.w(TAG, "USB AOA protocol error: ${e.message}")
            endStatus = endStatus ?: "Disconnected (protocol error: ${e.message})"
        } catch (e: IOException) {
            if (isRunning) {
                Log.w(TAG, "USB AOA I/O finished or disconnected: ${e.message}", e)
                endStatus = endStatus ?: "Disconnected (${e.message ?: "connection lost"})"
            }
        } catch (e: Exception) {
            Log.e(TAG, "USB AOA session error: ${e.message}", e)
            endStatus = endStatus ?: "Disconnected (${e.message ?: "error"})"
        } finally {
            sessionJob?.cancel()
            inputJob?.cancel()
            channel.close()
            disconnect(gen)
            scheduleReconnect(gen)
        }
    }

    /**
     * After session [gen] ended, tries again while the user still wants a
     * connection and the accessory is still attached (e.g. the host was
     * restarted). A detach ends the loop; the next attach starts a new one.
     */
    private fun scheduleReconnect(gen: Int) {
        scope.launch {
            delay(RECONNECT_DELAY_MS)
            if (!autoReconnect || isRunning || gen != generation) return@launch
            val acc = findConnectedAccessory() ?: return@launch
            if (!usbManager.hasPermission(acc)) return@launch
            Log.i(TAG, "Reconnecting to ${acc.model}")
            openAccessoryConnection(acc, connectingStatus = "Waiting for the host on USB...")
        }
    }

    fun queueInput(packet: ByteArray) {
        if (!isRunning) return
        inputChannel.trySend(packet)
    }

    private suspend fun sendWorker(out: OutputStream, channel: Channel<ByteArray>) = withContext(Dispatchers.IO) {
        try {
            for (packet in channel) {
                if (!isActive || !isRunning) break
                out.write(packet)
                out.flush()
                if (packet === byePacket) byeSent?.complete(Unit)
            }
        } catch (e: Exception) {
            Log.w(TAG, "USB AOA Input sender ended: ${e.message}")
        }
    }

    /** Ends the session with [status] as the final text (host BYE or a silent link). */
    private fun endSession(status: String, gen: Int) {
        if (gen != generation) return
        endStatus = endStatus ?: status
        disconnect(gen)
    }

    @Volatile
    private var byePacket: ByteArray? = null

    /**
     * Disconnects after telling the host: BYE(NORMAL) goes out through the send
     * worker (so it cannot land mid-packet) and the link is closed once it is
     * written, or after a short timeout if the link is already stuck.
     */
    fun disconnectGracefully() {
        autoReconnect = false
        val sess = session
        if (!isRunning || sess == null) {
            disconnect()
            return
        }
        val bye = sess.byePacket()
        byePacket = bye
        byeSent = CompletableDeferred()
        endStatus = endStatus ?: "Disconnected (closed by user)"
        queueInput(bye)
        scope.launch {
            withTimeoutOrNull(500) { byeSent?.await() }
            disconnect()
        }
    }

    fun isConnected(): Boolean = isRunning

    /**
     * Closes the connection. With [gen], only if that session is still the
     * current one.
     */
    @Synchronized
    fun disconnect(gen: Int? = null) {
        if (gen != null && gen != generation) return
        if (!isRunning && fileDescriptor == null) return
        isRunning = false
        if (videoDecoder?.listener === session) videoDecoder?.listener = null
        if (session != null) onSessionChanged(null)
        session?.close()
        session = null

        try {
            inputStream?.close()
        } catch (_: Exception) {}
        try {
            outputStream?.close()
        } catch (_: Exception) {}
        try {
            fileDescriptor?.close()
        } catch (_: Exception) {}

        inputStream = null
        outputStream = null
        fileDescriptor = null
        accessory = null

        onStatusChanged(endStatus ?: "Disconnected")
        endStatus = null
    }

    fun release() {
        autoReconnect = false
        disconnect()
        if (isReceiverRegistered) {
            try {
                context.unregisterReceiver(usbReceiver)
            } catch (_: Exception) {}
            isReceiverRegistered = false
        }
        scope.cancel()
    }
}

/**
 * Safe InputStream wrapper around /dev/usb_accessory.
 * Android's f_accessory kernel driver rejects read requests exceeding BULK_BUFFER_SIZE (16384 bytes)
 * with -EINVAL. This wrapper ensures all underlying read() calls never exceed 16384 bytes.
 */
class SafeAoaInputStream(private val rawIn: FileInputStream) : InputStream() {
    private val buffer = ByteArray(16384)
    private var bufferPos = 0
    private var bufferLimit = 0

    override fun read(): Int {
        if (bufferPos >= bufferLimit) {
            val n = rawIn.read(buffer, 0, 16384)
            if (n <= 0) return -1
            bufferPos = 0
            bufferLimit = n
        }
        return buffer[bufferPos++].toInt() and 0xFF
    }

    override fun read(b: ByteArray, off: Int, len: Int): Int {
        if (len <= 0) return 0
        if (bufferPos >= bufferLimit) {
            // Read at most 16384 bytes directly from rawIn
            val n = rawIn.read(buffer, 0, 16384)
            if (n <= 0) return -1
            bufferPos = 0
            bufferLimit = n
        }
        val available = bufferLimit - bufferPos
        val bytesToCopy = minOf(len, available)
        System.arraycopy(buffer, bufferPos, b, off, bytesToCopy)
        bufferPos += bytesToCopy
        return bytesToCopy
    }

    override fun close() {
        rawIn.close()
    }
}

/**
 * Safe OutputStream wrapper around /dev/usb_accessory ensuring writes never exceed 16384 bytes.
 */
class SafeAoaOutputStream(private val rawOut: FileOutputStream) : OutputStream() {
    override fun write(b: Int) {
        val single = byteArrayOf(b.toByte())
        rawOut.write(single, 0, 1)
    }

    override fun write(b: ByteArray, off: Int, len: Int) {
        var written = 0
        while (written < len) {
            val chunk = minOf(len - written, 16384)
            rawOut.write(b, off + written, chunk)
            written += chunk
        }
    }

    override fun flush() {
        rawOut.flush()
    }

    override fun close() {
        rawOut.close()
    }
}
