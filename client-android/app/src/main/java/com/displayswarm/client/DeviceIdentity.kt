package com.displayswarm.client

import android.content.Context
import android.provider.Settings
import java.util.UUID

/**
 * A stable per-device id. It comes from ANDROID_ID, which survives reinstalls and
 * data clears (it only changes on a factory reset or a different signing key), so
 * the host keeps one entry per device. It is hashed, so the raw value never leaves
 * the phone. Without a usable ANDROID_ID, a random UUID is created on first use.
 */
object DeviceIdentity {
    private const val PREFS = "displayswarm_identity"
    private const val KEY = "device_id"
    /** Shared by a batch of old devices; not an identity. */
    private const val BROKEN_ANDROID_ID = "9774d56d682e549c"

    @Synchronized
    fun id(context: Context): String {
        val app = context.applicationContext
        derived(androidId(app))?.let { return it }
        val prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        prefs.getString(KEY, null)?.takeIf { it.isNotBlank() }?.let { return it }
        val fresh = UUID.randomUUID().toString()
        prefs.edit().putString(KEY, fresh).apply()
        return fresh
    }

    private fun androidId(context: Context): String? = try {
        Settings.Secure.getString(context.contentResolver, Settings.Secure.ANDROID_ID)
    } catch (_: Exception) {
        null
    }

    internal fun derived(androidId: String?): String? {
        if (androidId.isNullOrBlank() || androidId == BROKEN_ANDROID_ID) return null
        return UUID.nameUUIDFromBytes("displayswarm:$androidId".toByteArray(Charsets.UTF_8)).toString()
    }
}
