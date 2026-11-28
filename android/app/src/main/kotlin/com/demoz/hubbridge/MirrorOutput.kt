package com.demoz.hubbridge

import android.content.Context
import android.hardware.display.DisplayManager
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.Display
import android.view.Surface
import java.io.IOException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/**
 * The running mirrors: for each stream a [MirrorPresentation] on the monitor and a
 * [MirrorDecoder] drawing on it. The agent calls [start], [frame], [stats] and [stop] from its
 * threads (never the main thread).
 *
 * Android cancels the window of a monitor that changes its mode. While the display is there,
 * the window and the decoder are made anew (see [SurfaceRecovery] and [DecoderSlot]).
 */
class MirrorOutput(private val context: Context, private val onChanged: () -> Unit) {

    private class Mirror(
        val displayId: Int,
        val displayName: String,
        val codec: String,
        val width: Int,
        val height: Int,
    ) {
        /** The window that counts; what an earlier one reports is ignored. */
        @Volatile var window: MirrorPresentation? = null
        val decoder = DecoderSlot<MirrorDecoder>()
        val recovery = SurfaceRecovery()

        // Both under the mirror's lock.
        var running = false
        var retired = MirrorDecoder.Counters()
    }

    private val main = Handler(Looper.getMainLooper())
    private val displays = context.getSystemService(DisplayManager::class.java)
    private val mirrors = ConcurrentHashMap<Long, Mirror>()

    /** The monitor of a running mirror, for the notification. */
    val shownOn: String? get() = mirrors.values.firstOrNull()?.displayName

    fun start(streamId: Long, displayId: Int, width: Int, height: Int): Int {
        // With DeX on, Android draws our window and the monitor keeps showing DeX.
        if (DisplayWatcher.isDesktopMode(context)) return DEX_ON
        val codec = MirrorDecoder.find(width, height) ?: return NO_DECODER
        val display = presentationDisplay(displayId) ?: return NO_DISPLAY
        val mirror = Mirror(displayId, display.name, codec, width, height)
        mirrors[streamId] = mirror
        val result = open(streamId, mirror, display)
        if (result != STARTED) {
            stop(streamId)
            return result
        }
        onChanged()
        return STARTED
    }

    private fun presentationDisplay(displayId: Int): Display? =
        displays.getDisplays(DisplayManager.DISPLAY_CATEGORY_PRESENTATION)
            .firstOrNull { it.displayId == displayId }

    /** Open a window on [display] and start a decoder on its surface. May block. */
    private fun open(streamId: Long, mirror: Mirror, display: Display): Int {
        val surface = AtomicReference<Surface?>()
        val opened = AtomicReference<MirrorPresentation?>()
        val ready = CountDownLatch(1)
        main.post {
            if (mirrors[streamId] !== mirror) {
                ready.countDown()
                return@post
            }
            try {
                var made: MirrorPresentation? = null
                val window = MirrorPresentation(
                    context, display, mirror.width, mirror.height,
                    onSurfaceReady = {
                        surface.set(it)
                        ready.countDown()
                    },
                    onSurfaceLost = {
                        made?.let { lost(streamId, mirror, it) }
                        ready.countDown()
                    },
                )
                made = window
                opened.set(window)
                mirror.window = window
                window.show()
            } catch (e: RuntimeException) {
                // `InvalidDisplayException` among others.
                Log.e(TAG, "cannot open a window on display ${mirror.displayId}", e)
                ready.countDown()
            }
        }
        val shown = ready.await(SURFACE_TIMEOUT_MS, TimeUnit.MILLISECONDS)
        val target = surface.get()
        if (!shown || target == null) {
            discard(opened.get())
            return NO_WINDOW
        }
        val decoder = try {
            MirrorDecoder.start(
                mirror.codec, mirror.width, mirror.height, target,
                onOutputError = { NativeAgent.requestKeyframe(streamId) },
                onFailed = {
                    NativeAgent.mirrorFailed(streamId, END_DECODER_FAILED, "the phone's video decoder failed")
                },
            )
        } catch (e: IOException) {
            Log.e(TAG, "cannot create the decoder ${mirror.codec}", e)
            discard(opened.get())
            return DECODER_FAILED
        } catch (e: RuntimeException) {
            Log.e(TAG, "cannot start the decoder ${mirror.codec}", e)
            discard(opened.get())
            return DECODER_FAILED
        }
        synchronized(mirror) {
            // The window may have gone, or the mirror ended, while the decoder started.
            if (mirrors[streamId] === mirror && mirror.window === opened.get() && target.isValid) {
                mirror.decoder.install(decoder)
                mirror.running = true
                return STARTED
            }
        }
        decoder.release()
        discard(opened.get())
        return NO_WINDOW
    }

    /** Close a window that is not going to be used. */
    private fun discard(window: MirrorPresentation?) {
        if (window != null) main.post { window.close() }
    }

    fun frame(streamId: Long, data: ByteArray, ptsUs: Long, flags: Int): Int {
        val mirror = mirrors[streamId] ?: return MirrorDecoder.ERROR
        val config = flags and FLAG_CONFIG != 0
        val keyframe = flags and FLAG_KEY != 0
        return mirror.decoder.frame(config, keyframe, MirrorDecoder.QUEUED, MirrorDecoder.ERROR) {
            it.queue(data, ptsUs, config, keyframe)
        }
    }

    fun stats(streamId: Long): String {
        val mirror = mirrors[streamId] ?: return ""
        synchronized(mirror) {
            val decoder = mirror.decoder.current
            val counters = mirror.retired + (decoder?.counters() ?: MirrorDecoder.Counters())
            return MirrorDecoder.statsJson(counters, decoder?.decodeUs() ?: 0)
        }
    }

    /** Remove the picture. Unknown and stopped streams are ignored. */
    fun stop(streamId: Long) {
        val mirror = mirrors.remove(streamId) ?: return
        synchronized(mirror) { mirror.decoder.clear() }?.release()
        main.post { mirror.window?.close() }
        onChanged()
    }

    /** The service goes away. Main thread. */
    fun stopAll() {
        for (id in mirrors.keys.toList()) {
            val mirror = mirrors.remove(id) ?: continue
            synchronized(mirror) { mirror.decoder.clear() }?.release()
            mirror.window?.close()
        }
    }

    /**
     * Main thread: Android took the surface or the window away. The decoder lets go of the
     * surface before this returns.
     */
    private fun lost(streamId: Long, mirror: Mirror, window: MirrorPresentation) {
        if (mirrors[streamId] !== mirror || mirror.window !== window) return
        mirror.window = null
        val running: Boolean
        val already: Boolean
        val old: MirrorDecoder?
        synchronized(mirror) {
            running = mirror.running
            already = mirror.decoder.isRebuilding
            old = mirror.decoder.retire()
        }
        if (old != null) {
            old.release()
            synchronized(mirror) { mirror.retired += old.counters() }
        }
        // Not from inside the window's own callback.
        main.post { window.close() }
        val present = displays.getDisplay(mirror.displayId) != null
        Log.i(TAG, "mirror surface lost, display ${if (present) "present" else "gone"}")
        when {
            !present -> end(streamId, SurfaceRecovery.Verdict.DISPLAY_REMOVED)
            // Still starting (the start is refused).
            !running -> end(streamId, SurfaceRecovery.Verdict.GIVE_UP)
            // Otherwise the thread that is at it tries again.
            !already -> Thread({ recreate(streamId, mirror) }, "hubbridge-video-window").start()
        }
    }

    /** A new window and decoder on the same display, for as long as that makes sense. */
    private fun recreate(streamId: Long, mirror: Mirror) {
        while (mirrors[streamId] === mirror) {
            // A removal or DeX coming on may be right behind the cancel.
            SystemClock.sleep(SETTLE_MS)
            if (mirrors[streamId] !== mirror) return
            val verdict = mirror.recovery.next(
                SystemClock.elapsedRealtime(),
                displayPresent = displays.getDisplay(mirror.displayId) != null,
                desktopMode = DisplayWatcher.isDesktopMode(context),
            )
            if (verdict != SurfaceRecovery.Verdict.RECREATE) {
                end(streamId, verdict)
                return
            }
            val display = presentationDisplay(mirror.displayId) ?: continue
            if (open(streamId, mirror, display) == STARTED) {
                Log.i(TAG, "mirror shown again on display ${mirror.displayId}")
                NativeAgent.requestKeyframe(streamId)
                return
            }
        }
    }

    /** Tell the agent why and remove the picture. */
    private fun end(streamId: Long, why: SurfaceRecovery.Verdict) {
        when (why) {
            SurfaceRecovery.Verdict.DISPLAY_REMOVED ->
                NativeAgent.mirrorFailed(streamId, END_DISPLAY_REMOVED, "the monitor was disconnected")
            SurfaceRecovery.Verdict.DESKTOP_MODE -> NativeAgent.mirrorFailed(
                streamId, END_SURFACE_LOST, "Samsung DeX was turned on: turn DeX off on the phone, then start again",
            )
            else -> NativeAgent.mirrorFailed(streamId, END_SURFACE_LOST, "the phone took the picture off the display")
        }
        // Also for a stream the agent does not know yet (still starting).
        stop(streamId)
    }

    companion object {
        private const val TAG = "HubBridge"
        private const val SURFACE_TIMEOUT_MS = 3000L
        private const val SETTLE_MS = 300L
        private const val FLAG_KEY = 1
        private const val FLAG_CONFIG = 2

        // Start codes: `start_result` in crates/android.
        const val STARTED = 0
        const val DEX_ON = 1
        const val NO_DISPLAY = 2
        const val NO_WINDOW = 3
        const val NO_DECODER = 4
        const val DECODER_FAILED = 5

        // Reasons of `mirrorFailed`: `end_reason` in crates/android.
        const val END_SURFACE_LOST = 0
        const val END_DECODER_FAILED = 1
        const val END_DISPLAY_REMOVED = 3
    }
}
