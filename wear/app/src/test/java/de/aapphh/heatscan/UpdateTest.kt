package de.aapphh.heatscan

import androidx.test.core.app.ApplicationProvider
import android.content.Context
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowNetwork

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class UpdateTest {
    @get:Rule val tmp = TemporaryFolder()

    private val body = "apk-bytes".toByteArray()
    private val sha = java.security.MessageDigest.getInstance("SHA-256").digest(body).joinToString("") { b -> "%02x".format(b) }

    private fun info(code: Long = 32, sha: String = this.sha, size: Long = body.size.toLong()) = UpdateInfo(code, "4.1.0", sha, size)

    @Test
    fun parsesContractAndComparesVersions() {
        val i = UpdateRules.parse("""{"version_code":33,"version_name":"4.2.0","sha256":"${"AB".repeat(32)}","size":10}""")!!
        assertEquals(UpdateInfo(33, "4.2.0", "ab".repeat(32), 10), i)
        assertTrue(UpdateRules.newer(i, 32))
        assertFalse(UpdateRules.newer(i, 33))
        assertFalse(UpdateRules.newer(i, 34))
        assertNull(UpdateRules.parse("garbage"))
        assertNull(UpdateRules.parse("""{"version_code":33,"version_name":"x","sha256":"short","size":10}"""))
    }

    @Test
    fun shaOrSizeMismatchRejects() {
        val f = tmp.newFile("update.apk").apply { writeBytes(body) }
        assertTrue(UpdateRules.intact(f, info()))
        assertFalse(UpdateRules.intact(f, info(sha = "0".repeat(64))))
        assertFalse(UpdateRules.intact(f, info(size = body.size + 1L)))
        assertFalse(UpdateRules.intact(tmp.root.resolve("missing.apk"), info()))
    }

    @Test
    fun noNetworkDoesNothing() {
        val c = ApplicationProvider.getApplicationContext<Context>()
        val store = Store(c)
        assertFalse(Updater.start(c, 1_000_000L, null))
        assertEquals(0L, store.updateAt)
        assertFalse(store.updateFailed)
    }

    @Test
    fun networkAloneStartsACheckWithoutCharger() {
        val c = ApplicationProvider.getApplicationContext<Context>()
        val store = Store(c)
        val done = java.util.concurrent.CountDownLatch(1)
        assertTrue(Updater.start(c, 1_000_000L, ShadowNetwork.newInstance(1)) { done.countDown() })
        assertEquals(1_000_000L, store.updateAt)
        assertTrue(done.await(10, java.util.concurrent.TimeUnit.SECONDS))
    }

    @Test
    fun gapIsSixtyMinutes() {
        assertEquals(3_600_000L, UpdateRules.GAP_MS)
        val now = 100 * UpdateRules.GAP_MS
        assertFalse(UpdateRules.due(now, now - 59 * 60_000L, false, false))
        assertTrue(UpdateRules.due(now, now - 60 * 60_000L, false, false))
    }

    @Test
    fun networkIsKeptWhileUpdatingUntilTheCap() {
        assertTrue(UpdateRules.keepNetwork(true, 0))
        assertTrue(UpdateRules.keepNetwork(true, UpdateRules.HOLD_CAP_MS - 1))
        assertFalse(UpdateRules.keepNetwork(true, UpdateRules.HOLD_CAP_MS))
        assertFalse(UpdateRules.keepNetwork(false, 1_000L))
        assertFalse(UpdateRules.keepNetwork(true, -1L))
    }

    @Test
    fun throttleAllowsOneCheckPerGapAndOneRetryPerSession() {
        val now = 100 * UpdateRules.GAP_MS
        assertTrue(UpdateRules.due(now, 0L, false, false))
        assertFalse(UpdateRules.due(now, now - 1_000L, false, false))
        assertFalse(UpdateRules.due(now, now - UpdateRules.GAP_MS + 1, false, false))
        assertTrue(UpdateRules.due(now, now - UpdateRules.GAP_MS, false, false))
        assertTrue(UpdateRules.due(now, now - 1_000L, true, false))
        assertFalse(UpdateRules.due(now, now - 1_000L, true, true))
        assertTrue(UpdateRules.due(now, now + 5_000L, false, false))
    }

    @Test
    fun failedVersionIsSkippedUntilHigherOne() {
        val i = info(code = 33)
        assertFalse(UpdateRules.newer(i, 32, 33))
        assertTrue(UpdateRules.newer(info(code = 34), 32, 33))
        assertTrue(UpdateRules.verdict(android.content.pm.PackageInstaller.STATUS_FAILURE, 0))
        assertFalse(UpdateRules.verdict(android.content.pm.PackageInstaller.STATUS_SUCCESS, 5))
        assertFalse(UpdateRules.verdict(android.content.pm.PackageInstaller.STATUS_PENDING_USER_ACTION, 1))
        assertTrue(UpdateRules.verdict(android.content.pm.PackageInstaller.STATUS_PENDING_USER_ACTION, 2))
    }

    @Test
    fun retryFlagResetsOnlyAfterTheWindow() {
        val now = 100 * UpdateRules.GAP_MS
        assertFalse(UpdateRules.windowOver(now, now - 1_000L))
        assertTrue(UpdateRules.windowOver(now, now - UpdateRules.GAP_MS))
        assertTrue(UpdateRules.windowOver(now, 0L))
    }

    @Test
    fun downloadIsCappedAtDeclaredSize() {
        val out = java.io.ByteArrayOutputStream()
        UpdateRules.copyCapped(java.io.ByteArrayInputStream(ByteArray(1000)), out, 10)
        assertEquals(11, out.size())
        val small = java.io.ByteArrayOutputStream()
        UpdateRules.copyCapped(java.io.ByteArrayInputStream(ByteArray(5)), small, 10)
        assertEquals(5, small.size())
    }
}
