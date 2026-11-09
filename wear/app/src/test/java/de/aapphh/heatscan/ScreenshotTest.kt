package de.aapphh.heatscan

import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Density
import androidx.wear.compose.material3.AppScaffold
import androidx.wear.compose.material3.ScreenScaffold
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeUp
import com.github.takahirom.roborazzi.captureRoboImage
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [35], qualifiers = "w192dp-h192dp-round-xhdpi")
class ScreenshotTest {
    @get:Rule val rule = createComposeRule()

    private fun shot(name: String, fontScale: Float = 1f, swipe: Int = 0, short: Boolean = false, content: @Composable () -> Unit) {
        rule.setContent { val d = LocalDensity.current; CompositionLocalProvider(LocalDensity provides Density(d.density, fontScale)) { HeatTheme { AppScaffold { content() } } } }
        repeat(swipe) { rule.onRoot().performTouchInput { if (short) swipeUp(startY = height * 0.8f, endY = height * 0.5f) else swipeUp() } }
        rule.onRoot().captureRoboImage("screenshots/$name.png")
    }

    @Composable private fun page(content: @Composable () -> Unit) = ScreenScaffold(timeText = {}) { content() }

    private fun footer() = Footer.line("12:34", true, 0)

    @Composable private fun demo() = DataPage(Demo.ROOMS, ColorBook.assign(emptyMap(), Rings.palette(Rings.order(Demo.ROOMS)), 0L), Demo.NAMED, Demo.APS, Demo.GUESS, Demo.P, footer())

    private val ask = WindowAsk("w1", "kueche", true, 0L, "Temperatur fällt schnell")

    private val alert = Alert("a@1", "Thermostat", "Bad antwortet nicht.")

    private val until = 1_700_000_000_000L

    private val glance = Glance(
        1L, listOf(
            RoomGlance("wohnzimmer", 21.5, 22.0, false, "occupied"), RoomGlance("schlafzimmer", 18.2, 19.0, true, "window_open"),
            RoomGlance("bad", 22.0, 22.0, false, "boost", true, until), RoomGlance("kueche", 20.4, 21.0, false, "away"), RoomGlance("flur", 19.9, 20.0, false, "home")
        )
    )

    private val heating = glance.copy(rooms = glance.rooms.map { if (it.room == "kueche") it.copy(temp = 19.0, target = 21.0, reason = "home") else it })

    @Composable private fun detail(room: String, g: RoomGlance?, result: BoostResult? = null, busy: Boolean = false) = RoomDetailScreen(room, g, result, busy, true, {}, {})

    private fun rg(room: String) = RoomView.glance(glance, room)

    private val tech = TechState("Lernen", false, "WLAN 10 min", false, footer())

    @Test fun rooms_none() = shot("rooms_none") { page { RoomsPage(null, null) {} } }

    @Test fun rooms_guess_kueche() = shot("rooms_guess_kueche") { page { RoomsPage("kueche", null) {} } }

    @Test fun rooms_label_bad_guess_kueche() = shot("rooms_label_bad_guess_kueche") { page { RoomsPage("kueche", "bad") {} } }

    @Test fun rooms_label_keiner() = shot("rooms_label_keiner") { page { RoomsPage("keiner", null) {} } }

    @Test fun rooms_label_keiner_guess_flur() = shot("rooms_label_keiner_guess_flur") { page { RoomsPage("flur", "keiner") {} } }

    @Test fun wheel_normal() = shot("wheel_normal") { page { RoomsPage(null, null, glance = heating) {} } }

    @Test fun wheel_badge() = shot("wheel_badge") { page { RoomsPage(null, null, glance = glance, badge = "2") {} } }

    @Test fun wheel_guess() = shot("wheel_guess") { page { RoomsPage("kueche", null, glance = heating) {} } }

    @Test fun wheel_font_1_3() = shot("wheel_font_1_3", 1.3f) { page { RoomsPage("kueche", null, glance = heating, badge = "2") {} } }

    @Test @Config(qualifiers = "w192dp-h192dp-round-xhdpi") fun wheel_small() = shot("wheel_small") { page { RoomsPage("wohnzimmer", null, glance = glance, badge = "2") {} } }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun wheel_45mm() = shot("wheel_45mm") { page { RoomsPage("kueche", "bad", glance = glance, badge = "2") {} } }

    private val two = listOf(alert, Alert("a@2", "Heizung", "Bad liefert keine Wärme."))

    @Composable private fun rooms(alerts: List<Alert> = two, ask: WindowAsk? = null) = RoomsScreen("kueche", "bad", false, null, glance, alerts, ask, true, {}, {}, {}) {}

    @Test fun rooms_screen() = shot("rooms_screen") { rooms() }

    @Test fun rooms_list() = shot("rooms_list", swipe = 1) { rooms() }

    @Test fun rooms_list_font_1_3() = shot("rooms_list_font_1_3", 1.3f, 1) { rooms() }

    @Test fun rooms_list_header_font_1_3() = shot("rooms_list_header_font_1_3", 1.3f, 1, true) { rooms() }

    @Test fun rooms_list_rows_font_1_3() = shot("rooms_list_rows_font_1_3", 1.3f, 4) { rooms() }

    @Test fun tech_footer_font_1_3() = shot("tech_footer_font_1_3", 1.3f, 2) { TechScreen(TechState("Stopp", true, "WLAN 8 min", true, footer()), {}, {}, {}) }

    @Test @Config(qualifiers = "w192dp-h192dp-round-xhdpi") fun wheel_rooms_192() = shot("wheel_rooms_192") { rooms() }

    @Test @Config(qualifiers = "w204dp-h204dp-round-xhdpi") fun wheel_41mm() = shot("wheel_41mm") { rooms() }

    @Test @Config(qualifiers = "w204dp-h204dp-round-xhdpi") fun wheel_41mm_ask() = shot("wheel_41mm_ask") { rooms(emptyList(), ask) }

    @Test @Config(qualifiers = "w204dp-h204dp-round-xhdpi") fun detail_41mm() = shot("detail_41mm") { detail("wohnzimmer", rg("wohnzimmer")) }

    @Test @Config(qualifiers = "w204dp-h204dp-round-xhdpi") fun detail_saved_41mm() = shot("detail_saved_41mm") { RoomDetailScreen("kueche", rg("kueche"), null, false, true, {}, {}, true) {} }

    private fun control(active: Manual? = null, line: BoostResult? = null) = SetpointUi(22.5, HoldChoice.TWO_HOURS, false, line, active, {}, {}, {}, {})

    @Test fun detail_control() = shot("detail_control", swipe = 1, short = true) { RoomDetailScreen("wohnzimmer", rg("wohnzimmer"), null, false, true, {}, {}, control = control()) }

    @Test @Config(qualifiers = "w204dp-h204dp-round-xhdpi") fun detail_control_active_41mm() = shot("detail_control_active_41mm", swipe = 1, short = true) { RoomDetailScreen("wohnzimmer", rg("wohnzimmer"), null, false, true, {}, {}, control = control(Manual(22.5, until, "watch"), BoostResult(true, "22,5 °C bis 18:40"))) }

    @Test fun detail_saved() = shot("detail_saved") { RoomDetailScreen("kueche", rg("kueche"), null, false, true, {}, {}, true) {} }

    @Test fun detail_saved_font_1_3() = shot("detail_saved_font_1_3", 1.3f) { RoomDetailScreen("kueche", rg("kueche"), null, false, true, {}, {}, true) {} }

    @Test fun tech_font_1_3() = shot("tech_font_1_3", 1.3f) { TechScreen(TechState("Stopp", true, "WLAN 8 min", true, footer()), {}, {}, {}) }

    @Test fun detail_home() = shot("detail_home") { detail("wohnzimmer", rg("wohnzimmer")) }

    @Test fun detail_window() = shot("detail_window") { detail("schlafzimmer", rg("schlafzimmer")) }

    @Test fun detail_boost() = shot("detail_boost") { detail("bad", rg("bad"), BoostResult(true, "Boost läuft: 5 min")) }

    @Test fun detail_away() = shot("detail_away") { detail("kueche", rg("kueche"), BoostResult(false, BoostText.plain("kein Boost: Fenster war vor 5 min offen"))) }

    @Test fun detail_edge() = shot("detail_edge", swipe = 1) { detail("bad", rg("bad")) }

    @Test fun detail_edge_font_1_3() = shot("detail_edge_font_1_3", 1.3f, 1) { detail("wohnzimmer", rg("wohnzimmer")) }

    @Test fun detail_font_1_3() = shot("detail_font_1_3", 1.3f) { detail("bad", rg("bad")) }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun detail_45mm() = shot("detail_45mm") { detail("kueche", rg("kueche")) }

    @Test fun boost_confirm() = shot("boost_confirm") { BoostConfirmScreen("kueche", {}) {} }

    @Test fun alerts_page() = shot("alerts_page") { AlertsScreen(listOf(alert), ask) {} }

    @Test fun alerts_empty() = shot("alerts_empty") { AlertsScreen(emptyList(), null) {} }

    @Test fun alerts_font_1_3() = shot("alerts_font_1_3", 1.3f) { AlertsScreen(listOf(alert), ask) {} }

    @Test fun tech_small() = shot("tech_small") { TechScreen(tech, {}, {}, {}) }

    @Test fun data_demo() = shot("data_demo") { demo() }

    @Test fun data_no_model() = shot("data_no_model") {
        val named = Demo.NAMED.take(4)
        DataPage(emptyList(), ColorBook.assign(emptyMap(), named.map { it.bssid }, 0L), named, emptyList(), null, null, footer())
    }

    @Test fun ask_small() = shot("ask_small") { AskScreen(ask) {} }

    @Test fun ask_closed_small() = shot("ask_closed_small") { AskScreen(ask.copy(open = false)) {} }

    @Test fun saved_small() = shot("saved_small") { SavedPage("Fenster Küche auf", {}, {}) }

    @Test fun ask_font_1_3() = shot("ask_font_1_3", 1.3f) { AskScreen(ask) {} }

    @Test @Config(qualifiers = "w213dp-h213dp-round-xhdpi") fun data_demo_213() = shot("data_demo_213") { demo() }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun data_demo_45mm() = shot("data_demo_45mm") { demo() }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun ask_45mm() = shot("ask_45mm") { AskScreen(ask) {} }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun rooms_45mm() = shot("rooms_45mm") { page { RoomsPage("kueche", "bad") {} } }
}
