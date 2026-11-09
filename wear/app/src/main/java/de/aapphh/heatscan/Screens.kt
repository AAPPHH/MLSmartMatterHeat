package de.aapphh.heatscan

import androidx.compose.foundation.focusable
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.requiredHeight
import androidx.compose.foundation.layout.wrapContentHeight
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
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
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.rotary.onPreRotaryScrollEvent
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import androidx.wear.compose.foundation.lazy.TransformingLazyColumn
import androidx.wear.compose.foundation.lazy.TransformingLazyColumnScope
import androidx.wear.compose.foundation.lazy.rememberTransformingLazyColumnState
import androidx.wear.compose.material3.Button
import androidx.wear.compose.material3.ColorScheme
import androidx.wear.compose.material3.EdgeButton
import androidx.wear.compose.material3.EdgeButtonSize
import androidx.wear.compose.material3.FilledTonalButton
import androidx.wear.compose.material3.Icon
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
private const val LINES = 8
private val PEEK = 6.dp

internal class Idle {
    private var at = System.currentTimeMillis()

    fun touch() { at = System.currentTimeMillis() }

    suspend fun await(ms: Long) {
        touch()
        while (true) {
            val left = at + ms - System.currentTimeMillis()
            if (left <= 0) return
            delay(left)
        }
    }
}

internal fun Modifier.onTouch(touched: () -> Unit): Modifier = pointerInput(Unit) {
    awaitPointerEventScope {
        while (true) {
            awaitPointerEvent(PointerEventPass.Initial)
            touched()
        }
    }
}

private fun onOff(on: Boolean) = if (on) "an" else "aus"

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

private fun TransformingLazyColumnScope.header(spec: TransformationSpec, text: String, sub: String? = null) = item {
    ListHeader(Modifier.fillMaxWidth().transformedHeight(this, spec), transformation = SurfaceTransformation(spec)) {
        Column(horizontalAlignment = Alignment.CenterHorizontally) {
            Text(text, textAlign = TextAlign.Center)
            sub?.let { Text(it, Modifier.padding(top = 2.dp), style = MaterialTheme.typography.bodyExtraSmall, textAlign = TextAlign.Center) }
        }
    }
}

private fun TransformingLazyColumnScope.action(spec: TransformationSpec, label: String, secondary: String? = null, tonal: Boolean = true, enabled: Boolean = true, icon: Int? = null, onClick: () -> Unit) = item {
    val mod = Modifier.fillMaxWidth().defaultMinSize(minHeight = TARGET).transformedHeight(this, spec)
    val second: (@Composable RowScope.() -> Unit)? = secondary?.let { { Text(it, maxLines = LINES) } }
    val glyph: (@Composable BoxScope.() -> Unit)? = icon?.let { { Icon(painterResource(it), null, Modifier.size(MARK_DP.dp)) } }
    if (tonal) FilledTonalButton(onClick, mod, enabled = enabled, transformation = SurfaceTransformation(spec), icon = glyph, secondaryLabel = second) { Text(label, maxLines = LINES) }
    else Button(onClick, mod, enabled = enabled, transformation = SurfaceTransformation(spec), icon = glyph, secondaryLabel = second) { Text(label, maxLines = LINES) }
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
internal fun RoomsScreen(
    hot: String?, ringed: String?, locked: Boolean, learn: LearnView?, glance: Glance, alerts: List<Alert>, ask: WindowAsk?, showBadge: Boolean,
    onDetail: (String) -> Unit, onAlerts: () -> Unit, onTech: () -> Unit, onTap: (String) -> Unit
) {
    val state = rememberTransformingLazyColumnState()
    val spec = rememberTransformationSpec()
    val count = RoomView.count(alerts, ask)
    ScreenScaffold(scrollState = state, timeText = {}) { padding ->
        BoxWithConstraints(Modifier.fillMaxSize()) {
            val full = maxHeight
            TransformingLazyColumn(state = state, contentPadding = PaddingValues(start = padding.calculateLeftPadding(LayoutDirection.Ltr), end = padding.calculateRightPadding(LayoutDirection.Ltr), bottom = padding.calculateBottomPadding())) {
                item {
                    Box(Modifier.fillMaxWidth().height(full - PEEK)) {
                        Box(Modifier.fillMaxWidth().wrapContentHeight(Alignment.Top, unbounded = true).requiredHeight(full)) {
                            RoomsPage(hot, ringed, locked, learn, glance, RoomView.badge(alerts, ask).takeIf { showBadge }, onAlerts, onDetail, onTap)
                        }
                    }
                }
                RoomView.pill(alerts, ask)?.let { label -> action(spec, label, tonal = false, onClick = onAlerts) }
                header(spec, "Räume", RoomView.LEGEND)
                Defaults.ROOMS.forEach { room ->
                    val g = RoomView.glance(glance, room)
                    action(spec, WindowEvent.caption(room), RoomView.row(g), icon = RoomView.mark(g).res) { onDetail(room) }
                }
                if (count == 0) action(spec, "Hinweise (0)", onClick = onAlerts)
                action(spec, "Technik", onClick = onTech)
            }
        }
    }
}

class SetpointUi(val target: Double, val hold: HoldChoice, val busy: Boolean, val line: BoostResult?, val active: Manual?, val onStep: (Double) -> Unit, val onHold: () -> Unit, val onApply: () -> Unit, val onEnd: () -> Unit, val open: Boolean = false)

private fun TransformingLazyColumnScope.setpoint(spec: TransformationSpec, ui: SetpointUi, colors: ColorScheme, type: androidx.wear.compose.material3.Typography) {
    ui.active?.let { note(spec, SetpointControl.held(it, ui.open), colors.primary, type.bodySmall) }
    item {
        Row(Modifier.fillMaxWidth().padding(horizontal = 12.dp).transformedHeight(this, spec), horizontalArrangement = Arrangement.spacedBy(GAP), verticalAlignment = Alignment.CenterVertically) {
            FilledTonalButton({ ui.onStep(-SetpointControl.STEP) }, Modifier.width(TARGET).defaultMinSize(minHeight = TARGET).describe("Temperatur senken"), contentPadding = PaddingValues(0.dp)) { Text("–", Modifier.fillMaxWidth(), style = type.titleMedium, textAlign = TextAlign.Center, maxLines = 1) }
            Text(SetpointControl.celsius(ui.target) + "°", Modifier.weight(1f), style = type.titleSmall, textAlign = TextAlign.Center, maxLines = 1)
            FilledTonalButton({ ui.onStep(SetpointControl.STEP) }, Modifier.width(TARGET).defaultMinSize(minHeight = TARGET).describe("Temperatur erhöhen"), contentPadding = PaddingValues(0.dp)) { Text("+", Modifier.fillMaxWidth(), style = type.titleMedium, textAlign = TextAlign.Center, maxLines = 1) }
        }
    }
    action(spec, "Dauer: " + ui.hold.label, onClick = ui.onHold)
    action(spec, if (ui.busy) "…" else SetpointControl.celsius(ui.target) + " °C setzen", tonal = false, enabled = !ui.busy, onClick = ui.onApply)
    ui.line?.let { note(spec, it.line, if (it.ok) colors.primary else colors.error, type.bodySmall) }
    if (ui.active != null) action(spec, SetpointControl.END, enabled = !ui.busy, onClick = ui.onEnd)
}

@Composable
internal fun RoomDetailScreen(room: String, g: RoomGlance?, result: BoostResult?, busy: Boolean, canBoost: Boolean, onWindow: () -> Unit, onBoost: () -> Unit, saved: Boolean = false, applying: Boolean = saved, control: SetpointUi? = null, onUndo: () -> Unit = {}) {
    val colors = MaterialTheme.colorScheme
    val type = MaterialTheme.typography
    ListScreen { spec ->
        header(spec, (if (saved) "✓ Gespeichert: " else "") + WindowEvent.caption(room))
        note(spec, RoomView.temp(g) ?: "–", colors.onBackground, type.numeralSmall)
        (if (applying) listOf(RoomView.APPLYING) else listOfNotNull(RoomView.target(g), RoomView.why(g))).takeIf { it.isNotEmpty() }?.let { note(spec, it.joinToString(" · "), colors.onSurface, type.bodySmall) }
        if (canBoost && control?.active == null) action(spec, if (busy) "…" else "Boost", tonal = false, enabled = !busy, icon = R.drawable.ic_mark_boost, onClick = onBoost)
        result?.let { note(spec, it.line, if (it.ok) colors.primary else colors.error, type.bodySmall) }
        if (control != null && !saved) setpoint(spec, control, colors, type)
        action(spec, if (g?.open == true) "Fenster zu melden" else "Fenster offen melden", icon = R.drawable.ic_mark_window, onClick = onWindow)
        if (saved) action(spec, Tap.UNDO, onClick = onUndo)
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
                TitleCard(onClick = {}, title = { Text(al.title, maxLines = LINES) }, modifier = Modifier.fillMaxWidth().transformedHeight(this, spec), transformation = SurfaceTransformation(spec)) { Text(al.text, maxLines = LINES) }
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
            SwitchButton(s.learning, { onLearn() }, Modifier.fillMaxWidth().defaultMinSize(minHeight = TARGET).transformedHeight(this, spec), transformation = SurfaceTransformation(spec), secondaryLabel = s.learnChip.takeIf { it == Learn.FINISHED }?.let { { Text(it) } }, label = { Text("Raum lernen: " + onOff(s.learning), maxLines = LINES) })
        }
        item {
            SwitchButton(s.holdOn, { onHold() }, Modifier.fillMaxWidth().defaultMinSize(minHeight = TARGET).transformedHeight(this, spec), transformation = SurfaceTransformation(spec), secondaryLabel = { Text(s.holdChip, maxLines = LINES) }, label = { Text("WLAN halten: " + onOff(s.holdOn), maxLines = LINES) })
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
    val idle = remember { Idle() }
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
    LaunchedEffect(Unit) {
        haptic.performHapticFeedback(HapticFeedbackType.LongPress)
        idle.await(WindowEvent.UNDO_MS)
        onDone()
    }
    ScreenScaffold {
        Column(Modifier.fillMaxSize().onTouch { idle.touch() }.onPreRotaryScrollEvent { idle.touch(); false }.focusRequester(focus).focusable().padding(horizontal = 28.dp), Arrangement.Center, Alignment.CenterHorizontally) {
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
