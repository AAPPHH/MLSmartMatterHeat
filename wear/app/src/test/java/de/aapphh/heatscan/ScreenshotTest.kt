package de.aapphh.heatscan

import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Density
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
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

    private fun shot(name: String, fontScale: Float = 1f, content: @Composable () -> Unit) {
        rule.setContent { val d = LocalDensity.current; CompositionLocalProvider(LocalDensity provides Density(d.density, fontScale)) { HeatTheme { content() } } }
        rule.onRoot().captureRoboImage("screenshots/$name.png")
    }

    private fun footer() = Footer.line("12:34", true, 0)

    @Composable private fun demo() = DataPage(Demo.ROOMS, ColorBook.assign(emptyMap(), Rings.palette(Rings.order(Demo.ROOMS)), 0L), Demo.NAMED, Demo.APS, Demo.GUESS, Demo.P, footer())

    private val ask = WindowAsk("w1", "kueche", true, 0L, "Temperatur faellt schnell")

    private val home = HomeState("kueche", listOf("bad"), ask, listOf(Alert("a@1", "Heizung: Thermostat", "Bad antwortet nicht.")), "Lernen", false, "WLAN 10 min", false, footer())

    private val noop = HomeActions({}, {}, {}, {}, {}, {}, {}, {})

    @Composable private fun boost() = BoostScreen(
        listOf(BoostRoom("wohnzimmer", "idle", "kein Boost-Modell geladen"), BoostRoom("kueche", "boosting", "Szenario manual")), BoostResult(false, "kein Boost-Modell geladen"), null
    ) {}

    @Test fun rooms_none() = shot("rooms_none") { RoomsPage(null, null) {} }

    @Test fun rooms_guess_kueche() = shot("rooms_guess_kueche") { RoomsPage("kueche", null) {} }

    @Test fun rooms_label_bad_guess_kueche() = shot("rooms_label_bad_guess_kueche") { RoomsPage("kueche", "bad") {} }

    @Test fun rooms_label_keiner() = shot("rooms_label_keiner") { RoomsPage("keiner", null) {} }

    @Test fun rooms_label_keiner_guess_flur() = shot("rooms_label_keiner_guess_flur") { RoomsPage("flur", "keiner") {} }

    @Test fun data_demo() = shot("data_demo") { demo() }

    @Test fun data_no_model() = shot("data_no_model") {
        val named = Demo.NAMED.take(4)
        DataPage(emptyList(), ColorBook.assign(emptyMap(), named.map { it.bssid }, 0L), named, emptyList(), null, null, footer())
    }

    @Test fun home_small() = shot("home_small") { HomeScreen(home, noop) }

    @Test fun home_quiet_small() = shot("home_quiet_small") { HomeScreen(home.copy(room = null, openWindows = emptyList(), ask = null, alerts = emptyList()), noop) }

    @Test fun ask_small() = shot("ask_small") { AskScreen(ask) {} }

    @Test fun windows_small() = shot("windows_small") { WindowsScreen(listOf("wohnzimmer", "bad", "kueche"), listOf("bad")) { _, _ -> } }

    @Test fun boost_small() = shot("boost_small") { boost() }

    @Test fun saved_small() = shot("saved_small") { SavedPage("Fenster Küche auf", {}, {}) }

    @Test @Config(qualifiers = "w204dp-h204dp-round-xhdpi") fun home_41mm() = shot("home_41mm") { HomeScreen(home, noop) }

    @Test fun ask_font_1_3() = shot("ask_font_1_3", 1.3f) { AskScreen(ask) {} }

    @Test fun home_font_1_3() = shot("home_font_1_3", 1.3f) { HomeScreen(home, noop) }

    @Test @Config(qualifiers = "w213dp-h213dp-round-xhdpi") fun data_demo_213() = shot("data_demo_213") { demo() }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun data_demo_45mm() = shot("data_demo_45mm") { demo() }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun home_45mm() = shot("home_45mm") { HomeScreen(home, noop) }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun ask_45mm() = shot("ask_45mm") { AskScreen(ask) {} }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun boost_45mm() = shot("boost_45mm") { boost() }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun rooms_45mm() = shot("rooms_45mm") { RoomsPage("kueche", "bad") {} }
}
