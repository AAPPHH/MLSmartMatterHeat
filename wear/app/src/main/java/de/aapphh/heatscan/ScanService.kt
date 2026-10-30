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
import android.provider.Settings
import android.util.Log
import org.json.JSONArray
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
    private var lastEnqueueAt = 0L
    private var lastScanRequestAt = Long.MIN_VALUE / 4
    private var lastScanMs = 0L
    private var lastScanTs = 0L
    private var usedScanMs = 0L
    private var screenOnAt = Reuse.NONE
    private var loggedMode: Mode? = null
    private val flusher = Flusher()
    private var sampler: SensorSampler? = null
    private val appVersion by lazy { try { packageManager.getPackageInfo(packageName, 0).versionName.orEmpty() } catch (e: Exception) { "-" } }
    private val flushPending = AtomicBoolean(false)
    @Volatile private var offBody: Boolean? = null
    @Volatile private var lastException = ""
    @Volatile private var lastRoom = ""
    @Volatile private var motionSeen = false

    private val stepListener = object : SensorEventListener {
        override fun onSensorChanged(e: SensorEvent) {
            motion(e.timestamp / 1_000_000)
        }

        override fun onAccuracyChanged(s: Sensor?, a: Int) {}
    }

    private val offBodyListener = object : SensorEventListener {
        override fun onSensorChanged(e: SensorEvent) {
            val off = OffBody.isOff(e.values[0])
            if (offBody != off) Trace.i("offbody", "off" to off)
            offBody = off
            sampler?.onOffBody(e, off)
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

    private val holdCallback = object : ConnectivityManager.NetworkCallback() {}
    private var holding = false
    private val holdRelease = Runnable { syncHold() }

    private fun syncHold() {
        val now = System.currentTimeMillis()
        val until = store.wifiHoldUntil
        handler.removeCallbacks(holdRelease)
        if (active && WifiHold.active(until, now)) {
            if (!holding) {
                try {
                    cm.requestNetwork(NetworkRequest.Builder().addTransportType(NetworkCapabilities.TRANSPORT_WIFI).addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET).build(), holdCallback)
                    holding = true
                    Trace.i("wifi_hold", "on" to true, "until" to until)
                } catch (e: Exception) {
                    Trace.w("wifi_hold", e, "on" to true)
                    store.wifiHoldUntil = 0L
                }
            }
            if (holding) handler.postDelayed(holdRelease, until - now)
        } else {
            if (until != 0L) store.wifiHoldUntil = 0L
            if (holding) {
                holding = false
                try { cm.unregisterNetworkCallback(holdCallback) } catch (e: Exception) { Log.w(TAG, "unregister hold", e) }
                Trace.i("wifi_hold", "on" to false)
            }
        }
    }

    fun toggleWifiHold() {
        handler.post {
            store.wifiHoldUntil = WifiHold.toggle(store.wifiHoldUntil, System.currentTimeMillis())
            syncHold()
        }
    }

    private fun onReplyText(text: String) {
        val min = WifiHold.minutes(text) ?: return
        handler.post {
            store.wifiHoldUntil = WifiHold.extend(store.wifiHoldUntil, System.currentTimeMillis(), min)
            syncHold()
        }
    }

    private val receiver = object : BroadcastReceiver() {
        override fun onReceive(c: Context, i: Intent) {
            when (i.action) {
                WifiManager.SCAN_RESULTS_AVAILABLE_ACTION -> collect(i.getBooleanExtra(WifiManager.EXTRA_RESULTS_UPDATED, true), true)
                Intent.ACTION_SCREEN_ON -> {
                    Trace.i("screen_on", "queue" to queue.size)
                    screenOnAt = SystemClock.elapsedRealtime()
                    sampler?.flushFifo()
                    sampler?.burst()
                    flush("screen_on")
                    onWake("screen_on", power.isDeviceIdleMode)
                }
                Intent.ACTION_POWER_CONNECTED -> {
                    Trace.i("charger", "queue" to queue.size)
                    sampler?.flushFifo()
                    sampler?.onPower()
                    flush("charger")
                }
                Intent.ACTION_POWER_DISCONNECTED -> sampler?.onPower()
                PowerManager.ACTION_DEVICE_IDLE_MODE_CHANGED, PowerManager.ACTION_POWER_SAVE_MODE_CHANGED -> {
                    Trace.i("power_change", "action" to i.action, "doze" to power.isDeviceIdleMode, "saver" to power.isPowerSaveMode, "power" to powerState(), "queue" to queue.size)
                    if (!power.isDeviceIdleMode) {
                        flush("idle_exit")
                        if (i.action == PowerManager.ACTION_DEVICE_IDLE_MODE_CHANGED) onWake("idle_exit", false)
                    }
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
        store.learnAt = 0L
        store.learnPromptAt = 0L
        instance = this
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (!active) {
            try {
                startForeground(1, notification(), Health.foregroundTypes(Build.VERSION.SDK_INT, healthGranted()))
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
            lastEnqueueAt = SystemClock.elapsedRealtime()
            policy = ScanPolicy(adaptive, lastEnqueueAt)
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
                addAction(Intent.ACTION_POWER_DISCONNECTED)
                addAction(PowerManager.ACTION_DEVICE_IDLE_MODE_CHANGED)
                addAction(PowerManager.ACTION_POWER_SAVE_MODE_CHANGED)
            }, RECEIVER_NOT_EXPORTED)
            sampler = SensorSampler(this, sensors, store, handler, appVersion) { enqueueMeta(it) }.also {
                it.start()
                it.inventory()?.let { inv -> enqueueMeta(inv) }
            }
            store.running = true
            Poster.onReply = ::onReplyText
            syncHold()
            Trace.i("service_started", "queue" to queue.size, "doze" to power.isDeviceIdleMode, "saver" to power.isPowerSaveMode, "bgLoc" to ScanStarter.bgLocation(this), "whitelist" to power.isIgnoringBatteryOptimizations(packageName))
            Trace.i("policy", "adaptive" to adaptive, "step" to (step != null), "wakeStep" to (wakeStep != null), "triggers" to triggers.size, "offBodySensor" to (offBodySensor != null))
            tick()
            flush("start")
        }
        return START_STICKY
    }

    override fun onDestroy() {
        stopLearn("destroy")
        Poster.onReply = null
        handler.removeCallbacks(holdRelease)
        if (holding) {
            holding = false
            try { cm.unregisterNetworkCallback(holdCallback) } catch (e: Exception) { Log.w(TAG, "unregister hold", e) }
        }
        store.wifiHoldUntil = 0L
        handler.removeCallbacks(tick)
        handler.removeCallbacks(beatTimeout)
        if (active) {
            alarms.cancel(alarmIntent)
            releaseSensors()
            sensors.unregisterListener(offBodyListener)
            sampler?.stop()
            sampler = null
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

    fun refreshSensors() {
        handler.post { sampler?.restart() }
    }

    fun onLabel() {
        handler.post { sampler?.onLabel() }
    }

    private fun enqueueMeta(entry: JSONObject) {
        try {
            io.execute {
                val dropped = queue.add(entry, System.currentTimeMillis())
                Trace.i("enqueue", "kind" to entry.optString("kind"), "queue" to queue.size, "dropped" to dropped)
            }
        } catch (e: RejectedExecutionException) {
            Log.w(TAG, "meta enqueue rejected", e)
        }
    }

    private fun healthGranted() = checkSelfPermission(Health.HR) == PackageManager.PERMISSION_GRANTED || checkSelfPermission(Health.SKIN) == PackageManager.PERMISSION_GRANTED

    private fun powerState() = PowerState.of(power.isPowerSaveMode, power.isDeviceIdleMode, offBody)

    private fun bedtime(): Boolean? {
        fun probe(read: (String) -> Int) = { key: String -> try { read(key).takeIf { it == 0 || it == 1 } } catch (e: Exception) { null } }
        val global = probe { Settings.Global.getInt(contentResolver, it, -1) }
        val secure = probe { Settings.Secure.getInt(contentResolver, it, -1) }
        return Bedtime.of(Bedtime.GLOBAL_KEYS.map(global) + Bedtime.SECURE_KEYS.map(secure))
    }

    private fun motion(at: Long) {
        if (offBody == true) return
        motionSeen = true
        val now = SystemClock.elapsedRealtime()
        val before = policy.mode
        val scanDue = policy.onMotion(now, at)
        if (scanDue) scan()
        if (scanDue || policy.mode != before) schedule(now)
    }

    private fun tick() {
        val now = SystemClock.elapsedRealtime()
        if (holding) syncHold()
        sampler?.closeIfDue()
        val before = policy.mode
        if (policy.onTick(now)) scan(policy.mode.sleeps || now - lastEnqueueAt >= ScanConfig.HEARTBEAT_MS)
        if (Wake.settled(before, policy.mode)) promptNoScan("settled", false)
        schedule(now)
    }

    private fun onWake(reason: String, idle: Boolean) {
        if (!active) return
        val now = SystemClock.elapsedRealtime()
        val freshAge = System.currentTimeMillis() - store.lastFreshAt
        if (!suppressed() && Wake.scanDue(now, lastScanRequestAt, idle) && freshAge !in 0 until Wake.FRESH_MS && policy.claimWake(now)) {
            Trace.i("wake_scan", "reason" to reason)
            scan()
        }
        promptNoScan(reason, true)
    }

    private fun promptNoScan(reason: String, wake: Boolean) {
        val away = policy.away
        val off = offBody == true
        if (!Wake.promptAllowed(away, offBody, wake, power.isInteractive)) return
        try {
            io.execute {
                val d = store.stay { it.promptWithoutScan(System.currentTimeMillis(), store.guess, wake) }
                if (d.ask) {
                    Trace.i("prompt_no_scan", "reason" to reason, "since" to d.since)
                    Prompter.ask(this, store, away, off, d)
                }
            }
        } catch (e: RejectedExecutionException) {
            Log.w(TAG, "prompt rejected", e)
        }
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
        if (policy.mode.sleeps && store.learnAt == 0L) {
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

    private fun suppressed() = Reuse.suppress(foregroundSince != Reuse.NONE, store.learnAt != 0L)

    private fun usableScan(now: Long) = Reuse.usable(now, lastScanMs, foregroundSince, screenOnAt, usedScanMs)

    fun onForeground() {
        handler.post {
            if (!active || foregroundSince == Reuse.NONE) return@post
            val now = SystemClock.elapsedRealtime()
            val open = Reuse.openScan(usableScan(now), store.learnAt != 0L)
            Trace.i("foreground", "openScan" to open)
            if (open && policy.claimWake(now)) scan(open = true)
        }
    }

    private fun scan(beat: Boolean = false, open: Boolean = false) {
        if (!open && suppressed()) {
            policy.refund()
            Trace.i("scan_suppressed", "beat" to beat)
            return
        }
        sampler?.burst()
        if (beat) {
            beatPending = true
            handler.removeCallbacks(beatTimeout)
            handler.postDelayed(beatTimeout, ScanConfig.BEAT_WAIT_MS)
        }
        try {
            val ok = wifi.startScan()
            if (ok) lastScanRequestAt = SystemClock.elapsedRealtime()
            Trace.i("scan_requested", "accepted" to ok, "beat" to beat)
            if (!ok) {
                store.rejected += 1
                collect()
            }
        } catch (e: Exception) {
            Trace.w("scan_requested", e, "accepted" to false, "beat" to beat)
            store.rejected += 1
            collect()
        }
    }

    private var learnPending = false
    @Volatile private var learnReady = false
    private var learnTapMs = 0L
    private var learnIdleAt = 0L
    private val learnTick = object : Runnable {
        override fun run() {
            val now = System.currentTimeMillis()
            if (learnPending && !Learn.waiting(true, learnTapMs, SystemClock.elapsedRealtime())) {
                Trace.i("learn_pending_timeout")
                learnPending = false
            }
            val step = Learn.step(store.learnAt, now, offBody == true, store.progress?.done == true, store.learnPromptAt, learnPending, learnIdleAt).let { if (!learnReady && it != LearnStep.STOP) LearnStep.WAIT else it }
            when (step) {
                LearnStep.STOP -> stopLearn("auto")
                LearnStep.DISCARD -> {
                    Trace.i("learn_discard")
                    store.learnPromptAt = 0L
                    learnIdleAt = now
                    Prompter.cancel(applicationContext)
                }
                LearnStep.SCAN -> learnScan()
                LearnStep.WAIT -> {}
            }
            if (store.learnAt != 0L) handler.postDelayed(this, Learn.TICK_MS)
        }
    }

    fun setLearning(on: Boolean) {
        handler.post {
            if (!on) return@post stopLearn("button")
            if (!active || store.learnAt != 0L) return@post
            store.learnAt = System.currentTimeMillis()
            store.learnPromptAt = 0L
            learnIdleAt = 0L
            learnPending = false
            learnReady = false
            store.progressText = null
            Trace.i("learn_start")
            fetchProgress()
            handler.removeCallbacks(learnTick)
            handler.postDelayed(learnTick, Learn.TICK_MS)
            schedule(SystemClock.elapsedRealtime())
        }
    }

    private fun stopLearn(reason: String) {
        handler.removeCallbacks(learnTick)
        learnPending = false
        if (store.learnAt == 0L) return
        Trace.i("learn_stop", "reason" to reason)
        store.learnAt = 0L
        store.learnPromptAt = 0L
        Prompter.cancel(applicationContext)
        if (active && reason != "destroy") schedule(SystemClock.elapsedRealtime())
    }

    private fun fetchProgress() {
        try {
            io.execute {
                try {
                    val r = Poster.get(store.server, Learn.PATH, network)
                    if (r.code in 200..299 && Learn.parse(r.text) != null) store.progressText = r.text
                } catch (e: Exception) {
                    Trace.w("progress", e)
                } finally {
                    learnReady = true
                }
            }
        } catch (e: RejectedExecutionException) {
            learnReady = true
            Log.w(TAG, "progress rejected", e)
        }
    }

    private fun learnScan() {
        val now = SystemClock.elapsedRealtime()
        learnIdleAt = System.currentTimeMillis()
        if (!policy.claimManual(now)) return
        val ok = try { wifi.startScan() } catch (e: Exception) { false }
        Trace.i("scan_requested", "accepted" to ok, "learn" to true)
        if (!ok) return
        lastScanRequestAt = now
        learnPending = true
        learnTapMs = now
    }

    fun learnLabel(room: String, heat: Boolean = false) {
        handler.post {
            val at = store.learnScanAt
            if (store.learnPromptAt == 0L || room !in Defaults.LABELS) return@post
            val wall = System.currentTimeMillis()
            store.learnPromptAt = 0L
            learnIdleAt = wall
            Prompter.cancel(applicationContext)
            manualPending = false
            manualAwaiting = true
            handler.removeCallbacks(manualTimeout)
            handler.removeCallbacks(ackTimeout)
            handler.postDelayed(ackTimeout, Tap.ACK_WAIT_MS)
            store.manualUntil = 0L
            store.manual = Tap.WAIT
            try {
                io.execute {
                    LabelRecorder.point(applicationContext, room, at, heat)
                    flush("label")
                }
            } catch (e: RejectedExecutionException) {
                failManual("Fehler beim Speichern")
            }
        }
    }

    private var manualPending = false
    private var manualAwaiting = false
    private var manualRoom = ""
    private var manualHeat = true
    private var manualTapMs = 0L
    private val manualTimeout = Runnable { throttleManual() }
    private val ackTimeout = Runnable { setManual(Tap.QUEUED) }

    private fun setManual(state: String, until: Long = 0L) {
        manualPending = false
        manualAwaiting = false
        handler.removeCallbacks(manualTimeout)
        handler.removeCallbacks(ackTimeout)
        store.manualUntil = until
        store.manual = state
    }

    private fun throttleManual(fallbackS: Int = Tap.FALLBACK_WAIT_S) {
        if (!manualPending) return
        val s = Tap.waitS(policy.starts(), SystemClock.elapsedRealtime(), fallbackS)
        Trace.i("manual_throttled", "waitS" to s)
        setManual(Tap.THROTTLED, System.currentTimeMillis() + s * 1000L)
    }

    private fun failManual(reason: String) {
        Trace.i("manual_failed", "reason" to reason)
        setManual(Tap.failed(reason))
    }

    private fun ackManual(code: Int, remaining: Int, deferred: Int) {
        if (!manualAwaiting) return
        Tap.ack(code, remaining, deferred)?.let {
            setManual(it)
            if (store.learnAt != 0L) fetchProgress()
        }
    }

    fun manualScan(room: String, tapMs: Long, heat: Boolean = true) {
        handler.post {
            if (manualPending || manualAwaiting) return@post
            if (!active) return@post failManual("Scan pausiert")
            val now = SystemClock.elapsedRealtime()
            if (store.learnAt == 0L && usableScan(now)) {
                usedScanMs = lastScanMs
                manualRoom = room
                manualHeat = heat
                manualAwaiting = true
                handler.postDelayed(ackTimeout, Tap.ACK_WAIT_MS)
                Trace.i("manual_reuse", "ageMs" to now - lastScanMs)
                try {
                    val ts = lastScanTs
                    io.execute { LabelRecorder.point(applicationContext, room, ts, heat) }
                } catch (e: RejectedExecutionException) {
                    failManual("Fehler beim Speichern")
                }
                return@post
            }
            manualPending = true
            manualRoom = room
            manualHeat = heat
            manualTapMs = tapMs
            val claimed = policy.claimManual(now)
            val ok = claimed && try { wifi.startScan() } catch (e: Exception) { false }
            Trace.i("scan_requested", "accepted" to ok, "manual" to true)
            if (ok) {
                lastScanRequestAt = now
                handler.postDelayed(manualTimeout, ScanConfig.MANUAL_WAIT_MS)
            } else {
                store.rejected += 1
                throttleManual(if (claimed) Tap.FALLBACK_WAIT_S else Tap.GAP_WAIT_S)
            }
        }
    }

    private fun collect(updated: Boolean = true, broadcast: Boolean = false) {
        try {
            val results = wifi.scanResults
            val pending = manualPending && broadcast
            val learn = learnPending && broadcast && !pending && Tap.verdict(updated, results.size, results.maxOfOrNull { it.timestamp } ?: 0L, learnTapMs) == Verdict.FRESH
            if (pending) {
                when (Tap.verdict(updated, results.size, results.maxOfOrNull { it.timestamp } ?: 0L, manualTapMs)) {
                    Verdict.EMPTY -> return failManual(Diag.emptyScan(getSystemService(LocationManager::class.java).isLocationEnabled, wifi.isWifiEnabled))
                    Verdict.STALE -> return throttleManual()
                    Verdict.FRESH -> {}
                }
            }
            if (results.isEmpty()) {
                val lm = getSystemService(LocationManager::class.java)
                val cause = Diag.emptyScan(lm.isLocationEnabled, wifi.isWifiEnabled)
                Trace.i("scan_result", "count" to 0, "cause" to cause)
                store.status = stamp() + " " + cause
                return
            }
            val beat = beatPending
            val manual = pending
            val newest = results.maxOf { it.timestamp }
            if (!dedup.accept(newest)) {
                Trace.i("scan_result", "count" to results.size, "dedup" to true)
                if (manual) throttleManual()
                if (learn) learnPending = false
                return
            }
            if (manual) {
                manualPending = false
                manualAwaiting = true
                handler.removeCallbacks(manualTimeout)
                handler.postDelayed(ackTimeout, Tap.ACK_WAIT_MS)
            }
            store.lastFreshAt = System.currentTimeMillis()
            store.rejected = 0
            store.setLastAps(results.map { NamedAp(it.BSSID.lowercase(), it.SSID.orEmpty(), it.level, it.frequency) })
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
            if (!manual) {
                lastScanMs = if (home) newest / 1000 else 0L
                lastScanTs = ts
            }
            enqueue(ts, if (home) results.map { Ap(it.BSSID, it.level, it.frequency) } else emptyList(), policy.away, if (home || manual) "scan" else "away", manual, newest * 1000, learn)
            if (learn) {
                learnPending = false
                if (home && store.learnAt != 0L) {
                    store.learnScanAt = ts
                    store.learnPromptAt = System.currentTimeMillis()
                    Prompter.askLearn(this)
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "collect failed", e)
            if (manualPending) failManual("Fehler beim Lesen")
        }
    }

    private fun enqueue(ts: Long, aps: List<Ap>, away: Boolean, kind: String, manual: Boolean = false, elapsedNs: Long? = null, learn: Boolean = false) {
        lastEnqueueAt = SystemClock.elapsedRealtime()
        val state = powerState()
        val motion = policy.mode.motion
        val moving = policy.mode == Mode.MOVING
        val moved = motionSeen
        motionSeen = false
        val off = offBody == true
        try {
            io.execute {
                val wall = System.currentTimeMillis()
                val labelId = if (manual) LabelRecorder.point(applicationContext, manualRoom, ts, manualHeat, false) else null
                val d = if (learn || (manual && !manualHeat)) Decision() else if (kind == "away") store.stay { it.reset() }.let { Decision() } else if (manual) store.stay { it.onManual(ts, wall, aps, moving, moved, store.guess) } else store.stay { it.onScan(ts, wall, aps, moving, moved, store.guess) }
                val body = Payload.build(ts, store.device, if (manual) (if (manualHeat) manualRoom else null) else d.label, aps, motion, state, away, store.nextId(), elapsedNs, bedtime(), if (manual && !manualHeat || learn) false else null, labelId)
                val dropped = queue.add(body, wall)
                Trace.i("enqueue", "kind" to kind, "aps" to aps.size, "queue" to queue.size, "dropped" to dropped, "motion" to motion, "power" to state, "away" to away, "label" to (d.label ?: "-"), "ask" to d.ask)
                store.status = stamp() + " " + kind + " q" + queue.size
                if (d.ask) Prompter.ask(this, store, away, off, d)
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
        if (reply.code in 200..299) store.setReply(ReplyJson.parse(reply.text), ReplyJson.postedAps(body))
        if (reply.code in 200..299) Poster.room(reply.text).let { if (it.isNotEmpty()) lastRoom = it; (if (it in Defaults.ROOMS) it else null).let { g -> if (store.guess != g) store.guess = g } }
        if (reply.code in 200..299 && ReplyJson.postedAps(body)) onReplyGuess(body, reply.text)
        reply.code
    } catch (e: Exception) {
        lastException = e.javaClass.simpleName
        store.lastError = e.toString()
        Trace.w("post", e, "path" to path)
        -1
    }

    private fun onReplyGuess(body: String, text: String) {
        val room = Poster.room(text).takeIf { it in Defaults.ROOMS } ?: return
        val ts = ReplyJson.lastTs(body) ?: return
        val now = System.currentTimeMillis()
        val p = ReplyJson.parse(text).probability
        val d = store.stay { it.onGuess(ts, room, p, now) }
        if (d.ask) Prompter.ask(this, store, store.atHome == false, offBody == true, d)
        else if (store.stay { it.refresh(ts, now, room) }) Prompter.refresh(this, store)
    }

    private fun plain(server: String, path: String, body: String, net: Network?): Int = try {
        Poster.send(server, path, body, net).code
    } catch (ex: Exception) {
        lastException = ex.javaClass.simpleName
        store.lastError = ex.toString()
        Trace.w("post", ex, "path" to path)
        -1
    }

    private fun sendLabel(server: String, e: JSONObject, net: Network?): Int {
        if (LabelEvent.isVoid(e)) {
            val b = LabelEvent.voidBody(e)
            val c = plain(server, Measure.PATH_VOID, b, net)
            return if (c in Measure.FALLBACK_CODES) plain(server, "/api/label/void", b, net) else c
        }
        val c = plain(server, Measure.PATH_LABEL, LabelEvent.watchBody(e), net)
        return if (c in Measure.FALLBACK_CODES) plain(server, "/api/label", LabelEvent.body(e), net) else c
    }

    private fun sendMeta(server: String, e: JSONObject, net: Network?): Int {
        val c = if (MeasureBatch.isMeasure(e)) plain(server, Measure.PATH, MeasureBatch.body(e), net) else plain(server, Measure.PATH_SENSORS, Inventory.body(e), net)
        return if (c == 405) 404 else c
    }

    private fun sendScan(server: String, e: JSONObject, net: Network?): Int = send(server, "/api/scan", e.toString(), net)

    private fun sendScans(server: String, a: JSONArray, net: Network?): Int = send(server, "/api/scans", JSONObject().put("scans", a).toString(), net)

    private fun runFlush(reason: String) {
        val server = store.server
        val net = network
        val before = queue.size
        if (before == 0) return
        val lock = flushWake?.also { it.acquire(ScanConfig.FLUSH_HOLD_MS) }
        val r = try {
            flusher.run(
                queue, System.currentTimeMillis(),
                { a -> sendScans(server, a, net) },
                { e -> sendScan(server, e, net) },
                { e -> sendLabel(server, e, net) },
                { e -> sendMeta(server, e, net) }
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
        if (r.deferred > 0) {
            store.labels404 += r.deferred
            Trace.i("label_404", "deferred" to r.deferred, "total" to store.labels404, "queue" to queue.size)
        }
        if (!ok) {
            store.postsFailed += 1
            if (r.code != -1) store.lastError = "HTTP ${r.code}"
        }
        Trace.i("flush", "reason" to reason, "batch" to !flusher.singleOnly, "code" to r.code, "sent" to r.sent, "queueBefore" to before, "queue" to queue.size, "okTotal" to store.postsOk, "failedTotal" to store.postsFailed)
        handler.post { ackManual(r.code, queue.size, r.deferred) }
        store.postOk = ok
        store.postHm = Footer.hm(stamp())
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
        @Volatile var foregroundSince = Reuse.NONE

        fun setForeground(on: Boolean) {
            foregroundSince = if (on) SystemClock.elapsedRealtime() else Reuse.NONE
            if (on) instance?.onForeground()
        }

        fun alarmIntent(c: Context): PendingIntent = PendingIntent.getBroadcast(
            c, 0, Intent(c, TickReceiver::class.java).setAction(ACTION_TICK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
    }
}

object Prompter {
    private const val CHANNEL = "room"
    private const val QUIET = "room_quiet"
    private const val ID = 3
    const val ACTION = "de.aapphh.heatscan.LABEL"

    private fun nm(c: Context): NotificationManager = c.getSystemService(NotificationManager::class.java).also {
        it.createNotificationChannel(NotificationChannel(CHANNEL, "Raum", NotificationManager.IMPORTANCE_DEFAULT).apply { enableVibration(true) })
        it.createNotificationChannel(NotificationChannel(QUIET, "Raum leise", NotificationManager.IMPORTANCE_LOW).apply { enableVibration(false) })
    }

    private fun action(c: Context, room: String, guess: String?): Notification.Action {
        val pi = PendingIntent.getBroadcast(
            c, Defaults.LABELS.indexOf(room) + 10, Intent(c, LabelReceiver::class.java).setAction(ACTION).setData(android.net.Uri.parse("room:$room")).putExtra("room", room),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        return Notification.Action.Builder(android.graphics.drawable.Icon.createWithResource(c, RoomIcons.RES.getValue(room)), Prompt.caption(room, guess), pi).build()
    }

    private fun post(c: Context, guess: String?, change: Boolean, loud: Boolean, alertAgain: Boolean, learn: Boolean = false) {
        val b = Notification.Builder(c, if (loud) CHANNEL else QUIET)
            .setContentTitle(Prompt.title(change, learn))
            .setSmallIcon(R.drawable.ic_launcher)
            .setTimeoutAfter(if (learn) Learn.ANSWER_MS else Prompt.TIMEOUT_MS)
            .setOnlyAlertOnce(!alertAgain)
        val open = PendingIntent.getActivity(
            c, 1, Intent(c, MainActivity::class.java).putExtra("prompt", true).putExtra("change", change).putExtra("learn", learn).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        b.setContentIntent(open)
        if (loud) b.setFullScreenIntent(open, true).setPriority(Notification.PRIORITY_HIGH)
        val wear = Notification.WearableExtender()
        Prompt.rooms(guess).forEach {
            val a = action(c, it, guess)
            b.addAction(a)
            wear.addAction(a)
        }
        nm(c).notify(ID, b.extend(wear).build())
    }

    fun ask(c: Context, store: Store, away: Boolean, offBody: Boolean, d: Decision) {
        if (store.learnAt != 0L) return
        try {
            val now = System.currentTimeMillis()
            val loud = Prompt.alert(now, store.lastPromptAlertAt, away, offBody, java.util.Calendar.getInstance().get(java.util.Calendar.HOUR_OF_DAY))
            if (loud) store.lastPromptAlertAt = now
            store.stay { it.askLoud = loud }
            if (d.change) store.refreshTile()
            post(c, store.guess, d.change, loud, loud)
        } catch (e: Exception) {
            Trace.w("prompt_failed", e)
        }
    }

    fun askLearn(c: Context) {
        try {
            post(c, null, false, true, true, true)
        } catch (e: Exception) {
            Trace.w("prompt_failed", e)
        }
    }

    fun refresh(c: Context, store: Store) {
        if (store.learnAt != 0L) return
        try {
            val (guess, change, loud) = store.stay { Triple(it.askGuess, it.askChange, it.askLoud) }
            post(c, guess, change, loud, false)
        } catch (e: Exception) {
            Trace.w("prompt_refresh_failed", e)
        }
    }

    fun cancel(c: Context) = nm(c).cancel(ID)

    fun confirm(c: Context, room: String) {
        try {
            nm(c).notify(ID, Notification.Builder(c, QUIET)
                .setContentTitle(Prompt.done(room))
                .setSmallIcon(R.drawable.ic_launcher)
                .setTimeoutAfter(Prompt.DONE_TIMEOUT_MS)
                .setOnlyAlertOnce(true)
                .build())
        } catch (e: Exception) {
            Trace.w("confirm_failed", e)
        }
    }
}

object LabelRecorder {
    private val io = Executors.newSingleThreadExecutor()

    fun async(c: Context, room: String, done: () -> Unit = {}) {
        val app = c.applicationContext
        try {
            io.execute { try { record(app, room) } finally { done() } }
        } catch (e: RejectedExecutionException) {
            done()
        }
    }

    fun undo(c: Context) {
        val app = c.applicationContext
        try {
            io.execute { void(app) }
        } catch (e: RejectedExecutionException) {
            Trace.i("label_void_rejected")
        }
    }

    private fun void(c: Context) {
        val store = Store(c)
        val id = store.lastLabelId
        if (id.isEmpty()) return
        val now = System.currentTimeMillis()
        ScanQueue.get(c).add(LabelEvent.voidEntry(store.device, id, now), now)
        store.stay { it.void(id) }
        store.lastLabelId = ""
        store.manualUntil = 0L
        store.manual = Tap.VOIDED
        store.refreshTile()
        Trace.i("label_void", "id" to id)
        ScanService.instance?.flush("label")
    }

    fun point(c: Context, room: String, at: Long, heat: Boolean, flush: Boolean = true): String {
        val store = Store(c)
        val now = System.currentTimeMillis()
        val id = LabelEvent.newId()
        ScanQueue.get(c).add(LabelEvent.point(store.device, at, room, heat, id), now)
        store.lastLabelId = id
        if (heat) store.stay { it.confirm(room, now, id) }
        store.refreshTile()
        Trace.i("label", "room" to room, "from" to at - LabelEvent.POINT_MS, "to" to at + LabelEvent.POINT_MS, "heat" to heat)
        ScanService.instance?.onLabel()
        if (flush) ScanService.instance?.flush("label")
        return id
    }

    fun record(c: Context, room: String, notify: Boolean = true) {
        if (room !in Defaults.LABELS) return
        val store = Store(c)
        if (store.learnPromptAt != 0L && ScanService.instance != null) return ScanService.instance!!.learnLabel(room)
        val now = System.currentTimeMillis()
        val (from, to) = LabelEvent.window(store.stay { it.windowStart(now) }, now)
        ScanQueue.get(c).add(LabelEvent.entry(store.device, from, to, room), now)
        store.stay { it.confirm(room, now) }
        store.refreshTile()
        if (notify) Prompter.confirm(c, room)
        Trace.i("label", "room" to room, "from" to from, "to" to to)
        ScanService.instance?.onLabel()
        ScanService.instance?.flush("label")
    }

    fun measure(c: Context, room: String, heat: Boolean, done: () -> Unit = {}) {
        val tapMs = SystemClock.elapsedRealtime()
        val store = Store(c)
        if (store.learnPromptAt != 0L && ScanService.instance != null) {
            ScanService.instance?.learnLabel(room, heat)
            return done()
        }
        store.manualUntil = 0L
        store.manual = Tap.WAIT
        ScanService.instance?.manualScan(room, tapMs, heat) ?: store.run { manual = Tap.failed("Dienst läuft nicht") }
        done()
    }
}

class LabelReceiver : BroadcastReceiver() {
    override fun onReceive(c: Context, i: Intent) {
        if (i.action != Prompter.ACTION) return
        val room = i.getStringExtra("room") ?: return
        val pending = goAsync()
        LabelRecorder.async(c, room) { pending.finish() }
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
    ).plus(Health.requestable(Build.VERSION.SDK_INT)).filter { c.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }

    fun missingBackground(c: Context): List<String> = Health.backgroundStep(
        Build.VERSION.SDK_INT,
        Health.requestable(Build.VERSION.SDK_INT).any { c.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED },
        c.checkSelfPermission(Health.BACKGROUND) == PackageManager.PERMISSION_GRANTED
    )

    fun bgLocation(c: Context): Boolean = c.checkSelfPermission(Manifest.permission.ACCESS_BACKGROUND_LOCATION) == PackageManager.PERMISSION_GRANTED

    fun exempt(c: Context, source: String): Boolean = source in VISIBLE || source.startsWith("boot:") ||
        (source == "alarm" && Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && c.getSystemService(AlarmManager::class.java).canScheduleExactAlarms()) ||
        c.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(c.packageName)

    fun extra(c: Context): List<Pair<String, Any?>> = listOf(
        "startError" to Store(c).startError,
        "lastFreshScan" to Footer.fresh(Store(c).lastFreshAt),
        "rejected" to Store(c).rejected,
        "queue" to ScanQueue.get(c).size,
        "sensors" to Store(c).sensorNote.replace(' ', ','),
        "battPctH" to Store(c).battPerHour.let { if (it.isNaN()) "-" else "%.2f".format(Locale.US, it) },
        "samplePeriodS" to Store(c).samplePeriodS,
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
                i.getStringExtra("sample_period_s")?.toIntOrNull()?.takeIf { it in 10..3600 }?.let { store.samplePeriodS = it; ScanService.instance?.refreshSensors() }
                Trace.i("config", "server" to store.server, "home" to store.home, "device" to store.device)
            }
            ACTION_START -> {
                store.paused = false
                ScanStarter.start(c, "adb")
            }
            ACTION_PAUSE -> ScanStarter.setPaused(c, i.getBooleanExtra("on", true))
            ACTION_FLUSH -> ScanService.instance?.flush("adb")
            ACTION_DEMO -> store.setDemo()
        }
        val line = Summary.line(
            ScanService.running, store.paused, store.mode, store.stayLabel(), store.server, store.home, store.atHome,
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
        const val ACTION_DEMO = "de.aapphh.heatscan.DEMO"
    }
}
