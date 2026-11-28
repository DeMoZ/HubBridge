package com.demoz.hubbridge

import android.content.Context
import android.content.res.Configuration
import android.hardware.display.DisplayManager
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Display
import org.json.JSONArray
import org.json.JSONObject
import kotlin.math.roundToInt

/**
 * The phone's presentation displays (monitors on the USB-C port or hub) as JSON for the agent.
 * The field names are those of `AndroidDisplays` in crates/android (test `parses_display_json`).
 * Main thread only.
 */
class DisplayWatcher(private val context: Context, private val publish: (String) -> Unit) {

    private val displays = context.getSystemService(DisplayManager::class.java)
    private var watching = false
    private var last = ""

    private val listener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) = push()
        override fun onDisplayRemoved(displayId: Int) = push()
        override fun onDisplayChanged(displayId: Int) = push()
    }

    /** Watch, and publish the list as it is now (also again after the agent restarted). */
    fun start() {
        if (!watching) {
            displays.registerDisplayListener(listener, Handler(Looper.getMainLooper()))
            watching = true
        }
        last = ""
        push()
    }

    fun stop() {
        if (watching) {
            displays.unregisterDisplayListener(listener)
            watching = false
        }
    }

    /** Publish the list if it differs from the last one. */
    fun push() {
        if (!watching) return
        val json = scanJson()
        if (json == last) return
        last = json
        Log.i(TAG, "displays: $json")
        publish(json)
    }

    private fun scanJson(): String {
        val list = JSONArray()
        for (d in displays.getDisplays(DisplayManager.DISPLAY_CATEGORY_PRESENTATION)) list.put(describe(d))
        return JSONObject()
            .put("desktop_mode", isDesktopMode(context))
            .put("displays", list)
            .toString()
    }

    private fun describe(d: Display): JSONObject {
        val modes = JSONArray()
        for (m in d.supportedModes) modes.put(mode(m))
        return mode(d.mode)
            .put("id", d.displayId)
            .put("name", d.name)
            .put("modes", modes)
            .put("presentation", d.flags and Display.FLAG_PRESENTATION != 0)
            .put("secure", d.flags and Display.FLAG_SECURE != 0)
    }

    private fun mode(m: Display.Mode): JSONObject = JSONObject()
        .put("width", m.physicalWidth)
        .put("height", m.physicalHeight)
        .put("refresh_mhz", (m.refreshRate * 1000).roundToInt())

    companion object {
        private const val TAG = "HubBridge"

        /**
         * Whether Samsung DeX is on, by the reflection Samsung documents for it; false on other
         * phones and whenever the fields are not there.
         */
        fun isDesktopMode(context: Context): Boolean = try {
            val config: Configuration = context.resources.configuration
            val type = config.javaClass
            val enabled = type.getField("SEM_DESKTOP_MODE_ENABLED").getInt(type)
            type.getField("semDesktopModeEnabled").getInt(config) == enabled
        } catch (_: ReflectiveOperationException) {
            false
        } catch (_: IllegalArgumentException) {
            false
        }
    }
}
