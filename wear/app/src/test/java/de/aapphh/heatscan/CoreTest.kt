package de.aapphh.heatscan

import org.json.JSONObject
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
        assertEquals(example.keys().asSequence().toSet(), built.keys().asSequence().toSet())
        assertEquals(shape(example), shape(built))
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
        p.onMotion(25 * s)
        assertTrue(p.onTick(30 * s))
        assertEquals(Mode.MOVING, p.mode)
    }

    @Test
    fun settlesWithOneExtraScanThenStillAfterTwoMinutes() {
        val p = ScanPolicy(true, 0)
        p.onTick(0)
        p.onMotion(50 * s)
        assertTrue(p.onTick(30 * s))
        assertEquals(20 * s, p.delay(40 * s))
        assertFalse(p.onTick(55 * s))
        assertTrue(p.onTick(110 * s))
        assertEquals(Mode.SETTLING, p.mode)
        assertEquals(60 * s, p.delay(110 * s))
        assertFalse(p.onTick(120 * s))
        assertEquals(Mode.SETTLING, p.mode)
        assertFalse(p.onTick(170 * s))
        assertEquals(Mode.STILL, p.mode)
        assertEquals(ScanConfig.HEARTBEAT_MS, p.delay(170 * s))
    }

    @Test
    fun stillScansOnlyOnHeartbeat() {
        val p = still()
        assertFalse(p.onTick(170 * s + 599 * s))
        assertTrue(p.onTick(170 * s + 600 * s))
        assertEquals(ScanConfig.HEARTBEAT_MS, p.delay(770 * s))
        assertEquals(Mode.STILL, p.mode)
    }

    @Test
    fun motionWakesFromStillWithImmediateScan() {
        val p = still()
        assertTrue(p.onMotion(300 * s))
        assertEquals(Mode.MOVING, p.mode)
        assertEquals(30 * s, p.delay(300 * s))
        assertFalse(p.onMotion(301 * s))
    }

    @Test
    fun motionWhileSettlingReturnsToMovingWithoutExtraScan() {
        val p = ScanPolicy(true, 0)
        p.onTick(0)
        assertTrue(p.onTick(60 * s))
        assertFalse(p.onMotion(80 * s))
        assertEquals(Mode.MOVING, p.mode)
        assertEquals(30 * s, p.delay(80 * s))
    }

    @Test
    fun fixedFallbackIgnoresMotionAndScansEvery30s() {
        val p = ScanPolicy(false, 0)
        assertEquals(Mode.FIXED, p.mode)
        assertTrue(p.onTick(0))
        assertFalse(p.onMotion(5 * s))
        assertFalse(p.onTick(29 * s))
        assertTrue(p.onTick(30 * s))
        assertEquals(30 * s, p.delay(30 * s))
        assertEquals(Mode.FIXED, p.mode)
    }

    private fun still(): ScanPolicy = ScanPolicy(true, 0).also {
        it.onTick(0)
        it.onTick(60 * s)
        it.onTick(170 * s)
    }
}
