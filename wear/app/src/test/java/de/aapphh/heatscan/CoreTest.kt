package de.aapphh.heatscan

import org.json.JSONObject
import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class CoreTest {
    private val example = JSONObject(javaClass.classLoader!!.getResource("scan_example.json")!!.readText())
    private val aps = listOf(
        Ap("AA:BB:CC:00:00:01", -45, 2412),
        Ap("aa:bb:cc:00:00:02", -67, 5180),
        Ap("aa:bb:cc:00:00:03", -82, 2437),
        Ap("aa:bb:cc:00:00:99", -90, 2462)
    )

    private fun shape(o: JSONObject): Map<String, String> =
        o.keys().asSequence().associateWith { k -> o.get(k).let { if (it === JSONObject.NULL) "null" else it.javaClass.simpleName } }

    @Test
    fun payloadMatchesContract() {
        val built = Payload.build(example.getLong("ts"), "pixel-watch", "wohnzimmer", aps)
        assertTrue(built.keys().asSequence().toSet().containsAll(example.keys().asSequence().toSet()))
        assertEquals(shape(example), shape(built).filterKeys { it in example.keys().asSequence().toSet() })
        assertEquals(setOf("motion", "power", "away"), built.keys().asSequence().toSet() - example.keys().asSequence().toSet())
        assertEquals(example.getLong("ts"), built.getLong("ts"))
        assertEquals("wohnzimmer", built.getString("label"))
        val exAps = example.getJSONArray("aps")
        val bAps = built.getJSONArray("aps")
        assertEquals(exAps.length(), bAps.length())
        for (i in 0 until exAps.length()) {
            assertEquals(shape(exAps.getJSONObject(i)), shape(bAps.getJSONObject(i)))
            assertEquals(exAps.getJSONObject(i).getString("bssid"), bAps.getJSONObject(i).getString("bssid"))
            assertEquals(exAps.getJSONObject(i).getInt("rssi"), bAps.getJSONObject(i).getInt("rssi"))
            assertEquals(exAps.getJSONObject(i).getInt("freq"), bAps.getJSONObject(i).getInt("freq"))
        }
    }

    @Test
    fun noLabelSerializesAsNull() {
        val built = Payload.build(1L, "pixel-watch", null, aps)
        assertTrue(built.has("label"))
        assertTrue(built.isNull("label"))
        assertTrue(built.toString().contains("\"label\":null"))
    }

    @Test
    fun dedupAcceptsOnlyNewerTimestamps() {
        val d = Dedup()
        assertTrue(d.accept(100))
        assertFalse(d.accept(100))
        assertFalse(d.accept(50))
        assertTrue(d.accept(101))
    }

    @Test
    fun diagnosticsPickCause() {
        assertEquals("Standort aus", Diag.emptyScan(false, false))
        assertEquals("WLAN aus", Diag.emptyScan(true, false))
        assertEquals("keine Scanergebnisse", Diag.emptyScan(true, true))
        assertEquals(null, Diag.permissionIssue(true, false))
        assertEquals("Nur ungefährer Standort, genauer nötig", Diag.permissionIssue(false, true))
        assertEquals("Standort verweigert", Diag.permissionIssue(false, false))
        assertTrue(Diag.needsLocalNetwork(37, false))
        assertFalse(Diag.needsLocalNetwork(37, true))
        assertFalse(Diag.needsLocalNetwork(36, false))
        assertEquals("ERR X (LAN-Zugriff fehlt)", Diag.postError("X", true))
        assertEquals("ERR X", Diag.postError("X", false))
    }

    private val s = 1000L

    @Test
    fun startScansImmediatelyThenEvery30sWhileMoving() {
        val p = ScanPolicy(true, 0)
        assertTrue(p.onTick(0))
        assertEquals(30 * s, p.delay(0))
        assertFalse(p.onTick(10 * s))
        p.onMotion(25 * s, 25 * s)
        assertTrue(p.onTick(30 * s))
        assertEquals(Mode.MOVING, p.mode)
    }

    @Test
    fun settlesWithOneExtraScanThenStillAfterTwoMinutes() {
        val p = ScanPolicy(true, 0)
        p.onTick(0)
        p.onMotion(30 * s, 30 * s)
        assertTrue(p.onTick(30 * s))
        assertFalse(p.onTick(55 * s))
        assertTrue(p.onTick(90 * s))
        assertEquals(Mode.SETTLING, p.mode)
        assertEquals(60 * s, p.delay(90 * s))
        assertFalse(p.onTick(120 * s))
        assertEquals(Mode.SETTLING, p.mode)
        assertFalse(p.onTick(150 * s))
        assertEquals(Mode.STILL, p.mode)
        assertEquals(ScanConfig.HEARTBEAT_MS, p.delay(150 * s))
    }

    @Test
    fun stillScansOnlyOnHeartbeat() {
        val p = still()
        assertFalse(p.onTick(150 * s + 719 * s))
        assertTrue(p.onTick(150 * s + 720 * s))
        assertEquals(ScanConfig.HEARTBEAT_MS, p.delay(870 * s))
        assertEquals(Mode.STILL, p.mode)
    }

    @Test
    fun freshMotionWakesFromStillWithImmediateScan() {
        val p = still()
        assertTrue(p.onMotion(300 * s, 299 * s))
        assertEquals(Mode.MOVING, p.mode)
        assertEquals(30 * s, p.delay(300 * s))
        assertFalse(p.onMotion(301 * s, 301 * s))
    }

    @Test
    fun motionRightAfterEnteringStillIsIgnored() {
        val p = still()
        assertFalse(p.onMotion(151 * s, 150 * s + 100))
        assertEquals(Mode.STILL, p.mode)
        assertFalse(p.onMotion(152 * s, 151 * s))
        assertEquals(Mode.STILL, p.mode)
        assertFalse(p.onMotion(154 * s, 154 * s))
        assertEquals(Mode.STILL, p.mode)
        assertEquals(ScanConfig.HEARTBEAT_MS - 4 * s, p.delay(154 * s))
    }

    @Test
    fun motionAfterGraceWakesFromStill() {
        val p = still()
        assertTrue(p.onMotion(156 * s, 155 * s + ScanConfig.STILL_GRACE_MS - 5 * s + 1))
        assertEquals(Mode.MOVING, p.mode)
    }

    @Test
    fun graceRestartsOnEveryStillEntry() {
        val p = still()
        assertTrue(p.onMotion(300 * s, 300 * s))
        p.onTick(360 * s)
        p.onTick(430 * s)
        assertEquals(Mode.STILL, p.mode)
        assertFalse(p.onMotion(432 * s, 431 * s))
        assertEquals(Mode.STILL, p.mode)
    }

    @Test
    fun staleBufferedMotionIsIgnoredInStill() {
        val p = still()
        assertFalse(p.onMotion(300 * s, 270 * s))
        assertEquals(Mode.STILL, p.mode)
        assertFalse(p.onMotion(300 * s, 279 * s))
        assertEquals(Mode.STILL, p.mode)
        assertTrue(p.onMotion(300 * s, 280 * s))
    }

    @Test
    fun motionTimeComesFromEventNotDelivery() {
        val p = ScanPolicy(true, 0)
        p.onTick(0)
        p.onMotion(40 * s, 30 * s)
        p.onTick(30 * s)
        p.onTick(60 * s)
        assertTrue(p.onTick(90 * s))
        assertEquals(Mode.SETTLING, p.mode)
    }

    @Test
    fun noExtraScanWithin20sOfPreviousOne() {
        val p = still()
        assertTrue(p.onTick(870 * s))
        assertFalse(p.onMotion(880 * s, 880 * s))
        assertEquals(Mode.MOVING, p.mode)
        assertEquals(10 * s, p.delay(880 * s))
        assertTrue(p.onTick(890 * s))
    }

    @Test
    fun settlingTransitionScanSkippedRightAfterPeriodicScan() {
        val p = ScanPolicy(true, 0)
        assertTrue(p.onTick(0))
        assertTrue(p.onTick(30 * s))
        p.onMotion(40 * s, 40 * s)
        assertTrue(p.onTick(60 * s))
        assertTrue(p.onTick(90 * s))
        assertFalse(p.onTick(100 * s))
        assertEquals(Mode.SETTLING, p.mode)
    }

    @Test
    fun motionWhileSettlingReturnsToMovingWithoutExtraScan() {
        val p = ScanPolicy(true, 0)
        p.onTick(0)
        assertTrue(p.onTick(60 * s))
        assertFalse(p.onMotion(80 * s, 80 * s))
        assertEquals(Mode.MOVING, p.mode)
        assertEquals(30 * s, p.delay(80 * s))
    }

    @Test
    fun fixedFallbackIgnoresMotionAndScansEvery30s() {
        val p = ScanPolicy(false, 0)
        assertEquals(Mode.FIXED, p.mode)
        assertTrue(p.onTick(0))
        assertFalse(p.onMotion(5 * s, 5 * s))
        assertFalse(p.onTick(29 * s))
        assertTrue(p.onTick(30 * s))
        assertEquals(30 * s, p.delay(30 * s))
        assertEquals(Mode.FIXED, p.mode)
    }

    @Test
    fun awayWhenNoHomeSsidChecksEvery15MinAndIgnoresSteps() {
        val p = ScanPolicy(true, 0)
        assertTrue(p.onTick(0))
        p.onScan(false, 1 * s)
        assertEquals(Mode.MOVING, p.mode)
        repeat(ScanConfig.AWAY_AFTER_MISSES - 1) { p.onScan(false, 1 * s) }
        assertEquals(Mode.AWAY, p.mode)
        assertEquals("Unterwegs", p.mode.label)
        assertFalse(p.onMotion(100 * s, 100 * s))
        assertEquals(Mode.AWAY, p.mode)
        assertEquals(ScanConfig.AWAY_MS, p.delay(1 * s))
        assertFalse(p.onTick(500 * s))
        assertTrue(p.onTick(1 * s + ScanConfig.AWAY_MS))
        p.onScan(false, 2 * s + ScanConfig.AWAY_MS)
        assertEquals(Mode.AWAY, p.mode)
    }

    @Test
    fun homeSsidSeenReturnsToAdaptiveBehaviour() {
        val p = ScanPolicy(true, 0)
        p.onTick(0)
        repeat(ScanConfig.AWAY_AFTER_MISSES) { p.onScan(false, 1 * s) }
        val back = ScanConfig.AWAY_MS + 1 * s
        p.onTick(back)
        p.onScan(true, back)
        assertEquals(Mode.MOVING, p.mode)
        assertEquals(30 * s, p.delay(back))
        assertFalse(p.onMotion(back + s, back + s))
        val f = ScanPolicy(false, 0)
        f.onScan(false, 0)
        f.onScan(true, 5 * s)
        assertEquals(Mode.FIXED, f.mode)
    }

    @Test
    fun homeScanKeepsModeWhenAlreadyHome() {
        val p = still()
        p.onScan(true, 200 * s)
        assertEquals(Mode.STILL, p.mode)
    }

    @Test
    fun homeSsidMatching() {
        val home = Home.parse("FRITZ!Box 1234 AB, Other")
        assertEquals(listOf("FRITZ!Box 1234 AB", "Other"), home)
        assertTrue(Home.isHome(listOf("x", "FRITZ!Box 1234 AB"), home))
        assertTrue(Home.isHome(listOf("\"Other\""), home))
        assertFalse(Home.isHome(listOf("x", "", "FRITZ!Box 1234 AB"), home))
        assertFalse(Home.isHome(emptyList(), home))
        assertTrue(Home.isHome(listOf("x"), Home.parse(" , ")))
        assertEquals(listOf(Defaults.HOME), Home.parse(Defaults.HOME))
    }

    @Test
    fun roomsAreTheFiveFixedOnes() {
        assertEquals(listOf("wohnzimmer", "schlafzimmer", "bad", "kueche", "flur"), Defaults.ROOMS)
        assertEquals(Defaults.ROOMS.toSet(), Defaults.CAPTIONS.keys)
    }

    @Test
    fun labelToggle() {
        assertEquals("bad", Labels.toggle(null, "bad"))
        assertEquals("bad", Labels.toggle("flur", "bad"))
        assertEquals(null, Labels.toggle("bad", "bad"))
    }

    @Test
    fun tileClickOnlyAcceptsCurrentNonce() {
        assertEquals("bad", TileClick.room(TileClick.id("bad", 3), 3))
        assertEquals(null, TileClick.room(TileClick.id("bad", 3), 4))
        assertEquals(null, TileClick.room(TileClick.id("buero", 3), 3))
        assertEquals(null, TileClick.room("open", 3))
        assertEquals(null, TileClick.room("", 0))
    }

    @Test
    fun tileStatusLine() {
        assertEquals("Ruhe | 12:00:00 200", TileClick.statusLine(true, "Ruhe", "12:00:00 200"))
        assertEquals("Scan aus | -", TileClick.statusLine(false, "Ruhe", "-"))
        assertEquals("-", TileClick.statusLine(true, "", "-"))
    }

    private fun still(): ScanPolicy = ScanPolicy(true, 0).also {
        it.onTick(0)
        it.onTick(60 * s)
        it.onTick(150 * s)
    }

    @Test
    fun summaryLineAndTraceFormat() {
        assertEquals("ev a=1 b=x_y", Trace.fmt("ev", "a" to 1, "b" to "x y"))
        assertEquals(
            "status running=true paused=false mode=Ruhe label=- server=http://s home=- atHome=true lastPost=12:00:00_200 ok=3 failed=1 lastError=-",
            Summary.line(true, false, "Ruhe", null, "http://s", "", true, "12:00:00 200", 3, 1, "-")
        )
        assertTrue(Summary.line(false, true, "", "bad", "s", "h", null, "-", 0, 0, "-").contains("atHome=- "))
    }

    @Test
    fun postOkOnlyFor2xx() {
        assertTrue(Diag.postOk("200 12 APs"))
        assertFalse(Diag.postOk("500 12 APs"))
        assertFalse(Diag.postOk("ERR SocketTimeoutException"))
    }

    private fun entry(ts: Long, label: String? = null) = Payload.build(ts, "d", label, emptyList())

    @Test
    fun extendedPayloadCarriesMotionPowerAway() {
        val b = Payload.build(5, "d", null, aps, "still", "doze", true)
        assertEquals("still", b.getString("motion"))
        assertEquals("doze", b.getString("power"))
        assertTrue(b.getBoolean("away"))
        val n = Payload.build(5, "d", null, emptyList())
        assertTrue(n.isNull("motion") && n.isNull("power"))
        assertFalse(n.getBoolean("away"))
        assertEquals(0, n.getJSONArray("aps").length())
    }

    @Test
    fun powerStatePriority() {
        assertEquals(null, PowerState.of(null, null, null))
        assertEquals("normal", PowerState.of(false, false, null))
        assertEquals("saver", PowerState.of(true, false, false))
        assertEquals("doze", PowerState.of(true, true, null))
        assertEquals("offbody", PowerState.of(false, true, true))
    }

    @Test
    fun modeMotionAndAwayFlag() {
        val p = ScanPolicy(true, 0)
        assertEquals("moving", p.mode.motion)
        assertEquals("still", still().mode.motion)
        assertEquals(null, ScanPolicy(false, 0).mode.motion)
        assertFalse(p.away)
        repeat(ScanConfig.AWAY_AFTER_MISSES - 1) { p.onScan(false, 1 * s) }
        assertFalse(p.away)
        p.onScan(false, 2 * s)
        assertTrue(p.away)
        p.onScan(true, 3 * s)
        assertFalse(p.away)
    }

    @Test
    fun heartbeatIsTwelveMinutes() {
        assertEquals(12 * 60_000L, ScanConfig.HEARTBEAT_MS)
        assertTrue(3_600_000L / ScanConfig.HEARTBEAT_MS <= 7)
    }

    @Test
    fun captureTimestampFromScanAge() {
        assertEquals(1_000_000L - 30_000, Capture.wallMs(1_000_000, 100_000, 70_000_000))
        assertEquals(1_000_000L, Capture.wallMs(1_000_000, 100_000, 100_000_000))
        assertEquals(1_000_000L, Capture.wallMs(1_000_000, 100_000, 120_000_000))
    }

    @Test
    fun queueDropsOldestAtCapAndKeepsOrder() {
        val q = ScanQueue(null, 3, 1_000_000)
        assertEquals(0, q.add(entry(10), 10))
        q.add(entry(11), 11)
        q.add(entry(12), 12)
        assertEquals(1, q.add(entry(13), 13))
        assertEquals(3, q.size)
        assertEquals(listOf(11L, 12L, 13L), q.peek(10, 13).map { it.json.getLong("ts") })
    }

    @Test
    fun queueDropsEntriesOlderThanMaxAge() {
        val q = ScanQueue(null, 10, 100)
        q.add(entry(0), 0)
        q.add(entry(50), 50)
        assertEquals(1, q.add(entry(150), 150))
        assertEquals(listOf(50L, 150L), q.peek(10, 150).map { it.json.getLong("ts") })
    }

    @Test
    fun queueMarksOnlyOldEntriesQueued() {
        val q = ScanQueue(null, 10, 10_000_000)
        q.add(entry(0), 0)
        q.add(entry(100_000), 100_000)
        val out = q.peek(10, 100_000 + ScanConfig.QUEUED_AFTER_MS).map { it.json }
        assertTrue(out[0].optBoolean("queued"))
        assertFalse(out[1].has("queued"))
        assertFalse(q.peek(10, 100_000 + ScanConfig.QUEUED_AFTER_MS - 100_000)[0].json.has("queued"))
    }

    @Test
    fun queuePersistsAppendsAndRemovals() {
        val f = File.createTempFile("queue", ".jsonl")
        try {
            val q = ScanQueue(f, 5, 1_000_000)
            for (t in 1L..3L) q.add(entry(t), t)
            assertEquals(3, ScanQueue(f, 5, 1_000_000).size)
            q.remove(q.peek(2, 3)[1].seq)
            val again = ScanQueue(f, 5, 1_000_000)
            assertEquals(listOf(3L), again.peek(5, 3).map { it.json.getLong("ts") })
            f.appendText("garbage\n")
            assertEquals(1, ScanQueue(f, 5, 1_000_000).size)
            for (t in 4L..9L) q.add(entry(t), t)
            assertEquals(5, ScanQueue(f, 5, 1_000_000).size)
        } finally {
            f.delete()
        }
    }

    @Test
    fun flushBatchesInOrderAndEmptiesQueue() {
        val q = ScanQueue(null, 100, 1_000_000)
        for (t in 1L..5L) q.add(entry(t), t)
        val sizes = mutableListOf<Int>()
        val seen = mutableListOf<Long>()
        val r = Flusher(2).run(q, 5, { a ->
            sizes.add(a.length())
            for (i in 0 until a.length()) seen.add(a.getJSONObject(i).getLong("ts"))
            200
        }, { 500 })
        assertEquals(listOf(2, 2, 1), sizes)
        assertEquals(listOf(1L, 2L, 3L, 4L, 5L), seen)
        assertEquals(5, r.sent)
        assertEquals(0, q.size)
    }

    @Test
    fun flushKeepsQueueOnFailureAndStops() {
        val q = ScanQueue(null, 100, 1_000_000)
        for (t in 1L..4L) q.add(entry(t), t)
        var calls = 0
        val r = Flusher(2).run(q, 4, { calls++; if (calls == 1) 200 else 503 }, { 200 })
        assertEquals(2, r.sent)
        assertEquals(503, r.code)
        assertEquals(2, q.size)
        assertEquals(2, calls)
        assertEquals(-1, Flusher(2).run(q, 4, { -1 }, { -1 }).code)
        assertEquals(2, q.size)
    }

    @Test
    fun flushFallsBackToSingleOn404AndRemembers() {
        val q = ScanQueue(null, 100, 1_000_000)
        for (t in 1L..3L) q.add(entry(t), t)
        val f = Flusher(10)
        var batchCalls = 0
        val singles = mutableListOf<Long>()
        val r = f.run(q, 3, { batchCalls++; 404 }, { singles.add(it.getLong("ts")); 200 })
        assertEquals(1, batchCalls)
        assertEquals(listOf(1L, 2L, 3L), singles)
        assertEquals(3, r.sent)
        assertTrue(f.singleOnly)
        q.add(entry(4), 4)
        f.run(q, 4, { batchCalls++; 404 }, { singles.add(it.getLong("ts")); 200 })
        assertEquals(1, batchCalls)
        assertEquals(4, singles.size)
    }

    @Test
    fun flushDropsPermanentlyRejectedBatchButNotTransientOnes() {
        val q = ScanQueue(null, 100, 1_000_000)
        q.add(entry(1), 1)
        val r = Flusher(10).run(q, 1, { 400 }, { 400 })
        assertEquals(0, r.sent)
        assertEquals(0, q.size)
        q.add(entry(2), 2)
        Flusher(10).run(q, 2, { 429 }, { 200 })
        assertEquals(1, q.size)
    }

    @Test
    fun flushMarksStaleEntriesQueuedInBatch() {
        val q = ScanQueue(null, 100, 10_000_000)
        q.add(entry(0), 0)
        q.add(entry(200_000), 200_000)
        val flags = mutableListOf<Boolean>()
        Flusher(10).run(q, 200_000, { a -> for (i in 0 until a.length()) flags.add(a.getJSONObject(i).optBoolean("queued")); 200 }, { 200 })
        assertEquals(listOf(true, false), flags)
    }

    @Test
    fun removeBySeqSurvivesHeadDroppedDuringFlush() {
        val q = ScanQueue(null, 3, 1_000_000)
        for (t in 1L..3L) q.add(entry(t), t)
        val sent = q.peek(2, 3)
        q.add(entry(4), 4)
        q.add(entry(5), 5)
        q.remove(sent.last().seq)
        assertEquals(listOf(3L, 4L, 5L), q.peek(10, 5).map { it.json.getLong("ts") })
    }

    @Test
    fun removeBySeqPersistsAtomicallyAndLazily() {
        val f = File.createTempFile("queue", ".jsonl")
        try {
            val q = ScanQueue(f, 10, 1_000_000)
            for (t in 1L..4L) q.add(entry(t), t)
            val seqs = q.peek(4, 4).map { it.seq }
            q.remove(seqs[0], false)
            q.remove(seqs[1], false)
            assertEquals(4, ScanQueue(f, 10, 1_000_000).size)
            q.commit()
            assertEquals(listOf(3L, 4L), ScanQueue(f, 10, 1_000_000).peek(10, 4).map { it.json.getLong("ts") })
            assertFalse(File(f.path + ".tmp").exists())
        } finally {
            f.delete()
        }
    }

    @Test
    fun entryIdIsStableAcrossPeeksAndReload() {
        val f = File.createTempFile("queue", ".jsonl")
        try {
            val q = ScanQueue(f, 10, 1_000_000)
            q.add(Payload.build(1, "pixel-watch", null, emptyList(), id = EntryId.format("pixel-watch", 123)), 1)
            val first = q.peek(1, 1)[0].json.getString("id")
            assertEquals("pixel-watch-000123", first)
            assertEquals(first, q.peek(1, 1)[0].json.getString("id"))
            assertEquals(first, ScanQueue(f, 10, 1_000_000).peek(1, 1)[0].json.getString("id"))
            assertFalse(Payload.build(1, "d", null, emptyList()).has("id"))
        } finally {
            f.delete()
        }
    }

    @Test
    fun retriedFlushResendsSameIds() {
        val q = ScanQueue(null, 100, 1_000_000)
        for (t in 1L..3L) q.add(Payload.build(t, "d", null, emptyList(), id = EntryId.format("d", t)), t)
        val ids = mutableListOf<List<String>>()
        val f = Flusher(10)
        f.run(q, 3, { a -> ids.add((0 until a.length()).map { a.getJSONObject(it).getString("id") }); -1 }, { -1 })
        f.run(q, 3, { a -> ids.add((0 until a.length()).map { a.getJSONObject(it).getString("id") }); 200 }, { 200 })
        assertEquals(ids[0], ids[1])
        assertEquals(0, q.size)
    }

    @Test
    fun scanRateWindowAllowsFourPerTwoMinutes() {
        val s = 1000L
        val p = ScanPolicy(false, 0)
        val fired = (0..13).map { i -> p.onTick(i * 10 * s) to i * 10 * s }.filter { it.first }.map { it.second / s }
        assertEquals(listOf(0L, 30L, 60L, 90L, 120L), fired.take(5))
        val q = ScanPolicy(true, 0)
        var n = 0
        for (t in 0L..119L step 20) if (q.onMotion(t * s, t * s)) n++
        assertTrue(n <= ScanConfig.SCAN_WINDOW_MAX)
    }

    @Test
    fun rateWindowDefersFifthScanUntilOldestExpires() {
        val s = 1000L
        val p = ScanPolicy(true, 0)
        val fired = mutableListOf<Long>()
        for (t in 0L..300L) {
            p.onMotion(t * s, t * s)
            if (p.onTick(t * s)) fired.add(t)
        }
        for (i in 4 until fired.size) assertTrue(fired[i] - fired[i - 4] >= 120)
    }

    @Test
    fun batchRejectionFallsBackToSingleSends() {
        val q = ScanQueue(null, 100, 1_000_000)
        for (t in 1L..3L) q.add(entry(t), t)
        val f = Flusher(10)
        val singles = mutableListOf<Long>()
        val r = f.run(q, 3, { 422 }, { e -> singles.add(e.getLong("ts")); if (e.getLong("ts") == 2L) 400 else 200 })
        assertEquals(listOf(1L, 2L, 3L), singles)
        assertEquals(2, r.sent)
        assertEquals(0, q.size)
        assertFalse(f.singleOnly)
    }

    @Test
    fun batchTooLargeHalvesBatchSize() {
        val q = ScanQueue(null, 100, 1_000_000)
        for (t in 1L..8L) q.add(entry(t), t)
        val sizes = mutableListOf<Int>()
        val f = Flusher(8)
        val r = f.run(q, 8, { a -> sizes.add(a.length()); if (a.length() > 2) 413 else 200 }, { 500 })
        assertEquals(listOf(8, 4, 2, 2, 2, 2), sizes.take(6))
        assertEquals(8, r.sent)
        assertEquals(0, q.size)
    }

    @Test
    fun highlightPrefersLabel() {
        assertEquals(Pair(null, null), Highlight.of(null, null))
        assertEquals(Pair("flur", null), Highlight.of(null, "flur"))
        assertEquals(Pair("bad", "bad"), Highlight.of("bad", null))
        assertEquals(Pair("bad", "bad"), Highlight.of("bad", "bad"))
        assertEquals(Pair("flur", "bad"), Highlight.of("bad", "flur"))
    }

    @Test
    fun quartersHitTest() {
        val d = 213f
        assertEquals("wohnzimmer", Quarters.room(0f, 0f, d))
        assertEquals("wohnzimmer", Quarters.room(0f, -41f, d))
        assertEquals("schlafzimmer", Quarters.room(0f, -77f, d))
        assertEquals("bad", Quarters.room(77f, 0f, d))
        assertEquals("kueche", Quarters.room(0f, 77f, d))
        assertEquals("flur", Quarters.room(-77f, 0f, d))
        assertEquals("schlafzimmer", Quarters.room(50f, -51f, d))
        assertEquals("bad", Quarters.room(51f, -50f, d))
        assertEquals("flur", Quarters.room(-60f, -59f, d))
        assertEquals("bad", Quarters.room(60f, 40f, 300f))
        assertEquals(listOf(0f, 90f, 180f, 270f), Quarters.WEDGES.map { Quarters.mid(it) })
    }

    private val replyText = """{"stored":4,"room":"wohnzimmer","probability":0.8,"rooms":[
        {"room":"bad","p":0.1,"parts":[{"bssid":"AA:01","share":0.1}]},
        {"room":"wohnzimmer","p":0.8,"parts":[{"bssid":"aa:01","share":0.4},{"bssid":"aa:02","share":0.2},{"bssid":"aa:03","share":0.1},{"bssid":"aa:04","share":0.05},{"bssid":"aa:05","share":0.02},{"bssid":"aa:06","share":0.01}]}],
        "aps":[{"bssid":"aa:01","rssi":-45,"freq":2412}]}"""

    @Test
    fun replyParsingIsTolerant() {
        val r = ReplyJson.parse(replyText)
        assertEquals(0.8f, r.probability!!, 1e-6f)
        assertEquals(2, r.rooms.size)
        assertEquals("aa:01", r.rooms[0].parts[0].bssid)
        assertEquals(listOf(Ap("aa:01", -45, 2412)), r.aps)
        val old = ReplyJson.parse("""{"stored":4,"room":"wohnzimmer"}""")
        assertEquals(null, old.probability)
        assertTrue(old.rooms.isEmpty() && old.aps.isEmpty())
        val nul = ReplyJson.parse("""{"probability":null,"rooms":[],"aps":[]}""")
        assertEquals(null, nul.probability)
        assertTrue(ReplyJson.parse("garbage").rooms.isEmpty())
        assertTrue(ReplyJson.parse("""{"rooms":[{"room":"x"}]}""").rooms.isEmpty())
    }

    @Test
    fun jsonRoundTrip() {
        val r = ReplyJson.parse(replyText)
        assertEquals(r.rooms, ReplyJson.rooms(ReplyJson.roomsJson(r.rooms)))
        assertEquals(r.aps, ReplyJson.aps(ReplyJson.apsJson(r.aps)))
        val named = listOf(NamedAp("aa:01", "Home", -45, 2412), NamedAp("aa:02", "", -60, 5180))
        assertEquals(named, ReplyJson.named(ReplyJson.namedJson(named)))
        assertTrue(ReplyJson.rooms(null).isEmpty())
        assertTrue(ReplyJson.named("not json").isEmpty())
    }

    @Test
    fun ringOrderAndCaptions() {
        assertEquals(Defaults.ROOMS, Rings.order(emptyList()).map { it.room })
        assertTrue(Rings.order(emptyList()).all { it.p == 0f })
        assertEquals(listOf("wohnzimmer", "bad"), Rings.order(ReplyJson.parse(replyText).rooms.reversed().sortedBy { it.p }).map { it.room })
        assertEquals(listOf("Wo", "Sch", "Bad", "Kü", "Fl"), Defaults.ROOMS.map { Rings.caption(it) })
        assertEquals("Wohnen", Rings.name("wohnzimmer"))
    }

    @Test
    fun segmentsAndColours() {
        val rooms = ReplyJson.parse(replyText).rooms
        val palette = Rings.palette(rooms)
        assertEquals(listOf("aa:01", "aa:02", "aa:03", "aa:04", "aa:05", "aa:06"), palette)
        val colors = ColorBook.assign(emptyMap(), palette, 1L)
        val pal = ColorBook.PALETTE
        val wohn = Rings.segments(rooms.first { it.room == "wohnzimmer" }, colors)
        assertEquals(pal + Rings.REST, wohn.map { it.color })
        assertEquals(0.4f * 270f, wohn[0].sweep, 1e-3f)
        assertEquals(0.8f * 270f, wohn.sumOf { it.sweep.toDouble() }.toFloat(), 1e-2f)
        val bad = Rings.segments(rooms.first { it.room == "bad" }, colors)
        assertEquals(listOf(pal[0]), bad.map { it.color })
        assertEquals(0.1f * 270f, bad[0].sweep, 1e-3f)
        val few = ColorBook.assign(emptyMap(), listOf("aa:01"), 1L)
        val other = Rings.segments(rooms.first { it.room == "wohnzimmer" }, few)
        assertEquals(listOf(pal[0], ColorBook.OTHER, Rings.REST), other.map { it.color })
        val unexplained = Rings.segments(RoomP("flur", 0.3f, emptyList()), colors)
        assertEquals(listOf(Rings.REST), unexplained.map { it.color })
        assertEquals(81f, unexplained[0].sweep, 1e-3f)
        assertTrue(Rings.segments(RoomP("flur", 0f, emptyList()), colors).isEmpty())
        assertEquals(0.5f * 270f, Rings.segments(RoomP("flur", 0.5f, listOf(Part("aa:01", 0.9f))), few).sumOf { it.sweep.toDouble() }.toFloat(), 1e-2f)
    }

    @Test
    fun colorBookIsStableAndEvictsLeastRecentlyUsed() {
        val first = ColorBook.assign(emptyMap(), listOf("a", "b", "c"), 10L)
        assertEquals(3, first.values.map { it.color }.toSet().size)
        val reordered = ColorBook.assign(first, listOf("c", "a", "b", "d"), 20L)
        listOf("a", "b", "c").forEach { assertEquals(first.getValue(it).color, reordered.getValue(it).color) }
        assertEquals(20L, reordered.getValue("a").used)
        val full = ColorBook.assign(reordered, listOf("e", "f"), 30L)
        assertEquals(6, full.size)
        val touched = ColorBook.assign(full, listOf("a", "d", "e", "f", "b", "g"), 40L)
        assertTrue("c" !in touched)
        assertEquals(full.getValue("c").color, touched.getValue("g").color)
        assertEquals(6, touched.values.map { it.color }.toSet().size)
        assertEquals(touched, ColorBook.parse(ColorBook.json(touched)))
        assertTrue(ColorBook.parse("junk").isEmpty())
        val churn = (1..40).fold(emptyMap<String, Slot>()) { b, i -> ColorBook.assign(b, listOf("x$i", "x${i + 1}", "x${i + 2}", "x${i % 7}", "x${i % 5}", "x${i % 3}").distinct(), i.toLong()) }
        assertTrue(churn.size <= 6 && churn.values.map { it.color }.toSet().size == churn.size)
    }

    @Test
    fun heartbeatKeepsDataSheet() {
        val data = ReplyJson.parse(replyText)
        val beat = ReplyJson.parse("""{"stored":1,"room":null,"probability":null,"rooms":[],"aps":[]}""")
        val old = ReplyJson.parse("""{"stored":4,"room":"wohnzimmer"}""")
        assertTrue(ReplyJson.keeps(data, true))
        assertTrue(!ReplyJson.keeps(beat, false))
        assertTrue(ReplyJson.keeps(beat, true))
        assertTrue(ReplyJson.keeps(old, false))
        assertTrue(ReplyJson.postedAps("""{"ts":1,"aps":[{"bssid":"a","rssi":-1,"freq":1}]}"""))
        assertTrue(!ReplyJson.postedAps("""{"ts":1,"aps":[]}"""))
        assertTrue(!ReplyJson.postedAps("""{"scans":[{"aps":[{"bssid":"a"}]},{"aps":[]}]}"""))
        assertTrue(ReplyJson.postedAps("""{"scans":[{"aps":[]},{"aps":[{"bssid":"a"}]}]}"""))
        assertEquals("pausiert", Footer.line("12:34", true, 3, true))
    }

    @Test
    fun ringsAreCappedAtFive() {
        val many = (1..8).map { RoomP("r$it", it / 10f, emptyList()) }
        assertEquals(5, Rings.order(many).size)
        assertEquals("r8", Rings.order(many)[0].room)
    }

    @Test
    fun apNames() {
        val named = listOf(NamedAp("aa:bb:cc:dd:ee:01", "Home", -50, 2412), NamedAp("aa:bb:cc:dd:ee:02", "Home", -60, 5180), NamedAp("aa:bb:cc:dd:ee:03", "", -70, 5180))
        assertEquals("…ee:03", ApNames.base("aa:bb:cc:dd:ee:03", named))
        assertEquals("…ee:09", ApNames.base("aa:bb:cc:dd:ee:09", named))
        val labels = ApNames.labels(listOf(Ap("aa:bb:cc:dd:ee:01", -50, 2412), Ap("aa:bb:cc:dd:ee:02", -60, 5180), Ap("aa:bb:cc:dd:ee:03", -70, 5180)), named)
        assertEquals(mapOf("aa:bb:cc:dd:ee:01" to "Home 2.4", "aa:bb:cc:dd:ee:02" to "Home 5", "aa:bb:cc:dd:ee:03" to "…ee:03"), labels)
        val long = listOf(NamedAp("a", "FRITZ!Box 1234 AB", -50, 2412), NamedAp("b", "FRITZ!Box 1234 AB", -50, 6000))
        assertEquals(listOf("FRITZ!Box 2.4", "FRITZ!Box 6"), ApNames.labels(long.map { Ap(it.bssid, it.rssi, it.freq) }, long).values.toList())
        assertEquals("Q3", Footer.line("12:34", true, 3).takeLast(2))
        assertEquals("12:34 · ERR · Q0", Footer.line("12:34", false, 0))
        assertEquals(listOf(-40, -50), Footer.topAps(listOf(NamedAp("a", "", -50, 1), NamedAp("b", "", -40, 1)), 8).map { it.rssi })
    }

    @Test
    fun legendAndLabelsClearOfRings() {
        for (d in listOf(213f, 227f)) {
            val half = d / 2f
            fun ringHit(x: Float, y: Float): Boolean {
                if (y > 0f && kotlin.math.abs(x) <= y) return false
                val dist = kotlin.math.hypot(x, y)
                return (0 until 5).any { kotlin.math.abs(dist - Rings.radius(it, d)) <= Rings.STROKE * d / 2f + 1f }
            }
            fun clear(x0: Float, x1: Float, y0: Float, y1: Float): Boolean {
                var x = x0
                while (x <= x1) {
                    var y = y0
                    while (y <= y1) {
                        if (ringHit(x, y) || kotlin.math.hypot(x, y) > half - 2f) return false
                        y += 0.5f
                    }
                    x += 0.5f
                }
                return true
            }
            for (i in 0 until Rings.LEGEND_MAX) {
                val y = (Rings.LEGEND_Y + i * Rings.LEGEND_PITCH) * d
                val x = Rings.LEGEND_X * d
                assertTrue("legend $i at $d", clear(x - 3f, x + 6f + 16 * 4.3f, y - 4.5f, y + 4.5f))
            }
            val legend = (0 until Rings.LEGEND_MAX).map { i ->
                val y = (Rings.LEGEND_Y + i * Rings.LEGEND_PITCH) * d
                val x = Rings.LEGEND_X * d
                floatArrayOf(x - 2.5f, y - 4.5f, x + 6f + 16 * 4.3f, y + 4.5f)
            }
            for (i in 0 until Rings.MAX_RINGS) {
                val a = Math.toRadians(Rings.START.toDouble())
                val lr = Rings.radius(i, d)
                val lx = lr * kotlin.math.cos(a).toFloat() + Rings.LABEL_DX * d
                val ly = lr * kotlin.math.sin(a).toFloat()
                val label = floatArrayOf(lx, ly - 3.5f, lx + 3f * 4.3f, ly + 3.5f)
                legend.forEach { l ->
                    val apart = l[0] - label[2] >= 2f || label[0] - l[2] >= 2f || l[1] - label[3] >= 2f || label[1] - l[3] >= 2f
                    assertTrue("legend vs label $i at $d", apart)
                }
            }
            for (i in 0 until 5) {
                val a = Math.toRadians(Rings.START.toDouble())
                val r = Rings.radius(i, d)
                val x = r * kotlin.math.cos(a).toFloat() + Rings.LABEL_DX * d
                val y = r * kotlin.math.sin(a).toFloat()
                assertTrue("label $i at $d", clear(x, x + 3f * 4.3f, y - 3.5f, y + 3.5f))
            }
        }
    }
}
