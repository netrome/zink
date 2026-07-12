package org.nearone.zink

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Keep the process (and its Rust subscription loops) alive when the
    // activity is backgrounded. Starting an FGS is allowed here — the app
    // is in the foreground during onCreate.
    startForegroundService(Intent(this, DeliveryService::class.java))
    requestBatteryExemption()
  }

  /**
   * Doze throttles background network for optimized apps; connection-based
   * delivery without a push service needs the exemption (the Signal/Molly
   * pattern — live-delivery.md §5). One system prompt, remembered.
   */
  private fun requestBatteryExemption() {
    val power = getSystemService(PowerManager::class.java)
    if (!power.isIgnoringBatteryOptimizations(packageName)) {
      startActivity(
        Intent(
          Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
          Uri.parse("package:$packageName"),
        )
      )
    }
  }
}
