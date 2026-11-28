package com.demoz.hubbridge

import android.app.Activity
import android.app.ActivityOptions
import android.app.KeyguardManager
import android.app.Presentation
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Point
import android.graphics.Rect
import android.graphics.Typeface
import android.hardware.display.DisplayManager
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock
import android.util.Log
import android.view.Display
import android.view.View
import android.view.WindowInsets
import android.view.WindowManager
import java.lang.ref.WeakReference

private const val SPIKE_TAG = "HubBridgeSpike"

/**
 * Throwaway probe (design D1): can HubBridge put its own picture on the phone's external
 * monitor? Reached only through MainActivity's debuggable-build check. Main thread only.
 */
class DisplaySpike(private val context: Context) {

    private val displays = context.getSystemService(DisplayManager::class.java)
    private val main = Handler(Looper.getMainLooper())
    private var presentation: Presentation? = null
    private var watching = false

    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) = logDisplayEvent("added", displayId)
        override fun onDisplayRemoved(displayId: Int) = logDisplayEvent("removed", displayId)
        override fun onDisplayChanged(displayId: Int) = logDisplayEvent("changed", displayId)
    }

    private val screenReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            logPhoneState(intent.action?.substringAfterLast('.') ?: "?")
        }
    }

    /** `route` is `s1`, `s3`, `s4` or `stop`. Starting a route removes the one before it. */
    fun run(route: String) {
        Log.i(SPIKE_TAG, "route $route requested")
        when (route) {
            "stop" -> stop()
            "s1", "s3", "s4" -> {
                removeWindows()
                watch()
                logPhoneState("start")
                logDisplays()
                logDecoder()
                when (route) {
                    "s1" -> showPresentation()
                    "s3" -> launchActivity(route, presentationDisplay())
                    else -> launchActivity(route, desktopDisplay())
                }
            }
            else -> Log.w(SPIKE_TAG, "unknown route $route")
        }
    }

    fun stop() {
        removeWindows()
        if (watching) {
            displays.unregisterDisplayListener(displayListener)
            context.unregisterReceiver(screenReceiver)
            watching = false
        }
        Log.i(SPIKE_TAG, "stopped")
    }

    private fun watch() {
        if (watching) return
        displays.registerDisplayListener(displayListener, main)
        val filter = IntentFilter().apply {
            addAction(Intent.ACTION_SCREEN_OFF)
            addAction(Intent.ACTION_SCREEN_ON)
            addAction(Intent.ACTION_USER_PRESENT)
        }
        // System broadcasts only, so the receiver does not need to be exported.
        if (Build.VERSION.SDK_INT >= 33) {
            context.registerReceiver(screenReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            context.registerReceiver(screenReceiver, filter)
        }
        watching = true
    }

    private fun removeWindows() {
        presentation?.let {
            presentation = null
            it.dismiss()
        }
        SpikeActivity.current?.get()?.finishAndRemoveTask()
        SpikeActivity.current = null
    }

    private fun presentationDisplay(): Display? =
        displays.getDisplays(DisplayManager.DISPLAY_CATEGORY_PRESENTATION).firstOrNull()

    /**
     * Samsung names the DeX desktop display "Desktop". It was missing from `displays.displays`
     * on the test phone, so low ids are also asked for one by one.
     */
    private fun desktopDisplay(): Display? =
        (displays.displays.asSequence() + (1..32).asSequence().mapNotNull { displays.getDisplay(it) })
            .firstOrNull { it.name == "Desktop" }

    private fun showPresentation() {
        val display = presentationDisplay()
        if (display == null) {
            Log.w(SPIKE_TAG, "s1: no presentation display")
            return
        }
        try {
            val p = Presentation(context, display)
            p.window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            p.setContentView(SpikePatternView(p.context, "s1", display))
            p.setOnCancelListener { Log.i(SPIKE_TAG, "s1: Presentation cancelled (display ${display.displayId})") }
            p.setOnDismissListener {
                Log.i(SPIKE_TAG, "s1: Presentation dismissed (display ${display.displayId})")
                if (presentation === p) presentation = null
            }
            p.show()
            presentation = p
            Log.i(SPIKE_TAG, "s1: Presentation shown on ${describe(display)}, isShowing=${p.isShowing}")
        } catch (e: RuntimeException) {
            Log.e(SPIKE_TAG, "s1: cannot show a Presentation on display ${display.displayId}", e)
        }
    }

    private fun launchActivity(route: String, display: Display?) {
        if (display == null) {
            Log.w(SPIKE_TAG, "$route: no target display")
            return
        }
        val options = ActivityOptions.makeBasic().setLaunchDisplayId(display.displayId)
        if (route == "s4") {
            // DeX opens windows; ask for one that covers the whole desktop.
            val size = realSize(display)
            options.setLaunchBounds(Rect(0, 0, size.x, size.y))
        }
        val intent = Intent(context, SpikeActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_MULTIPLE_TASK)
            .putExtra(SpikeActivity.EXTRA_ROUTE, route)
        try {
            context.startActivity(intent, options.toBundle())
            Log.i(SPIKE_TAG, "$route: activity start requested on ${describe(display)}")
        } catch (e: RuntimeException) {
            Log.e(SPIKE_TAG, "$route: cannot start the activity on display ${display.displayId}", e)
        }
    }

    private fun logDisplayEvent(what: String, displayId: Int) {
        val display = displays.getDisplay(displayId)
        Log.i(SPIKE_TAG, "display $what: " + (display?.let { describe(it) } ?: "id=$displayId"))
    }

    private fun logDisplays() {
        for (d in displays.displays) Log.i(SPIKE_TAG, "display: ${describe(d)}")
        val ids = displays.getDisplays(DisplayManager.DISPLAY_CATEGORY_PRESENTATION).map { it.displayId }
        Log.i(SPIKE_TAG, "presentation displays: $ids")
    }

    private fun logPhoneState(event: String) {
        val locked = context.getSystemService(KeyguardManager::class.java).isKeyguardLocked
        val interactive = context.getSystemService(PowerManager::class.java).isInteractive
        Log.i(SPIKE_TAG, "phone $event: keyguardLocked=$locked interactive=$interactive")
    }

    private fun logDecoder() {
        try {
            val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, 1280, 720)
            val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
            val name = list.findDecoderForFormat(format)
            if (name == null) {
                Log.w(SPIKE_TAG, "decoder: none for video/avc 1280x720")
                return
            }
            val info = list.codecInfos.first { it.name == name }
            val lowLatency = if (Build.VERSION.SDK_INT >= 30) {
                info.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                    .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
                    .toString()
            } else {
                "unknown (API < 30)"
            }
            Log.i(SPIKE_TAG, "decoder: $name for video/avc 1280x720, FEATURE_LowLatency=$lowLatency")
        } catch (e: RuntimeException) {
            Log.e(SPIKE_TAG, "decoder query failed", e)
        }
    }
}

private fun realSize(display: Display): Point {
    val size = Point()
    @Suppress("DEPRECATION")
    display.getRealSize(size)
    return size
}

private fun describe(display: Display): String {
    val size = realSize(display)
    return "id=${display.displayId} \"${display.name}\" ${size.x}x${size.y}@${display.refreshRate} " +
        "flags=0x${Integer.toHexString(display.flags)} state=${display.state}"
}

/** Animated test pattern: moving bar, frame counter, display id and size. */
private class SpikePatternView(context: Context, private val route: String, private val target: Display) :
    View(context) {

    private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply { typeface = Typeface.MONOSPACE }
    private var frames = 0L
    private var lastLog = 0L

    override fun onDraw(canvas: Canvas) {
        frames++
        val w = width.toFloat()
        val h = height.toFloat()
        canvas.drawColor(Color.rgb(0, 40, 90))

        paint.color = Color.rgb(255, 200, 0)
        val bar = w / 20
        val x = (frames * 8 % (w + bar).toLong().coerceAtLeast(1)) - bar
        canvas.drawRect(x, 0f, x + bar, h, paint)

        // A white frame at the edges shows cropping or scaling.
        paint.color = Color.WHITE
        val edge = h / 90
        canvas.drawRect(0f, 0f, w, edge, paint)
        canvas.drawRect(0f, h - edge, w, h, paint)
        canvas.drawRect(0f, 0f, edge, h, paint)
        canvas.drawRect(w - edge, 0f, w, h, paint)

        val size = realSize(target)
        paint.textSize = h / 10
        val left = w / 12
        canvas.drawText("HubBridge spike ${route.uppercase()}", left, h * 0.30f, paint)
        canvas.drawText("frame $frames", left, h * 0.50f, paint)
        canvas.drawText("display ${target.displayId}: ${size.x}x${size.y}", left, h * 0.70f, paint)
        canvas.drawText("view ${width}x$height", left, h * 0.85f, paint)

        val now = SystemClock.elapsedRealtime()
        if (now - lastLog >= 5000) {
            lastLog = now
            Log.i(SPIKE_TAG, "$route: drawing frame $frames, view ${width}x$height, window on display ${display?.displayId}")
        }
        postInvalidateOnAnimation()
    }
}

/** Full-screen test pattern for routes S3 and S4. Not exported. */
class SpikeActivity : Activity() {

    private var route = "?"

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        route = intent.getStringExtra(EXTRA_ROUTE) ?: "?"
        current = WeakReference(this)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        @Suppress("DEPRECATION")
        val shownOn = windowManager.defaultDisplay
        setContentView(SpikePatternView(this, route, shownOn))
        if (Build.VERSION.SDK_INT >= 30) {
            window.insetsController?.hide(WindowInsets.Type.systemBars())
        }
        Log.i(SPIKE_TAG, "$route: activity created on ${describe(shownOn)}")
    }

    override fun onStart() {
        super.onStart()
        Log.i(SPIKE_TAG, "$route: activity started")
    }

    override fun onStop() {
        Log.i(SPIKE_TAG, "$route: activity stopped")
        super.onStop()
    }

    override fun onDestroy() {
        Log.i(SPIKE_TAG, "$route: activity destroyed")
        if (current?.get() === this) current = null
        super.onDestroy()
    }

    companion object {
        const val EXTRA_ROUTE = "route"
        var current: WeakReference<SpikeActivity>? = null
    }
}
