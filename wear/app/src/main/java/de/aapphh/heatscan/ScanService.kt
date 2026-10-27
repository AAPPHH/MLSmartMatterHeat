package de.aapphh.heatscan

import android.Manifest
import android.app.Activity
import android.app.AlarmManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.hardware.TriggerEvent
import android.hardware.TriggerEventListener
import android.location.LocationManager
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock
import android.util.Log
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.Executors

class ScanService : Service() {
    private val handler = Handler(Looper.getMainLooper())
    private val io = Executors.newSingleThreadExecutor()
    private val dedup = Dedup()
    private lateinit var store: Store
    private lateinit var wifi: WifiManager
    private lateinit var cm: ConnectivityManager
    private var wake: PowerManager.WakeLock? = null
    @Volatile private var network: Network? = null
    private lateinit var alarms: AlarmManager
    private lateinit var sensors: SensorManager
    private lateinit var policy: ScanPolicy
    private lateinit var alarmIntent: PendingIntent
    private var step: Sensor? = null
    private var wakeStep: Sensor? = null
    private var triggers: List<Sensor> = emptyList()
    private var sensorState = 0
    private var alarmSet = false
    private var active = false
    private var loggedMode: Mode? = null

    private val stepListener = object : SensorEventListener {
        override fun onSensorChanged(e: SensorEvent) {
            motion(e.timestamp / 1_000_000)
        }

        override fun onAccuracyChanged(s: Sensor?, a: Int) {}
    }

    private val triggerListener = object : TriggerEventListener() {
        override fun onTrigger(e: TriggerEvent) {
            sensorState = 0
            motion(e.timestamp / 1_000_000)
            applySensors()
        }
    }

    private val netCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(n: Network) {
            network = n
        }

        override fun onLost(n: Network) {
            if (network == n) network = null
        }
    }

    private val receiver = object : BroadcastReceiver() {
        override fun onReceive(c: Context, i: Intent) {
            collect()
        }
    }

    private val tick = Runnable { tick() }

    override fun onBind(i: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        store = Store(this)
        wifi = applicationContext.getSystemService(WifiManager::class.java)
        cm = getSystemService(ConnectivityManager::class.java)
        alarms = getSystemService(AlarmManager::class.java)
        sensors = getSystemService(SensorManager::class.java)
        alarmIntent = alarmIntent(this)
        instance = this
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (!active) {
            try {
                startForeground(1, notification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION)
            } catch (e: Exception) {
                Trace.w("start_refused", e, "source" to "service")
                store.status = "Start nicht erlaubt"
                store.lastError = "startForeground " + e.javaClass.simpleName
                store.running = false
                ScanStarter.nudge(this)
                stopSelf()
                return START_NOT_STICKY
            }
            active = true
            wake = getSystemService(PowerManager::class.java).newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "heatscan:scan").also { it.setReferenceCounted(false) }
            hold(false)
            try {
                cm.requestNetwork(NetworkRequest.Builder().addTransportType(NetworkCapabilities.TRANSPORT_WIFI).build(), netCallback)
            } catch (e: Exception) {
                Log.w(TAG, "requestNetwork failed, using default network", e)
            }
            registerReceiver(receiver, IntentFilter(WifiManager.SCAN_RESULTS_AVAILABLE_ACTION), RECEIVER_NOT_EXPORTED)
            store.running = true
            Trace.i("service_started")
            wakeStep = sensors.getDefaultSensor(Sensor.TYPE_STEP_DETECTOR, true)
            step = sensors.getDefaultSensor(Sensor.TYPE_STEP_DETECTOR) ?: wakeStep
            triggers = listOfNotNull(sensors.getDefaultSensor(Sensor.TYPE_MOTION_DETECT, true), sensors.getDefaultSensor(Sensor.TYPE_SIGNIFICANT_MOTION))
            val adaptive = step != null && (wakeStep != null || triggers.isNotEmpty()) &&
                checkSelfPermission(Manifest.permission.ACTIVITY_RECOGNITION) == PackageManager.PERMISSION_GRANTED
            policy = ScanPolicy(adaptive, SystemClock.elapsedRealtime())
            Trace.i("policy", "adaptive" to adaptive, "step" to (step != null), "wakeStep" to (wakeStep != null), "triggers" to triggers.size)
            tick()
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        handler.removeCallbacks(tick)
        if (active) {
            alarms.cancel(alarmIntent)
            releaseSensors()
            try { unregisterReceiver(receiver) } catch (e: Exception) { Log.w(TAG, "unregister", e) }
            try { cm.unregisterNetworkCallback(netCallback) } catch (e: Exception) { Log.w(TAG, "unregister net", e) }
            wake?.let { if (it.isHeld) it.release() }
        }
        if (active) Trace.i("service_stopped", "paused" to paused)
        active = false
        if (instance === this) instance = null
        store.mode = ""
        store.running = false
        io.shutdown()
        super.onDestroy()
    }

    private fun hold(timed: Boolean) {
        wake?.let { if (timed) it.acquire(ScanConfig.SCAN_HOLD_MS) else it.acquire() }
    }

    fun onAlarm() {
        if (!active) {
            alarms.cancel(alarmIntent)
            stopSelf()
            return
        }
        hold(true)
        tick()
    }

    private fun motion(at: Long) {
        val now = SystemClock.elapsedRealtime()
        val before = policy.mode
        val scanDue = policy.onMotion(now, at)
        if (scanDue) scan()
        if (scanDue || policy.mode != before) schedule(now)
    }

    private fun tick() {
        val now = SystemClock.elapsedRealtime()
        if (policy.onTick(now)) scan()
        schedule(now)
    }

    private fun schedule(now: Long) {
        handler.removeCallbacks(tick)
        val delay = policy.delay(now)
        if (loggedMode != policy.mode) {
            loggedMode = policy.mode
            Trace.i("mode", "mode" to policy.mode.name, "label" to policy.mode.label, "nextInMs" to delay)
        }
        if (store.mode != policy.mode.label) store.mode = policy.mode.label
        applySensors()
        if (policy.mode.sleeps) {
            hold(true)
            alarms.setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, now + delay, alarmIntent)
            alarmSet = true
        } else {
            if (alarmSet) {
                alarms.cancel(alarmIntent)
                alarmSet = false
            }
            hold(false)
            handler.postDelayed(tick, delay)
        }
    }

    private fun releaseSensors() {
        sensors.unregisterListener(stepListener)
        triggers.forEach { sensors.cancelTriggerSensor(triggerListener, it) }
    }

    private fun applySensors() {
        val want = when (policy.mode) {
            Mode.STILL -> 1
            Mode.MOVING, Mode.SETTLING -> 2
            else -> 0
        }
        if (want == sensorState) return
        sensorState = want
        releaseSensors()
        when (want) {
            1 -> {
                (wakeStep ?: step)?.let { sensors.registerListener(stepListener, it, SensorManager.SENSOR_DELAY_NORMAL) }
                triggers.forEach { sensors.requestTriggerSensor(triggerListener, it) }
            }
            2 -> step?.let { sensors.registerListener(stepListener, it, SensorManager.SENSOR_DELAY_NORMAL) }
        }
    }

    private fun scan() {
        try {
            val ok = wifi.startScan()
            Trace.i("scan_requested", "accepted" to ok)
            if (!ok) collect()
        } catch (e: Exception) {
            Trace.w("scan_requested", e, "accepted" to false)
            collect()
        }
    }

    private fun collect() {
        try {
            val results = wifi.scanResults
            if (results.isEmpty()) {
                val lm = getSystemService(LocationManager::class.java)
                val cause = Diag.emptyScan(lm.isLocationEnabled, wifi.isWifiEnabled)
                Trace.i("scan_result", "count" to 0, "cause" to cause)
                store.status = stamp() + " " + cause
                return
            }
            if (!dedup.accept(results.maxOf { it.timestamp })) {
                Trace.i("scan_result", "count" to results.size, "dedup" to true)
                return
            }
            val home = Home.isHome(results.map { it.SSID }, store.homeList)
            Trace.i("scan_result", "count" to results.size, "home" to home)
            if (store.atHome != home) Trace.i("home_change", "home" to home)
            store.atHome = home
            val before = policy.mode
            val now = SystemClock.elapsedRealtime()
            policy.onScan(home, now)
            if (policy.mode != before) schedule(now)
            if (!home) {
                store.status = stamp() + " Unterwegs, kein POST"
                store.lastPost = store.status
                return
            }
            val body = Payload.build(
                System.currentTimeMillis(), store.device, store.label,
                results.map { Ap(it.BSSID, it.level, it.frequency) }
            )
            val server = store.server
            val net = network
            io.execute {
                val status = try {
                    Poster.post(server, body, net)
                } catch (e: Exception) {
                    Trace.w("post", e, "ok" to false)
                    store.lastError = e.toString()
                    Diag.postError(e.javaClass.simpleName, Diag.needsLocalNetwork(Build.VERSION.SDK_INT, checkSelfPermission(Diag.LOCAL_NETWORK) == PackageManager.PERMISSION_GRANTED))
                }
                val ok = Diag.postOk(status)
                if (ok) store.postsOk += 1 else store.postsFailed += 1
                if (!ok && !status.startsWith("ERR")) store.lastError = "HTTP $status"
                Trace.i("post", "result" to status, "ok" to ok, "label" to (body.opt("label")?.takeIf { it != org.json.JSONObject.NULL } ?: "-"), "okTotal" to store.postsOk, "failedTotal" to store.postsFailed)
                store.status = stamp() + " " + status
                store.lastPost = store.status
            }
        } catch (e: Exception) {
            Log.w(TAG, "collect failed", e)
        }
    }

    private fun stamp() = SimpleDateFormat("HH:mm:ss", Locale.US).format(Date())

    private fun notification(): Notification {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "HeatScan", NotificationManager.IMPORTANCE_LOW))
        return Notification.Builder(this, CHANNEL)
            .setContentTitle("HeatScan")
            .setContentText("WLAN-Scan aktiv")
            .setSmallIcon(R.drawable.ic_launcher)
            .setOngoing(true)
            .build()
    }

    companion object {
        const val ACTION_TICK = "de.aapphh.heatscan.TICK"
        private const val CHANNEL = "scan"
        @Volatile var running = false
        @Volatile var paused = false
        @Volatile var instance: ScanService? = null

        fun alarmIntent(c: Context): PendingIntent = PendingIntent.getBroadcast(
            c, 0, Intent(c, TickReceiver::class.java).setAction(ACTION_TICK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
    }
}

class TickReceiver : BroadcastReceiver() {
    override fun onReceive(c: Context, i: Intent) {
        val svc = ScanService.instance
        if (svc != null) svc.onAlarm() else c.getSystemService(AlarmManager::class.java).cancel(ScanService.alarmIntent(c))
    }
}

object ScanStarter {
    private const val NUDGE_CHANNEL = "nudge"

    fun missing(c: Context): List<String> = listOfNotNull(
        Manifest.permission.ACCESS_FINE_LOCATION, Manifest.permission.ACCESS_COARSE_LOCATION,
        Manifest.permission.POST_NOTIFICATIONS, Manifest.permission.ACTIVITY_RECOGNITION,
        Diag.LOCAL_NETWORK.takeIf { Build.VERSION.SDK_INT >= Diag.LOCAL_NETWORK_SDK }
    ).filter { c.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }

    fun start(c: Context, source: String, nudge: Boolean = false): Boolean {
        val store = Store(c)
        if (ScanService.paused) {
            Trace.i("start_skipped", "source" to source, "reason" to "paused")
            return false
        }
        if (ScanService.running) return true
        val issue = Diag.permissionIssue(
            c.checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION) == PackageManager.PERMISSION_GRANTED,
            c.checkSelfPermission(Manifest.permission.ACCESS_COARSE_LOCATION) == PackageManager.PERMISSION_GRANTED
        )
        if (issue != null) {
            store.status = issue
            store.lastError = issue
            Trace.i("start_skipped", "source" to source, "reason" to issue)
            if (nudge) nudge(c)
            return false
        }
        if (Diag.needsLocalNetwork(Build.VERSION.SDK_INT, c.checkSelfPermission(Diag.LOCAL_NETWORK) == PackageManager.PERMISSION_GRANTED)) store.status = "LAN-Zugriff fehlt"
        return try {
            c.startForegroundService(Intent(c, ScanService::class.java))
            Trace.i("start_requested", "source" to source)
            true
        } catch (e: Exception) {
            Trace.w("start_refused", e, "source" to source)
            store.lastError = "start($source) " + e.javaClass.simpleName
            if (nudge) nudge(c)
            false
        }
    }

    fun nudge(c: Context) {
        try {
            val nm = c.getSystemService(NotificationManager::class.java)
            nm.createNotificationChannel(NotificationChannel(NUDGE_CHANNEL, "HeatScan Start", NotificationManager.IMPORTANCE_DEFAULT))
            val tap = PendingIntent.getActivity(c, 1, Intent(c, StartActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
            nm.notify(2, Notification.Builder(c, NUDGE_CHANNEL)
                .setContentTitle("HeatScan")
                .setContentText("Scan aus, tippen zum Starten")
                .setSmallIcon(R.drawable.ic_launcher)
                .setContentIntent(tap)
                .setAutoCancel(true)
                .build())
            Trace.i("nudge_posted")
        } catch (e: Exception) {
            Trace.w("nudge_failed", e)
        }
    }
}

class StartActivity : Activity() {
    override fun onResume() {
        super.onResume()
        if (ScanStarter.missing(this).isEmpty()) ScanStarter.start(this, "trampoline")
        else startActivity(Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        finish()
    }
}

class BootReceiver : BroadcastReceiver() {
    override fun onReceive(c: Context, i: Intent) {
        Trace.i("boot", "action" to i.action)
        ScanStarter.start(c, "boot:" + i.action, true)
    }
}

class DebugReceiver : BroadcastReceiver() {
    override fun onReceive(c: Context, i: Intent) {
        val store = Store(c)
        when (i.action) {
            ACTION_CONFIG -> {
                i.getStringExtra("server")?.let { store.server = it }
                i.getStringExtra("home")?.let { store.home = it }
                i.getStringExtra("device")?.let { store.device = it }
                Trace.i("config", "server" to store.server, "home" to store.home, "device" to store.device)
            }
            ACTION_START -> {
                ScanService.paused = false
                ScanStarter.start(c, "adb")
            }
            ACTION_PAUSE -> {
                ScanService.paused = i.getBooleanExtra("on", true)
                if (ScanService.paused) ScanService.instance?.stopSelf()
                Trace.i("pause", "on" to ScanService.paused)
            }
        }
        val line = Summary.line(
            ScanService.running, ScanService.paused, store.mode, store.label, store.server, store.home, store.atHome,
            store.lastPost, store.postsOk, store.postsFailed, store.lastError
        )
        Log.i(TAG, line)
        if (isOrderedBroadcast) setResultData(line)
    }

    companion object {
        const val ACTION_STATUS = "de.aapphh.heatscan.STATUS"
        const val ACTION_START = "de.aapphh.heatscan.START"
        const val ACTION_CONFIG = "de.aapphh.heatscan.CONFIG"
        const val ACTION_PAUSE = "de.aapphh.heatscan.PAUSE"
    }
}
