package com.displayswarm.client

import java.net.HttpURLConnection
import java.net.URL
import org.json.JSONObject

/** The newest GitHub release: its tag (`v0.1.1`) and the page to download it from. */
data class ReleaseInfo(val tag: String, val url: String)

/**
 * Asks GitHub whether a newer release than the installed version exists. Nothing
 * is sent but the request itself, and it only runs when the settings screen is
 * opened or the user taps the button. The app never downloads or installs
 * anything: it opens the release page.
 */
object UpdateChecker {
    const val RELEASES_PAGE = "https://github.com/ratan00/displayswarm/releases"
    private const val LATEST_API = "https://api.github.com/repos/ratan00/displayswarm/releases/latest"

    /** `0.1.0`, `v0.1.0` or `0.1.0-beta` as a list of numbers; null if there are none. */
    fun parseVersion(v: String): List<Int>? {
        val core = v.trim().removePrefix("v").removePrefix("V").substringBefore('-').substringBefore('+')
        val parts = core.split('.').map { it.toIntOrNull() ?: return null }
        return parts.takeIf { it.isNotEmpty() }
    }

    /** True when [latest] is a higher version than [current]; unparseable input is never "newer". */
    fun isNewer(current: String, latest: String): Boolean {
        val a = parseVersion(current) ?: return false
        val b = parseVersion(latest) ?: return false
        for (i in 0 until maxOf(a.size, b.size)) {
            val x = a.getOrElse(i) { 0 }
            val y = b.getOrElse(i) { 0 }
            if (x != y) return y > x
        }
        return false
    }

    /** Blocking; call off the main thread. Null on any network or parse failure. */
    fun fetchLatest(): ReleaseInfo? = try {
        val c = URL(LATEST_API).openConnection() as HttpURLConnection
        c.connectTimeout = 8000
        c.readTimeout = 8000
        c.setRequestProperty("Accept", "application/vnd.github+json")
        c.setRequestProperty("User-Agent", "DisplaySwarm-Android")
        try {
            if (c.responseCode != 200) null
            else {
                val j = JSONObject(c.inputStream.bufferedReader().use { it.readText() })
                ReleaseInfo(j.getString("tag_name"), j.optString("html_url", RELEASES_PAGE))
            }
        } finally {
            c.disconnect()
        }
    } catch (e: Exception) {
        null
    }
}
