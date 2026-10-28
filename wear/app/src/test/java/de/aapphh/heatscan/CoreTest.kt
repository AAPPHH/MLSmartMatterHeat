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
        assertEquals(Pair("bad", true), Highlight.of("bad", "flur"))
        assertEquals(Pair("flur", false), Highlight.of(null, "flur"))
        assertEquals(Pair(null, false), Highlight.of(null, null))
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
}
