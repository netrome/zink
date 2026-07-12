package org.nearone.zink

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder

/**
 * The live-delivery foreground service (docs/design/live-delivery.md §5).
 *
 * A pure shell: it never touches messages, keys, or sockets. Its only job
 * is to EXIST, because a foreground service keeps this process — where the
 * Rust subscription loops already run — alive while the activity/webview is
 * backgrounded. Started by MainActivity on launch; START_STICKY so the OS
 * revives it after memory pressure (delivery then resumes on next app open,
 * the accepted MVP limit).
 */
class DeliveryService : Service() {
  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val channelId = "zink-delivery"
    val manager = getSystemService(NotificationManager::class.java)
    manager.createNotificationChannel(
      // IMPORTANCE_MIN: the mandatory FGS notification stays out of the way.
      NotificationChannel(channelId, "Live delivery", NotificationManager.IMPORTANCE_MIN)
    )
    val notification =
      Notification.Builder(this, channelId)
        .setContentTitle("zink is connected")
        .setSmallIcon(R.mipmap.ic_launcher)
        .setOngoing(true)
        .build()
    if (Build.VERSION.SDK_INT >= 34) {
      startForeground(1, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
    } else {
      startForeground(1, notification)
    }
    return START_STICKY
  }
}
