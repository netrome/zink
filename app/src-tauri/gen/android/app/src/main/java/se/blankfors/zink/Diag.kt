package se.blankfors.zink

import android.content.Context
import android.util.Log
import java.io.File

/**
 * C4c-i diagnostics: logcat + an append-only file next to the Rust side's
 * `diag.log`, so the overnight background-delivery diagnosis survives a
 * wrapped logcat ring buffer. Epoch-ms timestamps (java.time needs API 26).
 */
fun Context.diagLog(message: String) {
  Log.i("zink", message)
  try {
    File(filesDir, "kotlin-diag.log").appendText("${System.currentTimeMillis()} $message\n")
  } catch (_: Exception) {}
}
