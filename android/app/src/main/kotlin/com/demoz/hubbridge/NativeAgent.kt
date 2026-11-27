package com.demoz.hubbridge

/**
 * The Rust agent core (crates/android, libhubbridge_android.so).
 *
 * It owns the TCP server, PIN pairing, encrypted sessions and device events; Kotlin owns
 * Android APIs: UsbManager, permissions, NSD, the foreground service and the UI.
 * Device lists and status cross the boundary as JSON.
 */
object NativeAgent {
    init {
        System.loadLibrary("hubbridge_android")
    }

    /** Called from Rust worker threads, never the main thread. */
    interface Callbacks {
        fun onPermissionRequested(deviceId: Int)
        fun onStatusChanged()
    }

    /**
     * Start (or restart) the agent. [lan] = listen on all interfaces (Wi-Fi/Ethernet);
     * otherwise only 127.0.0.1, reachable just through `adb forward`.
     * Returns the bound port, or -1 (see logcat tag HubBridge).
     */
    external fun start(
        stateDir: String,
        name: String,
        model: String,
        os: String,
        appVersion: String,
        usbHost: Boolean,
        port: Int,
        lan: Boolean,
        callbacks: Callbacks,
    ): Int

    external fun stop()

    /** Opens a 2-minute pairing window and returns its 6-digit PIN ("" if not running). */
    external fun startPairing(): String

    external fun cancelPairing()

    /** JSON array from [UsbScanner.scanJson]. */
    external fun setDevices(json: String)

    /** JSON of `hubbridge_agent::AgentStatus`, or "" if not running. */
    external fun status(): String

    external fun forgetHost(hostId: String): Boolean
}
