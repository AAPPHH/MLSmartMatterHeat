package de.aapphh.heatscan

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
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
    private var active = false

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

    private val tick = object : Runnable {
        override fun run() {
            scan()
            handler.postDelayed(this, PERIOD_MS)
        }
    }

    override fun onBind(i: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        store = Store(this)
        wifi = applicationContext.getSystemService(WifiManager::class.java)
        cm = getSystemService(ConnectivityManager::class.java)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (!active) {
            active = true
            startForeground(1, notification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION)
            wake = getSystemService(PowerManager::class.java).newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "heatscan:scan").also { it.acquire() }
            cm.requestNetwork(NetworkRequest.Builder().addTransportType(NetworkCapabilities.TRANSPORT_WIFI).build(), netCallback)
            registerReceiver(receiver, IntentFilter(WifiManager.SCAN_RESULTS_AVAILABLE_ACTION), RECEIVER_NOT_EXPORTED)
            store.running = true
            handler.post(tick)
        }
        return START_STICKY
    }

    override fun onDestroy() {
        handler.removeCallbacks(tick)
        if (active) {
            try { unregisterReceiver(receiver) } catch (e: Exception) { Log.w(TAG, "unregister", e) }
            try { cm.unregisterNetworkCallback(netCallback) } catch (e: Exception) { Log.w(TAG, "unregister net", e) }
            wake?.let { if (it.isHeld) it.release() }
        }
        active = false
        store.running = false
        io.shutdown()
        super.onDestroy()
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
            if (results.isEmpty()) return
            if (!dedup.accept(results.maxOf { it.timestamp })) return
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
                    "ERR ${e.javaClass.simpleName}"
                }
                store.status = SimpleDateFormat("HH:mm:ss", Locale.US).format(Date()) + " " + status
            }
        } catch (e: Exception) {
            Log.w(TAG, "collect failed", e)
        }
    }

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
        private const val CHANNEL = "scan"
        private const val PERIOD_MS = 30_000L
    }
}
