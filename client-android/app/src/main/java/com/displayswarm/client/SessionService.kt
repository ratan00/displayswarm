package com.displayswarm.client

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log

/**
 * Keeps the process alive while a session is up, so the host's audio keeps
 * playing with the app minimised or the screen off. Android freezes or kills
 * background processes without a foreground service; the ongoing notification
 * is what the system requires in exchange, and it carries a Disconnect action.
 *
 * It holds a partial wake lock and a Wi-Fi lock for the same reason: the CPU
 * and radio would otherwise sleep with the screen. The session itself still
 * lives in [MainActivity]; this service only pins the process.
 */
class SessionService : Service() {
    private var lastText = "Connected to the host"
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val text = intent?.getStringExtra(EXTRA_TEXT) ?: lastText
        lastText = text
        val notification = buildNotification(text)
        try {
            if (Build.VERSION.SDK_INT >= 29) {
                startForeground(
                    NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK or ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
                )
            } else {
                startForeground(NOTIFICATION_ID, notification)
            }
        } catch (e: Exception) {
            // e.g. started from the background on a strict build: the session still works in the foreground.
            Log.w(TAG, "could not go foreground", e)
            stopSelf()
            return START_NOT_STICKY
        }
        acquireLocks()
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        releaseLocks()
        super.onDestroy()
    }

    private fun buildNotification(text: String): Notification {
        val nm = getSystemService(NotificationManager::class.java)
        if (Build.VERSION.SDK_INT >= 26 && nm.getNotificationChannel(CHANNEL_ID) == null) {
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, "DisplaySwarm session", NotificationManager.IMPORTANCE_DEFAULT).apply {
                    // Default importance keeps it out of the collapsed "silent" section, but with no sound.
                    setSound(null, null)
                    enableVibration(false)
                    description = "Shown while DisplaySwarm is streaming, so audio keeps playing in the background"
                    setShowBadge(false)
                },
            )
        }
        val open = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val disconnect = PendingIntent.getActivity(
            this, 1,
            Intent(this, MainActivity::class.java)
                .setAction(ACTION_DISCONNECT)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        // Android 14 lets the user swipe a foreground-service notification away while the
        // service keeps running; a swipe re-posts it so the session stays visible.
        val repost = PendingIntent.getService(
            this, 2, Intent(this, SessionService::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val b = if (Build.VERSION.SDK_INT >= 26) Notification.Builder(this, CHANNEL_ID) else Notification.Builder(this)
        return b.setSmallIcon(android.R.drawable.ic_lock_silent_mode_off)
            .setContentTitle("DisplaySwarm")
            .setContentText(text)
            .setOngoing(true)
            .setDeleteIntent(repost)
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .setContentIntent(open)
            .addAction(Notification.Action.Builder(null, "Disconnect", disconnect).build())
            .build()
    }

    private fun acquireLocks() {
        if (wakeLock == null) {
            wakeLock = getSystemService(PowerManager::class.java)
                .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "displayswarm:session").apply {
                    setReferenceCounted(false)
                    acquire()
                }
        }
        if (wifiLock == null) {
            @Suppress("DEPRECATION")
            wifiLock = (applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager)
                .createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "displayswarm:session").apply {
                    setReferenceCounted(false)
                    acquire()
                }
        }
    }

    private fun releaseLocks() {
        try { wakeLock?.takeIf { it.isHeld }?.release() } catch (_: Exception) {}
        try { wifiLock?.takeIf { it.isHeld }?.release() } catch (_: Exception) {}
        wakeLock = null
        wifiLock = null
    }

    companion object {
        private const val TAG = "DisplaySwarmSessionSvc"
        private const val CHANNEL_ID = "displayswarm_session_v2"
        private const val NOTIFICATION_ID = 41
        private const val EXTRA_TEXT = "text"
        const val ACTION_DISCONNECT = "com.displayswarm.client.DISCONNECT"

        /** Starts (or refreshes the text of) the foreground service. Call while the app is visible. */
        fun start(context: Context, text: String) {
            val i = Intent(context, SessionService::class.java).putExtra(EXTRA_TEXT, text)
            try {
                context.startForegroundService(i)
            } catch (e: Exception) {
                Log.w(TAG, "could not start the session service", e)
            }
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, SessionService::class.java))
        }
    }
}
