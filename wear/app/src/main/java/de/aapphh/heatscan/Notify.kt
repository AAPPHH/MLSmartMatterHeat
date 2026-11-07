package de.aapphh.heatscan

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.graphics.drawable.Icon
import android.net.Uri
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException

object Alerter {
    private const val CHANNEL = "br_alert"
    private const val QUIET = "br_alert_quiet"

    private fun nm(c: Context): NotificationManager = c.getSystemService(NotificationManager::class.java).also {
        it.createNotificationChannel(NotificationChannel(CHANNEL, "Heizung Alarm", NotificationManager.IMPORTANCE_HIGH).apply { enableVibration(true) })
        it.createNotificationChannel(NotificationChannel(QUIET, "Heizung Alarm leise", NotificationManager.IMPORTANCE_LOW))
    }

    fun show(c: Context, store: Store, alerts: List<Alert>) {
        val fresh = AlertList.fresh(alerts, store.alertSeen)
        if (fresh.isEmpty()) return
        try {
            val nm = nm(c)
            val open = PendingIntent.getActivity(c, 3, Intent(c, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
            fresh.forEachIndexed { i, a ->
                nm.notify(AlertList.notificationId(a), Notification.Builder(c, if (i == fresh.lastIndex) CHANNEL else QUIET)
                    .setContentTitle(a.title)
                    .setContentText(a.text)
                    .setStyle(Notification.BigTextStyle().bigText(a.text))
                    .setSmallIcon(R.drawable.ic_launcher)
                    .setContentIntent(open)
                    .setOnlyAlertOnce(true)
                    .build())
            }
            store.alertSeen = AlertList.remember(store.alertSeen, fresh)
        } catch (e: Exception) {
            Trace.w("alert_failed", e)
        }
    }
}

object WindowPrompter {
    private const val CHANNEL = "window_q"
    private const val OLD_CHANNEL = "window"
    private const val ID = 5
    const val ACTION = "de.aapphh.heatscan.WINDOW"
    private val VIBRATION = longArrayOf(0, 220)
    private val io = Executors.newSingleThreadExecutor()

    private fun nm(c: Context): NotificationManager = c.getSystemService(NotificationManager::class.java).also {
        it.deleteNotificationChannel(OLD_CHANNEL)
        it.createNotificationChannel(NotificationChannel(CHANNEL, "Fenster", NotificationManager.IMPORTANCE_HIGH).apply {
            enableVibration(true)
            vibrationPattern = VIBRATION
        })
    }

    private fun intent(c: Context, id: String, answer: WindowAnswer, via: String) = PendingIntent.getBroadcast(
        c, 20 + answer.ordinal, Intent(c, WindowReceiver::class.java).setAction(ACTION).setData(Uri.parse("window:$id/${answer.wire}/$via"))
            .putExtra("id", id).putExtra("answer", answer.wire).putExtra("via", via),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
    )

    private fun action(c: Context, a: WindowAsk, answer: WindowAnswer, label: String) =
        Notification.Action.Builder(Icon.createWithResource(c, R.drawable.ic_launcher), label, intent(c, a.id, answer, "notification")).build()

    fun show(c: Context, a: WindowAsk) {
        try {
            val open = PendingIntent.getActivity(
                c, 2, Intent(c, MainActivity::class.java).putExtra("window", true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            )
            val actions = listOf(action(c, a, WindowAnswer.YES, WindowEvent.YES), action(c, a, WindowAnswer.NO, WindowEvent.NO), action(c, a, WindowAnswer.UNKNOWN, WindowEvent.UNKNOWN))
            val b = Notification.Builder(c, CHANNEL)
                .setContentTitle(WindowEvent.QUESTION)
                .setContentText(WindowEvent.text(a))
                .setStyle(Notification.BigTextStyle().bigText(WindowEvent.text(a)))
                .setSmallIcon(R.drawable.ic_launcher)
                .setTimeoutAfter(WindowEvent.EXPIRY_MS)
                .setContentIntent(open)
                .setDeleteIntent(intent(c, a.id, WindowAnswer.TIMEOUT, "dismiss"))
                .setCategory(Notification.CATEGORY_MESSAGE)
                .setOnlyAlertOnce(true)
            actions.forEach { b.addAction(it) }
            nm(c).notify(ID, b.extend(Notification.WearableExtender().addActions(actions)).build())
        } catch (e: Exception) {
            Trace.w("window_prompt_failed", e)
        }
    }

    fun cancel(c: Context) = nm(c).cancel(ID)

    private fun post(c: Context, entry: (Store, Long) -> org.json.JSONObject, done: () -> Unit) {
        val app = c.applicationContext
        try {
            io.execute {
                try {
                    val store = Store(app)
                    val now = System.currentTimeMillis()
                    val e = entry(store, now)
                    ScanQueue.get(app).add(e, now)
                    Trace.i("window_label", "room" to e.getString("room"), "open" to e.getBoolean("open"), "answer" to e.optString("answer", "-"), "via" to e.optString("via", "-"), "prompt" to e.opt("prompt_id"))
                    ScanService.instance?.flush("window")
                    done()
                } catch (ex: Exception) {
                    Trace.w("window_label_failed", ex)
                    done()
                }
            }
        } catch (e: RejectedExecutionException) {
            Trace.i("window_label_rejected")
            done()
        }
    }

    fun answer(c: Context, id: String, answer: WindowAnswer, via: String, done: (String?) -> Unit = {}) {
        val app = c.applicationContext
        val ask = Store(app).takeWindow(id)
        if (ask == null) return done(null)
        cancel(app)
        val labelId = LabelEvent.newId()
        post(app, { s, now -> WindowEvent.answerEntry(s.device, now, ask, answer, via, labelId) }) { done(labelId) }
    }

    fun selfReport(c: Context, room: String, open: Boolean, done: (String) -> Unit = {}) {
        val app = c.applicationContext
        val labelId = LabelEvent.newId()
        post(app, { s, now -> WindowEvent.entry(s.device, now, room, open, null, labelId, via = "self") }) { done(labelId) }
    }

    fun lapse(c: Context, ask: WindowAsk?, now: Long) {
        if (ask == null) return
        val app = c.applicationContext
        val via = if (WindowEvent.expired(ask, now)) "expiry" else "replaced"
        post(app, { s, _ -> WindowEvent.answerEntry(s.device, WindowEvent.lapseTs(ask, now), ask, WindowAnswer.TIMEOUT, via) }) {}
    }

    fun sweep(c: Context) {
        val now = System.currentTimeMillis()
        val store = Store(c.applicationContext)
        val lapsed = store.lapseWindow(null, now)
        if (lapsed != null) {
            cancel(c)
            lapse(c, lapsed, now)
        }
    }

    fun void(c: Context, id: String) {
        val app = c.applicationContext
        try {
            io.execute {
                val now = System.currentTimeMillis()
                ScanQueue.get(app).add(LabelEvent.voidEntry(Store(app).device, id, now), now)
                Trace.i("window_void", "id" to id)
                ScanService.instance?.flush("window")
            }
        } catch (e: RejectedExecutionException) {
            Trace.i("window_label_rejected")
        }
    }
}

class WindowReceiver : BroadcastReceiver() {
    override fun onReceive(c: Context, i: Intent) {
        if (i.action != WindowPrompter.ACTION) return
        val id = i.getStringExtra("id") ?: return
        val answer = WindowAnswer.values().firstOrNull { it.wire == i.getStringExtra("answer") } ?: return
        val pending = goAsync()
        WindowPrompter.answer(c, id, answer, i.getStringExtra("via") ?: "notification") { pending.finish() }
    }
}
