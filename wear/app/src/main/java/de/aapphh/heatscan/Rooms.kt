package de.aapphh.heatscan

import java.text.SimpleDateFormat
import java.util.Calendar
import java.util.Date
import java.util.Locale
import java.util.TimeZone
import org.json.JSONObject

object Plain {
    private val PAIRS = listOf(
        "Heizung: Thermostat" to "Thermostat", "heatd versucht automatisch, die Verbindung wiederherzustellen" to "Die Verbindung wird automatisch neu aufgebaut",
        "heatd schreibt sie erneut" to "Sie wird erneut gesendet", "uebernommen" to "übernommen", "faellt" to "fällt", "ueber" to "über", "spaeter" to "später", "laeuft" to "läuft"
    )
    private val NODE = Regex(" \\(Node \\d+\\)")

    fun text(s: String): String = PAIRS.fold(NODE.replace(s, "")) { acc, (from, to) -> acc.replace(from, to) }
}

enum class RoomMark(val res: Int?, val word: String) {
    WINDOW(R.drawable.ic_mark_window, "Fenster offen"), BOOST(R.drawable.ic_mark_boost, "Boost"), AWAY(R.drawable.ic_mark_away, "weg"),
    HEATING(R.drawable.ic_mark_heating, "heizt"), NONE(null, "")
}

object RoomView {
    const val LAG_K = 0.3

    val REASONS = mapOf(
        "home" to "Du bist zuhause – normale Temperatur", "occupied" to "Du bist hier – normale Temperatur", "away" to "Du bist weg – Sparmodus",
        "window_open" to "Fenster offen – Heizung pausiert", "sleep" to "Nachtruhe – Schlaftemperatur", "default" to "Niemand hier – Grundtemperatur",
        "schedule" to "Nach Zeitplan", "exploration" to "Probiert gerade eine andere Temperatur aus", "boost" to "Schnell aufheizen", "boost_off" to "Aufheizen beendet",
        "no_supply" to "Heizung liefert gerade keine Wärme", "pi_reset" to "Thermostat wird neu eingestellt",
        "manual_api" to "Von dir vorgegeben", "manual_watch" to "Von dir vorgegeben"
    )
    const val LEGEND = "Gefüllt: dort hast du dich gemeldet. Ring: dort vermutet dich die Uhr."
    const val NO_DATA = "keine Daten"
    const val CALM = "normal"
    const val APPLYING = "Wird übernommen …"

    fun glance(g: Glance, room: String): RoomGlance? = g.rooms.firstOrNull { it.room == room }

    fun mark(g: RoomGlance?): RoomMark = when {
        g == null -> RoomMark.NONE
        g.open || g.reason == "window_open" -> RoomMark.WINDOW
        g.boosting -> RoomMark.BOOST
        g.reason == "away" -> RoomMark.AWAY
        g.temp != null && g.target != null && g.target - g.temp >= LAG_K -> RoomMark.HEATING
        else -> RoomMark.NONE
    }

    fun temp(g: RoomGlance?): String? = g?.temp?.let { Glance.temp(it) + "°" }

    fun target(g: RoomGlance?): String? = g?.target?.let { "Ziel " + Glance.temp(it) + "°" }

    fun clock(ms: Long): String = SimpleDateFormat("HH:mm", Locale.GERMANY).format(Date(ms))

    fun why(g: RoomGlance?): String? = when {
        g == null -> null
        g.open -> REASONS.getValue("window_open")
        g.boosting -> g.boostUntil?.let { "Schnell aufheizen bis " + clock(it) } ?: "Schnell aufheizen"
        else -> REASONS[g.reason]
    }

    fun state(g: RoomGlance?): String = if (g == null) NO_DATA else mark(g).word.ifEmpty { CALM }

    fun row(g: RoomGlance?): String = listOfNotNull(temp(g), state(g)).joinToString(" · ")

    fun count(alerts: List<Alert>, ask: WindowAsk?): Int = alerts.size + if (ask != null) 1 else 0

    fun pill(alerts: List<Alert>, ask: WindowAsk?): String? = when {
        alerts.isEmpty() && ask != null -> "Fenster?"
        alerts.isEmpty() -> null
        else -> "${count(alerts, ask)} " + if (count(alerts, ask) == 1) "Hinweis" else "Hinweise"
    }

    fun badge(alerts: List<Alert>, ask: WindowAsk?): String? = when {
        alerts.isEmpty() && ask != null -> "?"
        alerts.isEmpty() -> null
        else -> count(alerts, ask).toString()
    }

    const val APPLY_MS = 2_000L

    fun applying(glanceAt: Long, tapAt: Long): Boolean = glanceAt <= tapAt + APPLY_MS

    fun tile(g: RoomGlance?): String = listOfNotNull(temp(g), mark(g).word.takeIf { it.isNotEmpty() }).joinToString(" · ")
}

enum class HoldChoice(val label: String, val minutes: Int?) {
    ONE_HOUR("1 Std.", 60), TWO_HOURS("2 Std.", 120), FOUR_HOURS("4 Std.", 240), MORNING("bis morgen 6 Uhr", null);

    fun next(): HoldChoice = entries[(ordinal + 1) % entries.size]
}

object SetpointControl {
    const val PATH = "/api/setpoint"
    const val STEP = 0.5
    const val LOW = 8.0
    const val HIGH = 26.0
    const val MIN_HOLD = 15
    const val MAX_HOLD = 720
    const val END = "Vorgabe beenden"
    const val DENIED = "Dieser Wert ist nicht erlaubt"
    const val MORNING_HOUR = 6

    fun clamp(v: Double): Double = (Math.round(v.coerceIn(LOW, HIGH) * 2.0) / 2.0)

    fun step(v: Double, by: Double): Double = clamp(v + by)

    fun start(g: RoomGlance?, now: Long = 0L): Double = clamp(active(g, now)?.target ?: g?.target ?: 20.0)

    fun celsius(v: Double): String = if (v % 1.0 == 0.0) "%.0f".format(Locale.GERMANY, v) else "%.1f".format(Locale.GERMANY, v)

    fun minutes(choice: HoldChoice, now: Long, zone: TimeZone = TimeZone.getDefault()): Int = choice.minutes ?: morning(now, zone)

    fun morning(now: Long, zone: TimeZone = TimeZone.getDefault()): Int {
        val c = Calendar.getInstance(zone).apply { timeInMillis = now; set(Calendar.HOUR_OF_DAY, MORNING_HOUR); set(Calendar.MINUTE, 0); set(Calendar.SECOND, 0); set(Calendar.MILLISECOND, 0) }
        if (c.timeInMillis <= now) c.add(Calendar.DAY_OF_YEAR, 1)
        return ((c.timeInMillis - now + 59_999) / 60_000).toInt().coerceIn(MIN_HOLD, MAX_HOLD)
    }

    fun body(room: String, celsius: Double, minutes: Int): String = JSONObject().put("room", room).put("celsius", celsius).put("hold_min", minutes).put("source", "watch").toString()

    fun clearBody(room: String): String = JSONObject().put("room", room).put("clear", true).put("source", "watch").toString()

    fun line(m: Manual): String = celsius(m.target) + " °C bis " + RoomView.clock(m.until)

    fun held(m: Manual, open: Boolean): String = if (open) "Fenster offen – Vorgabe " + celsius(m.target) + " °C gilt, sobald es zu ist" else line(m)

    fun active(g: RoomGlance?, now: Long): Manual? = g?.manual?.takeIf { it.until > now }

    fun result(code: Int, text: String): BoostResult = try {
        val o = JSONObject(text)
        if (code in 200..299 && o.optBoolean("ok")) BoostResult(true, o.optString("note").ifEmpty { line(Manual(o.getDouble("target"), o.getLong("until_ts"), o.optString("source"))) }) else BoostResult(false, if (code == 400) DENIED else "Gerade nicht möglich")
    } catch (e: Exception) { BoostResult(false, if (code <= 0) "Server nicht erreichbar" else "Antwort unlesbar ($code)") }

    fun cleared(code: Int, text: String): BoostResult = try {
        BoostResult(code in 200..299 && JSONObject(text).optBoolean("ok"), if (code in 200..299) "Vorgabe beendet" else "Gerade nicht möglich")
    } catch (e: Exception) { BoostResult(false, if (code <= 0) "Server nicht erreichbar" else "Antwort unlesbar ($code)") }
}

object BoostText {
    private val RULES = listOf(
        "Fenster" to "Gerade kein Boost: Fenster war offen", "morgens" to "Boost nur morgens oder nach dem Heimkommen", "weg" to "Boost erst nach längerer Abwesenheit",
        "am Ziel" to "Der Raum ist schon warm genug", "Heizungsversorgung" to "Die Heizung hat gerade keine Wärme", "bereits ein Boost" to "Der Boost läuft schon",
        "Budget" to "Gerade zu oft aufgeheizt, bitte später", "Boost-Modell" to "Boost ist gerade nicht möglich",
        "frischer" to "Gerade keine aktuellen Messwerte", "Regelschritt" to "Gerade keine aktuellen Messwerte", "Ventilwert" to "Gerade keine aktuellen Messwerte"
    )
    const val FALLBACK = "Gerade kein Boost möglich"

    fun plain(reason: String): String = RULES.firstOrNull { reason.contains(it.first) }?.second ?: FALLBACK
}
