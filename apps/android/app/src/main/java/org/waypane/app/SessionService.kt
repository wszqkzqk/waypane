package org.waypane.app

import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat

class SessionService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_DISCONNECT) {
            (application as WaypaneApplication).sessionModel.returnToSetup()
            stopSelf()
            return START_NOT_STICKY
        }
        val endpoint = intent?.getStringExtra(EXTRA_ENDPOINT)?.takeUnless(String::isBlank)
            ?: getString(R.string.app_name)
        NotificationManagerCompat.from(this).createNotificationChannel(
            NotificationChannelCompat.Builder(CHANNEL_ID, NotificationManager.IMPORTANCE_LOW)
                .setName(getString(R.string.session_channel_name))
                .setDescription(getString(R.string.session_channel_description))
                .setShowBadge(false)
                .build(),
        )
        val notification = NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_stat_waypane)
            .setContentTitle(getString(R.string.session_notification_title))
            .setContentText(getString(R.string.session_notification_text, endpoint))
            .setContentIntent(openAppIntent())
            .setDeleteIntent(disconnectIntent())
            .addAction(0, getString(R.string.session_notification_open), openAppIntent())
            .addAction(0, getString(R.string.session_notification_disconnect), disconnectIntent())
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setOngoing(true)
            .setShowWhen(false)
            .setOnlyAlertOnce(true)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .build()
        val serviceType = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
        } else {
            0
        }
        ServiceCompat.startForeground(this, NOTIFICATION_ID, notification, serviceType)
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        super.onDestroy()
    }

    private fun openAppIntent(): PendingIntent {
        val intent = Intent(this, MainActivity::class.java)
        return PendingIntent.getActivity(
            this,
            REQUEST_OPEN,
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    private fun disconnectIntent(): PendingIntent {
        val intent = Intent(this, SessionService::class.java).setAction(ACTION_DISCONNECT)
        return PendingIntent.getService(
            this,
            REQUEST_DISCONNECT,
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    companion object {
        private const val CHANNEL_ID = "waypane.session"
        private const val NOTIFICATION_ID = 1
        private const val REQUEST_OPEN = 1
        private const val REQUEST_DISCONNECT = 2
        private const val ACTION_DISCONNECT = "org.waypane.app.action.DISCONNECT"
        private const val EXTRA_ENDPOINT = "org.waypane.app.extra.ENDPOINT"

        fun start(context: Context, endpoint: String) {
            val intent = Intent(context, SessionService::class.java)
                .putExtra(EXTRA_ENDPOINT, endpoint)
            ContextCompat.startForegroundService(context, intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, SessionService::class.java))
        }
    }
}
