package de.aapphh.heatscan

import androidx.concurrent.futures.ResolvableFuture
import androidx.wear.protolayout.ActionBuilders
import androidx.wear.protolayout.ColorBuilders
import androidx.wear.protolayout.DimensionBuilders
import androidx.wear.protolayout.LayoutElementBuilders
import androidx.wear.protolayout.ModifiersBuilders
import androidx.wear.protolayout.ResourceBuilders
import androidx.wear.protolayout.TimelineBuilders
import androidx.wear.tiles.EventBuilders
import androidx.wear.tiles.RequestBuilders
import androidx.wear.tiles.TileBuilders
import androidx.wear.tiles.TileService
import com.google.common.util.concurrent.ListenableFuture
import android.graphics.Color
import kotlin.math.cos
import kotlin.math.sin

class HeatTileService : TileService() {
    override fun onTileEnterEvent(requestParams: EventBuilders.TileEnterEvent) {
        ScanStarter.start(this, "tile_enter")
    }

    override fun onTileRequest(requestParams: RequestBuilders.TileRequest): ListenableFuture<TileBuilders.Tile> {
        val store = Store(this)
        val clicked = TileClick.room(requestParams.currentState.lastClickableId, store.nonce)
        if (clicked != null) {
            store.nonce = store.nonce + 1
            LabelRecorder.record(this, clicked, false)
        }
        val (hot, ring) = Highlight.of(store.recentLabel(), store.guess)
        val dp = requestParams.deviceConfiguration.let { minOf(it.screenWidthDp, it.screenHeightDp).toFloat() }.takeIf { it > 0f } ?: Quarters.REF
        val layout = layout(dp, hot, ring, store.nonce)
        val tile = TileBuilders.Tile.Builder()
            .setResourcesVersion(RES_VERSION)
            .setFreshnessIntervalMillis(60_000)
            .setTileTimeline(TimelineBuilders.Timeline.fromLayoutElement(layout))
            .build()
        return ResolvableFuture.create<TileBuilders.Tile>().also { it.set(tile) }
    }

    override fun onTileResourcesRequest(requestParams: RequestBuilders.ResourcesRequest): ListenableFuture<ResourceBuilders.Resources> =
        ResolvableFuture.create<ResourceBuilders.Resources>().also {
            val res = ResourceBuilders.Resources.Builder().setVersion(RES_VERSION)
            RoomIcons.RES.forEach { (room, id) ->
                res.addIdToImageMapping(
                    room,
                    ResourceBuilders.ImageResource.Builder()
                        .setAndroidResourceByResId(ResourceBuilders.AndroidImageResourceByResId.Builder().setResourceId(id).build())
                        .build()
                )
            }
            it.set(res.build())
        }

    private fun layout(d: Float, hot: String?, ring: String?, nonce: Long): LayoutElementBuilders.LayoutElement {
        val inner = Quarters.INNER * d
        val box = LayoutElementBuilders.Box.Builder().setWidth(dp(d)).setHeight(dp(d))
        Quarters.WEDGES.forEach { box.addContent(wedge(it, d, inner, it == hot, it == ring, nonce)) }
        box.addContent(centre(Quarters.CENTER_R * d * 2f, Quarters.CENTER == hot, Quarters.CENTER == ring, nonce))
        (Quarters.WEDGES + Quarters.CENTER).forEach { room ->
            val a = Math.toRadians(Quarters.mid(room) - 90.0)
            val r = if (room == Quarters.CENTER) 0f else Quarters.LABEL_R * d
            box.addContent(label(room, d, (r * cos(a)).toFloat(), (r * sin(a)).toFloat(), room == hot))
        }
        return box.build()
    }

    private fun click(room: String, nonce: Long) = ModifiersBuilders.Clickable.Builder()
        .setId(TileClick.id(room, nonce))
        .setOnClick(ActionBuilders.LoadAction.Builder().build())
        .build()

    private fun arc(room: String, size: Float, thickness: Float, length: Float, color: Int, nonce: Long): LayoutElementBuilders.LayoutElement =
        LayoutElementBuilders.Box.Builder().setWidth(dp(size)).setHeight(dp(size))
            .addContent(
                LayoutElementBuilders.Arc.Builder()
                    .setAnchorAngle(DimensionBuilders.degrees(Quarters.mid(room)))
                    .setAnchorType(LayoutElementBuilders.ARC_ANCHOR_CENTER)
                    .addContent(
                        LayoutElementBuilders.ArcLine.Builder()
                            .setLength(DimensionBuilders.degrees(length))
                            .setThickness(dp(thickness))
                            .setStrokeCap(LayoutElementBuilders.STROKE_CAP_BUTT)
                            .setColor(ColorBuilders.argb(color))
                            .setModifiers(ModifiersBuilders.ArcModifiers.Builder().setClickable(click(room, nonce)).build())
                            .build()
                    )
                    .build()
            )
            .build()

    private fun wedge(room: String, d: Float, inner: Float, selected: Boolean, ringed: Boolean, nonce: Long): LayoutElementBuilders.LayoutElement {
        val midR = (d / 2f + inner) / 2f
        val length = Quarters.SPAN - Math.toDegrees((Quarters.GAP / midR).toDouble()).toFloat()
        val fill = if (selected) SELECTED else IDLE
        val box = LayoutElementBuilders.Box.Builder().setWidth(dp(d)).setHeight(dp(d))
        if (ringed) {
            val ring = 3f
            val rd = d - 2 * ring
            val ringDeg = Math.toDegrees((ring / midR).toDouble()).toFloat()
            box.addContent(arc(room, d, d / 2f - inner, length, Color.WHITE, nonce))
            box.addContent(arc(room, rd, rd / 2f - inner - ring, length - 2 * ringDeg, fill, nonce))
        } else box.addContent(arc(room, d, d / 2f - inner, length, fill, nonce))
        return box.build()
    }

    private fun centre(size: Float, selected: Boolean, ringed: Boolean, nonce: Long): LayoutElementBuilders.LayoutElement {
        val mods = ModifiersBuilders.Modifiers.Builder()
            .setClickable(click(Quarters.CENTER, nonce))
            .setBackground(
                ModifiersBuilders.Background.Builder()
                    .setColor(ColorBuilders.argb(if (selected) SELECTED else IDLE))
                    .setCorner(ModifiersBuilders.Corner.Builder().setRadius(dp(size / 2f)).build())
                    .build()
            )
        if (ringed) mods.setBorder(ModifiersBuilders.Border.Builder().setWidth(dp(3f)).setColor(ColorBuilders.argb(Color.WHITE)).build())
        return LayoutElementBuilders.Box.Builder().setWidth(dp(size)).setHeight(dp(size)).setModifiers(mods.build()).build()
    }

    private fun label(room: String, d: Float, dx: Float, dy: Float, selected: Boolean): LayoutElementBuilders.LayoutElement {
        val centre = room == Quarters.CENTER
        val ink = if (selected) INK else Color.WHITE
        val icon = (if (centre) 28f else 24f)
        val text = LayoutElementBuilders.Text.Builder()
            .setText(Defaults.CAPTIONS.getValue(room))
            .setMaxLines(1)
            .setFontStyle(
                LayoutElementBuilders.FontStyle.Builder()
                    .setSize(DimensionBuilders.sp(if (centre) 11f else 10f))
                    .setColor(ColorBuilders.argb(ink))
                    .build()
            )
            .build()
        val image = LayoutElementBuilders.Image.Builder()
            .setResourceId(room)
            .setWidth(dp(icon))
            .setHeight(dp(icon))
            .setColorFilter(LayoutElementBuilders.ColorFilter.Builder().setTint(ColorBuilders.argb(ink)).build())
            .build()
        val pad = ModifiersBuilders.Padding.Builder()
            .setRtlAware(false)
            .setStart(dp(maxOf(0f, 2 * dx))).setEnd(dp(maxOf(0f, -2 * dx)))
            .setTop(dp(maxOf(0f, 2 * dy))).setBottom(dp(maxOf(0f, -2 * dy)))
            .build()
        return LayoutElementBuilders.Box.Builder()
            .setWidth(dp(d)).setHeight(dp(d))
            .setHorizontalAlignment(LayoutElementBuilders.HORIZONTAL_ALIGN_CENTER)
            .setVerticalAlignment(LayoutElementBuilders.VERTICAL_ALIGN_CENTER)
            .setModifiers(ModifiersBuilders.Modifiers.Builder().setPadding(pad).build())
            .addContent(
                LayoutElementBuilders.Column.Builder()
                    .setHorizontalAlignment(LayoutElementBuilders.HORIZONTAL_ALIGN_CENTER)
                    .addContent(image)
                    .addContent(text)
                    .build()
            )
            .build()
    }

    private fun dp(v: Float) = DimensionBuilders.dp(v)

    companion object {
        private const val RES_VERSION = "4"
        private const val SELECTED = 0xFFE8710A.toInt()
        private const val IDLE = 0xFF2A2A2A.toInt()
        private const val INK = 0xFF111111.toInt()
    }
}
