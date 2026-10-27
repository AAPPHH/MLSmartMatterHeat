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
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.atomic.AtomicBoolean

class ScanService : Service() {
    private val handler = Handler(Looper.getMainLooper())
    private val io = Executors.newSingleThreadExecutor()
    private val dedup = Dedup()
    private lateinit var store: Store
    private lateinit var wifi: WifiManager
    private lateinit var cm: ConnectivityManager
    private lateinit var power: PowerManager
    private lateinit var queue: ScanQueue
    private var wake: PowerManager.WakeLock? = null
    private var flushWake: PowerManager.WakeLock? = null
    @Volatile private var network: Network? = null
    private lateinit var alarms: AlarmManager
    private lateinit var sensors: SensorManager
    private lateinit var policy: ScanPolicy
    private lateinit var alarmIntent: PendingIntent
    private var step: Sensor? = null
    private var wakeStep: Sensor? = null
    private var offBodySensor: Sensor? = null
    private var triggers: List<Sensor> = emptyList()
    private var sensorState = 0
    private var alarmSet = false
    private var active = false
    private var beatPending = false
    private var loggedMode: Mode? = null
    private val flusher = Flusher()
    private val flushPending = AtomicBoolean(false)
    @Volatile private var offBody: Boolean? = null
    @Volatile private var lastException = ""
    @Volatile private var lastRoom = ""

    private val stepListener = object : SensorEventListener {
        override fun onSensorChanged(e: SensorEvent) {
            motion(e.timestamp / 1_000_000)
        }

        override fun onAccuracyChanged(s: Sensor?, a: Int) {}
    }

    private val offBodyListener = object : SensorEventListener {
        override fun onSensorChanged(e: SensorEvent) {
            val off = e.values[0] == 0f
            if (offBody != off) Trace.i("offbody", "off" to off)
            offBody = off
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
            Trace.i("net_available", "queue" to queue.size)
            flush("net")
        }

        override fun onLost(n: Network) {
            if (network == n) network = null
            Trace.i("net_lost")
        }
    }

    private val receiver = object : BroadcastReceiver() {
        override fun onReceive(c: Context, i: Intent) {
            when (i.action) {
                WifiManager.SCAN_RESULTS_AVAILABLE_ACTION -> collect()
                Intent.ACTION_SCREEN_ON -> {
                    Trace.i("screen_on", "queue" to queue.size)
                    flush("screen_on")
                }
                Intent.ACTION_POWER_CONNECTED -> {
                    Trace.i("charger", "queue" to queue.size)
                    flush("charger")
                }
                PowerManager.ACTION_DEVICE_IDLE_MODE_CHANGED, PowerManager.ACTION_POWER_SAVE_MODE_CHANGED -> {
                    Trace.i("power_change", "action" to i.action, "doze" to power.isDeviceIdleMode, "saver" to power.isPowerSaveMode, "power" to powerState(), "queue" to queue.size)
                    if (!power.isDeviceIdleMode) flush("idle_exit")
                }
            }
        }
    }

    private val tick = Runnable { tick() }

    private val beatTimeout = Runnable {
        if (beatPending) {
            beatPending = false
            enqueue(System.currentTimeMillis(), emptyList(), policy.away, "beat")
        }
    }

    override fun onBind(i: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        store = Store(this)
        wifi = applicationContext.getSystemService(WifiManager::class.java)
        cm = getSystemService(ConnectivityManager::class.java)
        alarms = getSystemService(AlarmManager::class.java)
        sensors = getSystemService(SensorManager::class.java)
        power = getSystemService(PowerManager::class.java)
        queue = ScanQueue.get(this)
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
                store.startError = "startForeground " + e.javaClass.simpleName
                store.running = false
                if (intent == null || intent.getBooleanExtra(ScanStarter.EXTRA_NUDGE, false)) ScanStarter.nudge(this)
                stopSelf()
                return START_NOT_STICKY
            }
            active = true
            wake = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "heatscan:scan").also { it.setReferenceCounted(false) }
            flushWake = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "heatscan:flush").also { it.setReferenceCounted(false) }
            hold(false)
            try {
                cm.requestNetwork(NetworkRequest.Builder().addTransportType(NetworkCapabilities.TRANSPORT_WIFI).build(), netCallback)
                Trace.i("net_request", "ok" to true)
            } catch (e: Exception) {
                Trace.w("net_request", e, "ok" to false)
            }
            wakeStep = sensors.getDefaultSensor(Sensor.TYPE_STEP_DETECTOR, true)
            step = sensors.getDefaultSensor(Sensor.TYPE_STEP_DETECTOR) ?: wakeStep
            triggers = listOfNotNull(sensors.getDefaultSensor(Sensor.TYPE_MOTION_DETECT, true), sensors.getDefaultSensor(Sensor.TYPE_SIGNIFICANT_MOTION))
            val adaptive = step != null && (wakeStep != null || triggers.isNotEmpty()) &&
                checkSelfPermission(Manifest.permission.ACTIVITY_RECOGNITION) == PackageManager.PERMISSION_GRANTED
            policy = ScanPolicy(adaptive, SystemClock.elapsedRealtime())
            offBodySensor = sensors.getDefaultSensor(Sensor.TYPE_LOW_LATENCY_OFFBODY_DETECT, true)
            try {
                offBodySensor?.let { sensors.registerListener(offBodyListener, it, SensorManager.SENSOR_DELAY_NORMAL) }
            } catch (e: Exception) {
                Trace.w("offbody_register", e)
                offBodySensor = null
            }
            registerReceiver(receiver, IntentFilter().apply {
                addAction(WifiManager.SCAN_RESULTS_AVAILABLE_ACTION)
                addAction(Intent.ACTION_SCREEN_ON)
                addAction(Intent.ACTION_POWER_CONNECTED)
                addAction(PowerManager.ACTION_DEVICE_IDLE_MODE_CHANGED)
                addAction(PowerManager.ACTION_POWER_SAVE_MODE_CHANGED)
            }, RECEIVER_NOT_EXPORTED)
            store.running = true
            Trace.i("service_started", "queue" to queue.size, "doze" to power.isDeviceIdleMode, "saver" to power.isPowerSaveMode, "bgLoc" to ScanStarter.bgLocation(this), "whitelist" to power.isIgnoringBatteryOptimizations(packageName))
            Trace.i("policy", "adaptive" to adaptive, "step" to (step != null), "wakeStep" to (wakeStep != null), "triggers" to triggers.size, "offBodySensor" to (offBodySensor != null))
            tick()
            flush("start")
        }
        return START_STICKY
    }

    override fun onDestroy() {
        handler.removeCallbacks(tick)
        handler.removeCallbacks(beatTimeout)
        if (active) {
            alarms.cancel(alarmIntent)
            releaseSensors()
            sensors.unregisterListener(offBodyListener)
            try { unregisterReceiver(receiver) } catch (e: Exception) { Log.w(TAG, "unregister", e) }
            try { cm.unregisterNetworkCallback(netCallback) } catch (e: Exception) { Log.w(TAG, "unregister net", e) }
            wake?.let { if (it.isHeld) it.release() }
            flushWake?.let { if (it.isHeld) it.release() }
        }
        if (active) Trace.i("service_stopped", "paused" to store.paused, "queue" to queue.size)
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
        Trace.i("alarm", "doze" to power.isDeviceIdleMode, "saver" to power.isPowerSaveMode, "queue" to queue.size)
        tick()
        flush("alarm")
    }

    private fun powerState() = PowerState.of(power.isPowerSaveMode, power.isDeviceIdleMode, offBody)

    private fun motion(at: Long) {
        val now = SystemClock.elapsedRealtime()
        val before = policy.mode
        val scanDue = policy.onMotion(now, at)
        if (scanDue) scan()
        if (scanDue || policy.mode != before) schedule(now)
    }

    private fun tick() {
        val now = SystemClock.elapsedRealtime()
        if (policy.onTick(now)) scan(policy.mode.sleeps)
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
            val exact = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && alarms.canScheduleExactAlarms()
            if (exact) alarms.setExactAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, now + delay, alarmIntent)
            else alarms.setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, now + delay, alarmIntent)
            Trace.i("alarm_set", "exact" to exact, "inMs" to delay)
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

    private fun scan(beat: Boolean = false) {
        if (beat) {
            beatPending = true
            handler.removeCallbacks(beatTimeout)
            handler.postDelayed(beatTimeout, ScanConfig.BEAT_WAIT_MS)
        }
        try {
            val ok = wifi.startScan()
            Trace.i("scan_requested", "accepted" to ok, "beat" to beat)
            if (!ok) collect()
        } catch (e: Exception) {
            Trace.w("scan_requested", e, "accepted" to false, "beat" to beat)
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
            val newest = results.maxOf { it.timestamp }
            if (!dedup.accept(newest)) {
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
            beatPending = false
            handler.removeCallbacks(beatTimeout)
            val ts = Capture.wallMs(System.currentTimeMillis(), now, newest)
            enqueue(ts, if (home) results.map { Ap(it.BSSID, it.level, it.frequency) } else emptyList(), policy.away, if (home) "scan" else "away")
        } catch (e: Exception) {
            Log.w(TAG, "collect failed", e)
        }
    }

    private fun enqueue(ts: Long, aps: List<Ap>, away: Boolean, kind: String) {
        val state = powerState()
        val motion = policy.mode.motion
        val label = store.label
        try {
            io.execute {
                val body = Payload.build(ts, store.device, label, aps, motion, state, away, store.nextId())
                val dropped = queue.add(body, System.currentTimeMillis())
                Trace.i("enqueue", "kind" to kind, "aps" to aps.size, "queue" to queue.size, "dropped" to dropped, "motion" to motion, "power" to state, "away" to away, "label" to (label ?: "-"))
                store.status = stamp() + " " + kind + " q" + queue.size
                flush(kind)
            }
        } catch (e: RejectedExecutionException) {
            Log.w(TAG, "enqueue rejected", e)
        }
    }

    fun flush(reason: String) {
        if (queue.size == 0 || !flushPending.compareAndSet(false, true)) return
        try {
            io.execute {
                flushPending.set(false)
                runFlush(reason)
            }
        } catch (e: RejectedExecutionException) {
            flushPending.set(false)
        }
    }

    private fun send(server: String, path: String, body: String, net: Network?): Int = try {
        val reply = Poster.send(server, path, body, net)
        if (reply.code in 200..299) Poster.room(reply.text).let { if (it.isNotEmpty()) lastRoom = it }
        reply.code
    } catch (e: Exception) {
        lastException = e.javaClass.simpleName
        store.lastError = e.toString()
        Trace.w("post", e, "path" to path)
        -1
    }

    private fun runFlush(reason: String) {
        val server = store.server
        val net = network
        val before = queue.size
        if (before == 0) return
        val lock = flushWake?.also { it.acquire(ScanConfig.FLUSH_HOLD_MS) }
        val r = try {
            flusher.run(
                queue, System.currentTimeMillis(),
                { a -> send(server, "/api/scans", JSONObject().put("scans", a).toString(), net) },
                { e -> send(server, "/api/scan", e.toString(), net) }
            )
        } finally {
            lock?.let { if (it.isHeld) it.release() }
        }
        val ok = r.code in 200..299
        val status = when {
            ok -> "${r.code} ${r.sent} sent" + (if (lastRoom.isNotEmpty()) " $lastRoom" else "")
            r.code == -1 -> Diag.postError(lastException, Diag.needsLocalNetwork(Build.VERSION.SDK_INT, checkSelfPermission(Diag.LOCAL_NETWORK) == PackageManager.PERMISSION_GRANTED))
            else -> "${r.code} rejected"
        }
        store.postsOk += r.sent
        if (!ok) {
            store.postsFailed += 1
            if (r.code != -1) store.lastError = "HTTP ${r.code}"
        }
        Trace.i("flush", "reason" to reason, "batch" to !flusher.singleOnly, "code" to r.code, "sent" to r.sent, "queueBefore" to before, "queue" to queue.size, "okTotal" to store.postsOk, "failedTotal" to store.postsFailed)
        store.status = stamp() + " " + status + " q" + queue.size
        store.lastPost = store.status
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
        if (svc != null) svc.onAlarm()
        else if (Store(c).paused) c.getSystemService(AlarmManager::class.java).cancel(ScanService.alarmIntent(c))
        else ScanStarter.start(c, "alarm", true)
    }
}

object ScanStarter {
    private const val NUDGE_CHANNEL = "nudge"
    const val EXTRA_NUDGE = "nudge"
    private val VISIBLE = setOf("resume", "permissions", "trampoline", "pause_button")

    fun missing(c: Context): List<String> = listOfNotNull(
        Manifest.permission.ACCESS_FINE_LOCATION, Manifest.permission.ACCESS_COARSE_LOCATION,
        Manifest.permission.POST_NOTIFICATIONS, Manifest.permission.ACTIVITY_RECOGNITION,
        Diag.LOCAL_NETWORK.takeIf { Build.VERSION.SDK_INT >= Diag.LOCAL_NETWORK_SDK }
    ).filter { c.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }

    fun bgLocation(c: Context): Boolean = c.checkSelfPermission(Manifest.permission.ACCESS_BACKGROUND_LOCATION) == PackageManager.PERMISSION_GRANTED

    fun exempt(c: Context, source: String): Boolean = source in VISIBLE || source.startsWith("boot:") ||
        (source == "alarm" && Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && c.getSystemService(AlarmManager::class.java).canScheduleExactAlarms()) ||
        c.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(c.packageName)

    fun extra(c: Context): List<Pair<String, Any?>> = listOf(
        "startError" to Store(c).startError,
        "queue" to ScanQueue.get(c).size,
        "bgLoc" to bgLocation(c),
        "idleWhitelist" to c.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(c.packageName),
        "exactAlarm" to (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && c.getSystemService(AlarmManager::class.java).canScheduleExactAlarms()),
        "doze" to c.getSystemService(PowerManager::class.java).isDeviceIdleMode,
        "saver" to c.getSystemService(PowerManager::class.java).isPowerSaveMode
    )

    fun setPaused(c: Context, on: Boolean) {
        Store(c).paused = on
        if (on) ScanService.instance?.stopSelf() else start(c, "pause_button")
        Trace.i("pause", "on" to on)
    }

    fun start(c: Context, source: String, nudge: Boolean = false): Boolean {
        val store = Store(c)
        if (store.paused) {
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
            store.startError = issue
            Trace.i("start_skipped", "source" to source, "reason" to issue)
            if (nudge) nudge(c)
            return false
        }
        if (!exempt(c, source)) {
            store.startError = "Hintergrundstart nicht erlaubt: Akku-Optimierung an, kein Boot/Alarm ($source)"
            Trace.i("start_skipped", "source" to source, "reason" to "no_bg_exemption")
            if (nudge) nudge(c)
            return false
        }
        if (Diag.needsLocalNetwork(Build.VERSION.SDK_INT, c.checkSelfPermission(Diag.LOCAL_NETWORK) == PackageManager.PERMISSION_GRANTED)) store.status = "LAN-Zugriff fehlt"
        return try {
            c.startForegroundService(Intent(c, ScanService::class.java).putExtra(EXTRA_NUDGE, nudge))
            Trace.i("start_requested", "source" to source)
            true
        } catch (e: Exception) {
            Trace.w("start_refused", e, "source" to source)
            store.startError = "start($source) " + e.javaClass.simpleName
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
        if (Store(this).paused) {
            finish()
            return
        }
        val fine = checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION) == PackageManager.PERMISSION_GRANTED
        val coarse = checkSelfPermission(Manifest.permission.ACCESS_COARSE_LOCATION) == PackageManager.PERMISSION_GRANTED
        if (Diag.permissionIssue(fine, coarse) == null) ScanStarter.start(this, "trampoline")
        else startActivity(Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        finish()
    }
}

class BootReceiver : BroadcastReceiver() {
    override fun onReceive(c: Context, i: Intent) {
        if (i.action != Intent.ACTION_BOOT_COMPLETED && i.action != Intent.ACTION_MY_PACKAGE_REPLACED) return
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
                store.paused = false
                ScanStarter.start(c, "adb")
            }
            ACTION_PAUSE -> ScanStarter.setPaused(c, i.getBooleanExtra("on", true))
            ACTION_FLUSH -> ScanService.instance?.flush("adb")
        }
        val line = Summary.line(
            ScanService.running, store.paused, store.mode, store.label, store.server, store.home, store.atHome,
            store.lastPost, store.postsOk, store.postsFailed, store.lastError, ScanStarter.extra(c)
        )
        Log.i(TAG, line)
        if (isOrderedBroadcast) setResultData(line)
    }

    companion object {
        const val ACTION_STATUS = "de.aapphh.heatscan.STATUS"
        const val ACTION_START = "de.aapphh.heatscan.START"
        const val ACTION_CONFIG = "de.aapphh.heatscan.CONFIG"
        const val ACTION_PAUSE = "de.aapphh.heatscan.PAUSE"
        const val ACTION_FLUSH = "de.aapphh.heatscan.FLUSH"
    }
}
