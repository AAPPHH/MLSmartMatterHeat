package de.aapphh.heatscan

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class BurstTest {
    private val min = 60_000L

    @Test
    fun cadenceAllowsOneBurstPerInterval() {
        val p = BurstPolicy()
        assertEquals(Burst.SEND, p.decide(0, 5, true, false, Trigger.CADENCE))
        assertEquals(Burst.NONE, p.decide(29 * min, 5, true, false, Trigger.CADENCE))
        assertEquals(Burst.SEND, p.decide(30 * min, 5, true, false, Trigger.CADENCE))
    }

    @Test
    fun awayNeverRequestsWifiExceptTheLeavingPing() {
        val p = BurstPolicy()
        assertEquals(Burst.NONE, p.decide(0, 5, false, false, Trigger.CADENCE))
        assertEquals(Burst.NONE, p.decide(5 * min, 5, false, false, Trigger.ARRIVED))
        assertEquals(Burst.NONE, p.decide(10 * min, 5, false, false, Trigger.OPEN))
        assertEquals(Burst.SEND, p.decide(15 * min, 5, false, false, Trigger.LEAVING))
    }

    @Test
    fun emptyQueueSendsABeatRowOnlyAtHome() {
        val p = BurstPolicy()
        assertEquals(Burst.NONE, p.decide(0, 0, false, false, Trigger.CADENCE))
        assertEquals(Burst.BEAT, p.decide(1, 0, true, false, Trigger.CADENCE))
        assertEquals(Burst.NONE, p.decide(10 * min, 0, true, false, Trigger.ARRIVED))
        assertEquals(Burst.NONE, p.decide(20 * min, 0, true, false, Trigger.LEAVING))
    }

    @Test
    fun arrivalAndOpenBypassTheCadenceButNotTheMinGap() {
        val p = BurstPolicy()
        assertEquals(Burst.SEND, p.decide(0, 5, true, false, Trigger.CADENCE))
        assertEquals(Burst.NONE, p.decide(1 * min, 5, true, false, Trigger.ARRIVED))
        assertEquals(Burst.SEND, p.decide(3 * min, 5, true, false, Trigger.ARRIVED))
        assertEquals(Burst.SEND, p.decide(6 * min, 0, true, false, Trigger.OPEN))
    }

    @Test
    fun noRequestWhileANetworkIsBound() {
        val p = BurstPolicy()
        assertEquals(Burst.NONE, p.decide(0, 5, true, true, Trigger.CADENCE))
        assertEquals(Burst.SEND, p.decide(1, 5, true, false, Trigger.CADENCE))
    }

    @Test
    fun homeWatchFiresArrivedOnAwayToHomeAndLeavingOnceWhenTheHomeWifiFades() {
        val w = HomeWatch()
        assertNull(w.onScan(true, -50))
        assertNull(w.onScan(true, -72))
        assertNull(w.onScan(true, -80))
        assertEquals(Trigger.LEAVING, w.onScan(true, -80))
        assertNull(w.onScan(false, null))
        assertEquals(Trigger.ARRIVED, w.onScan(true, -85))
        assertNull(w.onScan(true, -60))
        assertNull(w.onScan(false, null))
        assertEquals(Trigger.LEAVING, w.onScan(false, null))
        assertNull(w.onScan(false, null))
    }

    @Test
    fun releasingTheManualHoldClearsTheNetworkSoBurstsAreNotBlockedForever() {
        assertNull(HoldRelease.keep("wifi", false))
        assertEquals("wifi", HoldRelease.keep("wifi", true))
        val p = BurstPolicy()
        val bound = HoldRelease.keep("wifi", false) != null
        assertEquals(Burst.SEND, p.decide(0, 3, true, bound, Trigger.CADENCE))
    }

    @Test
    fun aSingleWeakOrMissingScanNeverFiresLeaving() {
        val w = HomeWatch()
        assertNull(w.onScan(true, -50))
        assertNull(w.onScan(true, -80))
        assertNull(w.onScan(true, -60))
        assertNull(w.onScan(false, null))
        assertEquals(Trigger.ARRIVED, w.onScan(true, -60))
        assertNull(w.onScan(true, -78))
        assertNull(w.onScan(true, -65))
        assertNull(w.onScan(true, -78))
    }

    @Test
    fun homeWatchNeedsAStrongSightingBeforeLeaving() {
        val w = HomeWatch()
        assertNull(w.onScan(false, null))
        assertEquals(Trigger.ARRIVED, w.onScan(true, -85))
        assertNull(w.onScan(false, null))
    }

    @Test
    fun strongestHomeRssiIgnoresForeignNetworks() {
        val seen = listOf("Home" to -80, "Other" to -40, "\"home\"" to -65)
        assertEquals(-65, Home.strongest(seen, listOf("home")))
        assertNull(Home.strongest(seen, listOf("nobody")))
    }

    @Test
    fun statsCountSinceLastDeliveryAndAttachAdditively() {
        val s = BurstStats()
        s.requested(80)
        s.granted()
        s.requested(77)
        s.failed()
        val row = Payload.build(1L, "w", null, emptyList(), hb = true)
        val sent = s.attach(row, 10 * min).getJSONObject("info").getJSONObject("diag")
        assertEquals(2, sent.getInt("requests"))
        assertEquals(1, sent.getInt("grants"))
        assertEquals(1, sent.getInt("failures"))
        assertEquals(80, sent.getInt("batt_first"))
        assertEquals(77, sent.getInt("batt_last"))
        assertTrue(sent.isNull("since_delivery_min"))
        assertFalse(row.getJSONObject("info").has("diag"))
        s.delivered(20 * min)
        val after = s.json(50 * min)
        assertEquals(0, after.getInt("requests"))
        assertEquals(30, after.getInt("since_delivery_min"))
        assertTrue(after.isNull("batt_first"))
    }

    @Test
    fun payloadMarksHeartbeatAndLeavingOnlyWhenAsked() {
        assertFalse(Payload.build(1L, "w", null, emptyList()).has("info"))
        val hb = Payload.build(1L, "w", null, emptyList(), hb = true).getJSONObject("info")
        assertTrue(hb.getBoolean("hb") && !hb.has("leaving"))
        val leaving = Payload.build(1L, "w", null, emptyList(), away = true, hb = true, leaving = true)
        assertTrue(leaving.getBoolean("away") && leaving.getJSONObject("info").getBoolean("leaving"))
    }
}
