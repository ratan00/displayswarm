package com.displayswarm.client

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.os.ParcelFileDescriptor
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.util.Log
import java.io.File
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.Semaphore
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

/**
 * File transfer, the phone side of `host/src/services/files.rs` (see its
 * protocol notes). Files from the PC are saved to Downloads/DisplaySwarm;
 * files go to the PC from the share sheet ([ShareToPcActivity]).
 *
 * The protocol logic is [FileTransferCore], plain Kotlin with storage and
 * sending injected, so it is unit-tested; this class adds Android storage and
 * notifications.
 */
class FileTransfer(context: Context) : PhoneService {
    private val app = context.applicationContext
    override val name = "files"
    override val features = Wire.FEATURE_FILES

    @Volatile
    private var core: FileTransferCore? = null

    override fun wants(msg: Wire.Message) =
        msg is Wire.Message.FileOffer || msg is Wire.Message.FileChunk || msg is Wire.Message.FileControl

    override fun onMessage(msg: Wire.Message) {
        core?.onMessage(msg)
    }

    override fun onSessionStart(ctx: PhoneServiceContext) {
        core = FileTransferCore(ctx.send, ::openSink, ::notify)
        current = this
    }

    override fun onSessionEnd() {
        core?.close()
        core = null
        if (current === this) current = null
    }

    /** Sends files the user shared; their descriptors were opened by the caller. */
    fun send(files: List<Shared>): Int {
        val c = core ?: return 0
        // The stream closes the descriptor when the transfer ends.
        files.forEach { f -> c.offer(f.name, f.mime, f.size) { ParcelFileDescriptor.AutoCloseInputStream(f.fd) } }
        return files.size
    }

    /** A file from the share sheet, opened while its permission grant was valid. */
    class Shared(val name: String, val mime: String, val size: Long, val fd: ParcelFileDescriptor)

    private fun openSink(name: String, mime: String, size: Long): FileTransferCore.Sink? = try {
        if (Build.VERSION.SDK_INT >= 29) mediaStoreSink(name, mime) else legacySink(name)
    } catch (e: Exception) {
        Log.w(TAG, "Cannot create $name: ${e.message}")
        null
    }

    /** Android 10+: a pending Downloads entry, published when complete (no permission needed). */
    private fun mediaStoreSink(name: String, mime: String): FileTransferCore.Sink? {
        val resolver = app.contentResolver
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, name)
            put(MediaStore.MediaColumns.MIME_TYPE, mime.ifBlank { "application/octet-stream" })
            put(MediaStore.MediaColumns.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/DisplaySwarm")
            put(MediaStore.MediaColumns.IS_PENDING, 1)
        }
        val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values) ?: return null
        val out = resolver.openOutputStream(uri) ?: run { resolver.delete(uri, null, null); return null }
        return object : FileTransferCore.Sink {
            override val out: OutputStream = out
            override fun commit(): Uri? {
                out.close()
                resolver.update(uri, ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }, null, null)
                return uri
            }
            override fun abort() {
                try { out.close() } catch (_: Exception) {}
                resolver.delete(uri, null, null)
            }
        }
    }

    /** Android 8-9: the app's own Downloads folder (no storage permission needed). */
    private fun legacySink(name: String): FileTransferCore.Sink? {
        val dir = File(app.getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS) ?: return null, "DisplaySwarm").apply { mkdirs() }
        var target = File(dir, name)
        var n = 2
        while (target.exists()) target = File(dir, name.substringBeforeLast('.', name) + " ($n)" +
            (if (name.contains('.')) "." + name.substringAfterLast('.') else "")).also { n++ }
        val part = File(dir, ".${target.name}.part")
        val out = part.outputStream()
        return object : FileTransferCore.Sink {
            override val out: OutputStream = out
            override fun commit(): Uri? {
                out.close()
                return if (part.renameTo(target)) Uri.fromFile(target) else null
            }
            override fun abort() {
                try { out.close() } catch (_: Exception) {}
                part.delete()
            }
        }
    }

    private fun notify(ev: FileTransferCore.Event) {
        val nm = app.getSystemService(NotificationManager::class.java) ?: return
        if (Build.VERSION.SDK_INT >= 26) {
            nm.createNotificationChannel(NotificationChannel(CHANNEL, "Files", NotificationManager.IMPORTANCE_DEFAULT))
        }
        val (title, text, open) = when (ev) {
            is FileTransferCore.Event.Received -> Triple("Received ${ev.name}", "From the PC, in Downloads/DisplaySwarm", ev.location)
            is FileTransferCore.Event.Sent -> Triple("Sent ${ev.name}", "To the PC's Downloads/DisplaySwarm", null)
            is FileTransferCore.Event.Failed -> Triple("${ev.name} was not transferred", ev.reason, null)
        }
        val builder = (if (Build.VERSION.SDK_INT >= 26) android.app.Notification.Builder(app, CHANNEL)
        else @Suppress("DEPRECATION") android.app.Notification.Builder(app))
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setContentTitle(title)
            .setContentText(text)
            .setAutoCancel(true)
        val uri = open as? Uri
        if (uri != null && uri.scheme == "content") {
            val view = Intent(Intent.ACTION_VIEW).setDataAndType(uri, app.contentResolver.getType(uri))
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
            builder.setContentIntent(PendingIntent.getActivity(app, uri.hashCode(), view, PendingIntent.FLAG_IMMUTABLE))
        }
        try {
            nm.notify(TAG, ids.incrementAndGet().toInt(), builder.build())
        } catch (e: SecurityException) {
            Log.i(TAG, "$title ($text)")
        }
    }

    companion object {
        private const val TAG = "FileTransfer"
        private const val CHANNEL = "files"
        private val ids = AtomicLong(1000)

        /** The running service, while a session with file transfer is up. */
        @Volatile
        var current: FileTransfer? = null
            private set

        /** Name, size and type of a shared URI. */
        fun describe(context: Context, uri: Uri): Triple<String, Long, String> {
            var name = uri.lastPathSegment ?: "file"
            var size = -1L
            try {
                context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)?.use { c ->
                    if (c.moveToFirst()) {
                        c.getString(0)?.let { name = it }
                        if (!c.isNull(1)) size = c.getLong(1)
                    }
                }
            } catch (_: Exception) {}
            return Triple(name, size, context.contentResolver.getType(uri) ?: "application/octet-stream")
        }
    }
}

/**
 * The transfer protocol without Android: [send] puts a message in the session's
 * queue, [openSink] creates a destination for an incoming file, [onEvent]
 * reports results.
 */
class FileTransferCore(
    private val send: (Wire.Message) -> Unit,
    private val openSink: (name: String, mime: String, size: Long) -> Sink?,
    private val onEvent: (Event) -> Unit,
    private val ackTimeoutMs: Long = 30_000,
) {
    interface Sink {
        val out: OutputStream
        /** Makes the file visible; returns where it is, if known. */
        fun commit(): Any?
        fun abort()
    }

    sealed class Event {
        data class Received(val name: String, val location: Any?) : Event()
        data class Sent(val name: String) : Event()
        data class Failed(val name: String, val reason: String) : Event()
    }

    private class Incoming(val name: String, val size: Long, val sink: Sink) {
        var received = 0L
        var chunks = 0
    }

    private class Outgoing(val name: String, val size: Long, val open: () -> InputStream) {
        val cancel = AtomicBoolean(false)
        val window = Semaphore(WINDOW)
    }

    private val incoming = HashMap<Long, Incoming>()
    private val outgoing = java.util.concurrent.ConcurrentHashMap<Long, Outgoing>()
    private val nextId = AtomicLong(1)

    @Synchronized
    fun onMessage(msg: Wire.Message) {
        when (msg) {
            is Wire.Message.FileOffer -> offered(msg)
            is Wire.Message.FileChunk -> chunk(msg)
            is Wire.Message.FileControl -> if (msg.id and HOST_ID_BIT != 0L) controlIncoming(msg) else controlOutgoing(msg)
            else -> {}
        }
    }

    private fun offered(o: Wire.Message.FileOffer) {
        if (o.id and HOST_ID_BIT == 0L || incoming.containsKey(o.id) || o.size < 0) {
            send(Wire.Message.FileControl(o.id, Wire.FILE_REJECT))
            return
        }
        val name = safeName(o.name)
        val sink = openSink(name, o.mime, o.size)
        if (sink == null) {
            onEvent(Event.Failed(name, "could not create the file"))
            send(Wire.Message.FileControl(o.id, Wire.FILE_REJECT))
            return
        }
        incoming[o.id] = Incoming(name, o.size, sink)
        send(Wire.Message.FileControl(o.id, Wire.FILE_ACCEPT))
    }

    private fun failIncoming(id: Long, reason: String, tellHost: Boolean) {
        incoming.remove(id)?.let {
            it.sink.abort()
            onEvent(Event.Failed(it.name, reason))
        }
        if (tellHost) send(Wire.Message.FileControl(id, Wire.FILE_CANCEL))
    }

    private fun chunk(c: Wire.Message.FileChunk) {
        val inc = incoming[c.id] ?: return
        if (c.offset != inc.received || inc.received + c.data.size > inc.size) {
            failIncoming(c.id, "the data arrived out of order or too long", true)
            return
        }
        try {
            inc.sink.out.write(c.data)
        } catch (e: Exception) {
            failIncoming(c.id, "could not write: ${e.message}", true)
            return
        }
        inc.received += c.data.size
        inc.chunks++
        if (inc.chunks % Wire.FILE_ACK_EVERY == 0) send(Wire.Message.FileControl(c.id, Wire.FILE_ACK))
    }

    private fun controlIncoming(m: Wire.Message.FileControl) {
        when (m.op) {
            Wire.FILE_COMPLETE -> {
                val inc = incoming[m.id] ?: return
                if (inc.received != inc.size) {
                    failIncoming(m.id, "the transfer ended early", true)
                    return
                }
                incoming.remove(m.id)
                val where = try { inc.sink.commit() } catch (e: Exception) { null }
                onEvent(Event.Received(inc.name, where))
            }
            Wire.FILE_CANCEL -> failIncoming(m.id, "cancelled on the PC", false)
        }
    }

    private fun controlOutgoing(m: Wire.Message.FileControl) {
        val o = outgoing[m.id] ?: return
        when (m.op) {
            Wire.FILE_ACCEPT -> Thread({ stream(m.id, o) }, "file-send").start()
            Wire.FILE_ACK -> o.window.release(Wire.FILE_ACK_EVERY)
            Wire.FILE_REJECT, Wire.FILE_CANCEL -> {
                outgoing.remove(m.id)
                o.cancel.set(true)
                o.window.release(WINDOW)
                onEvent(Event.Failed(o.name, if (m.op == Wire.FILE_REJECT) "the PC declined it" else "cancelled on the PC"))
            }
        }
    }

    /** Offers a file to the PC; [open] is called once it accepts. */
    fun offer(name: String, mime: String, size: Long, open: () -> InputStream): Long {
        val id = nextId.getAndIncrement() and (HOST_ID_BIT - 1)
        outgoing[id] = Outgoing(name, size, open)
        send(Wire.Message.FileOffer(id, size, name, mime))
        return id
    }

    private fun stream(id: Long, o: Outgoing) {
        val result = try {
            o.open().use { input ->
                val buf = ByteArray(CHUNK)
                var offset = 0L
                while (true) {
                    if (o.cancel.get()) return@use "cancelled"
                    val n = input.read(buf)
                    if (n <= 0) break
                    // At most WINDOW chunks unacknowledged: the session queue is small.
                    if (!o.window.tryAcquire(ackTimeoutMs, TimeUnit.MILLISECONDS)) return@use "the PC stopped answering"
                    if (o.cancel.get()) return@use "cancelled"
                    send(Wire.Message.FileChunk(id, offset, buf.copyOf(n)))
                    offset += n
                }
                if (o.size >= 0 && offset != o.size) return@use "the file changed while it was sent"
                send(Wire.Message.FileControl(id, Wire.FILE_COMPLETE))
                null
            }
        } catch (e: Exception) {
            e.message ?: "read error"
        }
        outgoing.remove(id)
        when (result) {
            null -> onEvent(Event.Sent(o.name))
            "cancelled" -> {}
            else -> {
                send(Wire.Message.FileControl(id, Wire.FILE_CANCEL))
                onEvent(Event.Failed(o.name, result))
            }
        }
    }

    /** The session ended: abort partial files, stop senders. */
    @Synchronized
    fun close() {
        incoming.keys.toList().forEach { failIncoming(it, "the PC disconnected", false) }
        outgoing.values.forEach { it.cancel.set(true); it.window.release(WINDOW) }
        outgoing.clear()
    }

    companion object {
        const val HOST_ID_BIT = 0x8000_0000L
        const val CHUNK = 32 * 1024
        /** Chunks in flight; twice the ACK interval so the sender rarely waits. */
        const val WINDOW = 16

        /** The same rules as the host's `sanitize_name`. */
        fun safeName(name: String): String {
            var s = name.substringAfterLast('/').substringAfterLast('\\')
                .map { if (it.isISOControl() || it in "<>:\"|?*") '_' else it }.joinToString("")
            s = s.trim().trimStart('.').trimEnd('.', ' ')
            if (s.length > 180) {
                val ext = s.substringAfterLast('.', "").let { if (it.isNotEmpty() && it.length < 16) ".$it" else "" }
                s = s.take(180 - ext.length) + ext
            }
            return s.ifEmpty { "file" }
        }
    }
}
