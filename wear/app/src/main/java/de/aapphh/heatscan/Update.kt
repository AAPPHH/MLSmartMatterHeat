package de.aapphh.heatscan

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageInstaller
import android.net.Network
import android.os.BatteryManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import org.json.JSONObject
import java.io.File
import java.io.IOException
import java.security.MessageDigest
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException

data class UpdateInfo(val versionCode: Long, val versionName: String, val sha256: String, val size: Long)

object UpdateRules {
    const val GAP_MS = 6 * 3_600_000L
    const val RETRY_DELAY_MS = 600_000L
    const val INFO_PATH = "/watch/update"
    const val APK_PATH = "/watch/update.apk"
    const val CACHE = "update.apk"

    fun parse(text: String): UpdateInfo? = try {
        JSONObject(text).let { UpdateInfo(it.getLong("version_code"), it.optString("version_name"), it.getString("sha256").lowercase(), it.getLong("size")) }
            .takeIf { it.size > 0 && it.sha256.length == 64 }
    } catch (e: Exception) { null }

    fun newer(info: UpdateInfo, installed: Long) = info.versionCode > installed

    fun digest(f: File): String = MessageDigest.getInstance("SHA-256").let { md ->
        f.inputStream().use { i ->
            val buf = ByteArray(65536)
            while (true) {
                val n = i.read(buf)
                if (n < 0) break
                md.update(buf, 0, n)
            }
        }
        md.digest().joinToString("") { "%02x".format(it) }
    }

    fun intact(f: File, info: UpdateInfo) = f.isFile && f.length() == info.size && digest(f) == info.sha256

    fun due(now: Long, at: Long, failed: Boolean, retried: Boolean): Boolean {
        val age = Span.ms(now, at)
        return at == 0L || age < 0 || age >= GAP_MS || (failed && !retried)
    }
}

object Updater {
    private const val CHANNEL = "update"
    private const val NOTIFICATION_ID = 70
    const val ACTION = "de.aapphh.heatscan.UPDATE_STATUS"
    private val io = Executors.newSingleThreadExecutor()
    private val handler = Handler(Looper.getMainLooper())

    fun charging(c: Context): Boolean = (c.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))?.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) ?: 0) != 0

    fun onCharger(c: Context) {
        Store(c.applicationContext).updateRetried = false
        start(c)
    }

    fun start(c: Context, now: Long = System.currentTimeMillis(), net: Network? = Remote.wifi(), plugged: Boolean = charging(c)): Boolean {
        val app = c.applicationContext
        val store = Store(app)
        if (net == null || !plugged || !UpdateRules.due(now, store.updateAt, store.updateFailed, store.updateRetried)) return false
        if (store.updateFailed && Span.ms(now, store.updateAt) in 0 until UpdateRules.GAP_MS) store.updateRetried = true
        store.updateAt = now
        try { io.execute { attempt(app, store, net) } } catch (e: RejectedExecutionException) { Trace.i("update_rejected") }
        return true
    }

    private fun attempt(app: Context, store: Store, net: Network) {
        val ok = try { check(app, store, net); true } catch (e: Exception) { Trace.w("update_failed", e); false }
        store.updateFailed = !ok
        if (!ok && !store.updateRetried) handler.postDelayed({ start(app) }, UpdateRules.RETRY_DELAY_MS)
    }

    private fun check(app: Context, store: Store, net: Network) {
        val file = File(app.cacheDir, UpdateRules.CACHE)
        file.delete()
        val conn = Poster.connect(store.server, UpdateRules.INFO_PATH, net)
        val info = try {
            conn.connectTimeout = 8000
            conn.readTimeout = 8000
            when (val code = conn.responseCode) {
                404 -> return Trace.i("update_none")
                200 -> UpdateRules.parse(conn.inputStream.bufferedReader().use { it.readText() }) ?: throw IOException("info_unreadable")
                else -> throw IOException("info_$code")
            }
        } finally { conn.disconnect() }
        val installed = app.packageManager.getPackageInfo(app.packageName, 0).longVersionCode
        if (!UpdateRules.newer(info, installed)) return Trace.i("update_current", "installed" to installed, "offered" to info.versionCode)
        if (!charging(app)) return Trace.i("update_unplugged")
        try {
            download(store.server, net, file)
            if (!UpdateRules.intact(file, info)) throw IOException("apk_mismatch")
            commit(app, file, info)
        } finally { file.delete() }
    }

    private fun download(server: String, net: Network, file: File) {
        val conn = Poster.connect(server, UpdateRules.APK_PATH, net)
        try {
            conn.connectTimeout = 8000
            conn.readTimeout = 15000
            if (conn.responseCode != 200) throw IOException("apk_${conn.responseCode}")
            conn.inputStream.use { i -> file.outputStream().use { i.copyTo(it) } }
        } finally { conn.disconnect() }
    }

    private fun commit(app: Context, file: File, info: UpdateInfo) {
        val installer = app.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL).apply {
            setAppPackageName(app.packageName)
            setSize(file.length())
            if (Build.VERSION.SDK_INT >= 31) setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
        }
        val id = installer.createSession(params)
        try {
            installer.openSession(id).use { s ->
                s.openWrite("update", 0, file.length()).use { o -> file.inputStream().use { it.copyTo(o) }; s.fsync(o) }
                val status = PendingIntent.getBroadcast(app, 0, Intent(app, UpdateStatusReceiver::class.java).setAction(ACTION), PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
                s.commit(status.intentSender)
            }
            Trace.i("update_committed", "version" to info.versionName, "code" to info.versionCode)
        } catch (e: Exception) {
            installer.abandonSession(id)
            throw e
        }
    }

    fun offer(c: Context, confirm: Intent) {
        try {
            val nm = c.getSystemService(NotificationManager::class.java)
            nm.createNotificationChannel(NotificationChannel(CHANNEL, "Update", NotificationManager.IMPORTANCE_LOW))
            val open = PendingIntent.getActivity(c, 71, confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
            nm.notify(NOTIFICATION_ID, Notification.Builder(c, CHANNEL).setContentTitle("Update bereit").setSmallIcon(R.drawable.ic_launcher).setContentIntent(open).setAutoCancel(true).build())
        } catch (e: Exception) {
            Trace.w("update_offer_failed", e)
        }
    }
}

class UpdateStatusReceiver : BroadcastReceiver() {
    override fun onReceive(c: Context, i: Intent) {
        if (i.action != Updater.ACTION) return
        val status = i.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)
        Trace.i("update_status", "status" to status, "message" to i.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE))
        File(c.cacheDir, UpdateRules.CACHE).delete()
        if (status != PackageInstaller.STATUS_PENDING_USER_ACTION) return
        val confirm = if (Build.VERSION.SDK_INT >= 33) i.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java) else @Suppress("DEPRECATION") i.getParcelableExtra<Intent>(Intent.EXTRA_INTENT)
        if (confirm != null) Updater.offer(c, confirm)
    }
}
