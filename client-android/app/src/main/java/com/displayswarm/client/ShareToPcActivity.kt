package com.displayswarm.client

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.widget.Toast

/**
 * "Share > DisplaySwarm": sends the shared files to the connected PC. No UI of
 * its own. The files are opened here, while the share grant is valid; the
 * transfer then reads them after this activity is gone.
 */
class ShareToPcActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val uris = sharedUris(intent)
        val service = FileTransfer.current
        val text = when {
            uris.isEmpty() -> "Nothing to send"
            service == null -> "Connect DisplaySwarm to a PC first"
            else -> {
                val files = uris.mapNotNull { uri ->
                    try {
                        val (name, size, mime) = FileTransfer.describe(this, uri)
                        contentResolver.openFileDescriptor(uri, "r")?.let { FileTransfer.Shared(name, mime, size, it) }
                    } catch (_: Exception) {
                        null
                    }
                }
                val n = service.send(files)
                if (n == 0) "Could not open the shared files" else "Sending $n file(s) to the PC"
            }
        }
        Toast.makeText(this, text, Toast.LENGTH_SHORT).show()
        finish()
    }

    @Suppress("DEPRECATION")
    private fun sharedUris(i: Intent): List<Uri> = when (i.action) {
        Intent.ACTION_SEND -> listOfNotNull(
            if (Build.VERSION.SDK_INT >= 33) i.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
            else i.getParcelableExtra(Intent.EXTRA_STREAM)
        )
        Intent.ACTION_SEND_MULTIPLE ->
            (if (Build.VERSION.SDK_INT >= 33) i.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
            else i.getParcelableArrayListExtra(Intent.EXTRA_STREAM)) ?: emptyList()
        else -> emptyList()
    }
}
