package de.aapphh.heatscan

import android.Manifest
import android.content.Intent
import android.content.SharedPreferences
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
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
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.wear.compose.foundation.lazy.ScalingLazyColumn
import androidx.wear.compose.material.Chip
import androidx.wear.compose.material.ChipDefaults
import androidx.wear.compose.material.Icon
import androidx.wear.compose.material.MaterialTheme
import androidx.wear.compose.material.Text
import kotlin.math.cos
import kotlin.math.sin

class MainActivity : ComponentActivity() {
    private val needed: List<String> by lazy {
        listOfNotNull(
            Manifest.permission.ACCESS_FINE_LOCATION, Manifest.permission.ACCESS_COARSE_LOCATION,
            Manifest.permission.POST_NOTIFICATIONS, Manifest.permission.ACTIVITY_RECOGNITION,
            Diag.LOCAL_NETWORK.takeIf { Build.VERSION.SDK_INT >= Diag.LOCAL_NETWORK_SDK }
        )
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val store = Store(this)
        intent.getStringExtra("server")?.let { store.server = it }
        intent.getStringExtra("device")?.let { store.device = it }
        intent.getStringExtra("home")?.let { store.home = it }
        setContent { MaterialTheme { Screen(store) } }
    }

    private fun granted(p: String) = checkSelfPermission(p) == PackageManager.PERMISSION_GRANTED

    private fun launchScan(store: Store, perms: ActivityResultLauncher<Array<String>>) {
        store.stopped = false
        val missing = needed.filter { !granted(it) }
        if (missing.isEmpty()) beginScan(store) else perms.launch(missing.toTypedArray())
    }

    private fun beginScan(store: Store) {
        val issue = Diag.permissionIssue(granted(Manifest.permission.ACCESS_FINE_LOCATION), granted(Manifest.permission.ACCESS_COARSE_LOCATION))
        if (issue != null) {
            store.status = issue
            return
        }
        if (Diag.needsLocalNetwork(Build.VERSION.SDK_INT, granted(Diag.LOCAL_NETWORK))) store.status = "LAN-Zugriff fehlt"
        if (!store.running) startForegroundService(Intent(this, ScanService::class.java))
    }

    private fun stopScan(store: Store) {
        store.stopped = true
        stopService(Intent(this, ScanService::class.java))
        store.running = false
    }

    @Composable
    private fun Screen(store: Store) {
        var version by remember { mutableIntStateOf(0) }
        DisposableEffect(Unit) {
            val l = SharedPreferences.OnSharedPreferenceChangeListener { _, _ -> version++ }
            store.sp.registerOnSharedPreferenceChangeListener(l)
            onDispose { store.sp.unregisterOnSharedPreferenceChangeListener(l) }
        }
        val perms = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { beginScan(store) }
        LaunchedEffect(Unit) {
            if (!store.running && !store.stopped) launchScan(store, perms)
        }
        val pager = rememberPagerState { 2 }
        Box(Modifier.fillMaxSize()) {
            HorizontalPager(pager, Modifier.fillMaxSize()) { page ->
                if (page == 0) RoomsPage(version, store, store.label, TileClick.statusLine(store.running, store.mode, store.status))
                else SettingsPage(version, store, { if (store.running) stopScan(store) else launchScan(store, perms) })
            }
            Row(Modifier.align(Alignment.BottomCenter).padding(bottom = 3.dp), horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                repeat(2) {
                    Box(Modifier.size(4.dp).clip(CircleShape).background(if (pager.currentPage == it) Color.White else Color.DarkGray))
                }
            }
        }
    }

    @Composable
    private fun RoomsPage(rev: Int, store: Store, label: String?, center: String) {
        BoxWithConstraints(Modifier.fillMaxSize()) {
            val size = 46.dp
            val radius = minOf(maxWidth, maxHeight) / 2 - size / 2 - 3.dp
            Defaults.ROOMS.forEachIndexed { i, room ->
                val a = Math.toRadians(-90.0 + i * 72.0)
                RoomButton(
                    room, room == label, size,
                    Modifier.align(Alignment.Center).offset((radius.value * cos(a)).dp, (radius.value * sin(a)).dp)
                ) { store.label = Labels.toggle(store.label, room) }
            }
            Text(
                center, Modifier.align(Alignment.Center).width(radius), fontSize = 9.sp, textAlign = TextAlign.Center,
                maxLines = 4, overflow = TextOverflow.Ellipsis
            )
        }
    }

    @Composable
    private fun RoomButton(room: String, selected: Boolean, size: androidx.compose.ui.unit.Dp, modifier: Modifier, onClick: () -> Unit) {
        Column(
            modifier.size(size).clip(CircleShape).background(if (selected) SELECTED else IDLE).clickable(onClick = onClick),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center
        ) {
            Icon(painterResource(RoomIcons.RES.getValue(room)), room, Modifier.size(22.dp), tint = Color.White)
            Text(Defaults.CAPTIONS.getValue(room), fontSize = 8.sp, maxLines = 1, color = Color.White)
        }
    }

    @Composable
    private fun SettingsPage(rev: Int, store: Store, toggle: () -> Unit) {
        ScalingLazyColumn(Modifier.fillMaxWidth()) {
            item {
                Chip(
                    onClick = toggle,
                    label = { Text(if (store.running) "Stop" else "Start") },
                    secondaryLabel = { Text("Label: ${store.label ?: "kein Label"}") },
                    colors = if (store.running) ChipDefaults.secondaryChipColors() else ChipDefaults.primaryChipColors(),
                    modifier = Modifier.fillMaxWidth()
                )
            }
            item { Text(TileClick.statusLine(store.running, store.mode, store.status), fontSize = 11.sp) }
            item { Field("Server", store.server) { store.server = it } }
            item { Field("Gerät", store.device) { store.device = it } }
            item { Field("Heim-WLANs (Komma)", store.home) { store.home = it } }
        }
    }

    @Composable
    private fun Field(title: String, value: String, onChange: (String) -> Unit) {
        var text by remember(value) { mutableStateOf(value) }
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

    companion object {
        private val SELECTED = Color(0xFFE8710A)
        private val IDLE = Color(0xFF2A2A2A)
    }
}
