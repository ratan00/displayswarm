package com.displayswarm.client

import android.content.Context
import java.util.UUID

/** A stable per-install id: a random UUID created on first use. */
object DeviceIdentity {
    private const val PREFS = "displayswarm_identity"
    private const val KEY = "device_id"

    @Synchronized
    fun id(context: Context): String {
        val prefs = context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        prefs.getString(KEY, null)?.takeIf { it.isNotBlank() }?.let { return it }
        val fresh = UUID.randomUUID().toString()
        prefs.edit().putString(KEY, fresh).apply()
        return fresh
    }
}
