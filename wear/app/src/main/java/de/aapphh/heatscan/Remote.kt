package de.aapphh.heatscan

import android.content.Context
import android.net.Network
import androidx.wear.tiles.TileService
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException

object AlertList {
    const val SEP = "\u001f"

    fun json(a: List<Alert>): String = JSONArray().also { arr -> a.forEach { arr.put(JSONObject().put("id", it.id).put("title", it.title).put("text", it.text)) } }.toString()

    fun fromJson(text: String?): List<Alert> = try {
        val arr = JSONArray(text ?: "[]")
        (0 until arr.length()).mapNotNull { ReplyJson.alert(arr.getJSONObject(it)) }
    } catch (e: Exception) { emptyList() }

    fun fresh(alerts: List<Alert>, seen: List<String>): List<Alert> = alerts.filter { it.id !in seen }.distinctBy { it.id }

    fun remember(seen: List<String>, shown: List<Alert>, max: Int = 40): List<String> = (seen + shown.map { it.id }).distinct().takeLast(max)

    fun migrate(seen: List<String>, old: String?, max: Int = 40): List<String> = if (old.isNullOrEmpty()) seen else (seen + old).distinct().takeLast(max)

    fun notificationId(a: Alert): Int = 1000 + (a.id.hashCode() and 0xFFFF)
}

data class RoomGlance(val room: String, val temp: Double?, val target: Double?, val open: Boolean, val reason: String? = null, val boosting: Boolean = false, val boostUntil: Long? = null)

data class Glance(val at: Long, val rooms: List<RoomGlance>) {
    fun json(): String = JSONObject().put("at", at).put("rooms", JSONArray().also { a ->
        rooms.forEach { a.put(JSONObject().put("room", it.room).put("temp", it.temp ?: JSONObject.NULL).put("target", it.target ?: JSONObject.NULL).put("open", it.open).put("reason", it.reason ?: JSONObject.NULL).put("boosting", it.boosting).put("boostUntil", it.boostUntil ?: JSONObject.NULL)) }
    }).toString()

    fun openRooms(): List<String> = rooms.filter { it.open }.map { it.room }

    companion object {
        val EMPTY = Glance(0L, emptyList())

        private fun num(o: JSONObject, k: String): Double? = if (o.isNull(k)) null else o.optDouble(k).takeIf { !it.isNaN() }

        private fun str(o: JSONObject, k: String): String? = if (o.isNull(k)) null else o.optString(k).takeIf { it.isNotEmpty() }

        fun fromJson(text: String?): Glance = try {
            val o = JSONObject(text ?: "")
            val a = o.getJSONArray("rooms")
            Glance(o.getLong("at"), (0 until a.length()).map { a.getJSONObject(it) }.map { RoomGlance(it.getString("room"), num(it, "temp"), num(it, "target"), it.optBoolean("open"), str(it, "reason"), it.optBoolean("boosting"), if (it.isNull("boostUntil")) null else it.optLong("boostUntil")) })
        } catch (e: Exception) { EMPTY }

        fun parse(text: String, now: Long): Glance? = try {
            val a = JSONObject(text).getJSONArray("rooms")
            Glance(now, (0 until a.length()).map { a.getJSONObject(it) }.filter { (it.optJSONArray("nodes")?.length() ?: 0) > 0 }.map { r ->
                val nodes = r.getJSONArray("nodes")
                val nodeList = (0 until nodes.length()).map { nodes.getJSONObject(it) }
                val boost = r.optJSONObject("boost")
                val reason = nodeList.firstNotNullOfOrNull { str(it, "reason") }
                val written = boost?.optJSONObject("cycle")?.let { num(it, "written_ts") }
                val minutes = boost?.optJSONObject("cycle")?.let { num(it, "minutes") }
                RoomGlance(
                    r.getString("name"),
                    nodeList.firstNotNullOfOrNull { num(it, "local_temperature") },
                    nodeList.firstNotNullOfOrNull { num(it, "commanded_setpoint") ?: num(it, "occupied_heating_setpoint") },
                    r.optBoolean("window_open"),
                    reason,
                    boost?.optString("state") in Boost.ACTIVE || reason == "boost",
                    if (written != null && minutes != null) (written + minutes * 60_000.0).toLong() else null
                )
            })
        } catch (e: Exception) { null }

        fun alerts(text: String): List<Alert>? = try { ReplyJson.alerts(JSONObject(text)) } catch (e: Exception) { null }

        fun temp(v: Double?): String = if (v == null) "-" else "%.1f".format(java.util.Locale.GERMANY, v)

        fun line(r: RoomGlance): String = "${Defaults.CAPTIONS[r.room] ?: r.room} ${temp(r.temp)}/${temp(r.target)}"

        const val STALE_MS = 3_600_000L
    }
}

data class BoostResult(val ok: Boolean, val line: String)

object Boost {
    const val PATH = "/api/boost"
    const val STATE_PATH = "/api/state"
    const val DEFAULT_MIN = 5
    const val START = "Starten"
    const val CANCEL = "Abbrechen"
    const val NO_WIFI = "Kein WLAN"
    const val STATE_MIN_GAP_MS = 60_000L
    val ACTIVE = setOf("armed", "boosting")

    fun body(room: String, minutes: Int = DEFAULT_MIN): String = JSONObject().put("room", room).put("minutes", minutes).toString()

    fun question(room: String) = "${WindowEvent.caption(room)} $DEFAULT_MIN min aufheizen?"

    fun result(code: Int, text: String): BoostResult = try {
        val o = JSONObject(text)
        val ok = code in 200..299 && o.optBoolean("ok")
        val reason = if (o.isNull("reason")) "" else o.optString("reason")
        val minutes = o.optJSONObject("cycle")?.optDouble("minutes")?.takeIf { !it.isNaN() }
        BoostResult(ok, if (ok) "Boost läuft: ${minutes?.let { Math.round(it) } ?: DEFAULT_MIN} min" else BoostText.plain(reason))
    } catch (e: Exception) { BoostResult(false, if (code <= 0) "Server nicht erreichbar" else "Antwort unlesbar ($code)") }

    fun failed(line: String = "Server nicht erreichbar"): BoostResult = BoostResult(false, line)
}

object Remote {
    private val io = Executors.newSingleThreadExecutor()

    private fun run(f: () -> Unit) {
        try { io.execute(f) } catch (e: RejectedExecutionException) { Trace.i("remote_rejected") }
    }

    fun wifi(): Network? = ScanService.instance?.currentNetwork

    fun refresh(c: Context, force: Boolean = false, done: () -> Unit = {}) {
        val app = c.applicationContext
        val store = Store(app)
        val now = System.currentTimeMillis()
        val net = wifi()
        if (net == null || (!force && Span.ms(now, maxOf(store.glance.at, store.glanceTryAt)) < Boost.STATE_MIN_GAP_MS)) return done()
        store.glanceTryAt = now
        run {
            try {
                val r = Poster.get(store.server, Boost.STATE_PATH, net)
                if (r.code in 200..299) {
                    Glance.parse(r.text, System.currentTimeMillis())?.let { store.glance = it }
                    Glance.alerts(r.text)?.let { store.alerts = it }
                    TileService.getUpdater(app).requestUpdate(HeatTileService::class.java)
                }
            } catch (e: Exception) {
                Trace.w("glance_failed", e)
            } finally {
                done()
            }
        }
    }

    fun boost(c: Context, room: String, done: (BoostResult) -> Unit) {
        val store = Store(c.applicationContext)
        val net = wifi() ?: return done(Boost.failed(Boost.NO_WIFI))
        run {
            done(try {
                Poster.send(store.server, Boost.PATH, Boost.body(room), net).let { Boost.result(it.code, it.text) }
            } catch (e: Exception) {
                Trace.w("boost_failed", e)
                Boost.failed()
            })
        }
    }
}
