package com.displayswarm.client

import android.app.Activity
import android.text.InputType
import android.widget.EditText
import androidx.appcompat.app.AlertDialog
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlin.coroutines.resume

/** A minimal PIN prompt for pairing; the connect screen may replace it with its own. */
object PairingDialog {
    /** Shows the PIN dialog and returns the entered PIN, or null if cancelled. Safe to call from any thread. */
    suspend fun askPin(activity: Activity, hint: String): String? = suspendCancellableCoroutine { cont ->
        activity.runOnUiThread {
            if (activity.isFinishing) {
                cont.resume(null)
                return@runOnUiThread
            }
            val input = EditText(activity).apply {
                inputType = InputType.TYPE_CLASS_NUMBER
                setHint("6-digit PIN")
            }
            val dialog = AlertDialog.Builder(activity)
                .setTitle("Pair with this PC")
                .setMessage(hint.ifBlank { "Enter the PIN shown on the PC" })
                .setView(input)
                .setCancelable(true)
                .setPositiveButton("Pair") { _, _ -> if (cont.isActive) cont.resume(input.text.toString()) }
                .setNegativeButton("Cancel") { _, _ -> if (cont.isActive) cont.resume(null) }
                .setOnCancelListener { if (cont.isActive) cont.resume(null) }
                .create()
            cont.invokeOnCancellation { activity.runOnUiThread { dialog.dismiss() } }
            dialog.show()
        }
    }
}
