package de.aapphh.heatscan

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import androidx.wear.compose.material.MaterialTheme
import com.github.takahirom.roborazzi.captureRoboImage
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [35], qualifiers = "w213dp-h213dp-round-xhdpi")
class ScreenshotTest {
    @get:Rule val rule = createComposeRule()

    private fun shot(name: String, content: @Composable () -> Unit) {
        rule.setContent { MaterialTheme { Box(Modifier.fillMaxSize().background(Color.Black)) { content() } } }
        rule.onRoot().captureRoboImage("screenshots/$name.png")
    }

    private fun footer() = Footer.line("12:34", true, 0)

    @Composable private fun demo() = DataPage(Demo.ROOMS, ColorBook.assign(emptyMap(), Rings.palette(Rings.order(Demo.ROOMS)), 0L), Demo.NAMED, Demo.APS, Demo.GUESS, Demo.P, footer())

    @Test fun rooms_none() = shot("rooms_none") { RoomsPage(null, null) {} }

    @Test fun rooms_guess_kueche() = shot("rooms_guess_kueche") { RoomsPage("kueche", null) {} }

    @Test fun rooms_label_bad_guess_kueche() = shot("rooms_label_bad_guess_kueche") { RoomsPage("kueche", "bad") {} }

    @Test fun data_demo() = shot("data_demo") { demo() }

    @Test fun data_no_model() = shot("data_no_model") {
        val named = Demo.NAMED.take(4)
        DataPage(emptyList(), ColorBook.assign(emptyMap(), named.map { it.bssid }, 0L), named, emptyList(), null, null, footer())
    }

    @Test @Config(qualifiers = "w227dp-h227dp-round-xhdpi") fun data_demo_45mm() = shot("data_demo_45mm") { demo() }
}
