package com.demoz.hubbridge

import android.content.Context
import android.hardware.display.DisplayManager
import android.os.Handler
import android.os.Looper
import android.util.Log
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
 */
class MirrorOutput(private val context: Context, private val onChanged: () -> Unit) {

    private class Mirror(val displayName: String) {
        @Volatile var window: MirrorPresentation? = null
        @Volatile var decoder: MirrorDecoder? = null
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
        val display = displays.getDisplays(DisplayManager.DISPLAY_CATEGORY_PRESENTATION)
            .firstOrNull { it.displayId == displayId } ?: return NO_DISPLAY
        val mirror = Mirror(display.name)
        mirrors[streamId] = mirror
        val surface = AtomicReference<Surface?>()
        val ready = CountDownLatch(1)
        main.post {
            if (mirrors[streamId] !== mirror) {
                ready.countDown()
                return@post
            }
            try {
                val window = MirrorPresentation(
                    context, display, width, height,
                    onSurfaceReady = {
                        surface.set(it)
                        ready.countDown()
                    },
                    onSurfaceLost = { lost(streamId, displayId) },
                )
                mirror.window = window
                window.show()
            } catch (e: RuntimeException) {
                // `InvalidDisplayException` among others.
                Log.e(TAG, "cannot open a window on display $displayId", e)
                ready.countDown()
            }
        }
        val shown = ready.await(SURFACE_TIMEOUT_MS, TimeUnit.MILLISECONDS)
        val target = surface.get()
        if (!shown || target == null) {
            stop(streamId)
            return NO_WINDOW
        }
        try {
            mirror.decoder = MirrorDecoder.start(codec, width, height, target) {
                NativeAgent.requestKeyframe(streamId)
            }
        } catch (e: IOException) {
            Log.e(TAG, "cannot create the decoder $codec", e)
            stop(streamId)
            return DECODER_FAILED
        } catch (e: RuntimeException) {
            Log.e(TAG, "cannot start the decoder $codec", e)
            stop(streamId)
            return DECODER_FAILED
        }
        // The surface may have gone while the decoder started.
        if (mirrors[streamId] !== mirror || !target.isValid) {
            mirror.decoder?.release()
            stop(streamId)
            return NO_WINDOW
        }
        onChanged()
        return STARTED
    }

    fun frame(streamId: Long, data: ByteArray, ptsUs: Long, flags: Int): Int {
        val decoder = mirrors[streamId]?.decoder ?: return MirrorDecoder.ERROR
        return decoder.queue(data, ptsUs, flags and FLAG_CONFIG != 0)
    }

    fun stats(streamId: Long): String = mirrors[streamId]?.decoder?.stats() ?: ""

    /** Remove the picture. Unknown and stopped streams are ignored. */
    fun stop(streamId: Long) {
        val mirror = mirrors.remove(streamId) ?: return
        mirror.decoder?.release()
        main.post { mirror.window?.close() }
        onChanged()
    }

    /** The service goes away. Main thread. */
    fun stopAll() {
        for (id in mirrors.keys.toList()) {
            val mirror = mirrors.remove(id) ?: continue
            mirror.decoder?.release()
            mirror.window?.close()
        }
    }

    /** Main thread: Android took the surface or the window away. */
    private fun lost(streamId: Long, displayId: Int) {
        val mirror = mirrors[streamId] ?: return
        mirror.decoder?.release()
        val gone = displays.getDisplay(displayId) == null
        Log.i(TAG, "mirror surface lost, display ${if (gone) "gone" else "present"}")
        if (gone) {
            NativeAgent.mirrorFailed(streamId, END_DISPLAY_REMOVED, "the monitor was disconnected")
        } else {
            NativeAgent.mirrorFailed(streamId, END_SURFACE_LOST, "the phone took the picture off the display")
        }
        // Also for a stream the agent does not know yet (still starting).
        stop(streamId)
    }

    companion object {
        private const val TAG = "HubBridge"
        private const val SURFACE_TIMEOUT_MS = 3000L
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
        const val END_DISPLAY_REMOVED = 3
    }
}
