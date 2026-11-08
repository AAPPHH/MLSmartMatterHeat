package de.aapphh.heatscan

import android.content.ComponentName
import androidx.concurrent.futures.ResolvableFuture
import androidx.wear.protolayout.ActionBuilders
import androidx.wear.protolayout.LayoutElementBuilders
import androidx.wear.protolayout.ModifiersBuilders
import androidx.wear.protolayout.ResourceBuilders
import androidx.wear.protolayout.TimelineBuilders
import androidx.wear.protolayout.material3.MaterialScope
import androidx.wear.protolayout.material3.Typography
import androidx.wear.protolayout.material3.card
import androidx.wear.protolayout.material3.materialScope
import androidx.wear.protolayout.material3.primaryLayout
import androidx.wear.protolayout.material3.text
import androidx.wear.protolayout.types.LayoutColor
import androidx.wear.protolayout.types.layoutString
import androidx.wear.tiles.EventBuilders
import androidx.wear.tiles.RequestBuilders
import androidx.wear.tiles.TileBuilders
import androidx.wear.tiles.TileService
import com.google.common.util.concurrent.ListenableFuture
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

object GlanceView {
    const val NONE = "Noch keine Daten"
    const val MAX_ROOMS = 5

    class Line(val text: String, val tone: Tone)

    enum class Tone { NORMAL, WARN, DIM }

    fun lines(g: Glance, alerts: List<Alert>, now: Long, here: String? = null): List<Line> {
        val out = mutableListOf<Line>()
        if (g.rooms.isEmpty() || Span.ms(now, g.at) > Glance.STALE_MS) out += Line(NONE, Tone.DIM)
        else g.rooms.sortedByDescending { it.room == here }.take(MAX_ROOMS).forEach {
            out += Line(listOf(WindowEvent.caption(it.room), RoomView.tile(it)).filter { t -> t.isNotEmpty() }.joinToString(" "), if (RoomView.mark(it) == RoomMark.WINDOW) Tone.WARN else Tone.NORMAL)
        }
        alerts.firstOrNull()?.let { out += Line(it.title.ifEmpty { it.text }, Tone.WARN) }
        if (g.at > 0L) out += Line("Stand " + SimpleDateFormat("HH:mm", Locale.GERMANY).format(Date(g.at)), Tone.DIM)
        return out
    }
}

class HeatTileService : TileService() {
    override fun onTileEnterEvent(requestParams: EventBuilders.TileEnterEvent) {
        ScanStarter.start(this, "tile_enter")
        Remote.refresh(this)
    }

    override fun onTileRequest(requestParams: RequestBuilders.TileRequest): ListenableFuture<TileBuilders.Tile> {
        val store = Store(this)
        val layout = materialScope(this, requestParams.deviceConfiguration) {
            val lines = GlanceView.lines(store.glance, store.alerts, System.currentTimeMillis(), (store.stayLabel() ?: store.guess)?.takeIf { it in Defaults.ROOMS })
            primaryLayout(mainSlot = { card(onClick = open(), content = { column(lines) }) }, onClick = open())
        }
        val tile = TileBuilders.Tile.Builder()
            .setResourcesVersion(RES_VERSION)
            .setTileTimeline(TimelineBuilders.Timeline.fromLayoutElement(layout))
            .build()
        return ResolvableFuture.create<TileBuilders.Tile>().also { it.set(tile) }
    }

    override fun onTileResourcesRequest(requestParams: RequestBuilders.ResourcesRequest): ListenableFuture<ResourceBuilders.Resources> =
        ResolvableFuture.create<ResourceBuilders.Resources>().also { it.set(ResourceBuilders.Resources.Builder().setVersion(RES_VERSION).build()) }

    private fun open(): ModifiersBuilders.Clickable = ModifiersBuilders.Clickable.Builder()
        .setId("open")
        .setOnClick(ActionBuilders.launchAction(ComponentName(packageName, MainActivity::class.java.name)))
        .build()

    private fun MaterialScope.column(lines: List<GlanceView.Line>): LayoutElementBuilders.LayoutElement {
        val col = LayoutElementBuilders.Column.Builder().setHorizontalAlignment(LayoutElementBuilders.HORIZONTAL_ALIGN_START)
        lines.forEach { l ->
            col.addContent(text(l.text.layoutString, typography = if (l.tone == GlanceView.Tone.DIM) Typography.BODY_EXTRA_SMALL else Typography.BODY_SMALL, color = tone(l.tone), maxLines = 1))
        }
        return col.build()
    }

    private fun MaterialScope.tone(t: GlanceView.Tone): LayoutColor = when (t) {
        GlanceView.Tone.NORMAL -> colorScheme.onSurface
        GlanceView.Tone.WARN -> colorScheme.tertiary
        GlanceView.Tone.DIM -> colorScheme.onSurfaceVariant
    }

    companion object {
        private const val RES_VERSION = "7"
    }
}
