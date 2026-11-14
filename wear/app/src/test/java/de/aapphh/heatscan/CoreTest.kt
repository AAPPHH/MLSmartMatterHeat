package de.aapphh.heatscan

import android.content.pm.ServiceInfo
import org.json.JSONObject
import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
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
        assertEquals(setOf("motion", "power", "away", "app_version"), built.keys().asSequence().toSet() - example.keys().asSequence().toSet())
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
        assertTrue(Home.isHome(listOf("x", "fritz!box 1234 ab"), home))
        assertTrue(Home.isHome(listOf("\"Other\""), home))
        assertFalse(Home.isHome(listOf("x", "", "FRITZ!Box 1234"), home))
        assertFalse(Home.isHome(emptyList(), home))
        assertTrue(Home.isHome(listOf("x"), Home.parse(" , ")))
        assertEquals(listOf(Defaults.HOME), Home.parse(Defaults.HOME))
    }

    @Test
    fun roomsAreTheFiveFixedOnes() {
        assertEquals(listOf("wohnzimmer", "schlafzimmer", "bad", "kueche", "flur"), Defaults.ROOMS)
        assertEquals(Defaults.LABELS.toSet(), Defaults.CAPTIONS.keys)
        assertEquals(Defaults.LABELS.toSet(), RoomIcons.RES.keys)
    }

    @Test
    fun labelWindowStartsAtGivenTsAndStaysWithinServerCap() {
        val now = 10_000_000_000L
        assertEquals(Pair(now - 100, now), LabelEvent.window(now - 100, now))
        assertEquals(Pair(now - 3_600_000, now), LabelEvent.window(0, now))
        assertEquals(Pair(now, now), LabelEvent.window(now + 5, now))
        assertEquals(Pair(Long.MIN_VALUE, Long.MIN_VALUE + 5), LabelEvent.window(Long.MIN_VALUE, Long.MIN_VALUE + 5))
    }

    @Test
    fun spanSaturatesInsteadOfOverflowing() {
        assertEquals(Long.MAX_VALUE, Span.ms(Long.MAX_VALUE, -5))
        assertEquals(Long.MIN_VALUE, Span.ms(Long.MIN_VALUE, 5))
        assertEquals(7L, Span.ms(10, 3))
        assertTrue(Span.within(10, 3, 7))
        assertFalse(Span.within(10, 3, 6))
        assertFalse(Span.within(3, 10, 100))
        assertFalse(Span.within(Long.MAX_VALUE, Long.MIN_VALUE, 100))
    }

    @Test
    fun labelEventJson() {
        val e = LabelEvent.entry("w", 5, 9, "bad")
        assertTrue(LabelEvent.isLabel(e))
        assertFalse(LabelEvent.isLabel(Payload.build(1, "w", null, emptyList())))
        assertEquals(9L, e.getLong("ts"))
        val b = JSONObject(LabelEvent.body(e))
        assertEquals(setOf("device", "from", "to", "label", "heat", "label_id"), b.keys().asSequence().toSet())
        assertEquals(e.getString("label_id"), b.getString("label_id"))
        assertEquals(36, b.getString("label_id").length)
        assertEquals(e.getString("label_id"), JSONObject(LabelEvent.watchBody(e)).getString("label_id"))
        assertFalse(JSONObject(LabelEvent.body(JSONObject(e.toString()).also { it.remove("label_id") })).has("label_id"))
        assertEquals("w", b.getString("device"))
        assertEquals(5L, b.getLong("from"))
        assertEquals(9L, b.getLong("to"))
        assertEquals("bad", b.getString("label"))
        assertTrue(b.getBoolean("heat"))
        assertFalse(JSONObject(LabelEvent.body(LabelEvent.entry("w", 5, 9, "bad", false))).getBoolean("heat"))
        assertTrue(Defaults.ROOMS.all { Regex("^[a-z_]{1,32}$").matches(it) })
    }

    @Test
    fun voidEventIsAQueuedLabelSpecialWithItsOwnBody() {
        val l = LabelEvent.point("w", 100, "bad", true, "id-1")
        val v = LabelEvent.voidEntry("w", "id-1", 500)
        assertTrue(LabelEvent.isLabel(v) && LabelEvent.isVoid(v) && !LabelEvent.isVoid(l))
        assertTrue(Special.isSpecial(v))
        val b = JSONObject(LabelEvent.voidBody(v))
        assertEquals(setOf("device", "label_id", "ts"), b.keys().asSequence().toSet())
        assertEquals("id-1", b.getString("label_id"))
        assertEquals(500L, b.getLong("ts"))
        val q = ScanQueue(null, 100, 1_000_000)
        q.add(l, 1)
        q.add(v, 2)
        val seen = mutableListOf<Boolean>()
        val r = Flusher(10).run(q, 5, { 200 }, { 200 }, { e -> seen.add(LabelEvent.isVoid(e)); 200 })
        assertEquals(listOf(false, true), seen)
        assertEquals(0, q.size)
        assertEquals(2, r.sent)
    }

    @Test
    fun voidClearsOnlyTheStayThatThatLabelSet() {
        val t = StayTracker()
        t.confirm("bad", 1_000, "a")
        assertEquals("a", StayTracker.parse(t.json()).labelId)
        assertFalse(t.void("b"))
        assertEquals("bad", t.holding(2_000))
        assertTrue(StayTracker.parse(t.json()).void("a"))
        assertTrue(t.void("a"))
        assertNull(t.holding(2_000))
        assertNull(t.labelId)
        assertFalse(t.void("a"))
        t.confirm("bad", 1_000)
        assertFalse(t.void("a"))
        assertEquals("bad", t.holding(2_000))
    }

    @Test
    fun voidedViewAndUndoWindow() {
        assertEquals(TapView(TapKind.VOIDED, "Label verworfen", "Bad"), Tap.view(Tap.VOIDED, "bad", 0, 0))
        assertTrue(Tap.undoable(TapKind.OK) && Tap.undoable(TapKind.QUEUED))
        assertFalse(Tap.undoable(TapKind.VOIDED) || Tap.undoable(TapKind.FAILED) || Tap.undoable(null))
        assertEquals(10_000L, Tap.showMs(TapKind.OK))
        assertEquals(Tap.SHOW_MS, Tap.showMs(TapKind.VOIDED))
        assertEquals("sent", Payload.build(1, "d", "bad", emptyList(), labelId = "sent").getString("label_id"))
        assertFalse(Payload.build(1, "d", "bad", emptyList()).has("label_id"))
    }

    private fun fp(vararg v: Pair<String, Int>) = v.map { Ap(it.first, it.second, 5180) }
    private val roomA = fp("a1" to -50, "a2" to -60, "a3" to -70, "a4" to -75)
    private val roomB = fp("b1" to -50, "b2" to -58, "b3" to -66, "a1" to -80)
    private fun noisy(base: List<Ap>, d: Int) = base.mapIndexed { i, a -> a.copy(rssi = a.rssi + if (i % 2 == 0) d else -d) }
    private val t0 = 1_000_000_000_000L

    private fun staying(room: String = "kueche", aps: List<Ap> = roomA): StayTracker {
        val t = StayTracker()
        assertTrue(t.onScan(t0, t0, aps, false, false, null).ask)
        t.confirm(room, t0 + 5_000)
        return t
    }

    @Test
    fun fingerprintDistanceUsesStrongUnionAndMissingFloor() {
        val a = Fingerprint.of(roomA)
        assertEquals(0f, Fingerprint.distance(a, a), 0f)
        assertEquals(3f, Fingerprint.distance(a, Fingerprint.of(noisy(roomA, 3))), 0.01f)
        assertEquals(0f, Fingerprint.distance(emptyMap(), emptyMap()), 0f)
        assertEquals(25f, Fingerprint.distance(mapOf("x" to -75f), emptyMap()), 0.01f)
        assertEquals(0f, Fingerprint.distance(mapOf("x" to -90f), mapOf("y" to -95f)), 0f)
        assertFalse(Fingerprint.differs(a, Fingerprint.of(noisy(roomA, 3))))
        assertTrue(Fingerprint.differs(a, Fingerprint.of(roomB)))
        assertTrue(Fingerprint.differs(a, Fingerprint.of(roomA.map { it.copy(rssi = it.rssi - 12) })))
        val sparse = Fingerprint.of(fp("a1" to -50, "a2" to -60, "n1" to -60, "n2" to -60))
        assertTrue(Fingerprint.overlap(a, sparse) < Fingerprint.MIN_OVERLAP)
        assertEquals(1f, Fingerprint.overlap(a, a), 0f)
    }

    @Test
    fun sameRoomNoiseKeepsStayAndLabelsScans() {
        val t = staying()
        for (i in 1..20) {
            val d = t.onScan(t0 + i * 30_000L, t0 + i * 30_000L, noisy(roomA, if (i % 2 == 0) 3 else -4), i % 3 == 0, i % 3 == 0, null)
            assertEquals("kueche", d.label)
            assertFalse(d.ask)
        }
    }

    @Test
    fun roomChangeAsksAfterTwoDifferingScansWithMovement() {
        val t = staying()
        val d1 = t.onScan(t0 + 60_000, t0 + 60_000, roomB, true, true, null)
        assertEquals("kueche", d1.label)
        assertFalse(d1.ask)
        val d2 = t.onScan(t0 + 90_000, t0 + 90_000, noisy(roomB, 2), false, false, null)
        assertTrue(d2.ask)
        assertTrue(d2.change)
        assertEquals(null, d2.label)
        assertEquals(t0 + 60_000, d2.since)
        assertEquals(null, t.holding(t0 + 90_000))
        assertEquals(t0 + 60_000, t.windowStart(t0 + 100_000))
        t.confirm("bad", t0 + 100_000)
        assertEquals("bad", t.holding(t0 + 100_000))
        val d3 = t.onScan(t0 + 120_000, t0 + 120_000, noisy(roomB, 1), false, false, null)
        assertEquals("bad", d3.label)
        assertFalse(d3.ask)
    }

    @Test
    fun noChangeWithoutMovementAndSingleDifferingScanIsIgnored() {
        val t = staying()
        for (i in 1..5) assertFalse(t.onScan(t0 + i * 30_000L, t0 + i * 30_000L, roomB, false, false, null).ask)
        assertEquals("kueche", t.holding(t0 + 200_000))
        val u = staying()
        assertFalse(u.onScan(t0 + 60_000, t0 + 60_000, roomB, true, true, null).ask)
        assertFalse(u.onScan(t0 + 90_000, t0 + 90_000, roomA, false, false, null).ask)
        assertFalse(u.onScan(t0 + 120_000, t0 + 120_000, roomB, false, false, null).ask)
        assertEquals("kueche", u.holding(t0 + 130_000))
    }

    @Test
    fun stayExpiresAfterTwoHoursAndAsksOnce() {
        val t = staying()
        val late = t0 + 5_000 + StayTracker.CAP_MS
        assertEquals("kueche", t.holding(late - 1))
        val d = t.onScan(late, late, roomA, false, false, null)
        assertTrue(d.ask)
        assertFalse(d.change)
        assertEquals(null, d.label)
        assertFalse(t.onScan(late + 720_000, late + 720_000, roomA, false, false, null).ask)
    }

    @Test
    fun confidentGuessDifferingTwiceCountsAsChangeWhenMoved() {
        val t = staying()
        t.onScan(t0 + 30_000, t0 + 30_000, roomA, true, true, null)
        assertFalse(t.onGuess(t0 + 30_000, "bad", 0.9f, t0 + 31_000).ask)
        val d = t.onGuess(t0 + 60_000, "bad", 0.7f, t0 + 61_000)
        assertTrue(d.ask)
        assertTrue(d.change)
        assertEquals(t0 + 30_000, d.since)
        assertEquals("bad", t.askGuess)
    }

    @Test
    fun weakInterruptedOrUnmovedGuessesDoNotChange() {
        val t = staying()
        t.onScan(t0 + 30_000, t0 + 30_000, roomA, true, true, null)
        assertFalse(t.onGuess(t0 + 30_000, "bad", 0.5f, t0 + 31_000).ask)
        assertFalse(t.onGuess(t0 + 60_000, "bad", 0.9f, t0 + 61_000).ask)
        assertFalse(t.onGuess(t0 + 90_000, "kueche", 0.9f, t0 + 91_000).ask)
        assertFalse(t.onGuess(t0 + 120_000, "bad", 0.9f, t0 + 121_000).ask)
        val u = staying()
        assertFalse(u.onGuess(t0 + 30_000, "bad", 0.9f, t0 + 31_000).ask)
        assertFalse(u.onGuess(t0 + 60_000, "bad", 0.9f, t0 + 61_000).ask)
    }

    @Test
    fun withoutStayAsksOnFirstScanAndAfterSettlingButNotWhileMoving() {
        val t = StayTracker()
        assertTrue(t.onScan(t0, t0, roomA, true, true, null).ask)
        assertFalse(t.onScan(t0 + 30_000, t0 + 30_000, roomB, true, true, null).ask)
        assertFalse(t.onScan(t0 + 60_000, t0 + 60_000, roomB, true, true, null).ask)
        assertTrue(t.onScan(t0 + 90_000, t0 + 90_000, roomB, false, false, null).ask)
        assertFalse(t.onScan(t0 + 810_000, t0 + 810_000, roomB, false, false, null).ask)
    }

    @Test
    fun stillHeartbeatPromptsWhenSettleScanDidNotCarryAps() {
        val t = StayTracker()
        t.onScan(t0, t0, roomA, false, false, null)
        t.onScan(t0 + 30_000, t0 + 30_000, roomA, true, true, null)
        assertFalse(t.onScan(t0 + 60_000, t0 + 60_000, emptyList(), false, false, null).ask)
        assertTrue(t.onScan(t0 + 720_000, t0 + 720_000, roomA, false, false, null).ask)
    }

    @Test
    fun awayResetEndsStayAndAsksOnReturn() {
        val t = staying()
        t.reset()
        assertEquals(null, t.holding(t0 + 10_000))
        assertTrue(t.onScan(t0 + 20_000, t0 + 20_000, roomA, false, false, null).ask)
    }

    @Test
    fun confirmUsesPromptFingerprintAndAveragesFirstScans() {
        val t = StayTracker()
        t.onScan(t0, t0, roomA, false, false, null)
        t.onScan(t0 + 30_000, t0 + 30_000, roomB, true, true, null)
        t.confirm("kueche", t0 + 40_000)
        assertEquals(Fingerprint.of(roomA), t.fp)
        t.onScan(t0 + 60_000, t0 + 60_000, noisy(roomA, 4), false, false, null)
        assertEquals(-48f, t.fp.getValue("a1"), 0.01f)
        assertEquals(2, t.n)
        repeat(10) { t.onScan(t0 + 90_000L + it * 1000, t0 + 90_000L + it * 1000, roomA, false, false, null) }
        assertEquals(StayTracker.AVG_SCANS, t.n)
    }

    @Test
    fun windowStartIsPromptScanOrLastScanAtMostTwoMinutesBack() {
        val now = t0 + 1_000_000
        val t = StayTracker()
        assertEquals(now - StayTracker.BACK_MS, t.windowStart(now))
        t.onScan(now - 400_000, now - 400_000, roomA, false, false, null)
        assertEquals(now - 400_000, t.windowStart(now - 390_000))
        assertEquals(now + 300_000 - StayTracker.BACK_MS, t.windowStart(now + 300_000))
        t.confirm("kueche", now - 380_000)
        t.onScan(now - 30_000, now - 30_000, roomA, false, false, null)
        assertEquals(now - 30_000, t.windowStart(now))
    }

    @Test
    fun promptRefreshesOnceWhenReplyGuessDiffers() {
        val t = StayTracker()
        t.onScan(t0, t0, roomA, false, false, "flur")
        assertFalse(t.refresh(t0, t0 + 1000, "flur"))
        assertFalse(t.refresh(t0 - 1, t0 + 1000, "bad"))
        assertTrue(t.refresh(t0, t0 + 1000, "bad"))
        assertFalse(t.refresh(t0, t0 + 2000, "bad"))
        assertFalse(t.refresh(t0, t0 + Prompt.TIMEOUT_MS + 1, "kueche"))
    }

    @Test
    fun stayStateSurvivesJsonRoundTrip() {
        val t = staying("bad")
        t.onScan(t0 + 60_000, t0 + 60_000, roomB, true, true, "flur")
        val r = StayTracker.parse(t.json())
        assertEquals(t.json(), r.json())
        assertEquals("bad", r.holding(t0 + 70_000))
        assertEquals(null, StayTracker.parse("kaputt").label)
        assertEquals(null, StayTracker.parse(null).label)
    }

    @Test
    fun replyTimestampComesFromLastScanOfBatch() {
        assertEquals(7L, ReplyJson.lastTs("{\"scans\":[{\"ts\":5},{\"ts\":7}]}"))
        assertEquals(9L, ReplyJson.lastTs("{\"ts\":9,\"aps\":[]}"))
        assertEquals(null, ReplyJson.lastTs("x"))
    }

    @Test
    fun promptTitleAndRateLimit() {
        assertEquals("Raum gewechselt? Wo bist du jetzt?", Prompt.title(true))
        assertEquals("Wo bist du?", Prompt.title(false))
        assertEquals(180_000L, Prompt.ALERT_GAP_MS)
        assertFalse(Prompt.alert(t0 + 179_999, t0, false, false, 12))
        assertTrue(Prompt.alert(t0 + 180_000, t0, false, false, 12))
        assertFalse(Prompt.alert(t0 + 999_999_999, t0, false, false, 22))
        assertTrue(Prompt.alert(Long.MAX_VALUE, Long.MIN_VALUE, false, false, 12))
    }

    @Test
    fun deferredLabelStaysQueuedWhileScansFlow() {
        val q = ScanQueue(null, 100, 1_000_000)
        q.add(LabelEvent.entry("d", 0, 1, "bad"), 1)
        q.add(entry(2), 2)
        q.add(entry(3), 3)
        val sent = mutableListOf<Long>()
        val r = Flusher(10).run(q, 5, { a -> (0 until a.length()).forEach { sent.add(a.getJSONObject(it).getLong("ts")) }; 200 }, { 200 }, { 404 })
        assertEquals(listOf(2L, 3L), sent)
        assertEquals(2, r.sent)
        assertEquals(1, r.deferred)
        assertEquals(200, r.code)
        assertEquals(1, q.size)
        assertTrue(LabelEvent.isLabel(q.peek(1, 5)[0].json))
        val r2 = Flusher(10).run(q, 5, { 200 }, { 200 }, { 404 })
        assertEquals(404, r2.code)
        assertEquals(1, r2.deferred)
        assertEquals(1, q.size)
        assertEquals(1, Flusher(10).run(q, 5, { 200 }, { 200 }, { 200 }).sent)
        assertEquals(0, q.size)
    }

    @Test
    fun oldStickyLabelsAreStrippedOnLoadButLabelEventsStay() {
        val f = File.createTempFile("queue", ".jsonl")
        f.writeText(entry(1, "bad").toString() + "\n" + LabelEvent.entry("d", 0, 2, "flur").toString() + "\n" + entry(3).toString() + "\n")
        val q = ScanQueue(f, 100, Long.MAX_VALUE, true)
        val items = q.peek(10, 5)
        assertTrue(items[0].json.isNull("label"))
        assertEquals("flur", items[1].json.getString("label"))
        assertFalse(f.readText().contains("\"bad\""))
        assertEquals(3, ScanQueue(f, 100, Long.MAX_VALUE, false).size)
        f.delete()
    }

    @Test
    fun promptAlertThrottleAndQuietRules() {
        val gap = Prompt.ALERT_GAP_MS
        assertTrue(Prompt.alert(gap, 0, false, false, 12))
        assertFalse(Prompt.alert(gap - 1, 0, false, false, 12))
        assertTrue(Prompt.alert(2 * gap, gap, false, false, 12))
        assertFalse(Prompt.alert(gap, 0, true, false, 12))
        assertFalse(Prompt.alert(gap, 0, false, true, 12))
        assertFalse(Prompt.alert(gap, 0, false, false, 22))
        assertFalse(Prompt.alert(gap, 0, false, false, 3))
        assertFalse(Prompt.alert(gap, 0, false, false, 6))
        assertTrue(Prompt.alert(gap, 0, false, false, 7))
        assertTrue(Prompt.alert(gap, 0, false, false, 21))
    }

    @Test
    fun promptListsGuessFirstAndMarked() {
        assertEquals(listOf("kueche", "wohnzimmer", "flur", "bad", "schlafzimmer", "keiner"), Prompt.rooms(null))
        assertEquals(listOf("bad", "kueche", "wohnzimmer", "flur", "schlafzimmer", "keiner"), Prompt.rooms("bad"))
        assertEquals(Prompt.ORDER + "keiner", Prompt.rooms("unbekannt"))
        assertEquals(Prompt.ORDER + "keiner", Prompt.rooms("keiner"))
        assertEquals("Keiner", Prompt.caption("keiner", "bad"))
        assertEquals("✓ Gespeichert: Kein Raum", Prompt.done("keiner"))
        assertEquals("keiner", StayTracker.parse(StayTracker().also { it.confirm("keiner", 1000L) }.json()).label)
        assertEquals("● Bad", Prompt.caption("bad", "bad"))
        assertEquals("Küche", Prompt.caption("kueche", "bad"))
        assertEquals("✓ Gespeichert: Wohnen", Prompt.done("wohnzimmer"))
        assertEquals(Defaults.ROOMS.toSet(), Prompt.ORDER.toSet())
    }

    @Test
    fun labelsFlushAfterQueuedScansInOrder() {
        val q = ScanQueue(null, 100, 1_000_000)
        q.add(entry(1), 1)
        q.add(entry(2), 2)
        q.add(LabelEvent.entry("d", 0, 3, "bad"), 3)
        q.add(entry(4), 4)
        val sent = mutableListOf<String>()
        val r = Flusher(10).run(q, 5,
            { a -> sent.add("batch" + (0 until a.length()).map { a.getJSONObject(it).getLong("ts") }); 200 },
            { sent.add("one"); 200 },
            { sent.add("label" + it.getLong("ts")); 200 })
        assertEquals(listOf("batch[1, 2]", "label3", "batch[4]"), sent)
        assertEquals(4, r.sent)
        assertEquals(0, q.size)
    }

    @Test
    fun failedLabelKeepsItAndLaterScans() {
        val q = ScanQueue(null, 100, 1_000_000)
        q.add(LabelEvent.entry("d", 0, 1, "bad"), 1)
        q.add(entry(2), 2)
        assertEquals(503, Flusher(10).run(q, 5, { 200 }, { 200 }, { 503 }).code)
        assertEquals(2, q.size)
        Flusher(10).run(q, 5, { 200 }, { 200 }, { 400 })
        assertEquals(0, q.size)
    }

    @Test
    fun scansCarryNoStickyLabel() {
        assertTrue(Payload.build(1, "d", null, emptyList()).isNull("label"))
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
    fun bedtimeIsSentOnlyWhenReadable() {
        assertNull(Bedtime.of(listOf(null, null)))
        assertEquals(true, Bedtime.of(listOf(null, 0, 1)))
        assertEquals(false, Bedtime.of(listOf(0, null)))
        assertTrue(Payload.build(5, "d", null, emptyList(), bedtime = true).getBoolean("bedtime"))
        assertFalse(Payload.build(5, "d", null, emptyList()).has("bedtime"))
        val scan = Payload.build(5, "d", null, aps, bedtime = false)
        assertFalse(MeasureBatch.fromScan(scan, "2.6", 10, 1)!!.getBoolean("bedtime"))
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
        assertEquals("keiner", Quarters.room(0f, 91f, d))
        assertEquals("keiner", Quarters.room(25f, 95f, d))
        assertEquals("kueche", Quarters.room(0f, 70f, d))
        assertEquals("kueche", Quarters.room(60f, 85f, d))
        assertEquals("kueche", Quarters.room(0f, 120f, d))
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
    fun replyAlertParsing() {
        val a = ReplyJson.parse("""{"stored":1,"alert":{"id":"thread_down@5","title":"Heizung","text":"Thermostate stumm"}}""").alert!!
        assertEquals(Alert("thread_down@5", "Heizung", "Thermostate stumm"), a)
        assertEquals(null, ReplyJson.parse("""{"stored":1}""").alert)
        assertEquals(null, ReplyJson.parse("""{"alert":{}}""").alert)
    }

    @Test
    fun matchParsingAndText() {
        val m = ReplyJson.parse("""{"stored":4,"room":"flur","match":{"room":"wohnzimmer","p":0.62,"abstained":false,"reason":null,"top":[{"room":"wohnzimmer","p":0.62},{"room":"bad","p":0.2},{"room":"kueche","p":0.09}],"model":"v1"}}""").match!!
        assertEquals("wohnzimmer", m.room)
        assertEquals(3, m.top.size)
        assertEquals(Rings.name("wohnzimmer"), MatchView.title(m))
        assertEquals("62 % sicher", MatchView.sub(m))
        assertEquals("${Rings.name("wohnzimmer")} 62 | ${Rings.name("bad")} 20 | ${Rings.name("kueche")} 9", MatchView.top(m))
        assertEquals(m, ReplyJson.matchOf(ReplyJson.matchJson(m)))
    }

    @Test
    fun matchAbstainedAndMissing() {
        val none = ReplyJson.parse("""{"match":{"room":null,"p":0.0,"abstained":true,"reason":"no_model","top":[],"model":null}}""").match!!
        assertEquals(null, none.room)
        assertEquals("-", MatchView.title(none))
        assertEquals("unsicher - kein Modell", MatchView.sub(none))
        assertEquals("", MatchView.top(none))
        assertEquals(none, ReplyJson.matchOf(ReplyJson.matchJson(none)))
        val thin = ReplyJson.parse("""{"match":{"room":"bad","p":0.41,"abstained":true,"reason":"low_margin","top":[{"room":"bad","p":0.41}]}}""").match!!
        assertEquals("unsicher - zu knapp", MatchView.sub(thin))
        assertEquals(null, ReplyJson.parse("""{"stored":4,"room":"flur","rooms":[],"aps":[]}""").match)
        assertEquals(null, ReplyJson.parse("garbage").match)
        assertEquals(null, ReplyJson.matchOf(null))
        assertEquals(null, ReplyJson.matchOf("not json"))
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
            val h = Rings.LINE_H
            val heights = listOf(Rings.NAME_SP * h, Rings.SUB_SP * h + Rings.SUB_GAP) + List(Rings.LEGEND_MAX) { Rings.LINE_SP * h }
            val widths = listOf(6f * 15f * 0.6f, 12f * Rings.CHAR_W * 1.1f) + List(Rings.LEGEND_MAX) { Rings.LEGEND_CHARS * Rings.CHAR_W }
            var y = -heights.sum() / 2f
            val inner = Rings.radius(Rings.MAX_RINGS - 1, d) - Rings.STROKE * d / 2f
            val blocks = heights.indices.map { i ->
                val r = floatArrayOf(-widths[i] / 2f, y, widths[i] / 2f, y + heights[i])
                y += heights[i]
                r
            }
            blocks.forEachIndexed { i, r ->
                listOf(r[0] to r[1], r[2] to r[1], r[0] to r[3], r[2] to r[3]).forEach { (x, yy) ->
                    assertTrue("block line $i inside innermost ring at $d", kotlin.math.hypot(x, yy) <= inner)
                }
            }
            val fy = (Footer.Y - 0.5f) * d
            assertTrue("footer at $d", clear(-33f, 33f, fy - 4.5f, fy + 4.5f))
            for (i in 0 until Rings.MAX_RINGS) {
                val a = Math.toRadians(Rings.START.toDouble())
                val lr = Rings.radius(i, d)
                val lx = lr * kotlin.math.cos(a).toFloat() + Rings.LABEL_DX * d
                val ly = lr * kotlin.math.sin(a).toFloat()
                val label = floatArrayOf(lx, ly - 3.5f, lx + 3f * 4.3f, ly + 3.5f)
                (blocks + listOf(floatArrayOf(-33f, fy - 4.5f, 33f, fy + 4.5f))).forEach { l ->
                    val apart = l[0] - label[2] >= 1f || label[0] - l[2] >= 1f || l[1] - label[3] >= 1f || label[1] - l[3] >= 1f
                    assertTrue("block vs label $i at $d", apart)
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

    @Test
    fun legendLineAndDemoData() {
        assertEquals("FRITZ!Box 5 −48", Legend.text("FRITZ!Box 5", -48))
        assertEquals("X 7", Legend.text("X", 7))
        assertEquals(190f, Footer.Y * Rings.REF, 1e-3f)
        val sum = Demo.ROOMS.map { r -> r.parts.sumOf { it.share.toDouble() }.toFloat() to r.p }
        sum.forEach { (parts, p) -> assertTrue(parts <= p + 1e-6f && p - parts <= 0.03f) }
        assertEquals(0.82f, Demo.ROOMS[0].p, 0f)
        assertEquals(listOf("kueche", "wohnzimmer", "flur", "bad", "schlafzimmer"), Rings.order(Demo.ROOMS).map { it.room })
        val book = ColorBook.assign(emptyMap(), Rings.palette(Rings.order(Demo.ROOMS)), 1L)
        val legend = Legend.aps(Rings.palette(Rings.order(Demo.ROOMS)), Demo.APS, Demo.NAMED)
        assertEquals(4, legend.size)
        assertTrue(legend.all { it.bssid in book })
        assertEquals(listOf("FRITZ!Box 5", "FRITZ!Box 2.4"), ApNames.labels(legend, Demo.NAMED).values.take(2))
        val none = Legend.aps(emptyList(), emptyList(), Demo.NAMED + NamedAp("02:00:00:00:00:0f", "Weit", -90, 2412))
        assertEquals(listOf(-48, -55, -71, -79), none.map { it.rssi })
    }

    @Test
    fun wakeScanIsDebouncedAndSkippedInDoze() {
        assertTrue(Wake.scanDue(100_000, 100_000 - Wake.GAP_MS, false))
        assertFalse(Wake.scanDue(100_000, 100_000 - Wake.GAP_MS + 1, false))
        assertFalse(Wake.scanDue(100_000, 0, true))
        assertTrue(Wake.settled(Mode.MOVING, Mode.SETTLING))
        assertTrue(Wake.settled(Mode.MOVING, Mode.STILL))
        assertFalse(Wake.settled(Mode.SETTLING, Mode.STILL))
        assertFalse(Wake.settled(Mode.MOVING, Mode.MOVING))
    }

    @Test
    fun claimWakeHonoursBudgetAndAway() {
        val p = ScanPolicy(true, 0)
        assertTrue(p.claimWake(100_000))
        assertFalse(p.claimWake(100_000 + ScanConfig.MIN_GAP_MS - 1))
        var t = 100_000L
        repeat(ScanConfig.SCAN_WINDOW_MAX - 1) { t += ScanConfig.MIN_GAP_MS; assertTrue(p.claimWake(t)) }
        assertFalse(p.claimWake(t + ScanConfig.MIN_GAP_MS))
        val a = ScanPolicy(true, 0)
        repeat(ScanConfig.AWAY_AFTER_MISSES) { a.onScan(false, 1000L) }
        assertEquals(Mode.AWAY, a.mode)
        assertFalse(a.claimWake(10_000_000))
    }

    @Test
    fun claimManualIgnoresModeButHonoursWindowBudget() {
        val a = ScanPolicy(true, 0)
        repeat(ScanConfig.AWAY_AFTER_MISSES) { a.onScan(false, 1000L) }
        assertEquals(Mode.AWAY, a.mode)
        var t = 100_000L
        assertTrue(a.claimManual(t))
        assertFalse(a.claimManual(t + ScanConfig.MANUAL_GAP_MS - 1))
        repeat(ScanConfig.SCAN_WINDOW_MAX - 1) { t += ScanConfig.MANUAL_GAP_MS; assertTrue(a.claimManual(t)) }
        t += ScanConfig.MANUAL_GAP_MS
        assertFalse(a.claimManual(t))
        assertTrue(a.claimManual(100_000L + ScanConfig.SCAN_WINDOW_MS))
    }

    @Test
    fun manualScanCarriesTappedLabelAndAdoptsFingerprint() {
        val t = staying("kueche", roomA)
        t.confirm("bad", t0 + 10_000)
        val d = t.onManual(t0 + 12_000, t0 + 12_000, roomB, false, false, null)
        assertEquals("bad", d.label)
        assertFalse(d.ask)
        assertEquals(Fingerprint.of(roomB), t.fp)
        assertEquals(1, t.n)
    }

    @Test
    fun tapVerdictSeparatesFreshStaleAndEmpty() {
        assertEquals(Verdict.FRESH, Tap.verdict(true, 5, 10_000_000L, 9_999L))
        assertEquals(Verdict.FRESH, Tap.verdict(true, 5, 10_000_000L, 10_000L))
        assertEquals(Verdict.STALE, Tap.verdict(true, 5, 10_000_000L, 10_001L))
        assertEquals(Verdict.STALE, Tap.verdict(false, 5, 10_000_000L, 1L))
        assertEquals(Verdict.EMPTY, Tap.verdict(true, 0, 0L, 1L))
        assertEquals(Verdict.EMPTY, Tap.verdict(false, 0, 0L, 1L))
    }

    @Test
    fun throttleCountdownFollowsOwnScanStarts() {
        val w = ScanConfig.SCAN_WINDOW_MS
        assertEquals(30, Tap.waitS(emptyList(), 500_000L, 30))
        assertEquals(30, Tap.waitS(listOf(400_000L, 410_000L, 420_000L), 500_000L, 30))
        assertEquals(2, Tap.waitS(listOf(400_000L), 500_000L, 2))
        val full = listOf(400_000L, 410_000L, 420_000L, 430_000L)
        assertEquals(20, Tap.waitS(full, 500_000L, 30))
        assertEquals(1, Tap.waitS(full, 400_000L + w - 1, 30))
        assertEquals(30, Tap.waitS(full, 400_000L + w, 30))
        assertEquals(20, Tap.waitS(listOf(1L, 390_000L, 400_000L, 410_000L, 420_000L, 430_000L), 500_000L, 30))
        assertEquals(0, Tap.remainingS(1_000L, 1_000L))
        assertEquals(1, Tap.remainingS(1_001L, 1_000L))
        assertEquals(3, Tap.remainingS(4_000L, 1_000L))
        assertEquals(0, Tap.remainingS(1_000L, 5_000L))
    }

    @Test
    fun serverAckDecidesOutcome() {
        assertEquals(Tap.OK, Tap.ack(200, 0, 0))
        assertNull(Tap.ack(204, 2, 0))
        assertEquals(Tap.QUEUED, Tap.ack(-1, 1, 0))
        assertEquals(Tap.QUEUED, Tap.ack(200, 0, 1))
        assertEquals(Tap.failed("Server HTTP 500"), Tap.ack(500, 1, 0))
    }

    @Test
    fun tapViewShowsOneOutcomePerState() {
        assertEquals(TapView(TapKind.WAIT, "Messe…"), Tap.view(Tap.WAIT, "bad", 0, 0))
        assertEquals("✓ Messung gespeichert – Bad", Tap.view(Tap.OK, "bad", 0, 0)?.title)
        assertEquals(TapKind.QUEUED, Tap.view(Tap.QUEUED, "bad", 0, 0)?.kind)
        assertEquals("✓ Gespeichert (wird gesendet)", Tap.view(Tap.QUEUED, "bad", 0, 0)?.title)
        assertEquals("Zu schnell – in 5 s nochmal tippen", Tap.view(Tap.THROTTLED, "bad", 10_000, 5_000)?.title)
        assertEquals("Zu schnell – in 1 s nochmal tippen", Tap.view(Tap.THROTTLED, "bad", 10_000, 10_000)?.title)
        val f = Tap.view(Tap.failed("WLAN aus"), "bad", 0, 0)
        assertEquals(TapView(TapKind.FAILED, "Fehlgeschlagen – erneut versuchen", "WLAN aus"), f)
        assertNull(Tap.view("", "bad", 0, 0))
    }

    @Test
    fun roomsLockedWhileMeasuringOrCountingDown() {
        assertTrue(Tap.locked(Tap.WAIT, 0, 0))
        assertTrue(Tap.locked(Tap.THROTTLED, 10_000, 5_000))
        assertFalse(Tap.locked(Tap.THROTTLED, 10_000, 10_000))
        assertFalse(Tap.locked(Tap.OK, 0, 0))
        assertFalse(Tap.locked(Tap.failed("x"), 0, 0))
    }

    @Test
    fun promptsSkippedAwayOffBodyAndScreenOffWake() {
        assertTrue(Wake.promptAllowed(false, null, true, true))
        assertTrue(Wake.promptAllowed(false, false, false, false))
        assertFalse(Wake.promptAllowed(true, null, false, true))
        assertFalse(Wake.promptAllowed(false, true, false, true))
        assertFalse(Wake.promptAllowed(false, null, true, false))
    }

    @Test
    fun noQuestionUnlessHomePositivelyKnown() {
        val now = 10_000_000L
        val uni = Home.isHome(listOf("eduroam", "UniGuest"), listOf("FritzBox"))
        assertFalse(uni)
        assertFalse(Presence.known(uni, now - 1_000, now))
        assertFalse(Presence.known(null, 0L, now))
        assertFalse(Presence.known(true, 0L, now))
        assertFalse(Presence.known(true, now - Presence.FRESH_MS - 1, now))
        assertFalse(Presence.known(true, now + 5_000, now))
        assertTrue(Presence.known(true, now - 60_000, now))
    }

    @Test
    fun promptsBlockedWhenHomeUnknownOrForeign() {
        val now = 10_000_000L
        assertFalse(Wake.promptAllowed(false, false, now - 1_000, now, null, true, true))
        assertFalse(Wake.promptAllowed(false, null, 0L, now, null, true, true))
        assertFalse(Wake.promptAllowed(false, true, now - Presence.FRESH_MS - 1, now, null, false, true))
        assertFalse(Wake.promptAllowed(true, true, now - 1_000, now, null, false, true))
        assertTrue(Wake.promptAllowed(false, true, now - 1_000, now, null, true, true))
    }

    @Test
    fun learnDoesNotScanOrAskAwayFromHome() {
        val t0 = 1_000_000L
        assertEquals(LearnStep.SCAN, Learn.step(t0, t0 + 10_000, false, false, 0L, false, 0L, true))
        assertEquals(LearnStep.WAIT, Learn.step(t0, t0 + 10_000, false, false, 0L, false, 0L, false))
        assertEquals(LearnStep.DISCARD, Learn.step(t0, t0 + 80_000, false, false, t0 + 20_000, false, 0L, false))
        assertEquals(LearnStep.STOP, Learn.step(t0, t0 + 10_000, true, false, 0L, false, 0L, false))
    }

    @Test
    fun settleAndWakePromptWithoutFreshScan() {
        val t = StayTracker()
        val d = t.promptWithoutScan(t0, "bad", false)
        assertTrue(d.ask)
        assertFalse(d.change)
        assertEquals(t0, d.since)
        assertTrue(t.pending(t0 + 1000))
        assertFalse(t.promptWithoutScan(t0 + 1000, null, false).ask)
        t.askAt = 0L
        assertFalse(t.promptWithoutScan(t0 + 300_000, null, true).ask)
        assertTrue(t.promptWithoutScan(t0 + Prompt.WAKE_GAP_MS + 1, null, true).ask)
        val held = staying()
        assertFalse(held.promptWithoutScan(t0 + 10_000, null, true).ask)
        assertFalse(held.promptWithoutScan(t0 + 10_000, null, false).ask)
    }

    @Test
    fun windowStartsAtAnswerTimeWithoutRecentFreshScan() {
        val t = StayTracker()
        t.promptWithoutScan(t0, null, true)
        val now = t0 + 90_000
        assertEquals(Pair(now, now), LabelEvent.window(t.windowStart(now), now))
        t.confirm("bad", now)
        val d = t.onScan(now + 30_000, now + 30_000, roomB, false, false, null)
        assertEquals("bad", d.label)
        assertFalse(d.ask)
        val u = StayTracker()
        u.onScan(t0 - 40_000, t0 - 40_000, roomA, false, false, null)
        u.askAt = 0L
        u.lastAskAt = 0L
        u.promptWithoutScan(t0, null, true)
        assertEquals(t0 - 40_000, u.windowStart(t0 + 30_000))
        assertEquals(t0 + 200_000, u.windowStart(t0 + 200_000))
        u.confirm("bad", t0 + 200_000)
        assertTrue(u.fp.isEmpty())
        val w = StayTracker()
        w.onScan(t0 - 40_000, t0 - 40_000, roomA, false, false, null)
        w.promptWithoutScan(t0, null, true)
        w.confirm("bad", t0 + 30_000)
        assertFalse(w.fp.isEmpty())
    }

    @Test
    fun cachedScanResultsDoNotCountAsFreshScans() {
        val d = Dedup()
        val t = StayTracker()
        var asks = 0
        for (newest in listOf(100L, 100L, 100L, 50L)) if (d.accept(newest)) {
            if (t.onScan(newest, newest, roomA, false, false, null).ask) asks++
        }
        assertEquals(1, asks)
        assertEquals(100L, t.lastTs)
        assertEquals("-", Footer.fresh(0))
        assertEquals("pausiert", Footer.line("1", true, 0, true, "12:00", 2))
        assertEquals("12:34 · OK · Q3 · S12:30 · R2", Footer.line("12:34", true, 3, false, "12:30", 2))
    }

    private val measureExample = JSONObject(javaClass.classLoader!!.getResource("measurement_example.json")!!.readText())

    private fun kinds(o: JSONObject): Map<String, String> =
        o.keys().asSequence().associateWith { k -> o.get(k).let { if (it is Number) (if (it is Int || it is Long) "Int" else "Dec") else it.javaClass.simpleName } }

    private fun sample(i: Int) = Sample(i * 1_000_000_000L, 1000L + i, "temp", "skin", 33.0 + i, "C")

    @Test
    fun batcherClosesAtFiftySamples() {
        val b = Batcher()
        for (i in 1..49) assertEquals(null, b.add(sample(i), 0L))
        assertEquals(50, b.add(sample(50), 10L)!!.size)
        assertEquals(0, b.size)
        assertEquals(null, b.due(Long.MAX_VALUE / 2))
    }

    @Test
    fun batcherClosesAfterFiveMinutesFromFirstSample() {
        val b = Batcher()
        b.add(sample(1), 1_000L)
        b.add(sample(2), 200_000L)
        assertEquals(null, b.due(300_999L))
        assertEquals(2, b.due(301_000L)!!.size)
        b.add(sample(3), 400_000L)
        assertEquals(null, b.due(600_000L))
        assertEquals(1, b.due(700_000L)!!.size)
    }

    @Test
    fun measurementPayloadMatchesContract() {
        val samples = listOf(
            Sample(80799000000000, 1761580799000, "temp", "skin_temperature", 33.4, "C"),
            Sample(80790000000000, 1761580790000, "rssi", "wifi", -61.0, "dBm", "aa:bb:cc:00:00:01", null, 2437)
        )
        val built = MeasureBatch.build("pixel-watch", 1761580800500123, 1761580800000, 1761500000000, "2.4", samples, 0.42)
        assertEquals(kinds(measureExample), kinds(built))
        val ex = measureExample.getJSONArray("samples")
        assertEquals(kinds(ex.getJSONObject(0)), kinds(built.getJSONArray("samples").getJSONObject(0)))
        assertEquals(kinds(ex.getJSONObject(6)), kinds(built.getJSONArray("samples").getJSONObject(1)))
        assertEquals(setOf("elapsed_ns", "ts", "sensor_type", "sensor_name", "value", "unit"), built.getJSONArray("samples").getJSONObject(0).keys().asSequence().toSet())
        assertFalse(MeasureBatch.build("d", 1, 2, 3, "v", samples).has("battery_pct_per_h"))
        assertFalse(MeasureBatch.build("d", 1, 2, 3, "v", samples, Double.NaN).has("battery_pct_per_h"))
    }

    @Test
    fun measureEntryRoundTripsThroughQueueFileWithoutMetaKeys() {
        val f = File.createTempFile("queue", ".jsonl")
        try {
            val q = ScanQueue(f, 100, Long.MAX_VALUE)
            q.add(MeasureBatch.entry(MeasureBatch.build("w", 7, 5_000, 1, "2.4", listOf(sample(1)))), 5_000)
            val again = ScanQueue(f, 100, Long.MAX_VALUE).peek(10, 5_000)
            assertEquals(1, again.size)
            assertTrue(MeasureBatch.isMeasure(again[0].json))
            val body = JSONObject(MeasureBatch.body(again[0].json))
            assertEquals(setOf("device", "batch_id", "sent_ts", "clock_offset_ms", "app_version", "samples"), body.keys().asSequence().toSet())
            assertEquals(7L, body.getLong("batch_id"))
        } finally {
            f.delete()
        }
    }

    @Test
    fun scanBecomesRssiRowsWithStableBatchId() {
        val e = Payload.build(1_000_000L, "w", null, aps, "still", "normal", false, "w-abc-000123", 900_000_000_000L)
        val a = MeasureBatch.fromScan(e, "2.4", 2_000_000L, 5_000L)!!
        val b = MeasureBatch.fromScan(e, "2.4", 9_000_000L, 7_000L)!!
        assertEquals(a.getLong("batch_id"), b.getLong("batch_id"))
        assertEquals(1_000_000L * 1000 + 123, a.getLong("batch_id"))
        assertEquals(1_000_000L - 900_000L, a.getLong("clock_offset_ms"))
        val s = a.getJSONArray("samples")
        assertEquals(4, s.length())
        assertEquals("rssi", s.getJSONObject(0).getString("sensor_type"))
        assertEquals("aa:bb:cc:00:00:01", s.getJSONObject(0).getString("sender"))
        assertEquals(-45.0, s.getJSONObject(0).getDouble("value"), 0.0)
        assertEquals(2412, s.getJSONObject(0).getInt("freq"))
        assertEquals(900_000_000_000L, s.getJSONObject(0).getLong("elapsed_ns"))
        assertEquals("still", a.getString("motion"))
        assertEquals("w-abc-000123", a.getString("id"))
        assertEquals("normal", a.getString("power"))
        assertEquals(false, a.getBoolean("away"))
        assertFalse(a.has("label"))
        val labelled = MeasureBatch.fromScan(Payload.build(1_000_000L, "w", "kueche", aps, "moving", "doze", true, "w-abc-000124").put("queued", true), "2.4", 2_000_000L, 5_000L)!!
        assertEquals(listOf("kueche", "moving", "doze", true, true, "w-abc-000124"), listOf("label", "motion", "power", "away", "queued", "id").map { labelled.get(it) })
        assertEquals(null, MeasureBatch.fromScan(Payload.build(1L, "w", null, emptyList()), "2.4", 1L, 1L))
        assertFalse(Payload.build(1L, "w", null, emptyList()).has("elapsed_ns"))
    }

    @Test
    fun measurementReplyIsTreatedLikeScanReply() {
        val body = MeasureBatch.fromScan(Payload.build(5_000L, "w", null, aps), "2.4", 6_000L, 1L)!!.toString()
        assertTrue(ReplyJson.postedAps(body))
        assertEquals(5_000L, ReplyJson.lastTs(body))
        assertFalse(ReplyJson.postedAps(MeasureBatch.build("w", 1, 2, 3, "v", listOf(sample(1))).toString()))
    }

    @Test
    fun accelerometerBurstEmitsOneVarianceOrNothing() {
        val b = AccelBurst(3)
        for (i in 0 until 10) b.add(i * 1_000_000_000L, if (i % 2 == 0) 9.0 else 11.0)
        val (end, variance) = b.finish()!!
        assertEquals(9_000_000_000L, end)
        assertEquals(1.0, variance, 1e-9)
        assertEquals(null, b.finish())
        b.add(1, 10.0)
        b.add(2, 10.0)
        assertEquals(null, b.finish())
        b.add(3, 5.0)
        assertEquals(null, b.finish())
    }

    @Test
    fun burstGateAllowsOneWindowPerPeriod() {
        val g = BurstGate(60_000)
        assertTrue(g.take(1_000))
        assertFalse(g.take(30_000))
        assertTrue(g.take(61_000))
        assertFalse(g.take(100_000))
    }

    @Test
    fun offBodyValueFollowsAndroidConvention() {
        assertTrue(OffBody.isOff(0f))
        assertFalse(OffBody.isOff(1f))
        assertEquals(0.0, OffBody.value(true), 0.0)
        assertEquals(1.0, OffBody.value(false), 0.0)
        for (raw in listOf(0f, 1f)) assertEquals(raw.toDouble(), OffBody.value(OffBody.isOff(raw)), 0.0)
    }

    @Test
    fun permissionDegradeDecisions() {
        assertEquals(HealthPlan(true, true, "hr=ok skin=ok"), Health.plan(true, true, true, true))
        assertEquals(HealthPlan(false, true, "hr=Recht verweigert skin=ok"), Health.plan(true, false, true, true))
        assertEquals(HealthPlan(true, false, "hr=ok skin=Recht verweigert"), Health.plan(true, true, true, false))
        assertEquals(HealthPlan(false, false, "hr=Sensor fehlt skin=Sensor fehlt"), Health.plan(false, true, false, false))
        assertEquals(listOf(Health.HR, Health.SKIN), Health.requestable(36))
        assertEquals(emptyList<String>(), Health.requestable(35))
        assertEquals(listOf(Health.BACKGROUND), Health.backgroundStep(36, true, false))
        assertEquals(emptyList<String>(), Health.backgroundStep(36, false, false))
        assertEquals(emptyList<String>(), Health.backgroundStep(36, true, true))
        assertEquals(emptyList<String>(), Health.backgroundStep(35, true, false))
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION, Health.foregroundTypes(36, false))
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION or ServiceInfo.FOREGROUND_SERVICE_TYPE_HEALTH, Health.foregroundTypes(36, true))
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION, Health.foregroundTypes(33, true))
    }

    @Test
    fun drainMeterNeedsAnHourAndResetsOnCharging() {
        val d = DrainMeter()
        val h = 3_600_000L
        d.onReading(0, 80.0, false)
        d.onReading(h / 2, 79.0, false)
        assertEquals(null, d.perHour)
        d.onReading(2 * h, 77.0, false)
        assertEquals(1.5, d.perHour!!, 1e-9)
        d.onReading(3 * h, 90.0, true)
        assertEquals(null, d.perHour)
        d.onReading(5 * h, 88.0, false)
        assertEquals(1.0, d.perHour!!, 1e-9)
    }

    @Test
    fun skinSensorIsFoundByTypeOrStringTypePreferringNonWakeup() {
        val all = listOf(
            SensorInfo("acc", 1, "android.sensor.accelerometer", "g", 0.1, 8.0, false),
            SensorInfo("skin wake", 65555, Measure.SKIN_STRING, "g", 0.01, 50.0, true),
            SensorInfo("skin", 12345, Measure.SKIN_STRING, "g", 0.01, 50.0, false)
        )
        assertEquals(2, Inventory.pick(all, Measure.SKIN_TYPE, Measure.SKIN_STRING))
        assertEquals(1, Inventory.pick(all.take(2), Measure.SKIN_TYPE, Measure.SKIN_STRING))
        assertEquals(-1, Inventory.pick(all.take(1), Measure.SKIN_TYPE, Measure.SKIN_STRING))
        assertEquals(-1, Inventory.pick(all, 21))
    }

    @Test
    fun inventoryEntryAndHashChangeOnlyOnChange() {
        val a = listOf(SensorInfo("acc", 1, "s", "v", 0.1, 8.0, false))
        val e = Inventory.entry("w", 5, "2.4", a)
        assertTrue(Inventory.isSensors(e))
        val body = JSONObject(Inventory.body(e))
        assertEquals(setOf("device", "app_version", "sensors"), body.keys().asSequence().toSet())
        assertEquals(setOf("name", "android_type", "string_type", "vendor", "resolution", "max_range", "wake_up", "min_delay_us", "max_delay_us", "fifo_max", "reporting_mode"), body.getJSONArray("sensors").getJSONObject(0).keys().asSequence().toSet())
        assertEquals(Inventory.hash("2.4", a), Inventory.hash("2.4", a.toList()))
        assertTrue(Inventory.hash("2.4", a) != Inventory.hash("2.5", a))
        assertTrue(Inventory.hash("2.4", a) != Inventory.hash("2.4", a + a))
    }

    @Test
    fun flusherRoutesMeasureEntriesAndParksOnServerWithoutEndpoint() {
        val q = ScanQueue(null, 100, 1_000_000)
        q.add(MeasureBatch.entry(MeasureBatch.build("w", 1, 10, 0, "v", listOf(sample(1)))), 10)
        q.add(Inventory.entry("w", 11, "v", emptyList()), 11)
        q.add(Payload.build(12, "w", null, aps), 12)
        val metas = ArrayList<String>()
        val r = Flusher(10).run(q, 20, { 200 }, { 200 }, { 200 }, { metas.add(it.getString("kind")); 404 })
        assertEquals(listOf("measure", "sensors"), metas)
        assertEquals(1, r.sent)
        assertEquals(2, r.deferred)
        assertEquals(2, q.size)
        val ok = Flusher(10).run(q, 20, { 200 }, { 200 }, { 200 }, { 200 })
        assertEquals(2, ok.sent)
        assertEquals(0, q.size)
    }

    @Test
    fun watchLabelBodyCarriesRoomAndSource() {
        val b = JSONObject(LabelEvent.watchBody(LabelEvent.entry("w", 100, 200, "bad")))
        assertEquals("bad", b.getString("room"))
        assertEquals("watch", b.getString("source"))
        assertEquals(200L, b.getLong("ts"))
        assertEquals(100L, b.getLong("from"))
        assertFalse(b.has("label"))
    }

    @Test
    fun batchIdsAreStableAndDisjointBetweenSensorAndScan() {
        assertEquals(BatchId.sensor(5, 7), BatchId.sensor(5, 7))
        assertTrue(BatchId.sensor(5, 7) != BatchId.sensor(5, 8))
        assertTrue(BatchId.sensor(5, 499) % 1000 >= 500)
        assertTrue(BatchId.scan(5, "w-x-000499") % 1000 < 500)
        assertEquals(5000L, BatchId.scan(5, null))
    }

    @Test
    fun batteryReadings() {
        assertEquals(50.0, BatteryRead.pct(50, 100)!!, 0.0)
        assertEquals(null, BatteryRead.pct(-1, 100))
        assertTrue(BatteryRead.charging(1, 3))
        assertFalse(BatteryRead.charging(0, 3))
    }

    private val progressText = """{"target":10,"since":null,"rooms":[{"room":"wohnzimmer","n":10},{"room":"schlafzimmer","n":3},{"room":"bad","n":3},{"room":"kueche","n":7},{"room":"flur","n":4}],"done":false}"""

    @Test
    fun heatChoiceIsSkippedForRoomsWithoutRadiator() {
        assertTrue(Heat.heatable("bad", null))
        assertFalse(Heat.heatable("flur", null))
        assertFalse(Heat.heatable(Defaults.NONE, null))
        val p = Learn.parse("""{"target":10,"since":null,"rooms":[{"room":"bad","n":1,"heatable":false},{"room":"flur","n":1,"heatable":true},{"room":"kueche","n":1}],"done":false}""")!!
        assertFalse(Heat.heatable("bad", p))
        assertTrue(Heat.heatable("flur", p))
        assertTrue(Heat.heatable("kueche", p))
        assertFalse(Heat.heatable(Defaults.NONE, p))
        assertEquals(emptyMap<String, Boolean>(), Learn.parse(progressText)!!.heatable)
    }

    @Test
    fun learnProgressParsing() {
        val p = Learn.parse(progressText)!!
        assertEquals(Progress(10, mapOf("wohnzimmer" to 10, "schlafzimmer" to 3, "bad" to 3, "kueche" to 7, "flur" to 4), false), p)
        assertNull(Learn.parse(null))
        assertNull(Learn.parse("not json"))
        assertNull(Learn.parse("""{"target":10}"""))
        assertTrue(Learn.parse("""{"target":2,"rooms":[],"done":true}""")!!.done)
    }

    @Test
    fun learnHighlightsFewestAndDimsDone() {
        val p = Learn.parse(progressText)!!
        assertEquals(
            mapOf("wohnzimmer" to RoomTone.DONE, "schlafzimmer" to RoomTone.LOW, "bad" to RoomTone.LOW, "kueche" to RoomTone.NORMAL, "flur" to RoomTone.NORMAL),
            Learn.tones(p)
        )
        assertEquals("✓ 10/10", Learn.badge(p, "wohnzimmer"))
        assertEquals("3/10", Learn.badge(p, "bad"))
        assertNull(Learn.badge(p, Defaults.NONE))
        assertTrue(Learn.tones(Progress(10, mapOf("bad" to 12, "flur" to 10), true)).values.all { it == RoomTone.DONE })
        assertEquals(emptyMap<String, RoomTone>(), Learn.tones(null))
        assertEquals("✓ 12/10", Learn.badge(Progress(10, mapOf("bad" to 12), false), "bad"))
    }

    @Test
    fun learnViewAndChip() {
        val p = Learn.parse(progressText)!!
        assertNull(Learn.view(false, p))
        assertNull(Learn.view(true, null))
        assertEquals("3/10", Learn.view(true, p)!!.badges["bad"])
        val done = Progress(10, mapOf("bad" to 10), true)
        assertEquals(RoomTone.DONE, Learn.view(false, done)!!.tones["bad"])
        assertEquals("Lernen", Learn.chip(false, p))
        assertEquals("Stopp", Learn.chip(true, p))
        assertEquals("Fertig ✓", Learn.chip(false, done))
        assertEquals("Stopp", Learn.chip(true, done))
    }

    @Test
    fun learnSchedulingDecisions() {
        val t0 = 1_000_000L
        fun step(now: Long, off: Boolean = false, done: Boolean = false, prompt: Long = 0L, pending: Boolean = false, idle: Long = 0L, start: Long = t0) =
            Learn.step(start, now, off, done, prompt, pending, idle)
        assertEquals(LearnStep.WAIT, step(t0 + 10_000, start = 0L))
        assertEquals(LearnStep.SCAN, step(t0 + 10_000))
        assertEquals(LearnStep.WAIT, step(t0 + 10_000, pending = true))
        assertEquals(LearnStep.WAIT, step(t0 + 10_000, idle = t0 + 8_000))
        assertEquals(LearnStep.SCAN, step(t0 + 10_000, idle = t0 + 5_000))
        assertEquals(LearnStep.WAIT, step(t0 + 30_000, prompt = t0 + 20_000))
        assertEquals(LearnStep.WAIT, step(t0 + 79_999, prompt = t0 + 20_000))
        assertEquals(LearnStep.DISCARD, step(t0 + 80_000, prompt = t0 + 20_000))
        assertEquals(LearnStep.STOP, step(t0 + 10_000, off = true))
        assertEquals(LearnStep.STOP, step(t0 + 10_000, done = true, prompt = t0 + 5_000))
        assertEquals(LearnStep.SCAN, step(t0 + Learn.MAX_MS - 1))
        assertEquals(LearnStep.STOP, step(t0 + Learn.MAX_MS))
        assertEquals(LearnStep.STOP, step(t0 + Learn.MAX_MS, prompt = t0 + Learn.MAX_MS - 5_000))
    }

    @Test
    fun learnPromptTitle() {
        assertEquals("Wo bist du gerade?", Prompt.title(false, true))
        assertEquals("Wo bist du gerade?", Prompt.title(true, true))
        assertEquals("Wo bist du?", Prompt.title(false))
    }

    @Test
    fun tapProducesOnePointLabelAtScanTime() {
        val e = LabelEvent.point("w", 10_000, "bad", true)
        assertEquals(9_000L, e.getLong("from"))
        assertEquals(11_000L, e.getLong("to"))
        assertTrue(LabelEvent.isLabel(e))
        val b = JSONObject(LabelEvent.watchBody(e))
        assertEquals("bad", b.getString("room"))
        assertTrue(b.getBoolean("heat"))
        val n = LabelEvent.point("w", 10_000, "bad", false)
        assertFalse(JSONObject(LabelEvent.watchBody(n)).getBoolean("heat"))
        assertTrue(JSONObject(LabelEvent.watchBody(LabelEvent.entry("w", 1, 2, "bad"))).getBoolean("heat"))
        val old = JSONObject(LabelEvent.entry("w", 1, 2, "bad").toString()).also { it.remove("heat") }
        assertTrue(JSONObject(LabelEvent.watchBody(old)).getBoolean("heat"))
    }

    @Test
    fun heatDefaultsByMode() {
        assertTrue(Heat.default(false))
        assertFalse(Heat.default(true))
        assertEquals(8_000L, Heat.CHOICE_MS)
    }

    @Test
    fun learnPendingTimesOut() {
        assertTrue(Learn.waiting(true, 1_000, 1_000 + Learn.PENDING_MS - 1))
        assertFalse(Learn.waiting(true, 1_000, 1_000 + Learn.PENDING_MS))
        assertFalse(Learn.waiting(false, 1_000, 1_001))
    }

    @Test
    fun reuseTapWithinWindowOfForegroundScan() {
        assertTrue(Reuse.usable(100_000, 90_000, 80_000, Reuse.NONE, 0))
        assertTrue(Reuse.usable(100_000, 80_000, 80_000, Reuse.NONE, 0))
        assertFalse(Reuse.usable(100_001, 80_000, 80_000, Reuse.NONE, 0))
        assertFalse(Reuse.usable(100_000, 70_000, 60_000, Reuse.NONE, 0))
    }

    @Test
    fun reuseNeedsForegroundOrScreenOnScanNotUsedYet() {
        assertFalse(Reuse.usable(100_000, 90_000, 95_000, Reuse.NONE, 0))
        assertTrue(Reuse.usable(100_000, 90_000, 95_000, 85_000, 0))
        assertFalse(Reuse.usable(100_000, 90_000, Reuse.NONE, Reuse.NONE, 0))
        assertFalse(Reuse.usable(100_000, 90_000, 80_000, Reuse.NONE, 90_000))
        assertFalse(Reuse.usable(100_000, 0, 0, 0, 0))
    }

    @Test
    fun autoScanSuppressedInForegroundExceptLearn() {
        assertTrue(Reuse.suppress(true, false))
        assertFalse(Reuse.suppress(false, false))
        assertFalse(Reuse.suppress(true, true))
    }

    @Test
    fun openScanOnlyWithoutUsableScan() {
        assertTrue(Reuse.openScan(false, false))
        assertFalse(Reuse.openScan(true, false))
        assertFalse(Reuse.openScan(false, true))
    }

    @Test
    fun wifiHoldParsesMinutes() {
        assertEquals(5, WifiHold.minutes("""{"room":"x","wifi_hold_min":5}"""))
        assertEquals(30, WifiHold.minutes("""{"wifi_hold_min":30}"""))
        assertNull(WifiHold.minutes("""{"wifi_hold_min":0}"""))
        assertNull(WifiHold.minutes("""{"wifi_hold_min":31}"""))
        assertNull(WifiHold.minutes("""{"wifi_hold_min":2.5}"""))
        assertNull(WifiHold.minutes("""{"wifi_hold_min":"5"}"""))
        assertNull(WifiHold.minutes("""{"room":"x"}"""))
        assertNull(WifiHold.minutes("[1]"))
        assertNull(WifiHold.minutes(null))
    }

    @Test
    fun wifiHoldTiming() {
        val m = 60_000L
        assertEquals(1_000_000L + 10 * m, WifiHold.toggle(0L, 1_000_000L))
        assertEquals(0L, WifiHold.toggle(1_000_000L + m, 1_000_000L))
        assertEquals(1_000_000L + 5 * m, WifiHold.extend(0L, 1_000_000L, 5))
        assertEquals(1_000_000L + 10 * m, WifiHold.extend(1_000_000L + 10 * m, 1_000_000L, 5))
        assertEquals(1_000_000L + 20 * m, WifiHold.extend(1_000_000L + 10 * m, 1_000_000L, 20))
        assertEquals(1_000_000L + 3 * m, WifiHold.extend(500_000L, 1_000_000L, 3))
        assertTrue(WifiHold.active(10L, 9L))
        assertFalse(WifiHold.active(10L, 10L))
        assertEquals(10, WifiHold.remainingMin(1_000_000L + 10 * m, 1_000_000L))
        assertEquals(1, WifiHold.remainingMin(1_000_000L + 1, 1_000_000L))
        assertEquals(0, WifiHold.remainingMin(1_000_000L, 1_000_000L))
        assertEquals("WLAN 10 min", WifiHold.chip(0L, 5L))
        assertEquals("WLAN 3 min", WifiHold.chip(5L + 3 * m, 5L))
    }

    @Test
    fun windowPromptParse() {
        val r = ReplyJson.parse("""{"probability":0.5,"window_prompt":{"id":"w1","room":"wohnzimmer","question":"open"}}""")
        assertEquals(WindowAsk("w1", "wohnzimmer", true, 7L), WindowEvent.parse(r.window, 7L))
        assertEquals(false, WindowEvent.parse(JSONObject("""{"id":"w2","room":"bad","question":"closed"}"""), 7L)!!.open)
        assertNull(WindowEvent.parse(ReplyJson.parse("""{"probability":0.5}""").window, 7L))
        assertNull(WindowEvent.parse(JSONObject("""{"id":"w1","room":"garage","question":"open"}"""), 7L))
        assertNull(WindowEvent.parse(JSONObject("""{"id":"w1","room":"bad","question":"maybe"}"""), 7L))
        assertNull(WindowEvent.parse(JSONObject("""{"room":"bad","question":"open"}"""), 7L))
        assertNull(WindowEvent.parse(null, 7L))
        assertEquals("Ist das Fenster im Wohnzimmer offen?", WindowEvent.title(WindowAsk("w1", "wohnzimmer", true)))
        assertEquals("Ist das Fenster im Schlafzimmer offen?", WindowEvent.title(WindowAsk("w1", "schlafzimmer", true)))
        assertEquals("Ist das Fenster im Bad zu?", WindowEvent.title(WindowAsk("w1", "bad", false)))
        assertEquals("Ist das Fenster in der Küche offen?", WindowEvent.title(WindowAsk("w1", "kueche", true)))
    }

    @Test
    fun windowQuestionMatchesServerQuestionAndYesIsNotInverted() {
        val open = WindowAsk("w1", "bad", true)
        val closed = WindowAsk("w2", "bad", false)
        assertEquals("Fenster offen?", WindowEvent.question(open))
        assertEquals("Fenster zu?", WindowEvent.question(closed))
        assertEquals("Ja, Fenster Bad ist offen", WindowEvent.yesDescription(open))
        assertEquals("Ja, Fenster Bad ist zu", WindowEvent.yesDescription(closed))
        assertEquals("Nein, Fenster Bad ist zu", WindowEvent.noDescription(open))
        assertEquals("Nein, Fenster Bad ist offen", WindowEvent.noDescription(closed))
        listOf(open, closed).forEach {
            assertEquals(WindowEvent.yesDescription(it).endsWith("offen"), WindowEvent.open(it, WindowAnswer.YES))
            assertEquals(WindowEvent.noDescription(it).endsWith("offen"), WindowEvent.open(it, WindowAnswer.NO))
            assertEquals("Fenster " + (if (WindowEvent.open(it, WindowAnswer.YES)) "offen" else "zu"), WindowEvent.outcome(it, WindowAnswer.YES))
        }
        assertEquals("Fenster zu", WindowEvent.outcome(closed, WindowAnswer.YES))
        assertEquals("Fenster offen", WindowEvent.outcome(closed, WindowAnswer.NO))
        assertEquals(WindowEvent.UNKNOWN, WindowEvent.outcome(closed, WindowAnswer.UNKNOWN))
    }

    @Test
    fun swipeIsUnknownWhileExpiredSwipeIsTimeout() {
        val ask = WindowAsk("w1", "bad", true, 1_000L)
        assertEquals(Pair(WindowAnswer.UNKNOWN, "swipe"), WindowEvent.resolve(ask, WindowAnswer.UNKNOWN, "swipe", 2_000L))
        assertEquals(Pair(WindowAnswer.TIMEOUT, "expiry"), WindowEvent.resolve(ask, WindowAnswer.UNKNOWN, "swipe", 1_000L + WindowEvent.EXPIRY_MS))
        assertEquals(Pair(WindowAnswer.YES, "page"), WindowEvent.resolve(ask, WindowAnswer.YES, "page", 1_000L + WindowEvent.EXPIRY_MS))
        val e = WindowEvent.answerEntry("w", 5L, ask, WindowAnswer.UNKNOWN, "swipe")
        assertEquals("unknown", e.getString("answer"))
        assertEquals("swipe", e.getString("via"))
    }

    @Test
    fun voidedEntryKeepsPromptRecord() {
        val e = WindowEvent.voidedEntry("w", 5L, WindowAsk("w1", "bad", true))
        assertEquals("voided", e.getString("answer"))
        assertEquals("undo", e.getString("via"))
        assertEquals("w1", e.getString("prompt_id"))
        assertEquals("bad", e.getString("room"))
        assertTrue(WindowEvent.isWindow(e))
        assertEquals("voided", JSONObject(WindowEvent.body(e)).getString("answer"))
    }

    @Test
    fun oldAlertIdMigratesIntoSeen() {
        assertEquals(listOf("a", "b"), AlertList.migrate(listOf("a"), "b"))
        assertEquals(listOf("a", "b"), AlertList.migrate(listOf("a", "b"), "b"))
        assertEquals(listOf("a"), AlertList.migrate(listOf("a"), null))
        assertEquals(listOf("a"), AlertList.migrate(listOf("a"), ""))
        assertEquals(40, AlertList.migrate((1..40).map { "i$it" }, "n").size)
        assertTrue(AlertList.fresh(listOf(Alert("b", "t", "x")), AlertList.migrate(emptyList(), "b")).isEmpty())
    }

    @Test
    fun boostConfirmationQuestionNamesRoomAndMinutes() {
        assertEquals("Küche 5 min aufheizen?", Boost.question("kueche"))
        assertEquals("Kein WLAN", Boost.failed(Boost.NO_WIFI).line)
    }

    @Test
    fun oneShotAllowsExactlyOneTap() {
        val gate = OneShot()
        assertEquals(true, gate.take())
        assertEquals(false, gate.take())
        assertEquals(false, gate.take())
    }

    @Test
    fun windowPromptDedupeAndExpiry() {
        val a = WindowAsk("w1", "bad", true)
        val (p1, seen1) = WindowEvent.admit(null, emptyList(), a, 1_000L)
        assertEquals(WindowAsk("w1", "bad", true, 1_000L), p1)
        assertEquals(listOf("w1"), seen1)
        val (p2, seen2) = WindowEvent.admit(p1, seen1, a, 5_000L)
        assertEquals(p1, p2)
        assertEquals(seen1, seen2)
        val (p3, _) = WindowEvent.admit(null, seen1, a, 5_000L)
        assertNull(p3)
        val (p4, seen4) = WindowEvent.admit(p1, seen1, WindowAsk("w2", "bad", false), 6_000L)
        assertEquals("w2", p4!!.id)
        assertEquals(listOf("w1", "w2"), seen4)
        val (p5, _) = WindowEvent.admit(p1, seen1, null, 1_000L + WindowEvent.EXPIRY_MS)
        assertNull(p5)
        val (p6, _) = WindowEvent.admit(p1, seen1, null, 1_000L + WindowEvent.EXPIRY_MS - 1)
        assertEquals(p1, p6)
        assertNull(WindowEvent.live(p1, 999L))
        val many = (1..30).fold(Pair<WindowAsk?, List<String>>(null, emptyList())) { acc, n -> WindowEvent.admit(acc.first, acc.second, WindowAsk("i$n", "bad", true), n.toLong()) }
        assertEquals(20, many.second.size)
        assertEquals("i30", many.second.last())
    }

    @Test
    fun windowBodyAndQueue() {
        val e = WindowEvent.entry("watch", 123L, "bad", true, "w1", "id-1")
        assertTrue(WindowEvent.isWindow(e))
        assertTrue(Special.isSpecial(e))
        assertFalse(LabelEvent.isLabel(e))
        val b = JSONObject(WindowEvent.body(e))
        assertEquals("watch", b.getString("device"))
        assertEquals(123L, b.getLong("ts"))
        assertEquals("bad", b.getString("room"))
        assertTrue(b.getBoolean("open"))
        assertEquals("w1", b.getString("prompt_id"))
        assertEquals("id-1", b.getString("label_id"))
        val m = JSONObject(WindowEvent.body(WindowEvent.entry("watch", 1L, "kueche", false, null, "id-2")))
        assertTrue(m.isNull("prompt_id"))
        assertFalse(m.getBoolean("open"))
        val back = JSONObject(e.toString())
        assertEquals(WindowEvent.body(e), WindowEvent.body(back))
        assertEquals(WindowAsk("w1", "bad", true, 9L), WindowEvent.fromJson(WindowEvent.json(WindowAsk("w1", "bad", true, 9L))))
        assertNull(WindowEvent.fromJson("garbage"))
        assertEquals(listOf("wohnzimmer", "schlafzimmer", "bad", "kueche"), WindowEvent.rooms(null))
    }

    @Test
    fun windowPromptCarriesReason() {
        val o = JSONObject("""{"id":"w1","room":"kueche","question":"open","reason":"Temperatur faellt schnell"}""")
        val a = WindowEvent.parse(o, 5L)!!
        assertEquals("Temperatur faellt schnell", a.reason)
        assertEquals("Küche: Temperatur fällt schnell", WindowEvent.text(a))
        assertEquals("", WindowEvent.parse(JSONObject("""{"id":"w1","room":"kueche","question":"open","reason":null}"""), 5L)!!.reason)
        assertEquals("Bad", WindowEvent.text(WindowAsk("w", "bad", true)))
        assertEquals(a, WindowEvent.fromJson(WindowEvent.json(a)))
    }

    @Test
    fun everyAnswerBecomesALabelWithExplicitField() {
        val a = WindowAsk("w1", "bad", true, 1_000L, "r")
        val expect = mapOf(WindowAnswer.YES to true, WindowAnswer.NO to false, WindowAnswer.UNKNOWN to true, WindowAnswer.TIMEOUT to true)
        expect.forEach { (answer, open) ->
            val b = JSONObject(WindowEvent.body(WindowEvent.answerEntry("watch", 5L, a, answer, "notification", "id-" + answer.wire)))
            assertEquals(answer.wire, b.getString("answer"))
            assertEquals(open, b.getBoolean("open"))
            assertEquals("w1", b.getString("prompt_id"))
            assertEquals("notification", b.getString("via"))
            assertEquals("id-" + answer.wire, b.getString("label_id"))
        }
        assertEquals(listOf("yes", "no", "unknown", "timeout", "voided"), WindowAnswer.values().map { it.wire })
        assertEquals(false, WindowEvent.open(WindowAsk("w", "bad", false), WindowAnswer.YES))
        assertEquals(true, WindowEvent.open(WindowAsk("w", "bad", false), WindowAnswer.NO))
        val self = JSONObject(WindowEvent.body(WindowEvent.entry("watch", 1L, "bad", true, null, "s1", via = "self")))
        assertFalse(self.has("answer"))
        assertEquals("self", self.getString("via"))
        assertTrue(self.isNull("prompt_id"))
        val back = JSONObject(WindowEvent.answerEntry("watch", 5L, a, WindowAnswer.UNKNOWN, "page", "i").toString())
        assertEquals("unknown", JSONObject(WindowEvent.body(back)).getString("answer"))
    }

    @Test
    fun unansweredPromptLapsesOnceByExpiryOrReplacement() {
        val live = WindowAsk("w1", "bad", true, 1_000L)
        val seen = listOf("w1")
        assertNull(WindowEvent.lapsed(null, seen, null, 2_000L))
        assertNull(WindowEvent.lapsed(live, seen, null, 2_000L))
        assertEquals(live, WindowEvent.lapsed(live, seen, null, 1_000L + WindowEvent.EXPIRY_MS))
        assertEquals(live, WindowEvent.lapsed(live, seen, WindowAsk("w2", "bad", true), 2_000L))
        assertNull(WindowEvent.lapsed(live, seen, WindowAsk("w1", "bad", true), 2_000L))
        assertNull(WindowEvent.lapsed(live, seen + "w2", WindowAsk("w2", "bad", true), 2_000L))
        assertEquals(1_000L + WindowEvent.EXPIRY_MS, WindowEvent.lapseTs(live, 9_000_000L))
        assertEquals(5_000L, WindowEvent.lapseTs(live, 5_000L))
    }

    @Test
    fun alertsListDedupeAndNoRebuzz() {
        val r = ReplyJson.parse("""{"stored":1,"alerts":[{"id":"a@1","title":"T1","text":"x"},{"id":"b@2","title":"T2","text":"y"}],"alert":{"id":"a@1","title":"T1","text":"x"}}""")
        assertEquals(listOf("a@1", "b@2"), r.alerts.map { it.id })
        assertEquals("a@1", r.alert!!.id)
        assertEquals(listOf("only"), ReplyJson.parse("""{"alert":{"id":"only","title":"T","text":"x"}}""").alerts.map { it.id })
        assertTrue(ReplyJson.parse("""{"stored":1}""").alerts.isEmpty())
        val seen = AlertList.remember(emptyList(), r.alerts.take(1))
        assertEquals(listOf("b@2"), AlertList.fresh(r.alerts, seen).map { it.id })
        assertTrue(AlertList.fresh(r.alerts, AlertList.remember(seen, r.alerts)).isEmpty())
        assertEquals(40, AlertList.remember(emptyList(), (1..60).map { Alert("i$it", "", "") }).size)
        assertEquals(r.alerts, AlertList.fromJson(AlertList.json(r.alerts)))
        assertEquals(AlertList.notificationId(r.alerts[0]), AlertList.notificationId(r.alerts[0].copy()))
        assertTrue(AlertList.fromJson("garbage").isEmpty())
    }

    @Test
    fun glanceParsesStateAndFormatsGerman() {
        val state = """{"alerts":[],"rooms":[{"name":"wohnzimmer","window_open":true,"nodes":[{"local_temperature":21.54,"occupied_heating_setpoint":22.0,"commanded_setpoint":null}]},{"name":"bad","window_open":false,"nodes":[{"local_temperature":null,"occupied_heating_setpoint":19.0,"commanded_setpoint":20.5}]},{"name":"keller","nodes":[]}]}"""
        val g = Glance.parse(state, 77L)!!
        assertEquals(2, g.rooms.size)
        assertEquals(RoomGlance("wohnzimmer", 21.54, 22.0, true), g.rooms[0])
        assertEquals(RoomGlance("bad", null, 20.5, false), g.rooms[1])
        assertEquals(listOf("wohnzimmer"), g.openRooms())
        assertEquals("Wohnen 21,5/22,0", Glance.line(g.rooms[0]))
        assertEquals("Bad -/20,5", Glance.line(g.rooms[1]))
        assertEquals(g, Glance.fromJson(g.json()))
        assertEquals(Glance.EMPTY, Glance.fromJson("garbage"))
        assertNull(Glance.parse("garbage", 1L))
        assertEquals(listOf("A"), Glance.alerts("""{"alerts":[{"id":"A","title":"t","text":"x"}]}""")!!.map { it.id })
    }

    @Test
    fun boostResultShowsPlainWords() {
        JSONObject(Boost.body("bad")).let { assertEquals("bad", it.getString("room")); assertEquals(5, it.getInt("minutes")) }
        val ok = Boost.result(200, """{"ok":true,"state":"armed","reason":"Szenario manual","cycle":{"minutes":5.0}}""")
        assertTrue(ok.ok)
        assertEquals("Boost läuft: 5 min", ok.line)
        assertEquals("Boost ist gerade nicht möglich", Boost.result(409, """{"ok":false,"reason":"kein Boost-Modell geladen"}""").line)
        assertEquals("Gerade kein Boost: Fenster war offen", Boost.result(409, """{"ok":false,"reason":"kein Boost: Fenster war vor 5 min offen (Masse noch warm)"}""").line)
        assertEquals("Boost nur morgens oder nach dem Heimkommen", BoostText.plain("Szenario manual ist kein Boost-Szenario, geboostet wird nur morgens und bei Heimkehr"))
        assertEquals("Der Raum ist schon warm genug", BoostText.plain("Raum ist schon am Ziel"))
        assertEquals(BoostText.FALLBACK, BoostText.plain("irgendwas"))
        assertEquals("Server nicht erreichbar", Boost.result(-1, "").line)
        assertEquals("Antwort unlesbar (500)", Boost.result(500, "x").line)
    }

    @Test
    fun glanceCarriesReasonAndBoostForRoomView() {
        val state = """{"rooms":[{"name":"kueche","window_open":false,"boost":{"state":"boosting","cycle":{"minutes":5.0,"written_ts":1000000}},"nodes":[{"local_temperature":20.0,"occupied_heating_setpoint":22.0,"reason":"boost"}]},{"name":"bad","window_open":false,"boost":{"state":"idle","cycle":null},"nodes":[{"local_temperature":21.0,"occupied_heating_setpoint":18.0,"reason":"away"}]},{"name":"flur","window_open":false,"nodes":[{"local_temperature":19.9,"occupied_heating_setpoint":20.0,"reason":"home"}]},{"name":"schlafzimmer","window_open":true,"nodes":[{"local_temperature":18.0,"occupied_heating_setpoint":20.0,"reason":"window_open"}]},{"name":"wohnzimmer","window_open":false,"nodes":[{"local_temperature":19.0,"occupied_heating_setpoint":21.0,"reason":"home"}]}]}"""
        val g = Glance.parse(state, 5L)!!
        val k = RoomView.glance(g, "kueche")!!
        assertTrue(k.boosting)
        assertEquals(1_300_000L, k.boostUntil)
        assertEquals("Schnell aufheizen bis " + RoomView.clock(1_300_000L), RoomView.why(k))
        assertEquals(RoomMark.BOOST, RoomView.mark(k))
        assertEquals("Du bist weg – Sparmodus", RoomView.why(RoomView.glance(g, "bad")))
        assertEquals(RoomMark.AWAY, RoomView.mark(RoomView.glance(g, "bad")))
        assertEquals(RoomMark.NONE, RoomView.mark(RoomView.glance(g, "flur")))
        assertEquals("Du bist zuhause – normale Temperatur", RoomView.why(RoomView.glance(g, "flur")))
        assertEquals(RoomMark.WINDOW, RoomView.mark(RoomView.glance(g, "schlafzimmer")))
        assertEquals("Fenster offen – Heizung pausiert", RoomView.why(RoomView.glance(g, "schlafzimmer")))
        assertEquals(RoomMark.HEATING, RoomView.mark(RoomView.glance(g, "wohnzimmer")))
        assertEquals("19,0° · heizt", RoomView.tile(RoomView.glance(g, "wohnzimmer")))
        assertEquals("Ziel 22,0°", RoomView.target(k))
        assertNull(RoomView.temp(null))
        assertNull(RoomView.why(RoomGlance("bad", 20.0, 20.0, false, "mystery")))
        assertEquals("Aufheizen beendet", RoomView.why(RoomGlance("bad", 20.0, 20.0, false, "boost_off")))
        val said = mapOf("occupied" to "Du bist hier – normale Temperatur", "default" to "Niemand hier – Grundtemperatur", "no_supply" to "Heizung liefert gerade keine Wärme", "pi_reset" to "Thermostat wird neu eingestellt", "sleep" to "Nachtruhe – Schlaftemperatur", "home" to "Du bist zuhause – normale Temperatur")
        said.forEach { (reason, text) -> assertEquals(text, RoomView.why(RoomGlance("bad", 20.0, 20.0, false, reason))) }
        listOf("home", "occupied", "sleep", "default", "away", "window_open", "boost", "boost_off", "no_supply", "pi_reset", "schedule", "exploration", "manual_api", "manual_watch").forEach { assertNotNull(it, RoomView.REASONS[it]) }
        assertEquals("19,0° · heizt", RoomView.row(RoomView.glance(g, "wohnzimmer")))
        assertEquals("19,9° · normal", RoomView.row(RoomView.glance(g, "flur")))
        assertEquals("keine Daten", RoomView.row(null))
        assertEquals("Schnell aufheizen", RoomView.why(RoomGlance("bad", 20.0, 22.0, false, "boost", true)))
        assertEquals(g, Glance.fromJson(g.json()))
        val one = listOf(Alert("a", "t", "x"))
        val ask = WindowAsk("w1", "kueche", true, 0L, "x")
        assertEquals("1 Hinweis", RoomView.pill(one, null))
        assertEquals("2 Hinweise", RoomView.pill(one, ask))
        assertEquals("Fenster?", RoomView.pill(emptyList(), ask))
        assertNull(RoomView.pill(emptyList(), null))
        assertEquals("2", RoomView.badge(one, ask))
        assertEquals("?", RoomView.badge(emptyList(), ask))
        assertNull(RoomView.badge(emptyList(), null))
        assertTrue(RoomView.applying(10_000L, 10_000L))
        assertTrue(RoomView.applying(12_000L, 10_000L))
        assertFalse(RoomView.applying(12_001L, 10_000L))
        assertEquals(2, RoomView.count(one, ask))
    }

    @Test
    fun plainTextHidesJargon() {
        assertEquals("Thermostat", Plain.text("Heizung: Thermostat"))
        assertEquals("Bad antwortet nicht.", Plain.text("Bad (Node 5) antwortet nicht."))
        assertEquals("Sollwert wird nicht übernommen", Plain.text("Sollwert wird nicht uebernommen"))
        assertFalse(Plain.text("x heatd schreibt sie erneut").contains("heatd"))
    }

    @Test
    fun tileLinesSummariseRoomsWindowsAndAlerts() {
        val g = Glance(1_000L, listOf(RoomGlance("bad", 20.0, 21.0, false), RoomGlance("kueche", 19.0, 20.0, true)))
        val l = GlanceView.lines(g, listOf(Alert("a", "Thermostat", "t")), 2_000L, "kueche").map { it.text }
        assertEquals(listOf("Küche 19,0° · Fenster offen", "Bad 20,0° · heizt", "Thermostat"), l.take(3))
        assertTrue(l.last().startsWith("Stand "))
        assertEquals(GlanceView.NONE, GlanceView.lines(Glance.EMPTY, emptyList(), 5L).first().text)
        assertEquals(GlanceView.NONE, GlanceView.lines(g, emptyList(), 1_000L + Glance.STALE_MS + 1).first().text)
    }

    @Test
    fun setpointControlStepsClampsAndFormats() {
        assertEquals(22.5, SetpointControl.step(22.0, 0.5), 0.0)
        assertEquals(26.0, SetpointControl.step(26.0, 0.5), 0.0)
        assertEquals(8.0, SetpointControl.step(8.0, -0.5), 0.0)
        assertEquals(21.5, SetpointControl.clamp(21.4), 0.0)
        assertEquals("22", SetpointControl.celsius(22.0))
        assertEquals("22,5", SetpointControl.celsius(22.5))
        val held = Manual(23.0, 5_000L, "api")
        assertEquals(23.0, SetpointControl.start(RoomGlance("bad", 20.0, 19.0, false, null, false, null, held)), 0.0)
        assertEquals(19.0, SetpointControl.start(RoomGlance("bad", 20.0, 19.0, false)), 0.0)
        assertEquals(20.0, SetpointControl.start(null), 0.0)
        assertNull(SetpointControl.active(RoomGlance("bad", 20.0, 19.0, false, null, false, null, held), 5_000L))
        assertEquals(held, SetpointControl.active(RoomGlance("bad", 20.0, 19.0, false, null, false, null, held), 4_999L))
        assertEquals("23 °C bis " + RoomView.clock(5_000L), SetpointControl.line(held))
        assertEquals(listOf("1 Std.", "2 Std.", "4 Std.", "bis morgen 6 Uhr"), HoldChoice.entries.map { it.label })
        assertEquals(HoldChoice.ONE_HOUR, HoldChoice.MORNING.next())
    }

    @Test
    fun setpointControlHoldsMorningAndBodies() {
        val zone = java.util.TimeZone.getTimeZone("Europe/Berlin")
        val at = { h: Int, m: Int -> java.util.Calendar.getInstance(zone).apply { clear(); set(2026, 10, 9, h, m) }.timeInMillis }
        assertEquals(420, SetpointControl.morning(at(23, 0), zone))
        assertEquals(15, SetpointControl.morning(at(5, 50), zone))
        assertEquals(720, SetpointControl.morning(at(6, 10), zone))
        assertEquals(60, SetpointControl.minutes(HoldChoice.ONE_HOUR, 0L, zone))
        assertEquals(420, SetpointControl.minutes(HoldChoice.MORNING, at(23, 0), zone))
        val o = JSONObject(SetpointControl.body("bad", 22.5, 120))
        assertEquals(listOf("bad", 22.5, 120, "watch"), listOf(o.getString("room"), o.getDouble("celsius"), o.getInt("hold_min"), o.getString("source")))
        val c = JSONObject(SetpointControl.clearBody("bad"))
        assertEquals(listOf("bad", true, "watch"), listOf(c.getString("room"), c.getBoolean("clear"), c.getString("source")))
        val ok = SetpointControl.result(200, """{"ok":true,"room":"bad","target":22.0,"hold_min":120.0,"until_ts":3600000,"source":"watch"}""")
        assertTrue(ok.ok)
        assertEquals("22 °C bis " + RoomView.clock(3_600_000L), ok.line)
        assertEquals("Fenster offen – Vorgabe 22 °C gilt, sobald es zu ist", SetpointControl.result(200, """{"ok":true,"target":22.0,"until_ts":3600000,"source":"watch","note":"Fenster offen – Vorgabe 22 °C gilt, sobald es zu ist"}""").line)
        assertEquals(BoostResult(false, SetpointControl.DENIED), SetpointControl.result(400, """{"ok":false,"error":"x"}"""))
        assertEquals(BoostResult(false, "Antwort unlesbar (500)"), SetpointControl.result(500, "nope"))
        assertEquals(BoostResult(true, "Vorgabe beendet"), SetpointControl.cleared(200, """{"ok":true,"cleared":true}"""))
        assertFalse(SetpointControl.cleared(404, """{"ok":false}""").ok)
    }

    @Test
    fun glanceCarriesTheManualHoldFromStateAndStorage() {
        val state = """{"rooms":[{"name":"bad","window_open":false,"manual":{"target":22.5,"value":22.5,"until_ts":1700000000000,"since_ts":1,"source":"watch"},"nodes":[{"local_temperature":20.0,"commanded_setpoint":22.5,"reason":"manual_watch"}]},{"name":"kueche","window_open":false,"manual":null,"nodes":[{"local_temperature":19.0,"commanded_setpoint":20.0}]}]}"""
        val g = Glance.parse(state, 7L)!!
        assertEquals(Manual(22.5, 1_700_000_000_000L, "watch"), RoomView.glance(g, "bad")!!.manual)
        assertNull(RoomView.glance(g, "kueche")!!.manual)
        assertEquals(g, Glance.fromJson(g.json()))
        assertEquals("Von dir vorgegeben", RoomView.why(RoomView.glance(g, "bad")))
    }

    @Test
    fun messfahrtBodiesStatusLinesAndReplies() {
        assertEquals("closed", JSONObject(Messfahrt.body("closed")).getString("doors"))
        assertEquals("watch", JSONObject(Messfahrt.body("open")).getString("source"))
        assertTrue(JSONObject(Messfahrt.cancelBody()).getBoolean("cancel"))
        assertEquals("Türen zu", Messfahrt.doorsLabel("closed"))
        assertEquals("Türen offen", Messfahrt.doorsLabel("open"))
        assertTrue(Messfahrt.question("open").contains("Türen offen") && Messfahrt.question("open").contains("wenn du weg bist"))
        val armed = Messfahrt.parse("""{"experiment": {"state": "armed", "doors": "closed", "phase_label": null, "planned_end_ms": null, "reason": null}}""")!!
        assertTrue(armed.active)
        assertEquals("bereit – startet, wenn du weg bist", Messfahrt.status(armed))
        val running = Messfahrt.parse("""{"experiment": {"state": "running", "doors": "open", "phase_label": "Takte", "planned_end_ms": 5000, "reason": null}}""")!!
        assertEquals("läuft: Takte, bis " + RoomView.clock(5_000L), Messfahrt.status(running))
        assertEquals(running, Messfahrt.fromJson(running.json()))
        assertEquals("fertig", Messfahrt.status(MessfahrtState("done", "open", null, null, "completed")))
        assertFalse(MessfahrtState("done", "open", null, null, "completed").active)
        assertEquals("abgebrochen", Messfahrt.status(MessfahrtState("aborted", "open", null, null, "cancel")))
        assertEquals("beendet: du bist zurück", Messfahrt.status(MessfahrtState("aborted", "open", null, null, "homecoming")))
        assertEquals("keine Messfahrt geplant", Messfahrt.status(Messfahrt.IDLE))
        assertEquals(Messfahrt.IDLE, Messfahrt.fromJson("kaputt"))
        assertNull(Messfahrt.parse("""{"rooms": []}"""))
        val ok = Messfahrt.result(200, """{"ok": true, "experiment": {"state": "armed", "doors": "closed"}}""")
        assertTrue(ok.ok)
        assertEquals("bereit – startet, wenn du weg bist", ok.line)
        assertFalse(Messfahrt.result(409, """{"ok": false, "error": "x"}""").ok)
        assertEquals("Läuft schon – erst abbrechen", Messfahrt.result(409, """{"ok": false}""").line)
        assertEquals("Server nicht erreichbar", Messfahrt.result(0, "").line)
    }
}
