package de.aapphh.heatscan

import android.content.Intent
import android.content.SharedPreferences
import android.graphics.Paint
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.clipPath
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.onClick
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.navigation.NavHostController
import androidx.wear.compose.material3.AppScaffold
import androidx.wear.compose.material3.FilledTonalButton
import androidx.wear.compose.material3.Icon
import androidx.wear.compose.material3.MaterialTheme
import androidx.wear.compose.material3.ScreenScaffold
import androidx.wear.compose.material3.Text
import androidx.wear.compose.navigation.SwipeDismissableNavHost
import androidx.wear.compose.navigation.composable
import androidx.wear.compose.navigation.rememberSwipeDismissableNavController
import kotlinx.coroutines.delay
import kotlin.math.cos
import kotlin.math.roundToInt
import kotlin.math.sin

class MainActivity : ComponentActivity() {
    private val perms = registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        val background = ScanStarter.missingBackground(this)
        if (background.isNotEmpty() && !askedBackground) {
            askedBackground = true
            backgroundPerms.launch(background.toTypedArray())
        } else done()
    }
    private val backgroundPerms = registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { done() }
    private var asked = false
    private var askedBackground = false
    private var newIntents by mutableIntStateOf(0)

    private fun done() {
        ScanService.instance?.refreshSensors()
        ScanStarter.start(this, "permissions")
    }
    private val prompt get() = intent.getBooleanExtra("prompt", false)
    private val windowIntent get() = intent.getBooleanExtra("window", false)
    private val learnPrompt get() = prompt && intent.getBooleanExtra("learn", false)

    private fun wake() {
        if (prompt || windowIntent) {
            setShowWhenLocked(true)
            setTurnScreenOn(true)
        }
    }

    private fun route(store: Store): String = when {
        prompt -> Routes.ROOMS
        windowIntent && WindowEvent.live(store.windowPending, System.currentTimeMillis()) != null -> Routes.ASK
        else -> Routes.HOME
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        wake()
        val store = Store(this)
        intent.getStringExtra("server")?.let { store.server = it }
        intent.getStringExtra("device")?.let { store.device = it }
        intent.getStringExtra("home")?.let { store.home = it }
        val start = route(store)
        setContent { HeatTheme { AppScaffold { Nav(store, start) } } }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        wake()
        newIntents++
    }

    override fun onResume() {
        super.onResume()
        val missing = ScanStarter.missing(this)
        if (missing.isNotEmpty() && !asked) {
            asked = true
            perms.launch(missing.toTypedArray())
        } else if (!askedBackground && ScanStarter.missingBackground(this).isNotEmpty()) {
            askedBackground = true
            backgroundPerms.launch(ScanStarter.missingBackground(this).toTypedArray())
        } else ScanStarter.start(this, "resume")
        ScanService.setForeground(true)
        WindowPrompter.sweep(this)
    }

    override fun onPause() {
        ScanService.setForeground(false)
        super.onPause()
    }

    private class Saved(val text: String, val labelId: String)

    @Composable
    private fun Nav(store: Store, start: String) {
        val nav = rememberSwipeDismissableNavController()
        var saved by remember { mutableStateOf<Saved?>(null) }
        var external by remember { mutableStateOf(start != Routes.HOME) }
        val leave = {
            if (external) finish() else nav.popBackStack(Routes.HOME, false)
            Unit
        }
        val ui = { f: () -> Unit -> runOnUiThread(f) }
        val show = { text: String, id: String ->
            saved = Saved(text, id)
            nav.navigate(Routes.SAVED) { popUpTo(Routes.HOME) }
            Unit
        }
        LaunchedEffect(newIntents) {
            if (newIntents > 0) {
                val r = route(store)
                external = r != Routes.HOME
                if (r != Routes.HOME) nav.navigate(r) { popUpTo(Routes.HOME); launchSingleTop = true }
            }
        }
        SwipeDismissableNavHost(nav, startDestination = start) {
            composable(Routes.HOME) { HomeRoute(store, nav, show, ui) }
            composable(Routes.ROOMS) { RoomsRoute(store, prompt, intent.getBooleanExtra("change", false), learnPrompt) { if (prompt) finish() } }
            composable(Routes.DATA) { DataRoute(store) }
            composable(Routes.ASK) { AskRoute(store, show, leave, ui) }
            composable(Routes.WINDOWS) { WindowsRoute(store, show, ui) }
            composable(Routes.BOOST) { BoostRoute() }
            composable(Routes.SAVED) {
                val s = saved
                if (s == null) LaunchedEffect(Unit) { leave() }
                else SavedPage(s.text, { WindowPrompter.void(this@MainActivity, s.labelId); leave() }, leave)
            }
        }
    }

    @Composable
    private fun HomeRoute(store: Store, nav: NavHostController, show: (String, String) -> Unit, ui: (() -> Unit) -> Unit) {
        val version = storeVersion(store)
        var now by remember { mutableLongStateOf(System.currentTimeMillis()) }
        val holdUntil = version.let { store.wifiHoldUntil }
        val holdOn = WifiHold.active(holdUntil, now)
        LaunchedEffect(Unit) { Remote.refresh(this@MainActivity) }
        LaunchedEffect(holdOn) {
            while (holdOn) {
                now = System.currentTimeMillis()
                delay(15_000)
            }
        }
        val learning = version.let { store.learnAt != 0L }
        val room = (store.stayLabel() ?: store.guess)?.takeIf { it in Defaults.ROOMS }
        val state = HomeState(
            room, version.let { store.glance.openRooms() }, WindowEvent.live(version.let { store.windowPending }, now), version.let { store.alerts },
            Learn.chip(learning, store.progress), learning, WifiHold.chip(holdUntil, now), holdOn,
            Footer.line(store.postHm, store.postOk, ScanQueue.get(this).size, store.paused, Footer.fresh(store.lastFreshAt), store.rejected)
        )
        HomeScreen(state, HomeActions(
            onAsk = { nav.navigate(Routes.ASK) },
            onWindow = { open -> room?.let { r -> WindowPrompter.selfReport(this, r, open) { id -> ui { show("Fenster ${WindowEvent.caption(r)} ${if (open) "auf" else "zu"}", id) } } } },
            onRooms = { nav.navigate(Routes.ROOMS) },
            onWindows = { nav.navigate(Routes.WINDOWS) },
            onBoost = { nav.navigate(Routes.BOOST) },
            onData = { nav.navigate(Routes.DATA) },
            onLearn = { ScanService.instance?.setLearning(!learning) },
            onHold = { ScanService.instance?.toggleWifiHold() }
        ))
    }

    @Composable
    private fun AskRoute(store: Store, show: (String, String) -> Unit, leave: () -> Unit, ui: (() -> Unit) -> Unit) {
        val version = storeVersion(store)
        val ask = WindowEvent.live(version.let { store.windowPending }, System.currentTimeMillis())
        if (ask == null) {
            LaunchedEffect(Unit) { leave() }
            return
        }
        AskScreen(ask) { answer ->
            WindowPrompter.answer(this, ask.id, answer, "page") { id ->
                ui { if (id == null) leave() else show(when (answer) { WindowAnswer.YES -> "Fenster offen"; WindowAnswer.NO -> "Fenster zu"; else -> "Weiß nicht" }, id) }
            }
        }
    }

    @Composable
    private fun WindowsRoute(store: Store, show: (String, String) -> Unit, ui: (() -> Unit) -> Unit) {
        val version = storeVersion(store)
        WindowsScreen(WindowEvent.rooms(version.let { store.progress }), version.let { store.glance.openRooms() }) { room, open ->
            WindowPrompter.selfReport(this, room, open) { id -> ui { show("Fenster ${WindowEvent.caption(room)} ${if (open) "auf" else "zu"}", id) } }
        }
    }

    @Composable
    private fun BoostRoute() {
        var rooms by remember { mutableStateOf<List<BoostRoom>?>(null) }
        var result by remember { mutableStateOf<BoostResult?>(null) }
        var busy by remember { mutableStateOf<String?>(null) }
        val reload = { Remote.boostState(this) { r -> runOnUiThread { rooms = r } } }
        LaunchedEffect(Unit) { reload() }
        BoostScreen(rooms, result, busy) { room ->
            busy = room
            Remote.boost(this, room) { r ->
                runOnUiThread {
                    result = r
                    busy = null
                    reload()
                }
            }
        }
    }

    @Composable
    private fun DataRoute(store: Store) {
        val version = storeVersion(store)
        ScreenScaffold(timeText = {}) {
            DataPage(
                version.let { store.rooms }, store.colors, store.lastAps, store.replyAps, store.guess, store.probability,
                Footer.line(store.postHm, store.postOk, ScanQueue.get(this@MainActivity).size, store.paused, Footer.fresh(store.lastFreshAt), store.rejected), store.match
            )
        }
    }
}

internal object Routes {
    const val HOME = "home"
    const val ROOMS = "rooms"
    const val DATA = "data"
    const val ASK = "ask"
    const val WINDOWS = "windows"
    const val BOOST = "boost"
    const val SAVED = "saved"
}

@Composable
private fun storeVersion(store: Store): Int {
    var version by remember { mutableIntStateOf(0) }
    DisposableEffect(Unit) {
        val l = SharedPreferences.OnSharedPreferenceChangeListener { _, _ -> version++ }
        store.sp.registerOnSharedPreferenceChangeListener(l)
        onDispose { store.sp.unregisterOnSharedPreferenceChangeListener(l) }
    }
    return version
}

@Composable
private fun RoomsRoute(store: Store, prompt: Boolean, change: Boolean, learnPrompt: Boolean, finish: () -> Unit) {
    val ctx = LocalContext.current
    val version = storeVersion(store)
    var tapped by remember { mutableStateOf<String?>(null) }
    var choosing by remember { mutableStateOf<String?>(null) }
    var now by remember { mutableLongStateOf(System.currentTimeMillis()) }
    val state = version.let { store.manual }
    val until = version.let { store.manualUntil }
    val locked = Tap.locked(state, until, now)
    val view = tapped?.let { Tap.view(state, it, until, now) }
    val kind = view?.kind
    val haptic = LocalHapticFeedback.current
    val learning = version.let { store.learnAt != 0L }
    val learnView = Learn.view(learning, version.let { store.progress })
    val pal = palette()
    LaunchedEffect(version, tapped) {
        if (learnPrompt && tapped == null && choosing == null && store.learnPromptAt == 0L) finish()
    }
    val pick = { room: String, heat: Boolean ->
        choosing = null
        LabelRecorder.measure(ctx, room, heat) { if (prompt) Prompter.cancel(ctx.applicationContext) }
        tapped = room
    }
    LaunchedEffect(choosing) {
        choosing?.let { room ->
            delay(Heat.CHOICE_MS)
            pick(room, Heat.default(store.learnAt != 0L))
        }
    }
    LaunchedEffect(locked, tapped) {
        while (locked || tapped != null) {
            now = System.currentTimeMillis()
            delay(500)
        }
    }
    LaunchedEffect(kind) {
        if (kind != null && kind != TapKind.WAIT) {
            haptic.performHapticFeedback(HapticFeedbackType.LongPress)
            if (kind == TapKind.THROTTLED || kind == TapKind.FAILED) {
                delay(250)
                haptic.performHapticFeedback(HapticFeedbackType.LongPress)
            }
            if (prompt) {
                delay(if (Tap.undoable(kind)) Tap.UNDO_MS else 2000)
                finish()
            } else {
                delay(Tap.showMs(kind))
                tapped = null
            }
        }
    }
    ScreenScaffold(timeText = {}) {
        Box(Modifier.fillMaxSize()) {
            val hint = if (locked && view == null && state == Tap.THROTTLED) "Warte ${Tap.remainingS(until, now)} s" else null
            (hint ?: Prompt.title(change, learnPrompt).takeIf { prompt })?.let { Text(it, Modifier.align(Alignment.TopCenter).padding(top = 14.dp), fontSize = 10.sp, color = pal.text, maxLines = 1) }
            val (hot, ringed) = Highlight.of(store.stayLabel(), store.guess)
            RoomsPage(hot, ringed, locked, learnView) {
                if (choosing == null && !Tap.locked(store.manual, store.manualUntil, System.currentTimeMillis())) {
                    if (Heat.heatable(it, store.progress)) choosing = it else pick(it, false)
                }
            }
            choosing?.let { room ->
                Column(Modifier.fillMaxSize().background(Color.Black).padding(horizontal = 24.dp), Arrangement.Center, Alignment.CenterHorizontally) {
                    Text(Defaults.CAPTIONS.getValue(room), style = MaterialTheme.typography.titleSmall, color = pal.text, maxLines = 1)
                    FilledTonalButton({ pick(room, true) }, Modifier.padding(top = 6.dp).fillMaxWidth().defaultMinSize(minHeight = TARGET)) { Text(Heat.YES, maxLines = 1) }
                    FilledTonalButton({ pick(room, false) }, Modifier.padding(top = 8.dp).defaultMinSize(minHeight = TARGET)) { Text(Heat.NO, maxLines = 1) }
                }
            }
            view?.let {
                val (bg, ink) = tapColors(it.kind)
                Column(Modifier.fillMaxSize().background(bg).pointerInput(Unit) { detectTapGestures { } }.padding(horizontal = 24.dp), Arrangement.Center, Alignment.CenterHorizontally) {
                    Text(it.title, style = MaterialTheme.typography.titleMedium, color = ink, textAlign = TextAlign.Center)
                    if (it.sub.isNotEmpty()) Text(it.sub, Modifier.padding(top = 6.dp), style = MaterialTheme.typography.bodySmall, color = ink, textAlign = TextAlign.Center)
                    if (Tap.undoable(it.kind) && version.let { store.lastLabelId.isNotEmpty() }) FilledTonalButton({ LabelRecorder.undo(ctx) }, Modifier.padding(top = 10.dp).defaultMinSize(minHeight = TARGET)) { Text(Tap.UNDO, maxLines = 1) }
                }
            }
        }
    }
}

internal class Palette(val selected: Color, val ink: Color, val idle: Color, val low: Color, val done: Color, val track: Color, val caption: Color, val legend: Color, val text: Color)

@Composable
internal fun palette(): Palette = MaterialTheme.colorScheme.let {
    Palette(it.primary, it.onPrimary, it.surfaceContainer, it.tertiaryContainer, it.surfaceContainerLow, it.surfaceContainerLow, it.onSurfaceVariant, it.onSurface, it.onSurface)
}

@Composable
internal fun RoomsPage(hot: String?, ringed: String?, locked: Boolean = false, learn: LearnView? = null, onTap: (String) -> Unit) {
    val pal = palette()
    val fill = { room: String -> if (room == hot) pal.selected else when (learn?.tones?.get(room)) { RoomTone.LOW -> pal.low; RoomTone.DONE -> pal.done; else -> pal.idle } }
    BoxWithConstraints(Modifier.fillMaxSize()) {
        val dDp = minOf(maxWidth, maxHeight)
        Canvas(
            Modifier.fillMaxSize().pointerInput(Unit) {
                detectTapGestures { o ->
                    val d = minOf(size.width, size.height).toFloat()
                    val room = Quarters.room(o.x - size.width / 2f, o.y - size.height / 2f, d)
                    onTap(room)
                }
            }
        ) {
            val d = minOf(size.width, size.height)
            val c = Offset(size.width / 2f, size.height / 2f)
            val outer = d / 2f
            val inner = Quarters.INNER * d
            val gap = Quarters.GAP.dp.toPx()
            val ring = 6.dp.toPx()
            Quarters.WEDGES.forEach { room ->
                val start = Quarters.mid(room) - 90f - Quarters.SPAN / 2
                val path = Path().apply {
                    arcTo(Rect(c, outer), start, Quarters.SPAN, true)
                    arcTo(Rect(c, inner), start + Quarters.SPAN, -Quarters.SPAN, false)
                    close()
                }
                drawPath(path, fill(room))
                if (room == ringed) clipPath(path) { drawPath(path, pal.text, style = Stroke(ring)) }
            }
            repeat(4) {
                val a = Math.toRadians(45.0 + it * 90.0)
                val u = Offset(cos(a).toFloat(), sin(a).toFloat())
                drawLine(Color.Black, c + u * (inner - 1f), c + u * (outer + 1f), gap)
            }
            val centre = Path().apply { addOval(Rect(c, Quarters.CENTER_R * d)) }
            drawPath(centre, fill(Quarters.CENTER))
            if (Quarters.CENTER == ringed) clipPath(centre) { drawPath(centre, pal.text, style = Stroke(ring)) }
            val pillTopLeft = Offset(c.x - Quarters.NONE_HALF_W * d, c.y + Quarters.NONE_TOP * d)
            val pillSize = Size(2f * Quarters.NONE_HALF_W * d, Quarters.NONE_H * d)
            val corner = CornerRadius(pillSize.height / 2f)
            drawRoundRect(Color.Black, pillTopLeft - Offset(gap, gap), Size(pillSize.width + 2f * gap, pillSize.height + 2f * gap), CornerRadius(corner.x + gap))
            drawRoundRect(if (Defaults.NONE == hot) pal.selected else pal.idle, pillTopLeft, pillSize, corner)
            if (Defaults.NONE == ringed) drawRoundRect(pal.text, pillTopLeft, pillSize, corner, style = Stroke(2.dp.toPx()))
        }
        Text(
            Prompt.caption(Defaults.NONE, null),
            Modifier.align(Alignment.Center).offset(y = ((Quarters.NONE_TOP + Quarters.NONE_H / 2f) * dDp.value).dp).semantics(mergeDescendants = true) {
                role = Role.Button
                contentDescription = "Raum " + Defaults.CAPTIONS.getValue(Defaults.NONE)
                onClick { onTap(Defaults.NONE); true }
            },
            fontSize = 10.sp, lineHeight = 10.sp, maxLines = 1, color = if (Defaults.NONE == hot) pal.ink else pal.text
        )
        (Quarters.WEDGES + Quarters.CENTER).forEach { room ->
            val isCenter = room == Quarters.CENTER
            val a = Math.toRadians(Quarters.mid(room) - 90.0)
            val r = if (isCenter) 0f else Quarters.labelDp(room, dDp.value)
            val dim = locked || learn?.tones?.get(room) == RoomTone.DONE
            val ink = if (room == hot) pal.ink else if (dim) pal.caption else pal.text
            Column(
                Modifier.align(Alignment.Center).offset((r * cos(a)).dp, (r * sin(a)).dp).semantics(mergeDescendants = true) {
                    role = Role.Button
                    contentDescription = "Raum " + Defaults.CAPTIONS.getValue(room)
                    onClick { onTap(room); true }
                },
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                Icon(painterResource(RoomIcons.RES.getValue(room)), room, Modifier.size((if (isCenter) 28 else 24).dp), tint = ink)
                Text(Defaults.CAPTIONS.getValue(room), fontSize = (if (isCenter) 11 else 10).sp, maxLines = 1, color = ink)
                learn?.badges?.get(room)?.let { Text(it, fontSize = 9.sp, lineHeight = 9.sp, maxLines = 1, color = ink) }
            }
        }
    }
}

@Composable
internal fun DataPage(stored: List<RoomP>, colors: Map<String, Slot>, named: List<NamedAp>, replyAps: List<Ap>, guess: String?, probability: Float?, footer: String, match: Match? = null) {
    val pal = palette()
    val rooms = Rings.order(stored)
    val palette = Rings.palette(rooms)
    val legend = Legend.aps(palette, replyAps, named)
    val names = ApNames.labels(legend, named)
    BoxWithConstraints(Modifier.fillMaxSize()) {
        val dDp = minOf(maxWidth, maxHeight).value
        Canvas(Modifier.fillMaxSize()) {
            val d = minOf(size.width, size.height)
            val c = Offset(size.width / 2f, size.height / 2f)
            val stroke = Rings.STROKE * d
            val paint = Paint().apply {
                isAntiAlias = true
                textSize = 8.sp.toPx()
            }
            val mid = -(paint.ascent() + paint.descent()) / 2f
            rooms.forEachIndexed { i, room ->
                val r = Rings.radius(i, d)
                val topLeft = Offset(c.x - r, c.y - r)
                val box = Size(2f * r, 2f * r)
                drawArc(pal.track, Rings.START, Rings.SWEEP, false, topLeft, box, style = Stroke(stroke))
                var at = Rings.START
                Rings.segments(room, colors).forEach { seg ->
                    drawArc(Color(seg.color), at, seg.sweep, false, topLeft, box, style = Stroke(stroke, cap = StrokeCap.Butt))
                    at += seg.sweep
                }
                val a = Math.toRadians(Rings.START.toDouble())
                paint.color = (if (i == 0 && stored.isNotEmpty()) pal.text else pal.caption).toArgb()
                drawIntoCanvas {
                    it.nativeCanvas.drawText(Rings.caption(room.room), c.x + r * cos(a).toFloat() + Rings.LABEL_DX * d, c.y + r * sin(a).toFloat() + mid, paint)
                }
            }
        }
        Column(Modifier.align(Alignment.Center), horizontalAlignment = Alignment.CenterHorizontally) {
            if (match != null) {
                val tone = if (match.abstained) pal.caption else pal.selected
                Text(MatchView.title(match), fontSize = Rings.NAME_SP.sp, lineHeight = (Rings.NAME_SP * Rings.LINE_H).sp, fontWeight = FontWeight.Bold, color = tone, maxLines = 1)
                Text(MatchView.sub(match), fontSize = Rings.SUB_SP.sp, lineHeight = (Rings.SUB_SP * Rings.LINE_H).sp, color = tone, maxLines = 1)
                if (match.top.isNotEmpty()) Text(MatchView.top(match), Modifier.padding(bottom = Rings.SUB_GAP.dp), fontSize = Rings.LINE_SP.sp, lineHeight = (Rings.LINE_SP * Rings.LINE_H).sp, color = pal.legend, maxLines = 1)
            } else if (stored.isNotEmpty()) {
                val shown = guess ?: rooms[0].room
                Text(Rings.name(shown), fontSize = Rings.NAME_SP.sp, lineHeight = (Rings.NAME_SP * Rings.LINE_H).sp, fontWeight = FontWeight.Bold, color = pal.selected, maxLines = 1)
                Text("${((probability ?: rooms[0].p) * 100f).roundToInt()} % sicher", Modifier.padding(bottom = Rings.SUB_GAP.dp), fontSize = Rings.SUB_SP.sp, lineHeight = (Rings.SUB_SP * Rings.LINE_H).sp, color = pal.selected, maxLines = 1)
            } else Text("kein Modell", Modifier.padding(bottom = Rings.SUB_GAP.dp), fontSize = Rings.NAME_SP.sp, lineHeight = (Rings.NAME_SP * Rings.LINE_H).sp, fontWeight = FontWeight.Bold, color = pal.caption, maxLines = 1)
            (if (dDp < 200f) legend.take(Rings.LEGEND_MAX - 1) else legend).forEach { ap ->
                val line = buildAnnotatedString {
                    withStyle(SpanStyle(color = Color(colors[ap.bssid]?.color ?: ColorBook.OTHER))) { append("●") }
                    append(" " + Legend.text(names.getValue(ap.bssid), ap.rssi))
                }
                Text(line, fontSize = Rings.LINE_SP.sp, lineHeight = (Rings.LINE_SP * Rings.LINE_H).sp, color = pal.legend, maxLines = 1)
            }
        }
        Text(
            footer,
            Modifier.align(Alignment.Center).offset(y = ((Footer.Y - 0.5f) * dDp).dp),
            fontSize = Rings.LINE_SP.sp, color = pal.caption, maxLines = 1
        )
    }
}
