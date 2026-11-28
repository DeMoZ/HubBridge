package com.demoz.hubbridge

import java.util.concurrent.ConcurrentSkipListMap

// The decisions of the video output, with no Android types, so that the JVM tests
// (`MirrorRulesTest`) can run them.

/**
 * What to do when Android takes a running mirror's window away. Android 12 cancels a
 * `Presentation` when its display's size changes, so a new window is opened while the display
 * is there, a limited number of times.
 */
class SurfaceRecovery(private val limit: Int = LIMIT, private val windowMs: Long = WINDOW_MS) {
    enum class Verdict { RECREATE, DISPLAY_REMOVED, DESKTOP_MODE, GIVE_UP }

    private val attempts = ArrayDeque<Long>()

    /** Asked before every attempt; [Verdict.RECREATE] counts as one. */
    @Synchronized
    fun next(nowMs: Long, displayPresent: Boolean, desktopMode: Boolean): Verdict {
        if (!displayPresent) return Verdict.DISPLAY_REMOVED
        // With DeX on, the monitor shows DeX whatever we draw.
        if (desktopMode) return Verdict.DESKTOP_MODE
        while (attempts.isNotEmpty() && nowMs - attempts.first() >= windowMs) attempts.removeFirst()
        if (attempts.size >= limit) return Verdict.GIVE_UP
        attempts.addLast(nowMs)
        return Verdict.RECREATE
    }

    companion object {
        const val LIMIT = 5
        const val WINDOW_MS = 30_000L
    }
}

/**
 * The decoder of a mirror, which is replaced when the window is. While there is none, and
 * until the new one has had a keyframe, pictures are taken and thrown away: the agent must
 * see neither a stalled decoder (2 s) nor a failing one, and the host's window must not stay
 * full. The keyframe asked for after the replacement brings the picture back.
 */
class DecoderSlot<D : Any> {
    private var decoder: D? = null
    private var rebuilding = false
    private var needKeyframe = false

    @get:Synchronized
    val current: D? get() = decoder

    @get:Synchronized
    val isRebuilding: Boolean get() = rebuilding

    @Synchronized
    fun install(new: D) {
        decoder = new
        // The first decoder of a stream gets the stream from its start.
        needKeyframe = rebuilding
        rebuilding = false
    }

    /** The window is being replaced: the caller releases what is returned. */
    @Synchronized
    fun retire(): D? {
        rebuilding = true
        return decoder.also { decoder = null }
    }

    /** The mirror ends: the caller releases what is returned. */
    @Synchronized
    fun clear(): D? {
        rebuilding = false
        return decoder.also { decoder = null }
    }

    /**
     * One frame of the stream; [queue] gives it to a decoder and may block. The codes are
     * those of [MirrorDecoder]: [taken] and [error].
     */
    fun frame(config: Boolean, keyframe: Boolean, taken: Int, error: Int, queue: (D) -> Int): Int {
        val target: D
        synchronized(this) {
            val d = decoder ?: return if (rebuilding) taken else error
            // A delta frame means nothing to a new decoder.
            if (needKeyframe && !config && !keyframe) return taken
            target = d
        }
        val result = queue(target)
        synchronized(this) {
            // Replaced meanwhile: whatever the old one said, the frame is gone.
            if (decoder !== target) return if (decoder != null || rebuilding) taken else result
            if (keyframe && result == taken) needKeyframe = false
        }
        return result
    }
}

/**
 * The agent's rule for decoder errors ("a keyframe failed too"), for the errors that show up
 * when pictures come out: the first asks for a keyframe; when decoding that keyframe fails as
 * well, before a picture came out of it or of anything after it, the decoder has failed.
 */
class OutputRecovery {
    private var recovering = false
    private var keyframePts: Long? = null

    /** True when the decoder has failed; otherwise ask for a keyframe. */
    @Synchronized
    fun onError(): Boolean {
        if (recovering && keyframePts != null) return true
        recovering = true
        return false
    }

    /** Call before the keyframe goes into the decoder. */
    @Synchronized
    fun onKeyframeQueued(ptsUs: Long) {
        if (recovering && keyframePts == null) keyframePts = ptsUs
    }

    @Synchronized
    fun onPicture(ptsUs: Long) {
        val keyframe = keyframePts ?: return
        if (ptsUs >= keyframe) {
            recovering = false
            keyframePts = null
        }
    }
}

/**
 * When each picture went into the decoder, by its timestamp. Pictures come out in order
 * (no reordering), so one older than the picture that came out never will.
 */
class DecodeTimes(private val limit: Int = 64) {
    private val queued = ConcurrentSkipListMap<Long, Long>()

    val size: Int get() = queued.size

    fun put(ptsUs: Long, nanos: Long) {
        queued[ptsUs] = nanos
        // With no output at all there is nothing to prune by.
        while (queued.size > limit) queued.pollFirstEntry()
    }

    /** When this picture went in; forgets it and all before it. */
    fun take(ptsUs: Long): Long? {
        queued.headMap(ptsUs).clear()
        return queued.remove(ptsUs)
    }

    fun clear() = queued.clear()
}
