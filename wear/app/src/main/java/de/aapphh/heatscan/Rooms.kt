package de.aapphh.heatscan

import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

object Plain {
    private val PAIRS = listOf(
        "Heizung: Thermostat" to "Thermostat", "heatd versucht automatisch, die Verbindung wiederherzustellen" to "Die Verbindung wird automatisch neu aufgebaut",
        "heatd schreibt sie erneut" to "Sie wird erneut gesendet", "uebernommen" to "übernommen", "faellt" to "fällt", "ueber" to "über", "spaeter" to "später", "laeuft" to "läuft"
    )
    private val NODE = Regex(" \\(Node \\d+\\)")

    fun text(s: String): String = PAIRS.fold(NODE.replace(s, "")) { acc, (from, to) -> acc.replace(from, to) }
}

enum class RoomMark(val res: Int?, val word: String) {
    WINDOW(R.drawable.ic_mark_window, "Fenster offen"), BOOST(R.drawable.ic_mark_boost, "Boost"), AWAY(R.drawable.ic_mark_away, "Sparmodus"),
    HEATING(R.drawable.ic_mark_heating, "heizt"), NONE(null, "")
}

object RoomView {
    const val LAG_K = 0.3

    private val REASONS = mapOf(
        "home" to "Du bist zuhause", "away" to "Du bist weg – Sparmodus", "window_open" to "Fenster offen – Heizung pausiert",
        "sleep" to "Nachtmodus", "default" to "Standard", "boost_off" to "Boost beendet"
    )

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

    fun tile(g: RoomGlance?): String = listOfNotNull(temp(g), mark(g).word.takeIf { it.isNotEmpty() }).joinToString(" · ")

    fun badge(alerts: List<Alert>, ask: WindowAsk?): Boolean = alerts.isNotEmpty() || ask != null
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
