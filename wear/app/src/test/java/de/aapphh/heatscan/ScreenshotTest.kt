package de.aapphh.heatscan

import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Density
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

    private fun shot(name: String, fontScale: Float = 1f, swipe: Boolean = false, content: @Composable () -> Unit) {
        rule.setContent { val d = LocalDensity.current; CompositionLocalProvider(LocalDensity provides Density(d.density, fontScale)) { HeatTheme { content() } } }
        if (swipe) rule.onRoot().performTouchInput { swipeUp() }
        rule.onRoot().captureRoboImage("screenshots/$name.png")
    }

    private fun footer() = Footer.line("12:34", true, 0)

    @Composable private fun demo() = DataPage(Demo.ROOMS, ColorBook.assign(emptyMap(), Rings.palette(Rings.order(Demo.ROOMS)), 0L), Demo.NAMED, Demo.APS, Demo.GUESS, Demo.P, footer())

    private val ask = WindowAsk("w1", "kueche", true, 0L, "Temperatur fällt schnell")

    private val alert = Alert("a@1", "Thermostat", "Bad antwortet nicht.")

    private val until = 1_700_000_000_000L

    private val glance = Glance(
        1L, listOf(
            RoomGlance("wohnzimmer", 21.5, 22.0, false, "home"), RoomGlance("schlafzimmer", 18.2, 19.0, true, "window_open"),
            RoomGlance("bad", 22.0, 22.0, false, "boost", true, until), RoomGlance("kueche", 20.4, 21.0, false, "away"), RoomGlance("flur", 19.9, 20.0, false, "home")
        )
    )

    private val heating = glance.copy(rooms = glance.rooms.map { if (it.room == "kueche") it.copy(temp = 19.0, target = 21.0, reason = "home") else it })

    @Composable private fun detail(room: String, g: RoomGlance?, result: BoostResult? = null, busy: Boolean = false) = RoomDetailScreen(room, g, result, busy, true, {}, {})

    private fun rg(room: String) = RoomView.glance(glance, room)

    private val tech = TechState("Lernen", false, "WLAN 10 min", false, footer())

    @Test fun rooms_none() = shot("rooms_none") { RoomsPage(null, null) {} }

    @Test fun rooms_guess_kueche() = shot("rooms_guess_kueche") { RoomsPage("kueche", null) {} }

    @Test fun rooms_label_bad_guess_kueche() = shot("rooms_label_bad_guess_kueche") { RoomsPage("kueche", "bad") {} }

    @Test fun rooms_label_keiner() = shot("rooms_label_keiner") { RoomsPage("keiner", null) {} }

    @Test fun rooms_label_keiner_guess_flur() = shot("rooms_label_keiner_guess_flur") { RoomsPage("flur", "keiner") {} }

    @Test fun wheel_normal() = shot("wheel_normal") { RoomsPage(null, null, glance = heating) {} }

    @Test fun wheel_badge() = shot("wheel_badge") { RoomsPage(null, null, glance = glance, badge = true) {} }

    @Test fun wheel_guess() = shot("wheel_guess") { RoomsPage("kueche", null, glance = heating) {} }

    @Test fun wheel_font_1_3() = shot("wheel_font_1_3", 1.3f) { RoomsPage("kueche", null, glance = heating, badge = true) {} }

    @Test @Config(qualifiers = "w192dp-h192dp-round-xhdpi") fun wheel_small() = shot("wheel_small") { RoomsPage("wohnzimmer", null, glance = glance, badge = true) {} }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun wheel_45mm() = shot("wheel_45mm") { RoomsPage("kueche", "bad", glance = glance, badge = true) {} }

    @Test fun detail_home() = shot("detail_home") { detail("wohnzimmer", rg("wohnzimmer")) }

    @Test fun detail_window() = shot("detail_window") { detail("schlafzimmer", rg("schlafzimmer")) }

    @Test fun detail_boost() = shot("detail_boost") { detail("bad", rg("bad"), BoostResult(true, "Boost läuft: 5 min")) }

    @Test fun detail_away() = shot("detail_away") { detail("kueche", rg("kueche"), BoostResult(false, BoostText.plain("kein Boost: Fenster war vor 5 min offen"))) }

    @Test fun detail_edge() = shot("detail_edge", swipe = true) { detail("bad", rg("bad")) }

    @Test fun detail_edge_font_1_3() = shot("detail_edge_font_1_3", 1.3f, true) { detail("wohnzimmer", rg("wohnzimmer")) }

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

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun rooms_45mm() = shot("rooms_45mm") { RoomsPage("kueche", "bad") {} }
}
