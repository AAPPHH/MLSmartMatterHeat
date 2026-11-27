package de.aapphh.heatscan

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AssistantTest {
    @Test fun replyWithoutPending() {
        val r = Assistant.result(200, """{"reply": "Im Bad sind es 22 Grad.", "pending": null}""")
        assertTrue(r.ok)
        assertEquals("Im Bad sind es 22 Grad.", r.text)
        assertNull(r.pending)
    }

    @Test fun replyWithPending() {
        val r = Assistant.result(200, """{"reply": "Soll ich das tun?", "pending": {"id": "p1", "summary": "Bad auf 23 Grad bis 18:00", "expires_s": 90}}""")
        assertTrue(r.ok)
        assertEquals(Pending("p1", "Bad auf 23 Grad bis 18:00", 90.0), r.pending)
    }

    @Test fun incompletePendingIsIgnored() {
        assertNull(Assistant.result(200, """{"reply": "ok", "pending": {"id": "", "summary": "x"}}""").pending)
        assertNull(Assistant.result(200, """{"reply": "ok", "pending": {"summary": "x"}}""").pending)
    }

    @Test fun pendingWithoutReplyIsValid() {
        val r = Assistant.result(200, """{"reply": "", "pending": {"id": "p2", "summary": "Boost Küche", "expires_s": 30}}""")
        assertTrue(r.ok)
        assertEquals("p2", r.pending?.id)
    }

    @Test fun unreadableAndEmpty() {
        assertEquals(AssistantResult(false, "Antwort unlesbar (200)"), Assistant.result(200, "nope"))
        assertEquals(AssistantResult(false, "Leere Antwort"), Assistant.result(200, "{}"))
    }

    @Test fun errorMapping() {
        assertEquals("Assistent ist aus", Assistant.result(503, """{"error": "assistant disabled"}""").text)
        assertEquals("Gemini antwortet nicht", Assistant.result(504, "").text)
        assertEquals("Gemini-Fehler", Assistant.result(502, "").text)
        assertEquals("Anfrage abgelaufen", Assistant.confirmed(404, "").text)
        assertEquals("Nicht erlaubt", Assistant.result(403, "").text)
        assertEquals("Fehler (500)", Assistant.result(500, "").text)
        assertEquals("Server nicht erreichbar", Assistant.failed(Assistant.error(0)).text)
        assertFalse(Assistant.result(503, "").ok)
    }

    @Test fun confirmedDropsPending() {
        val r = Assistant.confirmed(200, """{"reply": "Erledigt.", "pending": {"id": "x", "summary": "y"}}""")
        assertEquals(AssistantResult(true, "Erledigt."), r)
    }

    @Test fun bodies() {
        val b = JSONObject(Assistant.body("  Mach es wärmer  "))
        assertEquals("Mach es wärmer", b.getString("text"))
        assertEquals(500, JSONObject(Assistant.body("a".repeat(900))).getString("text").length)
        val c = JSONObject(Assistant.confirmBody("p1", true))
        assertEquals("p1", c.getString("id"))
        assertTrue(c.getBoolean("accept"))
    }

    @Test fun transcript() {
        assertNull(Assistant.transcript(null))
        assertNull(Assistant.transcript("   "))
        assertEquals("Hallo", Assistant.transcript(" Hallo "))
    }
}
