package de.aapphh.heatscan

import android.Manifest
import android.content.Intent
import android.content.SharedPreferences
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.wear.compose.foundation.lazy.ScalingLazyColumn
import androidx.wear.compose.material.Chip
import androidx.wear.compose.material.ChipDefaults
import androidx.wear.compose.material.MaterialTheme
import androidx.wear.compose.material.Text
import androidx.wear.compose.material.TimeText
import androidx.wear.compose.material.Scaffold

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val store = Store(this)
        intent.getStringExtra("server")?.let { store.server = it }
        intent.getStringExtra("device")?.let { store.device = it }
        setContent { MaterialTheme { Screen(store) } }
    }

    private fun toggle(store: Store) {
        val svc = Intent(this, ScanService::class.java)
        if (store.running) {
            stopService(svc)
            store.running = false
        } else {
            startForegroundService(svc)
        }
    }

    @Composable
    private fun Screen(store: Store) {
        var version by remember { mutableIntStateOf(0) }
        DisposableEffect(Unit) {
            val l = SharedPreferences.OnSharedPreferenceChangeListener { _, _ -> version++ }
            store.sp.registerOnSharedPreferenceChangeListener(l)
            onDispose { store.sp.unregisterOnSharedPreferenceChangeListener(l) }
        }
        val perms = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { granted ->
            val issue = Diag.permissionIssue(granted[Manifest.permission.ACCESS_FINE_LOCATION] == true, granted[Manifest.permission.ACCESS_COARSE_LOCATION] == true)
            if (issue != null) store.status = issue
            else {
                if (Diag.needsLocalNetwork(Build.VERSION.SDK_INT, granted[Diag.LOCAL_NETWORK] == true)) store.status = "LAN-Zugriff fehlt"
                toggle(store)
            }
        }
        val v = version
        val label = store.label
        Scaffold(timeText = { TimeText() }) {
            ScalingLazyColumn(Modifier.fillMaxWidth()) {
                item {
                    Chip(
                        onClick = {
                            if (store.running) toggle(store)
                            else perms.launch(arrayOf(Manifest.permission.ACCESS_FINE_LOCATION, Manifest.permission.ACCESS_COARSE_LOCATION, Manifest.permission.POST_NOTIFICATIONS, Manifest.permission.ACTIVITY_RECOGNITION, Diag.LOCAL_NETWORK))
                        },
                        label = { Text(if (store.running) "Stop" else "Start") },
                        secondaryLabel = { Text("Label: ${label ?: "kein Label"}") },
                        colors = if (store.running) ChipDefaults.secondaryChipColors() else ChipDefaults.primaryChipColors(),
                        modifier = Modifier.fillMaxWidth()
                    )
                }
                item { Text((if (store.running && store.mode.isNotEmpty()) store.mode + " | " else "") + store.status, fontSize = 11.sp) }
                (Defaults.ROOMS + null).forEach { room ->
                    item {
                        Chip(
                            onClick = { store.label = room },
                            label = { Text(room ?: "kein Label") },
                            colors = if (room == label) ChipDefaults.primaryChipColors() else ChipDefaults.secondaryChipColors(),
                            modifier = Modifier.fillMaxWidth()
                        )
                    }
                }
                item { Field("Server", store.server) { store.server = it } }
                item { Field("Gerät", store.device) { store.device = it } }
            }
        }
    }

    @Composable
    private fun Field(title: String, value: String, onChange: (String) -> Unit) {
        var text by remember(value) { androidx.compose.runtime.mutableStateOf(value) }
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
