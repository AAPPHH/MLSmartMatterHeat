package de.aapphh.heatscan

import androidx.concurrent.futures.ResolvableFuture
import androidx.wear.protolayout.ActionBuilders
import androidx.wear.protolayout.ColorBuilders
import androidx.wear.protolayout.DimensionBuilders
import androidx.wear.protolayout.LayoutElementBuilders
import androidx.wear.protolayout.ModifiersBuilders
import androidx.wear.protolayout.ResourceBuilders
import androidx.wear.protolayout.TimelineBuilders
import androidx.wear.tiles.RequestBuilders
import androidx.wear.tiles.TileBuilders
import androidx.wear.tiles.TileService
import com.google.common.util.concurrent.ListenableFuture

class HeatTileService : TileService() {
    override fun onTileRequest(requestParams: RequestBuilders.TileRequest): ListenableFuture<TileBuilders.Tile> {
        val store = Store(this)
        val clicked = TileClick.room(requestParams.currentState.lastClickableId, store.nonce)
        if (clicked != null) {
            store.nonce = store.nonce + 1
            store.setLabelQuietly(Labels.toggle(store.label, clicked))
        }
        val label = store.label
        val nonce = store.nonce
        val layout = LayoutElementBuilders.Column.Builder()
            .setHorizontalAlignment(LayoutElementBuilders.HORIZONTAL_ALIGN_CENTER)
            .addContent(row(Defaults.ROOMS.take(2), label, nonce))
            .addContent(spacer(6f))
            .addContent(row(Defaults.ROOMS.drop(2), label, nonce))
            .addContent(spacer(6f))
            .addContent(
                LayoutElementBuilders.Text.Builder()
                    .setText(TileClick.statusLine(store.running, store.mode, store.status))
                    .setMaxLines(2)
                    .setFontStyle(LayoutElementBuilders.FontStyle.Builder().setSize(DimensionBuilders.sp(10f)).build())
                    .build()
            )
            .build()
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

    private fun row(rooms: List<String>, label: String?, nonce: Long): LayoutElementBuilders.Row {
        val row = LayoutElementBuilders.Row.Builder().setVerticalAlignment(LayoutElementBuilders.VERTICAL_ALIGN_CENTER)
        rooms.forEachIndexed { i, room ->
            if (i > 0) row.addContent(spacer(6f, true))
            row.addContent(button(room, room == label, nonce))
        }
        return row.build()
    }

    private fun button(room: String, selected: Boolean, nonce: Long): LayoutElementBuilders.LayoutElement {
        val click = ModifiersBuilders.Clickable.Builder()
            .setId(TileClick.id(room, nonce))
            .setOnClick(ActionBuilders.LoadAction.Builder().build())
            .build()
        val mods = ModifiersBuilders.Modifiers.Builder()
            .setClickable(click)
            .setBackground(
                ModifiersBuilders.Background.Builder()
                    .setColor(ColorBuilders.argb(if (selected) SELECTED else IDLE))
                    .setCorner(ModifiersBuilders.Corner.Builder().setRadius(DimensionBuilders.dp(BUTTON / 2)).build())
                    .build()
            )
            .build()
        return LayoutElementBuilders.Box.Builder()
            .setWidth(DimensionBuilders.dp(BUTTON))
            .setHeight(DimensionBuilders.dp(BUTTON))
            .setHorizontalAlignment(LayoutElementBuilders.HORIZONTAL_ALIGN_CENTER)
            .setVerticalAlignment(LayoutElementBuilders.VERTICAL_ALIGN_CENTER)
            .setModifiers(mods)
            .addContent(
                LayoutElementBuilders.Image.Builder()
                    .setResourceId(room)
                    .setWidth(DimensionBuilders.dp(24f))
                    .setHeight(DimensionBuilders.dp(24f))
                    .build()
            )
            .build()
    }

    private fun spacer(size: Float, horizontal: Boolean = false) = LayoutElementBuilders.Spacer.Builder()
        .also { if (horizontal) it.setWidth(DimensionBuilders.dp(size)) else it.setHeight(DimensionBuilders.dp(size)) }
        .build()

    companion object {
        private const val RES_VERSION = "2"
        private const val BUTTON = 44f
        private const val SELECTED = 0xFFE8710A.toInt()
        private const val IDLE = 0xFF2A2A2A.toInt()
    }
}
