package de.aapphh.heatscan

import android.content.SharedPreferences
import android.graphics.Paint
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableStateOf
import kotlinx.coroutines.delay
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.graphics.drawscope.clipPath
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.wear.compose.material.Icon
import androidx.wear.compose.material.MaterialTheme
import androidx.wear.compose.material.Text
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

    private fun done() {
        ScanService.instance?.refreshSensors()
        ScanStarter.start(this, "permissions")
    }
    private val prompt get() = intent.getBooleanExtra("prompt", false)
    private val learnPrompt get() = prompt && intent.getBooleanExtra("learn", false)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (prompt) {
            setShowWhenLocked(true)
            setTurnScreenOn(true)
        }
        val store = Store(this)
        intent.getStringExtra("server")?.let { store.server = it }
        intent.getStringExtra("device")?.let { store.device = it }
        intent.getStringExtra("home")?.let { store.home = it }
        setContent { MaterialTheme { Screen(store) } }
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
    }

    override fun onPause() {
        ScanService.setForeground(false)
        super.onPause()
    }

    @Composable
    private fun Screen(store: Store) {
        var version by remember { mutableIntStateOf(0) }
        DisposableEffect(Unit) {
            val l = SharedPreferences.OnSharedPreferenceChangeListener { _, _ -> version++ }
            store.sp.registerOnSharedPreferenceChangeListener(l)
            onDispose { store.sp.unregisterOnSharedPreferenceChangeListener(l) }
        }
        val pager = rememberPagerState { 2 }
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
        val progress = version.let { store.progress }
        val learnView = Learn.view(learning, progress)
        LaunchedEffect(version, tapped) {
            if (learnPrompt && tapped == null && choosing == null && store.learnPromptAt == 0L) finish()
        }
        val pick = { room: String, heat: Boolean ->
            choosing = null
            LabelRecorder.measure(this@MainActivity, room, heat) { if (prompt) Prompter.cancel(applicationContext) }
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
                    delay(2000)
                    finish()
                } else {
                    delay(Tap.SHOW_MS)
                    tapped = null
                }
            }
        }
        Box(Modifier.fillMaxSize()) {
            val hint = if (locked && view == null && state == Tap.THROTTLED) "Warte ${Tap.remainingS(until, now)} s" else null
            (hint ?: Prompt.title(intent.getBooleanExtra("change", false), learnPrompt).takeIf { prompt })?.let { Text(it, Modifier.align(Alignment.TopCenter).padding(top = 14.dp), fontSize = 10.sp, color = Color.White, maxLines = 1) }
            HorizontalPager(pager, Modifier.fillMaxSize()) { page ->
                key(version) {
                    if (page == 0) {
                        val (hot, ringed) = Highlight.of(store.stayLabel(), store.guess)
                        RoomsPage(hot, ringed, locked, learnView) {
                            if (choosing == null && !Tap.locked(store.manual, store.manualUntil, System.currentTimeMillis())) choosing = it
                        }
                    } else DataPage(
                        store.rooms, store.colors, store.lastAps, store.replyAps, store.guess, store.probability,
                        Footer.line(store.postHm, store.postOk, ScanQueue.get(this@MainActivity).size, store.paused, Footer.fresh(store.lastFreshAt), store.rejected), store.match
                    )
                }
            }
            if (!prompt && view == null && choosing == null && pager.currentPage == 0) {
                Text(
                    Learn.chip(learning, progress),
                    Modifier.align(Alignment.TopCenter).padding(top = 2.dp).clip(RoundedCornerShape(50)).background(if (learning) SELECTED else IDLE)
                        .clickable { ScanService.instance?.setLearning(!learning) }.padding(horizontal = 10.dp, vertical = 2.dp),
                    fontSize = 9.sp, maxLines = 1, color = if (learning) INK else Color.White
                )
            }
            choosing?.let { room ->
                Column(Modifier.fillMaxSize().background(Color.Black).padding(horizontal = 24.dp), Arrangement.Center, Alignment.CenterHorizontally) {
                    Text(Defaults.CAPTIONS.getValue(room), fontSize = 12.sp, color = Color.White, maxLines = 1)
                    Text(
                        Heat.YES,
                        Modifier.padding(top = 6.dp).fillMaxWidth().clip(RoundedCornerShape(50)).background(SELECTED).clickable { pick(room, true) }.padding(vertical = 12.dp),
                        fontSize = 18.sp, fontWeight = FontWeight.Bold, color = INK, textAlign = TextAlign.Center
                    )
                    Text(
                        Heat.NO,
                        Modifier.padding(top = 8.dp).clip(RoundedCornerShape(50)).background(IDLE).clickable { pick(room, false) }.padding(horizontal = 14.dp, vertical = 4.dp),
                        fontSize = 10.sp, color = Color.White, maxLines = 1
                    )
                }
            }
            view?.let {
                Column(Modifier.fillMaxSize().background(TapColors.of(it.kind)).padding(horizontal = 24.dp), Arrangement.Center, Alignment.CenterHorizontally) {
                    Text(it.title, fontSize = 20.sp, lineHeight = 24.sp, fontWeight = FontWeight.Bold, color = Color.White, textAlign = TextAlign.Center)
                    if (it.sub.isNotEmpty()) Text(it.sub, Modifier.padding(top = 6.dp), fontSize = 12.sp, color = Color.White, textAlign = TextAlign.Center)
                }
            }
            Row(Modifier.align(Alignment.BottomCenter).padding(bottom = 3.dp), horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                repeat(2) {
                    Box(Modifier.size(4.dp).clip(CircleShape).background(if (pager.currentPage == it) Color.White else Color.DarkGray))
                }
            }
        }
    }
}

@Composable
internal fun RoomsPage(hot: String?, ringed: String?, locked: Boolean = false, learn: LearnView? = null, onTap: (String) -> Unit) {
    val fill = { room: String -> if (room == hot) SELECTED else when (learn?.tones?.get(room)) { RoomTone.LOW -> LOW; RoomTone.DONE -> DONE; else -> IDLE } }
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
                if (room == ringed) clipPath(path) { drawPath(path, Color.White, style = Stroke(ring)) }
            }
            repeat(4) {
                val a = Math.toRadians(45.0 + it * 90.0)
                val u = Offset(cos(a).toFloat(), sin(a).toFloat())
                drawLine(Color.Black, c + u * (inner - 1f), c + u * (outer + 1f), gap)
            }
            val centre = Path().apply { addOval(Rect(c, Quarters.CENTER_R * d)) }
            drawPath(centre, fill(Quarters.CENTER))
            if (Quarters.CENTER == ringed) clipPath(centre) { drawPath(centre, Color.White, style = Stroke(ring)) }
            val pillTopLeft = Offset(c.x - Quarters.NONE_HALF_W * d, c.y + Quarters.NONE_TOP * d)
            val pillSize = Size(2f * Quarters.NONE_HALF_W * d, Quarters.NONE_H * d)
            val corner = CornerRadius(pillSize.height / 2f)
            drawRoundRect(Color.Black, pillTopLeft - Offset(gap, gap), Size(pillSize.width + 2f * gap, pillSize.height + 2f * gap), CornerRadius(corner.x + gap))
            drawRoundRect(if (Defaults.NONE == hot) SELECTED else IDLE, pillTopLeft, pillSize, corner)
            if (Defaults.NONE == ringed) drawRoundRect(Color.White, pillTopLeft, pillSize, corner, style = Stroke(2.dp.toPx()))
        }
        Text(
            Prompt.caption(Defaults.NONE, null),
            Modifier.align(Alignment.Center).offset(y = ((Quarters.NONE_TOP + Quarters.NONE_H / 2f) * dDp.value).dp),
            fontSize = 10.sp, lineHeight = 10.sp, maxLines = 1, color = if (Defaults.NONE == hot) INK else Color.White
        )
        (Quarters.WEDGES + Quarters.CENTER).forEach { room ->
            val isCenter = room == Quarters.CENTER
            val a = Math.toRadians(Quarters.mid(room) - 90.0)
            val r = if (isCenter) 0f else Quarters.labelR(room) * dDp.value
            val dim = locked || learn?.tones?.get(room) == RoomTone.DONE
            val ink = if (room == hot) INK else if (dim) CAPTION else Color.White
            Column(
                Modifier.align(Alignment.Center).offset((r * cos(a)).dp, (r * sin(a)).dp),
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
                drawArc(TRACK, Rings.START, Rings.SWEEP, false, topLeft, box, style = Stroke(stroke))
                var at = Rings.START
                Rings.segments(room, colors).forEach { seg ->
                    drawArc(Color(seg.color), at, seg.sweep, false, topLeft, box, style = Stroke(stroke, cap = StrokeCap.Butt))
                    at += seg.sweep
                }
                val a = Math.toRadians(Rings.START.toDouble())
                paint.color = (if (i == 0 && stored.isNotEmpty()) Color.White else CAPTION).toArgb()
                drawIntoCanvas {
                    it.nativeCanvas.drawText(Rings.caption(room.room), c.x + r * cos(a).toFloat() + Rings.LABEL_DX * d, c.y + r * sin(a).toFloat() + mid, paint)
                }
            }
        }
        Column(Modifier.align(Alignment.Center), horizontalAlignment = Alignment.CenterHorizontally) {
            if (match != null) {
                val tone = if (match.abstained) CAPTION else SELECTED
                Text(MatchView.title(match), fontSize = Rings.NAME_SP.sp, lineHeight = (Rings.NAME_SP * Rings.LINE_H).sp, fontWeight = FontWeight.Bold, color = tone, maxLines = 1)
                Text(MatchView.sub(match), fontSize = Rings.SUB_SP.sp, lineHeight = (Rings.SUB_SP * Rings.LINE_H).sp, color = tone, maxLines = 1)
                if (match.top.isNotEmpty()) Text(MatchView.top(match), Modifier.padding(bottom = Rings.SUB_GAP.dp), fontSize = Rings.LINE_SP.sp, lineHeight = (Rings.LINE_SP * Rings.LINE_H).sp, color = LEGEND, maxLines = 1)
            } else if (stored.isNotEmpty()) {
                val shown = guess ?: rooms[0].room
                Text(Rings.name(shown), fontSize = Rings.NAME_SP.sp, lineHeight = (Rings.NAME_SP * Rings.LINE_H).sp, fontWeight = FontWeight.Bold, color = SELECTED, maxLines = 1)
                Text("${((probability ?: rooms[0].p) * 100f).roundToInt()} % sicher", Modifier.padding(bottom = Rings.SUB_GAP.dp), fontSize = Rings.SUB_SP.sp, lineHeight = (Rings.SUB_SP * Rings.LINE_H).sp, color = SELECTED, maxLines = 1)
            } else Text("kein Modell", Modifier.padding(bottom = Rings.SUB_GAP.dp), fontSize = Rings.NAME_SP.sp, lineHeight = (Rings.NAME_SP * Rings.LINE_H).sp, fontWeight = FontWeight.Bold, color = CAPTION, maxLines = 1)
            legend.forEach { ap ->
                val line = buildAnnotatedString {
                    withStyle(SpanStyle(color = Color(colors[ap.bssid]?.color ?: ColorBook.OTHER))) { append("●") }
                    append(" " + Legend.text(names.getValue(ap.bssid), ap.rssi))
                }
                Text(line, fontSize = Rings.LINE_SP.sp, lineHeight = (Rings.LINE_SP * Rings.LINE_H).sp, color = LEGEND, maxLines = 1)
            }
        }
        Text(
            footer,
            Modifier.align(Alignment.Center).offset(y = ((Footer.Y - 0.5f) * dDp).dp),
            fontSize = Rings.LINE_SP.sp, color = CAPTION, maxLines = 1
        )
    }
}

private val SELECTED = Color(0xFFE8710A)
private val IDLE = Color(0xFF2A2A2A)
private val LOW = Color(0xFF5A3A12)
private val DONE = Color(0xFF181818)
private val INK = Color(0xFF111111)
private val TRACK = Color(0xFF262626)
private val CAPTION = Color(0xFFA8A8A8)
private val LEGEND = Color(0xFFD0D0D0)

private object TapColors {
    fun of(k: TapKind) = when (k) {
        TapKind.OK, TapKind.QUEUED -> Color(0xFF1B7F3B)
        TapKind.THROTTLED -> Color(0xFFB36B00)
        TapKind.FAILED -> Color(0xFFB3261E)
        TapKind.WAIT -> Color(0xFF202020)
    }
}
