package de.aapphh.heatscan

import android.content.Context
import java.io.File
import java.nio.file.Files
import java.nio.file.StandardCopyOption
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
    const val HOME = "FRITZ!Box 1234 AB"
    val ROOMS = listOf("wohnzimmer", "schlafzimmer", "bad", "kueche", "flur")
    val CAPTIONS = mapOf("wohnzimmer" to "Wohnen", "schlafzimmer" to "Schlafen", "bad" to "Bad", "kueche" to "Küche", "flur" to "Flur")
}

object RoomIcons {
    val RES = mapOf(
        "wohnzimmer" to R.drawable.ic_wohnzimmer, "schlafzimmer" to R.drawable.ic_schlafzimmer, "bad" to R.drawable.ic_bad,
        "kueche" to R.drawable.ic_kueche, "flur" to R.drawable.ic_flur
    )
}

object Labels {
    fun toggle(current: String?, tapped: String): String? = if (current == tapped) null else tapped
}

object Highlight {
    fun of(label: String?, guess: String?): Pair<String?, String?> = Pair(guess ?: label, label)
}

object Quarters {
    val WEDGES = listOf("schlafzimmer", "bad", "kueche", "flur")
    const val CENTER = "wohnzimmer"
    const val REF = 213f
    const val INNER = 46f / REF
    const val CENTER_R = 42f / REF
    const val LABEL_R = 77f / REF
    const val GAP = 3f
    const val SPAN = 90f

    fun mid(room: String): Float = WEDGES.indexOf(room) * SPAN

    fun room(dx: Float, dy: Float, d: Float): String {
        if (kotlin.math.hypot(dx, dy) < CENTER_R * d) return CENTER
        val deg = Math.toDegrees(kotlin.math.atan2(dx.toDouble(), -dy.toDouble())).toFloat()
        return WEDGES[(((deg + SPAN / 2 + 360f) % 360f) / SPAN).toInt() % WEDGES.size]
    }
}

object Home {
    fun parse(csv: String): List<String> = csv.split(',').map { it.trim() }.filter { it.isNotEmpty() }

    private fun norm(s: String) = s.trim().trim('"').lowercase()

    fun isHome(seen: Collection<String>, home: Collection<String>): Boolean {
        val wanted = home.map(::norm).filter { it.isNotEmpty() }.toSet()
        return wanted.isEmpty() || seen.any { norm(it) in wanted }
    }
}

object TileClick {
    const val PREFIX = "room:"

    fun id(room: String, nonce: Long) = "$PREFIX$room:$nonce"

    fun room(id: String, nonce: Long): String? {
        if (!id.startsWith(PREFIX)) return null
        val parts = id.removePrefix(PREFIX).split(':')
        if (parts.size != 2 || parts[1] != nonce.toString()) return null
        return parts[0].takeIf { it in Defaults.ROOMS }
    }

    fun statusLine(running: Boolean, mode: String, status: String): String =
        (if (running && mode.isNotEmpty()) "$mode | " else if (!running) "Scan aus | " else "") + status
}

object Trace {
    fun fmt(event: String, vararg pairs: Pair<String, Any?>): String =
        (listOf(event) + pairs.map { "${it.first}=${it.second.toString().replace(' ', '_')}" }).joinToString(" ")

    fun i(event: String, vararg pairs: Pair<String, Any?>) {
        Log.i(TAG, fmt(event, *pairs))
    }

    fun w(event: String, e: Throwable, vararg pairs: Pair<String, Any?>) {
        Log.w(TAG, fmt(event, *pairs, "ex" to e.toString()), e)
    }
}

object Summary {
    fun line(
        running: Boolean, paused: Boolean, mode: String, label: String?, server: String, home: String, atHome: Boolean?,
        lastPost: String, ok: Long, failed: Long, lastError: String, extra: List<Pair<String, Any?>> = emptyList()
    ): String = Trace.fmt(
        "status", "running" to running, "paused" to paused, "mode" to mode.ifEmpty { "-" }, "label" to (label ?: "-"), "server" to server,
        "home" to home.ifEmpty { "-" }, "atHome" to (atHome ?: "-"), "lastPost" to lastPost, "ok" to ok, "failed" to failed, "lastError" to lastError, *extra.toTypedArray()
    )
}

object Payload {
    fun build(
        ts: Long, device: String, label: String?, aps: List<Ap>,
        motion: String? = null, power: String? = null, away: Boolean = false, id: String? = null
    ): JSONObject = JSONObject()
        .put("ts", ts)
        .put("device", device)
        .also { if (id != null) it.put("id", id) }
        .put("label", label ?: JSONObject.NULL)
        .put("motion", motion ?: JSONObject.NULL)
        .put("power", power ?: JSONObject.NULL)
        .put("away", away)
        .put("aps", JSONArray().also { arr ->
            aps.forEach { arr.put(JSONObject().put("bssid", it.bssid.lowercase()).put("rssi", it.rssi).put("freq", it.freq)) }
        })
}

object EntryId {
    fun format(device: String, n: Long) = "$device-%06d".format(n)
}

object PowerState {
    fun of(saver: Boolean?, idle: Boolean?, offBody: Boolean?): String? = when {
        saver == null && idle == null && offBody == null -> null
        offBody == true -> "offbody"
        idle == true -> "doze"
        saver == true -> "saver"
        else -> "normal"
    }
}

object Capture {
    fun wallMs(nowWall: Long, nowElapsedMs: Long, scanMicros: Long): Long = minOf(nowWall, nowWall - (nowElapsedMs - scanMicros / 1000))
}

class Queued(val seq: Long, val json: JSONObject)

class ScanQueue(private val file: File?, private val max: Int = ScanConfig.QUEUE_MAX, private val maxAgeMs: Long = ScanConfig.QUEUE_AGE_MS) {
    private class Item(val seq: Long, val ts: Long, val line: String)

    private val items = ArrayDeque<Item>()
    private var nextSeq = 1L
    private var dirty = false

    @Volatile var size = 0
        private set

    init {
        try {
            file?.takeIf { it.exists() }?.useLines { lines ->
                lines.forEach { l ->
                    try { items.addLast(Item(nextSeq, JSONObject(l).getLong("ts"), l)); nextSeq++ } catch (e: Exception) { Log.w(TAG, "queue line dropped", e) }
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "queue load failed", e)
        }
        val trimmed = items.size > max
        while (items.size > max) items.removeFirst()
        size = items.size
        if (trimmed) rewrite()
    }

    fun add(entry: JSONObject, now: Long): Int = synchronized(this) {
        val line = entry.toString()
        items.addLast(Item(nextSeq++, entry.getLong("ts"), line))
        var dropped = 0
        while (items.size > max || (items.isNotEmpty() && items.first().ts < now - maxAgeMs)) {
            items.removeFirst()
            dropped++
        }
        size = items.size
        if (dropped > 0 || dirty) rewrite()
        else try { file?.appendText(line + "\n") } catch (e: Exception) { Log.w(TAG, "queue append failed", e) }
        dropped
    }

    fun peek(n: Int, now: Long): List<Queued> = synchronized(this) {
        items.take(n).map { i ->
            Queued(i.seq, JSONObject(i.line).also { if (now - i.ts > ScanConfig.QUEUED_AFTER_MS) it.put("queued", true) })
        }
    }

    fun remove(upToSeq: Long, commit: Boolean = true) = synchronized(this) {
        while (items.isNotEmpty() && items.first().seq <= upToSeq) {
            items.removeFirst()
            dirty = true
        }
        size = items.size
        if (commit) commit()
    }

    fun commit() = synchronized(this) {
        if (dirty) rewrite()
    }

    private fun rewrite() {
        val f = file ?: return
        try {
            val tmp = File(f.path + ".tmp")
            tmp.writeText(items.joinToString("") { it.line + "\n" })
            Files.move(tmp.toPath(), f.toPath(), StandardCopyOption.ATOMIC_MOVE)
            dirty = false
        } catch (e: Exception) {
            Log.w(TAG, "queue rewrite failed", e)
        }
    }

    companion object {
        private var shared: ScanQueue? = null

        fun get(c: Context): ScanQueue = synchronized(this) {
            shared ?: ScanQueue(File(c.applicationContext.filesDir, "queue.jsonl")).also { shared = it }
        }
    }
}

data class FlushResult(val sent: Int, val code: Int)

class Flusher(private val maxBatch: Int = ScanConfig.BATCH) {
    private var batch = maxBatch

    var singleOnly = false
        private set

    val batchSize: Int get() = batch

    private fun accepted(c: Int) = c in 200..299

    private fun rejected(c: Int) = c in 400..499 && c != 404 && c != 408 && c != 429

    private fun settled(c: Int) = accepted(c) || rejected(c)

    fun run(queue: ScanQueue, now: Long, sendBatch: (JSONArray) -> Int, sendOne: (JSONObject) -> Int): FlushResult {
        var sent = 0
        var code = 0
        try {
            while (true) {
                val items = queue.peek(batch, now)
                if (items.isEmpty()) break
                if (!singleOnly) {
                    code = sendBatch(JSONArray().also { a -> items.forEach { a.put(it.json) } })
                    if (accepted(code)) {
                        queue.remove(items.last().seq)
                        sent += items.size
                        continue
                    }
                    if (code == 413 && items.size > 1) {
                        batch = maxOf(1, items.size / 2)
                        continue
                    }
                    if (code == 404) singleOnly = true
                    else if (!rejected(code)) return FlushResult(sent, code)
                }
                for (e in items) {
                    code = sendOne(e.json)
                    if (!settled(code)) return FlushResult(sent, code)
                    queue.remove(e.seq, false)
                    if (accepted(code)) sent++
                }
            }
        } finally {
            queue.commit()
        }
        batch = maxBatch
        return FlushResult(sent, code)
    }
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

    fun postOk(status: String): Boolean = status.startsWith("2")

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
        get() = sp.getString("device", null) ?: ctx.getString(R.string.default_device)
        set(v) = put("device", v)
    var label: String?
        get() = sp.getString("label", null)?.takeIf { it in Defaults.ROOMS }
        set(v) = put("label", v)
    var guess: String?
        get() = sp.getString("guess", null)?.takeIf { it in Defaults.ROOMS }
        set(v) = put("guess", v)
    var status: String
        get() = sp.getString("status", "-") ?: "-"
        set(v) = put("status", v)
    var home: String
        get() = sp.getString("home", Defaults.HOME) ?: Defaults.HOME
        set(v) = put("home", v)
    var lastPost: String
        get() = sp.getString("lastPost", "-") ?: "-"
        set(v) = put("lastPost", v)
    var lastError: String
        get() = sp.getString("lastError", "-") ?: "-"
        set(v) = put("lastError", v)
    var postsOk: Long
        get() = sp.getLong("postsOk", 0L)
        set(v) = sp.edit().putLong("postsOk", v).apply()
    var postsFailed: Long
        get() = sp.getLong("postsFailed", 0L)
        set(v) = sp.edit().putLong("postsFailed", v).apply()
    var atHome: Boolean?
        get() = if (sp.contains("atHome")) sp.getBoolean("atHome", true) else null
        set(v) = sp.edit().also { if (v == null) it.remove("atHome") else it.putBoolean("atHome", v) }.apply()
    val homeList: List<String> get() = Home.parse(home)
    var nonce: Long
        get() = sp.getLong("nonce", 0L)
        set(v) = sp.edit().putLong("nonce", v).apply()
    var mode: String
        get() = sp.getString("mode", "") ?: ""
        set(v) = put("mode", v)
    fun nextId(): String = synchronized(Store::class.java) {
        val n = sp.getLong("idSeq", 0L) + 1
        sp.edit().putLong("idSeq", n).commit()
        EntryId.format(device, n)
    }
    var paused: Boolean
        get() = sp.getBoolean("paused", false)
        set(v) = sp.edit().putBoolean("paused", v).apply()
    var startError: String
        get() = sp.getString("startError", "-") ?: "-"
        set(v) = put("startError", v)
    var running: Boolean
        get() = ScanService.running
        set(v) {
            ScanService.running = v
            sp.edit().putLong("rev", System.nanoTime()).apply()
            refreshTile()
        }

    fun setLabelQuietly(v: String?) = sp.edit().putString("label", v).apply()

    private fun put(key: String, v: String?) {
        sp.edit().putString(key, v).apply()
        if (key == "label" || key == "guess" || key == "status" || key == "mode" || key == "lastPost") refreshTile()
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
    class Reply(val code: Int, val text: String)

    fun send(server: String, path: String, body: String, network: Network?): Reply {
        val url = URL(server.trim().trimEnd('/') + path)
        val conn = (network?.openConnection(url) ?: url.openConnection()) as HttpURLConnection
        try {
            conn.requestMethod = "POST"
            conn.connectTimeout = 8000
            conn.readTimeout = 8000
            conn.doOutput = true
            conn.setRequestProperty("Content-Type", "application/json")
            conn.outputStream.use { it.write(body.toByteArray()) }
            val code = conn.responseCode
            val text = (if (code < 400) conn.inputStream else conn.errorStream)?.bufferedReader()?.use { it.readText() } ?: ""
            return Reply(code, text)
        } finally {
            conn.disconnect()
        }
    }

    fun room(text: String): String = try { JSONObject(text).optString("room", "").takeIf { it != "null" } ?: "" } catch (e: Exception) { "" }
}

object ScanConfig {
    const val MOVING_PERIOD_MS = 30_000L
    const val MOVE_WINDOW_MS = 60_000L
    const val STILL_AFTER_MS = 120_000L
    const val HEARTBEAT_MS = 720_000L
    const val SCAN_HOLD_MS = 30_000L
    const val MIN_GAP_MS = 20_000L
    const val SCAN_WINDOW_MS = 120_000L
    const val SCAN_WINDOW_MAX = 4
    const val FLUSH_HOLD_MS = 300_000L
    const val STALE_MS = 20_000L
    const val AWAY_MS = 900_000L
    const val AWAY_AFTER_MISSES = 3
    const val QUEUE_MAX = 2000
    const val QUEUE_AGE_MS = 48 * 3_600_000L
    const val QUEUED_AFTER_MS = 60_000L
    const val BATCH = 100
    const val BEAT_WAIT_MS = 8_000L
}

enum class Mode(val label: String, val sleeps: Boolean, val motion: String?) {
    MOVING("Bewegung", false, "moving"), SETTLING("Bewegung", false, "moving"), STILL("Ruhe", true, "still"),
    FIXED("Fest 30 s, Aktivitaet fehlt", false, null), AWAY("Unterwegs", true, "still")
}

class ScanPolicy(private val adaptive: Boolean, start: Long) {
    private val base get() = if (adaptive) Mode.MOVING else Mode.FIXED
    var mode = base
        private set
    private var lastMotion = start
    private var nextAt = start
    private var lastScanAt = Long.MIN_VALUE / 4
    private val recent = ArrayDeque<Long>()
    private var misses = 0
    val away: Boolean get() = mode == Mode.AWAY || misses >= ScanConfig.AWAY_AFTER_MISSES

    fun onMotion(now: Long, at: Long): Boolean {
        if (mode == Mode.FIXED || mode == Mode.AWAY || now - at > ScanConfig.STALE_MS) return false
        lastMotion = maxOf(lastMotion, at)
        return when (mode) {
            Mode.STILL -> {
                mode = Mode.MOVING
                val scan = claim(now)
                nextAt = if (scan) now + ScanConfig.MOVING_PERIOD_MS else retryAt()
                scan
            }
            Mode.SETTLING -> {
                mode = Mode.MOVING
                nextAt = now + ScanConfig.MOVING_PERIOD_MS
                false
            }
            else -> false
        }
    }

    fun onScan(home: Boolean, now: Long) {
        misses = if (home) 0 else misses + 1
        if (!home && (mode == Mode.AWAY || misses >= ScanConfig.AWAY_AFTER_MISSES)) {
            mode = Mode.AWAY
            nextAt = now + ScanConfig.AWAY_MS
        } else if (home && mode == Mode.AWAY) {
            mode = base
            lastMotion = now
            nextAt = now + ScanConfig.MOVING_PERIOD_MS
        }
    }

    fun onTick(now: Long): Boolean = when (mode) {
        Mode.FIXED -> due(now, ScanConfig.MOVING_PERIOD_MS)
        Mode.MOVING ->
            if (now - lastMotion >= ScanConfig.MOVE_WINDOW_MS) {
                mode = Mode.SETTLING
                claim(now)
            } else due(now, ScanConfig.MOVING_PERIOD_MS)
        Mode.SETTLING -> {
            if (now - lastMotion >= ScanConfig.STILL_AFTER_MS) {
                mode = Mode.STILL
                nextAt = now + ScanConfig.HEARTBEAT_MS
            }
            false
        }
        Mode.STILL -> due(now, ScanConfig.HEARTBEAT_MS)
        Mode.AWAY -> due(now, ScanConfig.AWAY_MS)
    }

    fun delay(now: Long): Long = maxOf(0L, when (mode) {
        Mode.FIXED, Mode.STILL, Mode.AWAY -> nextAt
        Mode.MOVING -> minOf(nextAt, lastMotion + ScanConfig.MOVE_WINDOW_MS)
        Mode.SETTLING -> lastMotion + ScanConfig.STILL_AFTER_MS
    } - now)

    private fun claim(now: Long): Boolean {
        if (now - lastScanAt < ScanConfig.MIN_GAP_MS) return false
        while (recent.isNotEmpty() && now - recent.first() >= ScanConfig.SCAN_WINDOW_MS) recent.removeFirst()
        if (recent.size >= ScanConfig.SCAN_WINDOW_MAX) return false
        recent.addLast(now)
        lastScanAt = now
        return true
    }

    private fun retryAt(): Long = maxOf(
        lastScanAt + ScanConfig.MIN_GAP_MS,
        if (recent.size >= ScanConfig.SCAN_WINDOW_MAX) recent.first() + ScanConfig.SCAN_WINDOW_MS else Long.MIN_VALUE
    )

    private fun due(now: Long, period: Long): Boolean {
        if (now < nextAt) return false
        if (!claim(now)) {
            nextAt = retryAt()
            return false
        }
        nextAt = now + period
        return true
    }
}

const val TAG = "HeatScan"
