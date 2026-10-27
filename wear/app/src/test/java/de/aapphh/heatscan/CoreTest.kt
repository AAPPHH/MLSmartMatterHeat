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
}
