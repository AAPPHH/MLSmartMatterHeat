package de.aapphh.heatscan

import android.content.Context
import android.net.Network
import android.util.Log
import androidx.wear.tiles.TileService
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL

data class Ap(val bssid: String, val rssi: Int, val freq: Int)

object Defaults {
    const val SERVER = "http://192.168.178.29:8080"
    const val DEVICE = "pixel-watch"
    val ROOMS = listOf("wohnzimmer", "schlafzimmer", "bad", "kueche", "buero", "flur")
}

object Payload {
    fun build(ts: Long, device: String, label: String?, aps: List<Ap>): JSONObject = JSONObject()
        .put("ts", ts)
        .put("device", device)
        .put("label", label ?: JSONObject.NULL)
        .put("aps", JSONArray().also { arr ->
            aps.forEach { arr.put(JSONObject().put("bssid", it.bssid.lowercase()).put("rssi", it.rssi).put("freq", it.freq)) }
        })
}

object Diag {
    const val LOCAL_NETWORK = "android.permission.ACCESS_LOCAL_NETWORK"
    const val LOCAL_NETWORK_SDK = 37

    fun emptyScan(locationOn: Boolean, wifiOn: Boolean): String = when {
        !locationOn -> "Standort aus"
        !wifiOn -> "WLAN aus"
        else -> "keine Scanergebnisse"
    }

    fun permissionIssue(fine: Boolean, coarse: Boolean): String? = when {
        fine -> null
        coarse -> "Nur ungefährer Standort, genauer nötig"
        else -> "Standort verweigert"
    }

    fun needsLocalNetwork(sdk: Int, granted: Boolean): Boolean = sdk >= LOCAL_NETWORK_SDK && !granted

    fun postError(error: String, lanMissing: Boolean): String = "ERR $error" + if (lanMissing) " (LAN-Zugriff fehlt)" else ""
}

class Dedup {
    private var last = Long.MIN_VALUE
    fun accept(newest: Long): Boolean {
        if (newest <= last) return false
        last = newest
        return true
    }
}

class Store(private val ctx: Context) {
    val sp = ctx.getSharedPreferences("heatscan", Context.MODE_PRIVATE)
    var server: String
        get() = sp.getString("server", Defaults.SERVER) ?: Defaults.SERVER
        set(v) = put("server", v)
    var device: String
        get() = sp.getString("device", Defaults.DEVICE) ?: Defaults.DEVICE
        set(v) = put("device", v)
    var label: String?
        get() = sp.getString("label", null)
        set(v) = put("label", v)
    var status: String
        get() = sp.getString("status", "-") ?: "-"
        set(v) = put("status", v)
    var mode: String
        get() = sp.getString("mode", "") ?: ""
        set(v) = put("mode", v)
    var running: Boolean
        get() = ScanService.running
        set(v) {
            ScanService.running = v
            sp.edit().putLong("rev", System.nanoTime()).apply()
            refreshTile()
        }

    private fun put(key: String, v: String?) {
        sp.edit().putString(key, v).apply()
        if (key == "label" || key == "status" || key == "mode") refreshTile()
    }

    private fun refreshTile() {
        try {
            TileService.getUpdater(ctx).requestUpdate(HeatTileService::class.java)
        } catch (e: Exception) {
            Log.w(TAG, "tile refresh failed", e)
        }
    }
}

object Poster {
    fun post(server: String, body: JSONObject, network: Network?): String {
        val url = URL(server.trim().trimEnd('/') + "/api/scan")
        val conn = (network?.openConnection(url) ?: url.openConnection()) as HttpURLConnection
        try {
            conn.requestMethod = "POST"
            conn.connectTimeout = 8000
            conn.readTimeout = 8000
            conn.doOutput = true
            conn.setRequestProperty("Content-Type", "application/json")
            conn.outputStream.use { it.write(body.toString().toByteArray()) }
            val code = conn.responseCode
            val text = (if (code < 400) conn.inputStream else conn.errorStream)?.bufferedReader()?.use { it.readText() } ?: ""
            val room = try { JSONObject(text).optString("room", "") } catch (e: Exception) { "" }
            return "$code ${body.getJSONArray("aps").length()} APs" + if (room.isNotEmpty() && room != "null") " $room" else ""
        } finally {
            conn.disconnect()
        }
    }
}

object ScanConfig {
    const val MOVING_PERIOD_MS = 30_000L
    const val MOVE_WINDOW_MS = 60_000L
    const val STILL_AFTER_MS = 120_000L
    const val HEARTBEAT_MS = 600_000L
    const val SCAN_HOLD_MS = 30_000L
}

enum class Mode(val label: String) {
    MOVING("Bewegung"), SETTLING("Bewegung"), STILL("Ruhe"), FIXED("Fest 30 s, Aktivitaet fehlt")
}

class ScanPolicy(adaptive: Boolean, start: Long) {
    var mode = if (adaptive) Mode.MOVING else Mode.FIXED
        private set
    private var lastMotion = start
    private var nextAt = start

    fun onMotion(now: Long): Boolean {
        if (mode == Mode.FIXED) return false
        lastMotion = now
        return when (mode) {
            Mode.STILL -> {
                mode = Mode.MOVING
                nextAt = now + ScanConfig.MOVING_PERIOD_MS
                true
            }
            Mode.SETTLING -> {
                mode = Mode.MOVING
                nextAt = now + ScanConfig.MOVING_PERIOD_MS
                false
            }
            else -> false
        }
    }

    fun onTick(now: Long): Boolean = when (mode) {
        Mode.FIXED -> due(now, ScanConfig.MOVING_PERIOD_MS)
        Mode.MOVING ->
            if (now - lastMotion >= ScanConfig.MOVE_WINDOW_MS) {
                mode = Mode.SETTLING
                true
            } else due(now, ScanConfig.MOVING_PERIOD_MS)
        Mode.SETTLING -> {
            if (now - lastMotion >= ScanConfig.STILL_AFTER_MS) {
                mode = Mode.STILL
                nextAt = now + ScanConfig.HEARTBEAT_MS
            }
            false
        }
        Mode.STILL -> due(now, ScanConfig.HEARTBEAT_MS)
    }

    fun delay(now: Long): Long = maxOf(0L, when (mode) {
        Mode.FIXED, Mode.STILL -> nextAt
        Mode.MOVING -> minOf(nextAt, lastMotion + ScanConfig.MOVE_WINDOW_MS)
        Mode.SETTLING -> lastMotion + ScanConfig.STILL_AFTER_MS
    } - now)

    private fun due(now: Long, period: Long): Boolean {
        if (now < nextAt) return false
        nextAt = now + period
        return true
    }
}

const val TAG = "HeatScan"
