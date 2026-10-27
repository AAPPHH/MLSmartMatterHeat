package de.aapphh.heatscan

import android.Manifest
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
        if (intent?.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (!active) {
            try {
                startForeground(1, notification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION)
            } catch (e: Exception) {
                Log.w(TAG, "startForeground failed", e)
                store.status = "Start nicht erlaubt"
                store.running = false
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
            step = sensors.getDefaultSensor(Sensor.TYPE_STEP_DETECTOR)
            wakeStep = sensors.getDefaultSensor(Sensor.TYPE_STEP_DETECTOR, true)
            triggers = listOfNotNull(sensors.getDefaultSensor(Sensor.TYPE_MOTION_DETECT, true), sensors.getDefaultSensor(Sensor.TYPE_SIGNIFICANT_MOTION))
            val adaptive = step != null && (wakeStep != null || triggers.isNotEmpty()) &&
                checkSelfPermission(Manifest.permission.ACTIVITY_RECOGNITION) == PackageManager.PERMISSION_GRANTED
            policy = ScanPolicy(adaptive, SystemClock.elapsedRealtime())
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
            if (!wifi.startScan()) collect()
        } catch (e: Exception) {
            Log.w(TAG, "startScan failed", e)
            collect()
        }
    }

    private fun collect() {
        try {
            val results = wifi.scanResults
            if (results.isEmpty()) {
                val lm = getSystemService(LocationManager::class.java)
                store.status = stamp() + " " + Diag.emptyScan(lm.isLocationEnabled, wifi.isWifiEnabled)
                return
            }
            if (!dedup.accept(results.maxOf { it.timestamp })) return
            val home = Home.isHome(results.map { it.SSID }, store.homeList)
            val before = policy.mode
            val now = SystemClock.elapsedRealtime()
            policy.onScan(home, now)
            if (policy.mode != before) schedule(now)
            if (!home) {
                store.status = stamp() + " Unterwegs, kein POST"
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
                    Log.w(TAG, "post failed", e)
                    Diag.postError(e.javaClass.simpleName, Diag.needsLocalNetwork(Build.VERSION.SDK_INT, checkSelfPermission(Diag.LOCAL_NETWORK) == PackageManager.PERMISSION_GRANTED))
                }
                store.status = stamp() + " " + status
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
        const val ACTION_STOP = "de.aapphh.heatscan.STOP"
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
        if (svc != null) svc.onAlarm() else c.getSystemService(AlarmManager::class.java).cancel(ScanService.alarmIntent(c))
    }
}
