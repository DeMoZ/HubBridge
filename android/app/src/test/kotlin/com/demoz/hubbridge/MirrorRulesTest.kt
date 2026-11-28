package com.demoz.hubbridge

import com.demoz.hubbridge.SurfaceRecovery.Verdict
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

private const val TAKEN = 0
private const val NO_INPUT = 1
private const val ERROR = 2

class MirrorRulesTest {

    @Test
    fun aWindowIsOpenedAgainOnlyOnAPresentDisplayWithoutDex() {
        val recovery = SurfaceRecovery()
        assertEquals(Verdict.RECREATE, recovery.next(0, displayPresent = true, desktopMode = false))
        assertEquals(Verdict.DISPLAY_REMOVED, recovery.next(1, displayPresent = false, desktopMode = false))
        assertEquals(Verdict.DISPLAY_REMOVED, recovery.next(2, displayPresent = false, desktopMode = true))
        assertEquals(Verdict.DESKTOP_MODE, recovery.next(3, displayPresent = true, desktopMode = true))
    }

    @Test
    fun openingAgainIsBounded() {
        val recovery = SurfaceRecovery(limit = 3, windowMs = 1000)
        for (now in listOf(0L, 100L, 200L)) {
            assertEquals(Verdict.RECREATE, recovery.next(now, true, false))
        }
        assertEquals(Verdict.GIVE_UP, recovery.next(300, true, false))
        assertEquals(Verdict.GIVE_UP, recovery.next(999, true, false))
        // A mode change much later is served again.
        assertEquals(Verdict.RECREATE, recovery.next(1000, true, false))
        assertEquals(Verdict.GIVE_UP, recovery.next(1001, true, false))
    }

    /** What the agent's sink thread would get for a frame. */
    private fun DecoderSlot<String>.give(
        keyframe: Boolean,
        config: Boolean = false,
        into: MutableList<String> = mutableListOf(),
        result: Int = TAKEN,
    ): Int = frame(config, keyframe, TAKEN, ERROR) {
        into.add(it)
        result
    }

    @Test
    fun framesAreThrownAwayWhileTheDecoderIsReplacedUntilAKeyframe() {
        val slot = DecoderSlot<String>()
        val fed = mutableListOf<String>()
        slot.install("old")
        assertEquals(TAKEN, slot.give(keyframe = false, into = fed))
        assertEquals("old", slot.retire())
        // No decoder: neither a stall nor an error, and the window stays free.
        assertEquals(TAKEN, slot.give(keyframe = false, into = fed))
        assertEquals(TAKEN, slot.give(keyframe = true, into = fed))
        assertEquals(listOf("old"), fed)
        slot.install("new")
        assertEquals(TAKEN, slot.give(keyframe = false, into = fed))
        assertEquals("delta frames mean nothing to it", listOf("old"), fed)
        assertEquals(TAKEN, slot.give(keyframe = false, config = true, into = fed))
        assertEquals(NO_INPUT, slot.give(keyframe = true, into = fed, result = NO_INPUT))
        assertEquals(TAKEN, slot.give(keyframe = false, into = fed))
        assertEquals(TAKEN, slot.give(keyframe = true, into = fed))
        assertEquals(TAKEN, slot.give(keyframe = false, into = fed))
        assertEquals(listOf("old", "new", "new", "new", "new"), fed)
    }

    @Test
    fun theFirstDecoderGetsEveryFrameAndItsErrorsCount() {
        val slot = DecoderSlot<String>()
        assertEquals("nothing to decode with", ERROR, slot.give(keyframe = true))
        slot.install("first")
        assertEquals(ERROR, slot.give(keyframe = false, result = ERROR))
        assertEquals("first", slot.clear())
        assertEquals("the mirror ended", ERROR, slot.give(keyframe = false))
    }

    @Test
    fun anErrorOfADecoderReplacedDuringTheCallDoesNotCount() {
        for (replaceAtOnce in listOf(false, true)) {
            val slot = DecoderSlot<String>()
            slot.install("old")
            val result = slot.frame(false, true, TAKEN, ERROR) {
                slot.retire()
                if (replaceAtOnce) slot.install("new")
                ERROR
            }
            assertEquals(TAKEN, result)
        }
    }

    @Test
    fun aKeyframeThatFailsTooEndsTheDecoder() {
        val recovery = OutputRecovery()
        assertFalse(recovery.onError())
        // Errors before the keyframe arrived ask again.
        assertFalse(recovery.onError())
        recovery.onKeyframeQueued(100)
        // A picture from before the keyframe proves nothing.
        recovery.onPicture(90)
        recovery.onKeyframeQueued(130)
        assertTrue(recovery.onError())
    }

    @Test
    fun aPictureFromTheKeyframeOnStartsOver() {
        for (pictured in listOf(100L, 133L)) {
            val recovery = OutputRecovery()
            // A keyframe that no error asked for counts for nothing.
            recovery.onKeyframeQueued(1)
            assertFalse(recovery.onError())
            recovery.onKeyframeQueued(100)
            recovery.onPicture(pictured)
            assertFalse(recovery.onError())
        }
    }

    @Test
    fun picturesThatNeverCameOutAreForgotten() {
        val times = DecodeTimes()
        for (pts in 1L..5L) times.put(pts, pts * 10)
        assertEquals(40L, times.take(4))
        assertEquals("1 to 3 never came out", 1, times.size)
        assertNull(times.take(2))
        assertEquals(50L, times.take(5))
        assertEquals(0, times.size)
    }

    @Test
    fun withoutAnyOutputTheTimesStayBounded() {
        val times = DecodeTimes(limit = 8)
        for (pts in 1L..1000L) times.put(pts, pts)
        assertEquals(8, times.size)
        assertEquals(1000L, times.take(1000))
    }
}
