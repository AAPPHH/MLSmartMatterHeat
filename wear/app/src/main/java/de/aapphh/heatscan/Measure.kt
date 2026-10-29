package de.aapphh.heatscan

import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.os.BatteryManager
import android.os.Handler
import android.os.SystemClock
import org.json.JSONArray
import org.json.JSONObject
import kotlin.math.sqrt

object Measure {
    const val SKIN_TYPE = 65555
    const val SKIN_STRING = "com.google.sensor.skin_temperature"
    const val PERIOD_S = 60
    const val LATENCY_US = 300_000_000
    const val ACCEL_HZ = 1
    const val BATCH_MAX = 50
    const val BATCH_AGE_MS = 300_000L
    const val ACCEL_MIN_N = 5
    const val FLUSH_WAIT_MS = 1_500L
    const val ROUTE_RETRY_MS = 3_600_000L
    const val DRAIN_MIN_MS = 3_600_000L
    const val DRAIN_MAX_MS = 12 * 3_600_000L
    const val KIND = "measure"
    const val KIND_SENSORS = "sensors"
    const val PATH = "/watch/measurement"
    const val PATH_SENSORS = "/watch/sensors"
    const val PATH_LABEL = "/watch/label"
    val META_KEYS = listOf("kind", "ts", "queued")
    val FALLBACK_CODES = setOf(400, 404, 405, 422)
}

data class Sample(
    val elapsedNs: Long?, val ts: Long, val type: String, val name: String, val value: Double, val unit: String,
    val sender: String? = null, val channel: Int? = null, val freq: Int? = null
) {
    fun json(): JSONObject = JSONObject().also { o ->
        if (elapsedNs != null) o.put("elapsed_ns", elapsedNs)
        o.put("ts", ts).put("sensor_type", type).put("sensor_name", name).put("value", value).put("unit", unit)
        sender?.let { o.put("sender", it) }
        channel?.let { o.put("channel", it) }
        freq?.let { o.put("freq", it) }
    }
}

object BatchId {
    fun sensor(ts: Long, seq: Long): Long = ts * 1000 + 500 + seq % 500

    fun scan(ts: Long, id: String?): Long = ts * 1000 + (id?.substringAfterLast('-')?.toLongOrNull() ?: 0L) % 500
}

class Batcher(private val maxSamples: Int = Measure.BATCH_MAX, private val maxAgeMs: Long = Measure.BATCH_AGE_MS) {
    private val buf = ArrayList<Sample>()
    private var openedAt = 0L

    val size: Int get() = buf.size

    fun add(s: Sample, nowMs: Long): List<Sample>? {
        if (buf.isEmpty()) openedAt = nowMs
        buf.add(s)
        return if (buf.size >= maxSamples) drain() else null
    }

    fun due(nowMs: Long): List<Sample>? = if (buf.isNotEmpty() && nowMs - openedAt >= maxAgeMs) drain() else null

    fun drain(): List<Sample> = buf.toList().also { buf.clear() }
}

object MeasureBatch {
    fun build(device: String, batchId: Long, sentTs: Long, clockOffsetMs: Long, appVersion: String, samples: List<Sample>, battPctPerHour: Double? = null): JSONObject =
        JSONObject().put("device", device).put("batch_id", batchId).put("sent_ts", sentTs).put("clock_offset_ms", clockOffsetMs)
            .put("app_version", appVersion)
            .also { o -> if (battPctPerHour != null && battPctPerHour.isFinite()) o.put("battery_pct_per_h", battPctPerHour) }
            .put("samples", JSONArray().also { a -> samples.forEach { a.put(it.json()) } })

    fun entry(body: JSONObject): JSONObject = body.put("kind", Measure.KIND).put("ts", body.getLong("sent_ts"))

    fun isMeasure(o: JSONObject) = o.optString("kind") == Measure.KIND

    fun body(o: JSONObject): String = JSONObject(o.toString()).also { b -> Measure.META_KEYS.forEach { b.remove(it) } }.toString()

    fun fromScan(e: JSONObject, appVersion: String, nowWall: Long, nowElapsedMs: Long): JSONObject? {
        val aps = e.optJSONArray("aps") ?: return null
        if (aps.length() == 0) return null
        val ts = e.getLong("ts")
        val el = if (e.has("elapsed_ns")) e.getLong("elapsed_ns") else null
        val offset = if (el != null) ts - el / 1_000_000 else nowWall - nowElapsedMs
        val samples = (0 until aps.length()).map { aps.getJSONObject(it) }.map {
            Sample(el, ts, "rssi", "wifi", it.getInt("rssi").toDouble(), "dBm", it.getString("bssid").lowercase(), null, it.getInt("freq"))
        }
        return build(e.getString("device"), BatchId.scan(ts, e.optString("id").ifEmpty { null }), nowWall, offset, appVersion, samples).also { o ->
            if (e.has("id")) o.put("id", e.getString("id"))
            listOf("motion", "power").forEach { k -> if (!e.isNull(k) && e.has(k)) o.put(k, e.getString(k)) }
            if (e.has("away")) o.put("away", e.getBoolean("away"))
            if (e.optBoolean("queued")) o.put("queued", true)
        }
    }
}

class AccelVar(private val intervalNs: Long, private val minN: Int = Measure.ACCEL_MIN_N) {
    private var bucket = Long.MIN_VALUE
    private var n = 0
    private var mean = 0.0
    private var m2 = 0.0

    fun add(elapsedNs: Long, magnitude: Double): Pair<Long, Double>? {
        val b = elapsedNs / intervalNs
        var out: Pair<Long, Double>? = null
        if (bucket != Long.MIN_VALUE && b != bucket) {
            if (n >= minN) out = Pair((bucket + 1) * intervalNs, m2 / n)
            n = 0
            mean = 0.0
            m2 = 0.0
        }
        bucket = b
        n++
        val d = magnitude - mean
        mean += d / n
        m2 += d * (magnitude - mean)
        return out
    }
}

class DrainMeter {
    private var anchorMs = 0L
    private var anchorPct = -1.0

    var perHour: Double? = null
        private set

    fun onReading(elapsedMs: Long, pct: Double, charging: Boolean) {
        if (charging || anchorPct < 0 || pct > anchorPct) {
            anchorMs = elapsedMs
            anchorPct = pct
            if (charging) perHour = null
            return
        }
        val span = elapsedMs - anchorMs
        if (span >= Measure.DRAIN_MIN_MS) perHour = (anchorPct - pct) * 3_600_000.0 / span
        if (span >= Measure.DRAIN_MAX_MS) {
            anchorMs = elapsedMs
            anchorPct = pct
        }
    }
}

object BatteryRead {
    fun pct(level: Int, scale: Int): Double? = if (level < 0 || scale <= 0) null else level * 100.0 / scale

    fun charging(plugged: Int, status: Int): Boolean = plugged != 0 || status == BatteryManager.BATTERY_STATUS_CHARGING
}

data class HealthPlan(val hr: Boolean, val skin: Boolean, val note: String)

object Health {
    const val HR = "android.permission.health.READ_HEART_RATE"
    const val SKIN = "android.permission.health.READ_SKIN_TEMPERATURE"
    const val SDK = 36

    fun requestable(sdk: Int): List<String> = if (sdk >= SDK) listOf(HR, SKIN) else emptyList()

    private fun state(present: Boolean, granted: Boolean) = when {
        !present -> "Sensor fehlt"
        !granted -> "Recht verweigert"
        else -> "ok"
    }

    fun plan(hrSensor: Boolean, hrGranted: Boolean, skinSensor: Boolean, skinGranted: Boolean): HealthPlan =
        HealthPlan(hrSensor && hrGranted, skinSensor && skinGranted, "hr=${state(hrSensor, hrGranted)} skin=${state(skinSensor, skinGranted)}")
}

data class SensorInfo(val name: String, val type: Int, val stringType: String, val vendor: String, val resolution: Double, val maxRange: Double, val wakeUp: Boolean) {
    fun json(): JSONObject = JSONObject().put("name", name).put("android_type", type).put("string_type", stringType).put("vendor", vendor)
        .put("resolution", resolution).put("max_range", maxRange)

    companion object {
        fun of(s: Sensor) = SensorInfo(s.name.orEmpty(), s.type, s.stringType.orEmpty(), s.vendor.orEmpty(), s.resolution.toDouble(), s.maximumRange.toDouble(), s.isWakeUpSensor)
    }
}

object Inventory {
    fun pick(all: List<SensorInfo>, type: Int, stringType: String? = null): Int {
        val idx = all.indices.filter { all[it].type == type || (stringType != null && all[it].stringType == stringType) }
        return idx.firstOrNull { !all[it].wakeUp } ?: idx.firstOrNull() ?: -1
    }

    fun entry(device: String, ts: Long, appVersion: String, all: List<SensorInfo>): JSONObject =
        JSONObject().put("kind", Measure.KIND_SENSORS).put("ts", ts).put("device", device).put("app_version", appVersion)
            .put("sensors", JSONArray().also { a -> all.forEach { a.put(it.json()) } })

    fun isSensors(o: JSONObject) = o.optString("kind") == Measure.KIND_SENSORS

    fun body(o: JSONObject): String = MeasureBatch.body(o)

    fun hash(appVersion: String, all: List<SensorInfo>): String =
        (appVersion + all.joinToString("|") { it.json().toString() }).hashCode().toString(36)
}

class RouteGate(private val retryMs: Long = Measure.ROUTE_RETRY_MS) {
    private var failedAt = Long.MIN_VALUE / 4

    fun allow(nowMs: Long) = nowMs - failedAt >= retryMs

    fun fail(nowMs: Long) {
        failedAt = nowMs
    }
}

object Special {
    fun isSpecial(o: JSONObject) = LabelEvent.isLabel(o) || MeasureBatch.isMeasure(o) || Inventory.isSensors(o)
}

class SensorSampler(
    private val ctx: Context, private val sm: SensorManager, private val store: Store, private val handler: Handler,
    private val appVersion: String, private val emit: (JSONObject) -> Unit
) {
    private val batcher = Batcher()
    private val drain = DrainMeter()
    private var accelVar = AccelVar(intervalNs())
    private var skin: Sensor? = null
    private var hr: Sensor? = null
    private var acc: Sensor? = null
    private var registered = false
    private val closeLater = Runnable { closeBatch() }

    private val listener = object : SensorEventListener {
        override fun onSensorChanged(e: SensorEvent) {
            onEvent(e)
        }

        override fun onAccuracyChanged(s: Sensor?, a: Int) {}
    }

    private fun intervalNs() = store.samplePeriodS * 1_000_000_000L

    fun infos(): List<SensorInfo> = sm.getSensorList(Sensor.TYPE_ALL).map { SensorInfo.of(it) }

    fun inventory(): JSONObject? {
        val all = infos()
        val h = Inventory.hash(appVersion, all)
        if (h == store.sensorsHash) return null
        store.sensorsHash = h
        return Inventory.entry(store.device, System.currentTimeMillis(), appVersion, all)
    }

    fun start() {
        if (registered) return
        val list = sm.getSensorList(Sensor.TYPE_ALL)
        val all = list.map { SensorInfo.of(it) }
        skin = Inventory.pick(all, Measure.SKIN_TYPE, Measure.SKIN_STRING).let { if (it >= 0) list[it] else null }
        hr = Inventory.pick(all, Sensor.TYPE_HEART_RATE).let { if (it >= 0) list[it] else null }
        acc = sm.getDefaultSensor(Sensor.TYPE_ACCELEROMETER)
        val plan = Health.plan(
            hr != null, ctx.checkSelfPermission(Health.HR) == PackageManager.PERMISSION_GRANTED,
            skin != null, ctx.checkSelfPermission(Health.SKIN) == PackageManager.PERMISSION_GRANTED
        )
        store.sensorNote = plan.note + " accel=" + (if (acc != null) "ok" else "Sensor fehlt")
        Trace.i("sensors", "plan" to store.sensorNote, "periodS" to store.samplePeriodS)
        accelVar = AccelVar(intervalNs())
        val period = store.samplePeriodS * 1_000_000
        if (plan.skin) reg(skin, period)
        if (plan.hr) reg(hr, period)
        reg(acc, 1_000_000 / Measure.ACCEL_HZ)
        registered = true
    }

    private fun reg(s: Sensor?, periodUs: Int) {
        if (s == null) return
        try {
            if (!sm.registerListener(listener, s, periodUs, Measure.LATENCY_US)) Trace.i("sensor_register", "name" to s.name, "ok" to false)
        } catch (e: Exception) {
            Trace.w("sensor_register", e, "name" to s.name)
        }
    }

    fun stop() {
        handler.removeCallbacks(closeLater)
        if (registered) {
            try { sm.flush(listener) } catch (e: Exception) { Trace.w("sensor_flush", e) }
            sm.unregisterListener(listener)
        }
        registered = false
        closeBatch()
    }

    fun restart() {
        stop()
        start()
    }

    fun flushFifo() {
        if (!registered) return
        try { sm.flush(listener) } catch (e: Exception) { Trace.w("sensor_flush", e) }
    }

    fun onLabel() {
        flushFifo()
        closeBatch()
        handler.removeCallbacks(closeLater)
        handler.postDelayed(closeLater, Measure.FLUSH_WAIT_MS)
    }

    fun onOffBody(e: SensorEvent, off: Boolean) {
        add(Sample(e.timestamp, wall(e.timestamp), "offbody", e.sensor.name.orEmpty(), if (off) 1.0 else 0.0, "bool"))
    }

    fun onPower() {
        batterySamples().forEach { add(it) }
    }

    fun closeIfDue() {
        batcher.due(SystemClock.elapsedRealtime())?.let { close(it) }
    }

    fun closeBatch() {
        if (batcher.size > 0) close(batcher.drain())
    }

    private fun wall(elapsedNs: Long): Long = Capture.wallMs(System.currentTimeMillis(), SystemClock.elapsedRealtime(), elapsedNs / 1000)

    private fun add(s: Sample) {
        batcher.add(s, SystemClock.elapsedRealtime())?.let { close(it) }
    }

    private fun onEvent(e: SensorEvent) {
        val v = e.values[0].toDouble()
        val name = e.sensor.name.orEmpty()
        when {
            e.sensor === acc -> {
                val m = sqrt(e.values[0].toDouble() * e.values[0] + e.values[1].toDouble() * e.values[1] + e.values[2].toDouble() * e.values[2])
                accelVar.add(e.timestamp, m)?.let { (end, variance) -> add(Sample(end, wall(end), "motion", name, variance, "m2/s4")) }
            }
            !v.isFinite() -> {}
            e.sensor === skin -> add(Sample(e.timestamp, wall(e.timestamp), "temp", name, v, "C"))
            e.sensor === hr -> if (v > 0.0) add(Sample(e.timestamp, wall(e.timestamp), "hr", name, v, "bpm"))
        }
        closeIfDue()
    }

    private fun batterySamples(): List<Sample> {
        val i = ctx.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED)) ?: return emptyList()
        val pct = BatteryRead.pct(i.getIntExtra(BatteryManager.EXTRA_LEVEL, -1), i.getIntExtra(BatteryManager.EXTRA_SCALE, -1)) ?: return emptyList()
        val charging = BatteryRead.charging(i.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0), i.getIntExtra(BatteryManager.EXTRA_STATUS, 0))
        val el = SystemClock.elapsedRealtimeNanos()
        val ts = System.currentTimeMillis()
        drain.onReading(el / 1_000_000, pct, charging)
        drain.perHour?.let { store.battPerHour = it.toFloat() }
        return listOf(Sample(el, ts, "battery", "battery", pct, "%"), Sample(el, ts, "charging", "battery", if (charging) 1.0 else 0.0, "bool"))
    }

    private fun close(samples: List<Sample>) {
        val wall = System.currentTimeMillis()
        val all = samples + batterySamples()
        val offset = wall - SystemClock.elapsedRealtimeNanos() / 1_000_000
        emit(MeasureBatch.entry(MeasureBatch.build(store.device, BatchId.sensor(wall, store.nextBatchSeq()), wall, offset, appVersion, all, drain.perHour)))
    }
}
