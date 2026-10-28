package de.aapphh.heatscan

import android.content.SharedPreferences
import android.graphics.Paint
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
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
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.ui.draw.clip
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
    private val perms = registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { ScanStarter.start(this, "permissions") }
    private var asked = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
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
        } else ScanStarter.start(this, "resume")
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
        var saved by remember { mutableStateOf<String?>(null) }
        LaunchedEffect(saved) {
            if (saved != null) {
                delay(Prompt.DONE_TIMEOUT_MS)
                saved = null
            }
        }
        Box(Modifier.fillMaxSize()) {
            saved?.let { Text(it, Modifier.align(Alignment.TopCenter).padding(top = 14.dp), fontSize = 10.sp, color = Color.White, maxLines = 1) }
            HorizontalPager(pager, Modifier.fillMaxSize()) { page ->
                key(version) {
                    if (page == 0) {
                        val (hot, ringed) = Highlight.of(store.stayLabel(), store.guess)
                        RoomsPage(hot, ringed) {
                            LabelRecorder.async(this@MainActivity, it)
                            saved = Prompt.done(it)
                        }
                    } else DataPage(
                        store.rooms, store.colors, store.lastAps, store.replyAps, store.guess, store.probability,
                        Footer.line(store.postHm, store.postOk, ScanQueue.get(this@MainActivity).size, store.paused, Footer.fresh(store.lastFreshAt), store.rejected)
                    )
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
internal fun RoomsPage(hot: String?, ringed: String?, onTap: (String) -> Unit) {
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
                drawPath(path, if (room == hot) SELECTED else IDLE)
                if (room == ringed) clipPath(path) { drawPath(path, Color.White, style = Stroke(ring)) }
            }
            repeat(4) {
                val a = Math.toRadians(45.0 + it * 90.0)
                val u = Offset(cos(a).toFloat(), sin(a).toFloat())
                drawLine(Color.Black, c + u * (inner - 1f), c + u * (outer + 1f), gap)
            }
            val centre = Path().apply { addOval(Rect(c, Quarters.CENTER_R * d)) }
            drawPath(centre, if (Quarters.CENTER == hot) SELECTED else IDLE)
            if (Quarters.CENTER == ringed) clipPath(centre) { drawPath(centre, Color.White, style = Stroke(ring)) }
        }
        (Quarters.WEDGES + Quarters.CENTER).forEach { room ->
            val isCenter = room == Quarters.CENTER
            val a = Math.toRadians(Quarters.mid(room) - 90.0)
            val r = if (isCenter) 0f else Quarters.LABEL_R * dDp.value
            val ink = if (room == hot) INK else Color.White
            Column(
                Modifier.align(Alignment.Center).offset((r * cos(a)).dp, (r * sin(a)).dp),
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                Icon(painterResource(RoomIcons.RES.getValue(room)), room, Modifier.size((if (isCenter) 28 else 24).dp), tint = ink)
                Text(Defaults.CAPTIONS.getValue(room), fontSize = (if (isCenter) 11 else 10).sp, maxLines = 1, color = ink)
            }
        }
    }
}

@Composable
internal fun DataPage(stored: List<RoomP>, colors: Map<String, Slot>, named: List<NamedAp>, replyAps: List<Ap>, guess: String?, probability: Float?, footer: String) {
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
            if (stored.isNotEmpty()) {
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
private val INK = Color(0xFF111111)
private val TRACK = Color(0xFF262626)
private val CAPTION = Color(0xFFA8A8A8)
private val LEGEND = Color(0xFFD0D0D0)
