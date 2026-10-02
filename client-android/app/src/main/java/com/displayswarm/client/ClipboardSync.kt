package com.displayswarm.client

import android.app.Activity
import android.app.Application
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.core.content.FileProvider
import java.io.ByteArrayOutputStream
import java.io.File

/** What the clipboard carries over the wire. */
sealed class ClipPayload {
    data class Text(val text: String) : ClipPayload()
    class Png(val bytes: ByteArray) : ClipPayload() {
        override fun equals(other: Any?) = other is Png && bytes.contentEquals(other.bytes)
        override fun hashCode() = bytes.contentHashCode()
    }
}

/**
 * Loop prevention, size caps and mime mapping; pure, so it is unit-tested.
 * Mirrors `host/src/services/clipboard.rs`.
 */
class ClipboardSyncState {
    private var last: Int? = null

    private fun hashOf(p: ClipPayload): Int = when (p) {
        is ClipPayload.Text -> p.text.hashCode() * 31
        is ClipPayload.Png -> p.bytes.contentHashCode() * 31 + 1
    }

    /** The local clipboard now holds [p]; returns whether to send it to the host. */
    fun shouldSend(p: ClipPayload): Boolean {
        val h = hashOf(p)
        if (last == h) return false
        last = h // also for oversized content: do not retry it
        return withinLimits(p)
    }

    /** [p] arrived from the host (or was there at connect) and is not to be sent back. */
    fun noteRemote(p: ClipPayload) {
        last = hashOf(p)
    }

    companion object {
        const val MAX_IMAGE = 8 * 1024 * 1024
        const val MAX_TEXT = 4 * 1024 * 1024
        private val PNG_MAGIC = byteArrayOf(
            0x89.toByte(), 'P'.code.toByte(), 'N'.code.toByte(), 'G'.code.toByte(), 0x0D, 0x0A, 0x1A, 0x0A
        )

        fun withinLimits(p: ClipPayload): Boolean = when (p) {
            is ClipPayload.Text -> p.text.isNotEmpty() && p.text.toByteArray().size <= MAX_TEXT
            is ClipPayload.Png -> p.bytes.isNotEmpty() && p.bytes.size <= MAX_IMAGE
        }

        fun toMessage(p: ClipPayload): Wire.Message.Clipboard = when (p) {
            is ClipPayload.Text -> Wire.Message.Clipboard("text/plain", p.text.toByteArray())
            is ClipPayload.Png -> Wire.Message.Clipboard("image/png", p.bytes)
        }

        /** Null for a mime type we do not sync, an oversized payload, or non-PNG image bytes. */
        fun fromWire(mime: String, data: ByteArray): ClipPayload? {
            val p = when (mime.substringBefore(';').trim().lowercase()) {
                "text/plain" -> ClipPayload.Text(String(data, Charsets.UTF_8))
                "image/png" ->
                    if (data.size >= PNG_MAGIC.size && data.copyOfRange(0, PNG_MAGIC.size).contentEquals(PNG_MAGIC)) {
                        ClipPayload.Png(data)
                    } else {
                        return null
                    }
                else -> return null
            }
            return p.takeIf { withinLimits(it) }
        }
    }
}

/**
 * Clipboard sync with the host, text and PNG images, both ways.
 *
 * Android 10+ only lets the focused app (or the default IME) read the
 * clipboard, so the phone side has two triggers:
 * - [ClipboardManager.OnPrimaryClipChangedListener], which fires while DisplaySwarm
 *   is in the foreground;
 * - a read shortly after any of our activities resumes, which catches whatever
 *   was copied in other apps while we were in the background.
 * Copies made in other apps while DisplaySwarm is in the background reach the host
 * only when the app is next brought to the front (the last copy wins). Writing
 * the host's clipboard to the phone works at any time.
 */
class ClipboardSync : PhoneService {
    override val name = "clipboard"
    override val features = Wire.FEATURE_CLIPBOARD

    private var ctx: PhoneServiceContext? = null
    private var cm: ClipboardManager? = null
    private var state = ClipboardSyncState()
    private val main = Handler(Looper.getMainLooper())
    private var listener: ClipboardManager.OnPrimaryClipChangedListener? = null
    private var callbacks: Application.ActivityLifecycleCallbacks? = null

    override fun wants(msg: Wire.Message) = msg is Wire.Message.Clipboard

    override fun onSessionStart(ctx: PhoneServiceContext) {
        this.ctx = ctx
        state = ClipboardSyncState()
        main.post {
            if (this.ctx == null) return@post
            val cm = ctx.appContext.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            this.cm = cm
            // Whatever is on the clipboard at connect is remembered, not sent.
            readLocal()?.let { state.noteRemote(it) }
            val l = ClipboardManager.OnPrimaryClipChangedListener { pushLocal() }
            listener = l
            cm.addPrimaryClipChangedListener(l)
            val cb = object : Application.ActivityLifecycleCallbacks {
                override fun onActivityResumed(activity: Activity) {
                    main.postDelayed({ pushLocal() }, 300) // window focus arrives after resume
                }
                override fun onActivityCreated(a: Activity, b: Bundle?) {}
                override fun onActivityStarted(a: Activity) {}
                override fun onActivityPaused(a: Activity) {}
                override fun onActivityStopped(a: Activity) {}
                override fun onActivitySaveInstanceState(a: Activity, b: Bundle) {}
                override fun onActivityDestroyed(a: Activity) {}
            }
            callbacks = cb
            (ctx.appContext as? Application)?.registerActivityLifecycleCallbacks(cb)
        }
    }

    override fun onSessionEnd() {
        val c = ctx ?: return
        ctx = null
        main.post {
            listener?.let { cm?.removePrimaryClipChangedListener(it) }
            callbacks?.let { (c.appContext as? Application)?.unregisterActivityLifecycleCallbacks(it) }
            listener = null
            callbacks = null
            cm = null
        }
    }

    override fun onMessage(msg: Wire.Message) {
        val m = msg as? Wire.Message.Clipboard ?: return
        val payload = ClipboardSyncState.fromWire(m.mime, m.data) ?: return
        main.post { writeLocal(payload) }
    }

    private fun pushLocal() {
        val c = ctx ?: return
        val p = readLocal() ?: return
        if (state.shouldSend(p)) c.send(ClipboardSyncState.toMessage(p))
    }

    private fun readLocal(): ClipPayload? {
        val cm = cm ?: return null
        val c = ctx ?: return null
        return try {
            val clip = cm.primaryClip ?: return null
            if (clip.itemCount == 0) return null
            val item = clip.getItemAt(0)
            val uri = item.uri
            if (uri != null && clip.description.hasMimeType("image/*")) {
                readImage(c.appContext, uri)
            } else {
                item.text?.toString()?.takeIf { it.isNotEmpty() }?.let { ClipPayload.Text(it) }
            }
        } catch (e: Exception) {
            Log.d(TAG, "Clipboard not readable now: $e")
            null
        }
    }

    private fun readImage(context: Context, uri: Uri): ClipPayload? {
        val raw = context.contentResolver.openInputStream(uri)?.use { it.readBytes() } ?: return null
        if (raw.size > ClipboardSyncState.MAX_IMAGE * 2) return null
        ClipboardSyncState.fromWire("image/png", raw)?.let { return it }
        // JPEG and friends are re-encoded as PNG.
        val bmp = BitmapFactory.decodeByteArray(raw, 0, raw.size) ?: return null
        val out = ByteArrayOutputStream()
        bmp.compress(Bitmap.CompressFormat.PNG, 100, out)
        return ClipPayload.Png(out.toByteArray())
    }

    private fun writeLocal(p: ClipPayload) {
        val cm = cm ?: return
        val c = ctx ?: return
        state.noteRemote(p)
        try {
            when (p) {
                is ClipPayload.Text -> cm.setPrimaryClip(ClipData.newPlainText("DisplaySwarm", p.text))
                is ClipPayload.Png -> {
                    val dir = File(c.appContext.cacheDir, "clipboard").apply { mkdirs() }
                    dir.listFiles()?.forEach { it.delete() }
                    val f = File(dir, "clip-${System.currentTimeMillis()}.png")
                    f.writeBytes(p.bytes)
                    val uri = FileProvider.getUriForFile(c.appContext, "${c.appContext.packageName}.clipfiles", f)
                    cm.setPrimaryClip(ClipData.newUri(c.appContext.contentResolver, "DisplaySwarm image", uri))
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "Could not set the clipboard", e)
        }
    }

    private companion object {
        const val TAG = "DisplaySwarmClipboard"
    }
}
