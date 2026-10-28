package de.aapphh.heatscan

import android.content.SharedPreferences
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
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
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.clipPath
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.wear.compose.foundation.lazy.ScalingLazyColumn
import androidx.wear.compose.material.Icon
import androidx.wear.compose.material.MaterialTheme
import androidx.wear.compose.material.Text
import kotlin.math.cos
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
        Box(Modifier.fillMaxSize()) {
            HorizontalPager(pager, Modifier.fillMaxSize()) { page ->
                if (page == 0) RoomsPage(version, store)
                else SettingsPage(version, store)
            }
            Row(Modifier.align(Alignment.BottomCenter).padding(bottom = 3.dp), horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                repeat(2) {
                    Box(Modifier.size(4.dp).clip(CircleShape).background(if (pager.currentPage == it) Color.White else Color.DarkGray))
                }
            }
        }
    }

    @Composable
    private fun RoomsPage(rev: Int, store: Store) {
        val (hot, confirmed) = Highlight.of(store.label, store.guess)
        BoxWithConstraints(Modifier.fillMaxSize()) {
            val dDp = minOf(maxWidth, maxHeight)
            val k = dDp.value / Quarters.REF
            Canvas(
                Modifier.fillMaxSize().pointerInput(Unit) {
                    detectTapGestures { o ->
                        val d = minOf(size.width, size.height).toFloat()
                        val room = Quarters.room(o.x - size.width / 2f, o.y - size.height / 2f, d)
                        store.label = Labels.toggle(store.label, room)
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
                    if (room == hot && confirmed) clipPath(path) { drawPath(path, Color.White, style = Stroke(ring)) }
                }
                repeat(4) {
                    val a = Math.toRadians(45.0 + it * 90.0)
                    val u = Offset(cos(a).toFloat(), sin(a).toFloat())
                    drawLine(Color.Black, c + u * (inner - 1f), c + u * (outer + 1f), gap)
                }
                val centre = Path().apply { addOval(Rect(c, Quarters.CENTER_R * d)) }
                drawPath(centre, if (Quarters.CENTER == hot) SELECTED else IDLE)
                if (Quarters.CENTER == hot && confirmed) clipPath(centre) { drawPath(centre, Color.White, style = Stroke(ring)) }
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
    private fun SettingsPage(rev: Int, store: Store) {
        val status = TileClick.statusLine(store.running, store.mode, store.status)
        ScalingLazyColumn(Modifier.fillMaxWidth()) {
            item { Text(status, fontSize = 11.sp) }
            item { Text("OK ${store.postsOk} / Fehler ${store.postsFailed}", fontSize = 10.sp) }
            item { Text(ScanStarter.extra(this@MainActivity).filter { it.first != "startError" }.joinToString(" ") { "${it.first}=${it.second}" }, fontSize = 9.sp) }
            item {
                Text(
                    if (store.paused) "Fortsetzen" else "Pausieren",
                    Modifier.border(1.dp, Color.Gray, RoundedCornerShape(8.dp)).clickable { ScanStarter.setPaused(this@MainActivity, !store.paused) }.padding(8.dp),
                    fontSize = 12.sp
                )
            }
            item { Field("Server", store.server) { store.server = it } }
            item { Field("Gerät", store.device) { store.device = it } }
            item { Field("Heim-WLANs (Komma)", store.home) { store.home = it } }
        }
    }

    @Composable
    private fun Field(title: String, value: String, onChange: (String) -> Unit) {
        var text by remember(value) { mutableStateOf(value) }
        Column {
            Text(title, fontSize = 11.sp)
            BasicTextField(
                value = text,
                onValueChange = {
                    text = it
                    onChange(it)
                },
                singleLine = true,
                textStyle = TextStyle(color = Color.White, fontSize = 12.sp),
                cursorBrush = SolidColor(Color.White),
                modifier = Modifier
                    .fillMaxWidth()
                    .border(1.dp, Color.Gray, RoundedCornerShape(8.dp))
                    .padding(8.dp)
            )
        }
    }

    companion object {
        private val SELECTED = Color(0xFFE8710A)
        private val IDLE = Color(0xFF2A2A2A)
        private val INK = Color(0xFF111111)
    }
}
