package de.aapphh.heatscan

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.wear.compose.foundation.lazy.TransformingLazyColumn
import androidx.wear.compose.foundation.lazy.TransformingLazyColumnScope
import androidx.wear.compose.foundation.lazy.rememberTransformingLazyColumnState
import androidx.wear.compose.material3.Button
import androidx.wear.compose.material3.ColorScheme
import androidx.wear.compose.material3.EdgeButton
import androidx.wear.compose.material3.EdgeButtonSize
import androidx.wear.compose.material3.FilledTonalButton
import androidx.wear.compose.material3.ListHeader
import androidx.wear.compose.material3.LocalContentColor
import androidx.wear.compose.material3.MaterialTheme
import androidx.wear.compose.material3.ScreenScaffold
import androidx.wear.compose.material3.SurfaceTransformation
import androidx.wear.compose.material3.SwitchButton
import androidx.wear.compose.material3.Text
import androidx.wear.compose.material3.TitleCard
import androidx.wear.compose.material3.dynamicColorScheme
import androidx.wear.compose.material3.lazy.TransformationSpec
import androidx.wear.compose.material3.lazy.rememberTransformationSpec
import androidx.wear.compose.material3.lazy.transformedHeight
import kotlinx.coroutines.delay

@Composable
internal fun HeatTheme(content: @Composable () -> Unit) {
    val ctx = LocalContext.current
    val scheme = remember(ctx) { try { dynamicColorScheme(ctx) } catch (e: Exception) { null } } ?: ColorScheme()
    MaterialTheme(colorScheme = scheme) {
        CompositionLocalProvider(LocalContentColor provides MaterialTheme.colorScheme.onBackground) {
            Box(Modifier.fillMaxSize().background(Color.Black)) { content() }
        }
    }
}

internal val TARGET = 48.dp
private val GAP = 8.dp

private fun Modifier.describe(text: String) = semantics { contentDescription = text }

@Composable
internal fun ListScreen(edge: (@Composable BoxScope.() -> Unit)? = null, content: TransformingLazyColumnScope.(TransformationSpec) -> Unit) {
    val state = rememberTransformingLazyColumnState()
    val spec = rememberTransformationSpec()
    if (edge == null) ScreenScaffold(scrollState = state) { padding ->
        TransformingLazyColumn(state = state, contentPadding = padding) { content(spec) }
    } else ScreenScaffold(scrollState = state, edgeButton = edge) { padding ->
        TransformingLazyColumn(state = state, contentPadding = padding) { content(spec) }
    }
}

@Composable
internal fun EdgeAction(text: String, onClick: () -> Unit): @Composable BoxScope.() -> Unit = {
    EdgeButton(onClick = onClick, buttonSize = EdgeButtonSize.Medium, modifier = Modifier.describe(text)) { Text(text, maxLines = 1) }
}

private fun TransformingLazyColumnScope.header(spec: TransformationSpec, text: String) = item {
    ListHeader(Modifier.fillMaxWidth().transformedHeight(this, spec), transformation = SurfaceTransformation(spec)) { Text(text, maxLines = 2) }
}

private fun TransformingLazyColumnScope.action(spec: TransformationSpec, label: String, secondary: String? = null, tonal: Boolean = true, enabled: Boolean = true, onClick: () -> Unit) = item {
    val mod = Modifier.fillMaxWidth().defaultMinSize(minHeight = TARGET).transformedHeight(this, spec)
    val second: (@Composable RowScope.() -> Unit)? = secondary?.let { { Text(it, maxLines = 2) } }
    if (tonal) FilledTonalButton(onClick, mod, enabled = enabled, transformation = SurfaceTransformation(spec), secondaryLabel = second) { Text(label, maxLines = 2) }
    else Button(onClick, mod, enabled = enabled, transformation = SurfaceTransformation(spec), secondaryLabel = second) { Text(label, maxLines = 2) }
}

private fun TransformingLazyColumnScope.pair(spec: TransformationSpec, first: String, firstTone: Boolean, second: String, secondTone: Boolean, describeFirst: String, describeSecond: String, onFirst: () -> Unit, onSecond: () -> Unit) = item {
    Row(Modifier.fillMaxWidth().transformedHeight(this, spec), horizontalArrangement = Arrangement.spacedBy(GAP)) {
        listOf(Triple(first, firstTone, describeFirst) to onFirst, Triple(second, secondTone, describeSecond) to onSecond).forEach { (t, click) ->
            val m = Modifier.weight(1f).defaultMinSize(minHeight = TARGET).describe(t.third)
            if (t.second) Button(click, m) { Text(t.first, maxLines = 1) }
            else FilledTonalButton(click, m) { Text(t.first, maxLines = 1) }
        }
    }
}

private fun TransformingLazyColumnScope.note(spec: TransformationSpec, text: String, color: Color, style: TextStyle) = item {
    Text(text, Modifier.fillMaxWidth().transformedHeight(this, spec), color = color, style = style, textAlign = TextAlign.Center)
}

@Composable
internal fun RoomDetailScreen(room: String, g: RoomGlance?, result: BoostResult?, busy: Boolean, canBoost: Boolean, onWindow: () -> Unit, onBoost: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val type = MaterialTheme.typography
    val edge: (@Composable BoxScope.() -> Unit)? = if (canBoost) {
        { EdgeButton(onClick = onBoost, buttonSize = EdgeButtonSize.Medium, enabled = !busy, modifier = Modifier.describe("Boost " + WindowEvent.caption(room))) { Text(if (busy) "…" else "Boost", maxLines = 1) } }
    } else null
    ListScreen(edge = edge) { spec ->
        header(spec, WindowEvent.caption(room))
        note(spec, RoomView.temp(g) ?: "–", colors.onBackground, type.numeralMedium)
        RoomView.target(g)?.let { note(spec, it, colors.onSurfaceVariant, type.bodyMedium) }
        RoomView.why(g)?.let { note(spec, it, colors.onSurface, type.bodyMedium) }
        result?.let { note(spec, it.line, if (it.ok) colors.primary else colors.error, type.bodySmall) }
        action(spec, if (g?.open == true) "Fenster ist zu" else "Fenster hier offen", onClick = onWindow)
    }
}

@Composable
internal fun BoostConfirmScreen(room: String, onStart: () -> Unit, onCancel: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val type = MaterialTheme.typography
    ListScreen(edge = EdgeAction(Boost.START, onStart)) { spec ->
        header(spec, "Boost")
        note(spec, Boost.question(room), colors.onSurface, type.bodyMedium)
        action(spec, Boost.CANCEL, onClick = onCancel)
    }
}

@Composable
internal fun AlertsScreen(alerts: List<Alert>, ask: WindowAsk?, onAsk: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val type = MaterialTheme.typography
    ListScreen { spec ->
        header(spec, "Hinweise")
        ask?.let { action(spec, WindowEvent.question(it), WindowEvent.text(it), tonal = false, onClick = onAsk) }
        alerts.forEach { al ->
            item(key = al.id) {
                TitleCard(onClick = {}, title = { Text(al.title, maxLines = 2) }, modifier = Modifier.fillMaxWidth().transformedHeight(this, spec), transformation = SurfaceTransformation(spec)) { Text(al.text, maxLines = 4) }
            }
        }
        if (ask == null && alerts.isEmpty()) note(spec, "Alles in Ordnung", colors.onSurfaceVariant, type.bodyMedium)
    }
}

class TechState(val learnChip: String, val learning: Boolean, val holdChip: String, val holdOn: Boolean, val footer: String)

@Composable
internal fun TechScreen(s: TechState, onData: () -> Unit, onLearn: () -> Unit, onHold: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val type = MaterialTheme.typography
    ListScreen { spec ->
        header(spec, "Technik")
        action(spec, "WLAN-Ringe", onClick = onData)
        item {
            SwitchButton(s.learning, { onLearn() }, Modifier.fillMaxWidth().defaultMinSize(minHeight = TARGET).transformedHeight(this, spec), transformation = SurfaceTransformation(spec), label = { Text("Lernen: " + s.learnChip, maxLines = 1) })
        }
        item {
            SwitchButton(s.holdOn, { onHold() }, Modifier.fillMaxWidth().defaultMinSize(minHeight = TARGET).transformedHeight(this, spec), transformation = SurfaceTransformation(spec), label = { Text("WLAN: " + s.holdChip, maxLines = 1) })
        }
        note(spec, s.footer, colors.onSurfaceVariant, type.bodyExtraSmall)
    }
}

@Composable
internal fun AskScreen(ask: WindowAsk, onAnswer: (WindowAnswer) -> Unit) {
    val state = rememberTransformingLazyColumnState()
    val spec = rememberTransformationSpec()
    ScreenScaffold(scrollState = state, edgeButton = EdgeAction(WindowEvent.YES) { onAnswer(WindowAnswer.YES) }) { padding ->
        TransformingLazyColumn(state = state, contentPadding = padding) {
            header(spec, WindowEvent.question(ask))
            item {
                Text(WindowEvent.text(ask), Modifier.fillMaxWidth().transformedHeight(this, spec), style = MaterialTheme.typography.bodyMedium, textAlign = TextAlign.Center)
            }
            pair(spec, WindowEvent.YES, true, WindowEvent.NO, false, WindowEvent.yesDescription(ask), WindowEvent.noDescription(ask), { onAnswer(WindowAnswer.YES) }, { onAnswer(WindowAnswer.NO) })
            action(spec, WindowEvent.UNKNOWN) { onAnswer(WindowAnswer.UNKNOWN) }
        }
    }
}

@Composable
internal fun SavedPage(text: String, onUndo: () -> Unit, onDone: () -> Unit) {
    val haptic = LocalHapticFeedback.current
    var spent by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) {
        haptic.performHapticFeedback(HapticFeedbackType.LongPress)
        delay(WindowEvent.UNDO_MS)
        onDone()
    }
    ScreenScaffold {
        Column(Modifier.fillMaxSize().padding(horizontal = 28.dp), Arrangement.Center, Alignment.CenterHorizontally) {
            Text(text, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center, fontWeight = FontWeight.Bold)
            FilledTonalButton({ if (!spent) { spent = true; onUndo() } }, Modifier.padding(top = GAP).defaultMinSize(minHeight = TARGET), enabled = !spent) { Text(Tap.UNDO, maxLines = 1) }
        }
    }
}

@Composable
internal fun tapColors(kind: TapKind): Pair<Color, Color> = MaterialTheme.colorScheme.let {
    when (kind) {
        TapKind.OK, TapKind.QUEUED -> it.primaryContainer to it.onPrimaryContainer
        TapKind.THROTTLED -> it.tertiaryContainer to it.onTertiaryContainer
        TapKind.FAILED -> it.errorContainer to it.onErrorContainer
        TapKind.WAIT -> it.surfaceContainer to it.onSurface
        TapKind.VOIDED -> it.surfaceContainerHigh to it.onSurface
    }
}
