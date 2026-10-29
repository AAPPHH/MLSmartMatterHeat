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
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

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

object Span {
    fun ms(a: Long, b: Long): Long {
        val r = a - b
        return if (((a xor b) and (a xor r)) < 0) (if (a < 0) Long.MIN_VALUE else Long.MAX_VALUE) else r
    }

    fun within(now: Long, at: Long, max: Long) = ms(now, at) in 0..max
}

object LabelEvent {
    const val KIND = "label"
    const val MAX_SPAN_MS = 3_600_000L

    fun window(from: Long, now: Long): Pair<Long, Long> = Pair(minOf(maxOf(from, Span.ms(now, MAX_SPAN_MS)), now), now)

    fun entry(device: String, from: Long, to: Long, label: String): JSONObject =
        JSONObject().put("ts", to).put("kind", KIND).put("device", device).put("from", from).put("to", to).put("label", label)

    fun isLabel(o: JSONObject) = o.optString("kind") == KIND

    fun watchBody(o: JSONObject): String = JSONObject().put("device", o.getString("device")).put("ts", o.getLong("to")).put("from", o.getLong("from")).put("to", o.getLong("to")).put("room", o.getString("label")).put("source", "watch").toString()

    fun body(o: JSONObject): String = JSONObject().put("device", o.getString("device")).put("from", o.getLong("from")).put("to", o.getLong("to")).put("label", o.getString("label")).toString()
}

object Prompt {
    const val ALERT_GAP_MS = 180_000L
    const val TIMEOUT_MS = 600_000L
    const val DONE_TIMEOUT_MS = 4_000L
    val ORDER = listOf("kueche", "wohnzimmer", "flur", "bad", "schlafzimmer")

    fun quiet(hour: Int) = hour >= 22 || hour < 7

    fun alert(now: Long, lastAlertAt: Long, away: Boolean, offBody: Boolean, hour: Int): Boolean =
        !away && !offBody && !quiet(hour) && Span.ms(now, lastAlertAt) >= ALERT_GAP_MS

    const val WAKE_GAP_MS = 600_000L

    fun title(change: Boolean) = if (change) "Raum gewechselt? Wo bist du jetzt?" else "Wo bist du?"

    fun rooms(guess: String?): List<String> = if (guess != null && guess in ORDER) listOf(guess) + ORDER.filter { it != guess } else ORDER

    fun caption(room: String, guess: String?): String = (if (room == guess) "● " else "") + Rings.name(room)

    fun done(room: String) = "✓ Gespeichert: " + Rings.name(room)

    fun measured(room: String, state: String) = when (state) {
        "wait" -> "Messe …"
        "ok" -> done(room) + " · Messung ok"
        "rejected" -> done(room) + " · Messung abgelehnt, gilt für die nächste"
        else -> done(room)
    }
}

object Fingerprint {
    const val FLOOR = -85
    const val MISSING = -100f
    const val MAX_DIST_DB = 8f
    const val MIN_OVERLAP = 0.5f

    fun of(aps: List<Ap>): Map<String, Float> = aps.groupBy { it.bssid.lowercase() }.mapValues { e -> e.value.maxOf { it.rssi }.toFloat() }

    private fun strong(f: Map<String, Float>) = f.filterValues { it >= FLOOR }.keys

    fun distance(a: Map<String, Float>, b: Map<String, Float>): Float {
        val keys = strong(a) + strong(b)
        return if (keys.isEmpty()) 0f else keys.sumOf { Math.abs((a[it] ?: MISSING) - (b[it] ?: MISSING)).toDouble() }.toFloat() / keys.size
    }

    fun overlap(a: Map<String, Float>, b: Map<String, Float>): Float {
        val u = strong(a) + strong(b)
        return if (u.isEmpty()) 1f else strong(a).intersect(strong(b)).size.toFloat() / u.size
    }

    fun differs(a: Map<String, Float>, b: Map<String, Float>): Boolean = overlap(a, b) < MIN_OVERLAP || distance(a, b) > MAX_DIST_DB

    fun blend(avg: Map<String, Float>, n: Int, cur: Map<String, Float>): Map<String, Float> =
        (avg.keys + cur.keys).associateWith { ((avg[it] ?: MISSING) * n + (cur[it] ?: MISSING)) / (n + 1) }

    fun json(f: Map<String, Float>): JSONObject = JSONObject().also { o -> f.forEach { (k, v) -> o.put(k, v.toDouble()) } }

    fun parse(o: JSONObject?): Map<String, Float> = o?.keys()?.asSequence()?.associateWith { o.getDouble(it).toFloat() } ?: emptyMap()
}

class Decision(val label: String? = null, val ask: Boolean = false, val since: Long = 0L, val change: Boolean = false)

class StayTracker {
    var label: String? = null
    var confirmedAt = 0L
    var fp: Map<String, Float> = emptyMap()
    var n = 0
    var moved = false
    var diff = 0
    var diffAt = 0L
    var gDiff = 0
    var gAt = 0L
    var gMoved = false
    var needAsk = true
    var lastFp: Map<String, Float> = emptyMap()
    var lastTs = 0L
    var askAt = 0L
    var askSince = 0L
    var askTs = 0L
    var askFp: Map<String, Float> = emptyMap()
    var askGuess: String? = null
    var askChange = false
    var askLoud = false
    var askWake = false
    var lastAskAt = 0L

    fun holding(now: Long): String? = label?.takeIf { Span.ms(now, confirmedAt) in 0 until CAP_MS }

    fun pending(now: Long) = askAt != 0L && Span.within(now, askAt, Prompt.TIMEOUT_MS)

    fun fresh(now: Long) = lastTs != 0L && Span.within(now, lastTs, BACK_MS)

    fun windowStart(now: Long): Long = when {
        pending(now) && askWake -> if (fresh(now)) lastTs else now
        pending(now) -> askSince
        else -> maxOf(lastTs, Span.ms(now, BACK_MS))
    }

    private fun end() {
        label = null
        diff = 0
        gDiff = 0
        gMoved = false
        moved = false
    }

    private fun ask(now: Long, ts: Long, since: Long, fp: Map<String, Float>, guess: String?, change: Boolean): Decision {
        needAsk = false
        askAt = now
        askSince = since
        askTs = ts
        askFp = fp
        askGuess = guess
        askChange = change
        askWake = false
        lastAskAt = now
        return Decision(null, true, since, change)
    }

    fun promptWithoutScan(now: Long, guess: String?, wake: Boolean): Decision {
        if (holding(now) != null || pending(now)) return Decision(holding(now))
        if (wake && Span.within(now, lastAskAt, Prompt.WAKE_GAP_MS)) return Decision()
        val since = if (fresh(now)) lastTs else now
        val d = ask(now, since, since, lastFp, guess, false)
        askWake = true
        return d
    }

    fun reset() {
        end()
        needAsk = true
    }

    fun confirm(room: String, now: Long) {
        val start = when {
            pending(now) && askWake -> if (fresh(now)) lastFp else emptyMap()
            pending(now) -> askFp
            else -> lastFp
        }
        end()
        fp = start
        n = 1
        label = room
        confirmedAt = now
        needAsk = false
        askAt = 0L
    }

    fun onScan(ts: Long, now: Long, aps: List<Ap>, moving: Boolean, motion: Boolean, guess: String?): Decision {
        val cur = Fingerprint.of(aps)
        if (cur.isEmpty()) return Decision(holding(now))
        val first = lastTs == 0L
        lastFp = cur
        lastTs = ts
        moved = moved || motion || moving
        gMoved = gMoved || motion || moving
        if (label != null && holding(now) == null) {
            end()
            needAsk = true
        }
        val s = label
        if (s != null && fp.isEmpty()) {
            fp = cur
            n = 1
            return Decision(s)
        }
        if (s == null) {
            if (motion || moving) needAsk = true
            return if (needAsk && (!moving || first)) ask(now, ts, ts, cur, guess, false) else Decision(null, false, ts)
        }
        if (!Fingerprint.differs(fp, cur)) {
            diff = 0
            moved = false
            if (n < AVG_SCANS) {
                fp = Fingerprint.blend(fp, n, cur)
                n++
            }
            return Decision(s)
        }
        if (diff == 0) diffAt = ts
        diff++
        if (moved && diff >= 2) {
            val at = diffAt
            end()
            return ask(now, ts, at, cur, guess, true)
        }
        return Decision(s)
    }

    fun onManual(ts: Long, now: Long, aps: List<Ap>, moving: Boolean, motion: Boolean, guess: String?): Decision {
        val d = onScan(ts, now, aps, moving, motion, guess)
        val cur = Fingerprint.of(aps)
        if (label != null && cur.isNotEmpty()) {
            fp = cur
            n = 1
            diff = 0
        }
        return d
    }

    fun onGuess(scanTs: Long, room: String, p: Float?, now: Long): Decision {
        val s = holding(now) ?: return Decision()
        if (p != null && p >= GUESS_P && room != s) {
            if (gDiff == 0) gAt = scanTs
            gDiff++
        } else {
            gDiff = 0
            gMoved = false
        }
        if (gDiff >= 2 && gMoved) {
            val at = gAt
            end()
            return ask(now, scanTs, at, lastFp, room, true)
        }
        return Decision(s)
    }

    fun refresh(scanTs: Long, now: Long, guess: String?): Boolean {
        if (!pending(now) || scanTs < askTs || guess == null || guess == askGuess) return false
        askGuess = guess
        return true
    }

    fun json(): String = JSONObject()
        .put("label", label ?: JSONObject.NULL).put("confirmedAt", confirmedAt).put("fp", Fingerprint.json(fp)).put("n", n).put("moved", moved)
        .put("diff", diff).put("diffAt", diffAt).put("gDiff", gDiff).put("gAt", gAt).put("gMoved", gMoved).put("needAsk", needAsk)
        .put("lastFp", Fingerprint.json(lastFp)).put("lastTs", lastTs).put("askAt", askAt).put("askSince", askSince).put("askTs", askTs)
        .put("askFp", Fingerprint.json(askFp)).put("askGuess", askGuess ?: JSONObject.NULL).put("askChange", askChange).put("askLoud", askLoud).put("askWake", askWake).put("lastAskAt", lastAskAt).toString()

    companion object {
        const val CAP_MS = 7_200_000L
        const val BACK_MS = 120_000L
        const val AVG_SCANS = 5
        const val GUESS_P = 0.6f

        fun parse(text: String?): StayTracker = StayTracker().also { t ->
            try {
                val o = JSONObject(text ?: return@also)
                t.label = if (o.isNull("label")) null else o.optString("label").takeIf { it in Defaults.ROOMS }
                t.confirmedAt = o.optLong("confirmedAt")
                t.fp = Fingerprint.parse(o.optJSONObject("fp"))
                t.n = o.optInt("n")
                t.moved = o.optBoolean("moved")
                t.diff = o.optInt("diff")
                t.diffAt = o.optLong("diffAt")
                t.gDiff = o.optInt("gDiff")
                t.gAt = o.optLong("gAt")
                t.gMoved = o.optBoolean("gMoved")
                t.needAsk = o.optBoolean("needAsk", true)
                t.lastFp = Fingerprint.parse(o.optJSONObject("lastFp"))
                t.lastTs = o.optLong("lastTs")
                t.askAt = o.optLong("askAt")
                t.askSince = o.optLong("askSince")
                t.askTs = o.optLong("askTs")
                t.askFp = Fingerprint.parse(o.optJSONObject("askFp"))
                t.askGuess = if (o.isNull("askGuess")) null else o.optString("askGuess")
                t.askChange = o.optBoolean("askChange")
                t.askLoud = o.optBoolean("askLoud")
                t.askWake = o.optBoolean("askWake")
                t.lastAskAt = o.optLong("lastAskAt")
            } catch (e: Exception) {
                Log.w(TAG, "stay parse failed", e)
            }
        }
    }
}

object Wake {
    const val GAP_MS = 20_000L

    const val FRESH_MS = 120_000L

    fun scanDue(now: Long, lastRequestAt: Long, idle: Boolean) = !idle && now - lastRequestAt >= GAP_MS

    fun promptAllowed(away: Boolean, offBody: Boolean?, wake: Boolean, interactive: Boolean) = !away && offBody != true && (!wake || interactive)

    fun settled(before: Mode, after: Mode) = before == Mode.MOVING && (after == Mode.SETTLING || after == Mode.STILL)
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

data class Part(val bssid: String, val share: Float)

data class RoomP(val room: String, val p: Float, val parts: List<Part>)

data class NamedAp(val bssid: String, val ssid: String, val rssi: Int, val freq: Int)

class Reply(val probability: Float?, val rooms: List<RoomP>, val aps: List<Ap>, val hasRooms: Boolean = true)

object ReplyJson {
    private fun <T> list(text: String?, f: (JSONObject) -> T): List<T> = try {
        val a = JSONArray(text ?: "[]")
        (0 until a.length()).map { f(a.getJSONObject(it)) }
    } catch (e: Exception) { emptyList() }

    private fun apOf(o: JSONObject) = Ap(o.getString("bssid").lowercase(), o.getInt("rssi"), o.getInt("freq"))

    fun rooms(text: String?): List<RoomP> = list(text) { o ->
        RoomP(o.getString("room"), o.getDouble("p").toFloat(), list(o.optJSONArray("parts")?.toString()) { Part(it.getString("bssid").lowercase(), it.getDouble("share").toFloat()) })
    }

    fun aps(text: String?): List<Ap> = list(text, ::apOf)

    fun named(text: String?): List<NamedAp> = list(text) { NamedAp(it.getString("bssid").lowercase(), it.optString("ssid", ""), it.getInt("rssi"), it.getInt("freq")) }

    fun roomsJson(rooms: List<RoomP>): String = JSONArray().also { a ->
        rooms.forEach { r ->
            a.put(JSONObject().put("room", r.room).put("p", r.p.toDouble()).put("parts", JSONArray().also { pa ->
                r.parts.forEach { pa.put(JSONObject().put("bssid", it.bssid).put("share", it.share.toDouble())) }
            }))
        }
    }.toString()

    fun apsJson(aps: List<Ap>): String = JSONArray().also { a ->
        aps.forEach { a.put(JSONObject().put("bssid", it.bssid).put("rssi", it.rssi).put("freq", it.freq)) }
    }.toString()

    fun namedJson(aps: List<NamedAp>): String = JSONArray().also { a ->
        aps.forEach { a.put(JSONObject().put("bssid", it.bssid).put("ssid", it.ssid).put("rssi", it.rssi).put("freq", it.freq)) }
    }.toString()

    fun parse(text: String): Reply = try {
        val o = JSONObject(text)
        Reply(
            if (o.isNull("probability")) null else o.optDouble("probability").toFloat().takeIf { !it.isNaN() },
            rooms(o.optJSONArray("rooms")?.toString()), aps(o.optJSONArray("aps")?.toString()), o.has("rooms")
        )
    } catch (e: Exception) { Reply(null, emptyList(), emptyList(), false) }

    private fun rssiTs(o: JSONObject): List<Long> = o.optJSONArray("samples")?.let { a ->
        (0 until a.length()).map { a.getJSONObject(it) }.filter { it.optString("sensor_type") == "rssi" }.map { it.getLong("ts") }
    } ?: emptyList()

    fun postedAps(body: String): Boolean = try {
        val o = JSONObject(body)
        val last = o.optJSONArray("scans")?.let { if (it.length() > 0) it.getJSONObject(it.length() - 1) else null } ?: o
        (last.optJSONArray("aps")?.length() ?: 0) > 0 || rssiTs(o).isNotEmpty()
    } catch (e: Exception) { false }

    fun lastTs(body: String): Long? = try {
        val o = JSONObject(body)
        rssiTs(o).maxOrNull() ?: (o.optJSONArray("scans")?.let { if (it.length() > 0) it.getJSONObject(it.length() - 1) else null } ?: o).optLong("ts").takeIf { it > 0L }
    } catch (e: Exception) { null }

    fun keeps(r: Reply, postedAps: Boolean): Boolean = !r.hasRooms || postedAps
}

data class Slot(val color: Int, val used: Long)

object ColorBook {
    val PALETTE = listOf(0xFFE8710A.toInt(), 0xFF6CB4FF.toInt(), 0xFFF5C542.toInt(), 0xFFB48CFF.toInt(), 0xFF4DD0B5.toInt(), 0xFFF28BB4.toInt())
    val OTHER = 0xFF5A5A5A.toInt()

    fun assign(current: Map<String, Slot>, used: List<String>, now: Long): Map<String, Slot> {
        val book = LinkedHashMap(current.filterValues { it.color in PALETTE })
        used.filter { it in book }.forEach { book[it] = book.getValue(it).copy(used = now) }
        used.filter { it !in book }.forEach { b ->
            val taken = book.values.map { it.color }.toSet()
            val color = PALETTE.firstOrNull { it !in taken } ?: book.entries.filter { it.key !in used }
                .minWith(compareBy<Map.Entry<String, Slot>> { it.value.used }.thenBy { it.key }).also { book.remove(it.key) }.value.color
            book[b] = Slot(color, now)
        }
        return book
    }

    fun json(book: Map<String, Slot>): String = JSONObject().also { o -> book.forEach { (b, s) -> o.put(b, JSONArray().put(s.color).put(s.used)) } }.toString()

    fun parse(text: String?): Map<String, Slot> = try {
        val o = JSONObject(text ?: "{}")
        o.keys().asSequence().associateWith { o.getJSONArray(it).let { a -> Slot(a.getInt(0), a.getLong(1)) } }.filterValues { it.color in PALETTE }
    } catch (e: Exception) { emptyMap() }
}

object ApNames {
    private const val MAX = 13

    private fun band(freq: Int) = when {
        freq >= 5925 -> " 6"
        freq >= 4900 -> " 5"
        else -> " 2.4"
    }

    fun base(bssid: String, named: List<NamedAp>): String =
        named.firstOrNull { it.bssid == bssid }?.ssid?.takeIf { it.isNotBlank() } ?: ("…" + bssid.takeLast(5))

    fun labels(aps: List<Ap>, named: List<NamedAp>, max: Int = MAX): Map<String, String> {
        val names = aps.associate { it.bssid to base(it.bssid, named) }
        val dup = names.values.groupingBy { it }.eachCount().filterValues { it > 1 }.keys
        return aps.associate { ap ->
            val n = names.getValue(ap.bssid)
            val suffix = if (n in dup) band(ap.freq) else ""
            ap.bssid to n.substringBefore(' ').ifEmpty { n }.take(max - 4).trimEnd('-', '_', '.', '!') + suffix
        }
    }
}

object Rings {
    const val REF = 213f
    const val STROKE = 9f / REF
    const val GAP = 2f / REF
    const val MARGIN = 8f / REF
    const val START = 135f
    const val SWEEP = 270f
    const val TOP_COLOURS = 6
    const val LEGEND_MAX = 4
    const val NAME_SP = 15f
    const val SUB_SP = 9f
    const val LINE_SP = 8f
    const val SUB_GAP = 3f
    const val LEGEND_CHARS = 17
    const val CHAR_W = 4.3f
    const val MAX_RINGS = 5
    const val LABEL_DX = 4f / REF
    const val LINE_H = 1.15f
    val REST = 0xFF3A3A3A.toInt()
    val CAPTIONS = mapOf("wohnzimmer" to "Wo", "schlafzimmer" to "Sch", "bad" to "Bad", "kueche" to "Kü", "flur" to "Fl")

    class Seg(val sweep: Float, val color: Int)

    fun order(rooms: List<RoomP>): List<RoomP> =
        if (rooms.isEmpty()) Defaults.ROOMS.map { RoomP(it, 0f, emptyList()) } else rooms.sortedByDescending { it.p }.take(MAX_RINGS)

    fun radius(i: Int, d: Float): Float = d / 2f - (MARGIN + i * (STROKE + GAP)) * d

    fun caption(room: String): String = CAPTIONS[room] ?: room.take(3).replaceFirstChar { it.uppercase() }

    fun name(room: String): String = Defaults.CAPTIONS[room] ?: room.replaceFirstChar { it.uppercase() }

    fun palette(rooms: List<RoomP>): List<String> =
        rooms.flatMap { it.parts }.groupBy { it.bssid }.mapValues { e -> e.value.sumOf { it.share.toDouble() } }
            .entries.sortedWith(compareByDescending<Map.Entry<String, Double>> { it.value }.thenBy { it.key }).take(TOP_COLOURS).map { it.key }

    fun segments(room: RoomP, colors: Map<String, Slot>): List<Seg> {
        val fill = room.p.coerceIn(0f, 1f)
        val out = ArrayList<Seg>()
        var used = 0f
        fun add(share: Float, color: Int) {
            val s = minOf(share, fill - used)
            if (s > 0f) {
                out.add(Seg(s * SWEEP, color))
                used += s
            }
        }
        ColorBook.PALETTE.forEach { c ->
            colors.filterValues { it.color == c }.keys.firstOrNull()?.let { b -> add(room.parts.filter { it.bssid == b }.sumOf { it.share.toDouble() }.toFloat(), c) }
        }
        add(room.parts.filter { it.bssid !in colors }.sumOf { it.share.toDouble() }.toFloat(), ColorBook.OTHER)
        add(fill - used, REST)
        return out
    }
}

object Legend {
    fun rssi(v: Int) = v.toString().replace('-', '−')

    fun text(name: String, rssi: Int) = "$name ${rssi(rssi)}"

    fun aps(palette: List<String>, replyAps: List<Ap>, named: List<NamedAp>): List<Ap> =
        palette.take(Rings.LEGEND_MAX).mapNotNull { b -> replyAps.firstOrNull { it.bssid == b } ?: named.firstOrNull { it.bssid == b }?.let { Ap(it.bssid, it.rssi, it.freq) } }
            .ifEmpty { Footer.topAps(named, Rings.LEGEND_MAX).map { Ap(it.bssid, it.rssi, it.freq) } }
}

object Demo {
    const val GUESS = "kueche"
    const val P = 0.82f
    val NAMED = listOf(
        NamedAp("02:00:00:00:00:0a", "FRITZ!Box 1234 AB", -48, 5180), NamedAp("02:00:00:00:00:0b", "FRITZ!Box 1234 AB", -55, 2412),
        NamedAp("02:00:00:00:00:0c", "Nachbar", -71, 2437), NamedAp("02:00:00:00:00:0d", "Vodafone-A3", -79, 5200), NamedAp("02:00:00:00:00:0e", "Magenta", -82, 2462)
    )
    val APS = NAMED.map { Ap(it.bssid, it.rssi, it.freq) }

    private fun parts(vararg v: Float) = v.mapIndexed { i, s -> Part(NAMED[i].bssid, s) }

    val ROOMS = listOf(
        RoomP("kueche", P, parts(0.40f, 0.25f, 0.10f, 0.05f)),
        RoomP("wohnzimmer", 0.09f, listOf(Part(NAMED[1].bssid, 0.05f), Part(NAMED[4].bssid, 0.03f))),
        RoomP("flur", 0.06f, listOf(Part(NAMED[2].bssid, 0.04f), Part(NAMED[3].bssid, 0.02f))),
        RoomP("bad", 0.02f, listOf(Part(NAMED[4].bssid, 0.02f))),
        RoomP("schlafzimmer", 0.01f, emptyList())
    )
}

object Footer {
    const val Y = 190f / Rings.REF

    fun line(postHm: String, ok: Boolean, queue: Int, paused: Boolean = false, fresh: String = "", rejected: Int = 0) =
        if (paused) "pausiert" else "${postHm.ifEmpty { "-" }} · ${if (ok) "OK" else "ERR"} · Q$queue" +
            (if (fresh.isNotEmpty() && fresh != "-") " · S$fresh" else "") + (if (rejected > 0) " · R$rejected" else "")

    fun fresh(ts: Long): String = if (ts <= 0L) "-" else SimpleDateFormat("HH:mm", Locale.US).format(Date(ts))

    fun hm(stamp: String): String = stamp.take(5)

    fun topAps(named: List<NamedAp>, n: Int = 8): List<NamedAp> = named.sortedByDescending { it.rssi }.take(n)
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
        motion: String? = null, power: String? = null, away: Boolean = false, id: String? = null, elapsedNs: Long? = null, bedtime: Boolean? = null
    ): JSONObject = JSONObject()
        .put("ts", ts)
        .put("device", device)
        .also { if (id != null) it.put("id", id) }
        .also { if (elapsedNs != null) it.put("elapsed_ns", elapsedNs) }
        .put("label", label ?: JSONObject.NULL)
        .put("motion", motion ?: JSONObject.NULL)
        .put("power", power ?: JSONObject.NULL)
        .put("away", away)
        .also { if (bedtime != null) it.put("bedtime", bedtime) }
        .put("aps", JSONArray().also { arr ->
            aps.forEach { arr.put(JSONObject().put("bssid", it.bssid.lowercase()).put("rssi", it.rssi).put("freq", it.freq)) }
        })
}

object EntryId {
    fun format(device: String, n: Long) = "$device-%06d".format(n)
}

object Bedtime {
    val GLOBAL_KEYS = listOf("bedtime_mode", "bedtime_mode_enabled", "wear_bedtime_mode", "setting_bedtime_mode_running_state")
    val SECURE_KEYS = listOf("bedtime_mode", "bedtime_mode_enabled")

    fun of(values: List<Int?>): Boolean? = values.filterNotNull().let { v -> if (v.isEmpty()) null else v.any { it == 1 } }
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

class ScanQueue(private val file: File?, private val max: Int = ScanConfig.QUEUE_MAX, private val maxAgeMs: Long = ScanConfig.QUEUE_AGE_MS, stripLabels: Boolean = false) {
    private class Item(val seq: Long, val ts: Long, val line: String)

    private val items = ArrayDeque<Item>()
    private var nextSeq = 1L
    private var dirty = false

    @Volatile var size = 0
        private set

    init {
        var stripped = false
        try {
            file?.takeIf { it.exists() }?.useLines { lines ->
                lines.forEach { l ->
                    try {
                        val o = JSONObject(l)
                        val old = stripLabels && !LabelEvent.isLabel(o) && !o.isNull("label") && o.has("label")
                        if (old) stripped = true
                        items.addLast(Item(nextSeq, o.getLong("ts"), if (old) o.put("label", JSONObject.NULL).toString() else l))
                        nextSeq++
                    } catch (e: Exception) { Log.w(TAG, "queue line dropped", e) }
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "queue load failed", e)
        }
        val trimmed = items.size > max
        while (items.size > max) items.removeFirst()
        size = items.size
        if (trimmed || stripped) rewrite()
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

    fun defer(seq: Long): Long = synchronized(this) {
        val i = items.first { it.seq == seq }
        items.remove(i)
        val moved = Item(nextSeq++, i.ts, i.line)
        items.addLast(moved)
        dirty = true
        moved.seq
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

        private const val FORMAT = 2

        fun get(c: Context): ScanQueue = synchronized(this) {
            shared ?: Store(c).let { st ->
                ScanQueue(File(c.applicationContext.filesDir, "queue.jsonl"), stripLabels = st.queueFormat < FORMAT).also { shared = it; st.queueFormat = FORMAT }
            }
        }
    }
}

data class FlushResult(val sent: Int, val code: Int, val deferred: Int = 0)

class Flusher(private val maxBatch: Int = ScanConfig.BATCH) {
    private var batch = maxBatch

    var singleOnly = false
        private set

    val batchSize: Int get() = batch

    private fun accepted(c: Int) = c in 200..299

    private fun rejected(c: Int) = c in 400..499 && c != 404 && c != 408 && c != 429

    private fun settled(c: Int) = accepted(c) || rejected(c)

    fun run(queue: ScanQueue, now: Long, sendBatch: (JSONArray) -> Int, sendOne: (JSONObject) -> Int, sendLabel: (JSONObject) -> Int = { 200 }, sendMeta: (JSONObject) -> Int = { 200 }): FlushResult {
        var sent = 0
        var code = 0
        val parked = HashSet<Long>()
        try {
            while (true) {
                val head = queue.peek(batch, now)
                if (head.isEmpty() || head[0].seq in parked) break
                if (Special.isSpecial(head[0].json)) {
                    val c = if (LabelEvent.isLabel(head[0].json)) sendLabel(head[0].json) else sendMeta(head[0].json)
                    if (c == 404) {
                        parked.add(queue.defer(head[0].seq))
                        if (sent == 0 && parked.size == queue.size) code = c
                        continue
                    }
                    code = c
                    if (!settled(code)) return FlushResult(sent, code, parked.size)
                    queue.remove(head[0].seq, false)
                    if (accepted(code)) sent++
                    continue
                }
                val items = head.takeWhile { !Special.isSpecial(it.json) }
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
                    else if (!rejected(code)) return FlushResult(sent, code, parked.size)
                }
                for (e in items) {
                    code = sendOne(e.json)
                    if (!settled(code)) return FlushResult(sent, code, parked.size)
                    queue.remove(e.seq, false)
                    if (accepted(code)) sent++
                }
            }
        } finally {
            queue.commit()
        }
        batch = maxBatch
        return FlushResult(sent, code, parked.size)
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
    fun <T> stay(f: (StayTracker) -> T): T = synchronized(Store::class.java) {
        val t = StayTracker.parse(sp.getString("stay", null))
        val r = f(t)
        sp.edit().putString("stay", t.json()).apply()
        r
    }
    fun stayLabel(now: Long = System.currentTimeMillis()): String? = stay { it.holding(now) }
    var queueFormat: Int
        get() = sp.getInt("queueFormat", 0)
        set(v) = sp.edit().putInt("queueFormat", v).apply()
    var samplePeriodS: Int
        get() = sp.getInt("samplePeriodS", Measure.PERIOD_S)
        set(v) = sp.edit().putInt("samplePeriodS", v).apply()
    var sensorNote: String
        get() = sp.getString("sensorNote", "-") ?: "-"
        set(v) = sp.edit().putString("sensorNote", v).apply()
    var sensorsHash: String
        get() = sp.getString("sensorsHash", "") ?: ""
        set(v) = sp.edit().putString("sensorsHash", v).apply()
    var battPerHour: Float
        get() = sp.getFloat("battPerHour", Float.NaN)
        set(v) = sp.edit().putFloat("battPerHour", v).apply()
    fun nextBatchSeq(): Long = synchronized(Store::class.java) {
        val n = sp.getLong("batchSeq", 0L) + 1
        sp.edit().putLong("batchSeq", n).commit()
        n
    }
    var labels404: Long
        get() = sp.getLong("labels404", 0L)
        set(v) = sp.edit().putLong("labels404", v).apply()
    var lastFreshAt: Long
        get() = sp.getLong("lastFreshAt", 0L)
        set(v) = sp.edit().putLong("lastFreshAt", v).apply()
    var manual: String
        get() = sp.getString("manual", "") ?: ""
        set(v) = sp.edit().putString("manual", v).apply()
    var rejected: Int
        get() = sp.getInt("rejected", 0)
        set(v) = sp.edit().putInt("rejected", v).apply()
    var lastPromptAlertAt: Long
        get() = sp.getLong("lastPromptAlertAt", 0L)
        set(v) = sp.edit().putLong("lastPromptAlertAt", v).apply()
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
        val epoch = sp.getString("idEpoch", null) ?: java.lang.Long.toString(System.currentTimeMillis(), 36).also { sp.edit().putString("idEpoch", it).commit() }
        val n = sp.getLong("idSeq", 0L) + 1
        sp.edit().putLong("idSeq", n).commit()
        EntryId.format("$device-$epoch", n)
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

    var probability: Float?
        get() = if (sp.contains("probability")) sp.getFloat("probability", 0f) else null
        set(v) = sp.edit().also { if (v == null) it.remove("probability") else it.putFloat("probability", v) }.apply()
    val rooms: List<RoomP> get() = ReplyJson.rooms(sp.getString("rooms", null))
    val replyAps: List<Ap> get() = ReplyJson.aps(sp.getString("replyAps", null))
    val lastAps: List<NamedAp> get() = ReplyJson.named(sp.getString("lastAps", null))
    var postHm: String
        get() = sp.getString("postHm", "") ?: ""
        set(v) = sp.edit().putString("postHm", v).apply()
    var postOk: Boolean
        get() = sp.getBoolean("postOk", true)
        set(v) = sp.edit().putBoolean("postOk", v).apply()

    var colors: Map<String, Slot>
        get() = ColorBook.parse(sp.getString("colors", null))
        set(v) = sp.edit().putString("colors", ColorBook.json(v)).apply()

    fun setReply(r: Reply, postedAps: Boolean = true, now: Long = System.currentTimeMillis()) {
        if (!ReplyJson.keeps(r, postedAps)) return
        sp.edit().also {
            if (r.probability == null) it.remove("probability") else it.putFloat("probability", r.probability)
            it.putString("rooms", ReplyJson.roomsJson(r.rooms)).putString("replyAps", ReplyJson.apsJson(r.aps))
            it.putString("colors", ColorBook.json(ColorBook.assign(colors, Rings.palette(Rings.order(r.rooms)), now)))
        }.apply()
    }

    fun setLastAps(aps: List<NamedAp>, now: Long = System.currentTimeMillis()) {
        val top = Footer.topAps(aps)
        sp.edit().also {
            it.putString("lastAps", ReplyJson.namedJson(top))
            if (rooms.isEmpty()) it.putString("colors", ColorBook.json(ColorBook.assign(colors, Footer.topAps(aps, Rings.LEGEND_MAX).map { a -> a.bssid }, now)))
        }.apply()
    }

    fun setDemo(now: Long = System.currentTimeMillis()) {
        sp.edit().also {
            it.putFloat("probability", Demo.P).putString("guess", Demo.GUESS)
            it.putString("rooms", ReplyJson.roomsJson(Demo.ROOMS)).putString("replyAps", ReplyJson.apsJson(Demo.APS)).putString("lastAps", ReplyJson.namedJson(Demo.NAMED))
            it.putString("colors", ColorBook.json(ColorBook.assign(colors, Rings.palette(Rings.order(Demo.ROOMS)), now)))
        }.apply()
        refreshTile()
    }

    private fun put(key: String, v: String?) {
        sp.edit().putString(key, v).apply()
        if (key == "guess" || key == "status" || key == "mode" || key == "lastPost") refreshTile()
    }

    fun refreshTile() {
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
    const val MANUAL_GAP_MS = 2_000L
    const val MANUAL_WAIT_MS = 10_000L
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
    const val STILL_GRACE_MS = 5_000L
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
    private var stillSince = Long.MIN_VALUE / 4
    private var lastScanAt = Long.MIN_VALUE / 4
    private val recent = ArrayDeque<Long>()
    private var misses = 0
    val away: Boolean get() = mode == Mode.AWAY || misses >= ScanConfig.AWAY_AFTER_MISSES

    fun onMotion(now: Long, at: Long): Boolean {
        if (mode == Mode.FIXED || mode == Mode.AWAY || now - at > ScanConfig.STALE_MS) return false
        if (mode == Mode.STILL && at < stillSince + ScanConfig.STILL_GRACE_MS) return false
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
                stillSince = now
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

    fun claimWake(now: Long) = mode != Mode.AWAY && claim(now)

    fun claimManual(now: Long) = claim(now, ScanConfig.MANUAL_GAP_MS)

    private fun claim(now: Long, gap: Long = ScanConfig.MIN_GAP_MS): Boolean {
        if (now - lastScanAt < gap) return false
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
