package com.demoz.hubbridge

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.hardware.usb.UsbManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.util.Log
import org.json.JSONObject
import java.util.concurrent.CopyOnWriteArraySet
import java.util.concurrent.Executors

/**
 * Foreground service that keeps the bridge alive with the screen off: runs the native
 * agent, holds a partial wake lock and a low-latency Wi-Fi lock, announces itself on NSD,
 * and pushes USB hot-plug changes to the agent.
 */
class BridgeService : Service(), NativeAgent.Callbacks {

    private val main = Handler(Looper.getMainLooper())
    private val io = Executors.newSingleThreadExecutor()
    private lateinit var scanner: UsbScanner
    private lateinit var nsd: NsdAdvertiser
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var running = false

    private val usbReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            Log.i(TAG, "USB event ${intent.action}")
            pushDevices()
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
        scanner = UsbScanner(this)
        nsd = NsdAdvertiser(this)
        createChannel()
        val filter = IntentFilter().apply {
            addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED)
            addAction(UsbManager.ACTION_USB_DEVICE_DETACHED)
            addAction(ACTION_USB_PERMISSION)
        }
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(usbReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            registerReceiver(usbReceiver, filter)
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stopSelf()
                return START_NOT_STICKY
            }
            ACTION_PAIR -> {
                startForegroundNow()
                if (!running) startAgent(intent.getBooleanExtra(EXTRA_LAN, true))
                pin = NativeAgent.startPairing()
                notifyChanged()
            }
            ACTION_RESCAN -> pushDevices()
            else -> {
                startForegroundNow()
                val lan = intent?.getBooleanExtra(EXTRA_LAN, true) ?: true
                if (!running || lan != lanMode) startAgent(lan)
            }
        }
        return START_STICKY
    }

    private fun startForegroundNow() {
        val n = buildNotification()
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        } else {
            startForeground(NOTIFICATION_ID, n)
        }
    }

    private fun startAgent(lan: Boolean) {
        val name = Settings.Global.getString(contentResolver, Settings.Global.DEVICE_NAME) ?: Build.MODEL
        val version = try {
            packageManager.getPackageInfo(packageName, 0).versionName ?: "?"
        } catch (_: PackageManager.NameNotFoundException) {
            "?"
        }
        val p = NativeAgent.start(
            filesDir.absolutePath,
            name,
            "${Build.MANUFACTURER} ${Build.MODEL}",
            "Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})",
            version,
            packageManager.hasSystemFeature(PackageManager.FEATURE_USB_HOST),
            PORT,
            lan,
            this,
        )
        running = p > 0
        port = p
        lanMode = lan
        pin = ""
        if (!running) {
            lastError = "Cannot start the agent on port $PORT (see logcat, tag HubBridge)"
            notifyChanged()
            return
        }
        lastError = ""
        acquireLocks()
        pushDevices()
        if (lan) {
            val id = try { JSONObject(NativeAgent.status()).getString("agent_id") } catch (_: Exception) { "" }
            nsd.register(p, id, name)
        } else {
            nsd.unregister()
        }
        notifyChanged()
    }

    private fun acquireLocks() {
        if (wakeLock == null) {
            val pm = getSystemService(PowerManager::class.java)
            wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "HubBridge:bridge").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
        if (wifiLock == null) {
            val wm = applicationContext.getSystemService(WifiManager::class.java)
            // Low-latency mode turns off Wi-Fi power save while the screen is off (API 29+).
            @Suppress("DEPRECATION")
            val mode = if (Build.VERSION.SDK_INT >= 29) WifiManager.WIFI_MODE_FULL_LOW_LATENCY else WifiManager.WIFI_MODE_FULL_HIGH_PERF
            wifiLock = wm.createWifiLock(mode, "HubBridge").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
    }

    /** Re-scan USB off the main thread (opening devices does I/O) and hand the list to Rust. */
    fun pushDevices() {
        if (!running) return
        io.execute {
            NativeAgent.setDevices(scanner.scanJson())
        }
    }

    // --- NativeAgent.Callbacks (Rust threads) -------------------------------------------

    override fun onPermissionRequested(deviceId: Int) {
        main.post {
            val dev = scanner.find(deviceId) ?: return@post
            val usb = getSystemService(UsbManager::class.java)
            if (usb.hasPermission(dev)) {
                pushDevices()
                return@post
            }
            // The system fills in extras, so the PendingIntent must be mutable on API 31+.
            val flags = if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0
            val pi = PendingIntent.getBroadcast(
                this, deviceId, Intent(ACTION_USB_PERMISSION).setPackage(packageName), flags
            )
            usb.requestPermission(dev, pi)
        }
    }

    override fun onStatusChanged() {
        main.post { notifyChanged() }
    }

    // --- UI / notification ---------------------------------------------------------------

    private fun notifyChanged() {
        if (running) {
            getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, buildNotification())
        }
        for (l in listeners) l()
    }

    private fun createChannel() {
        val ch = NotificationChannel(CHANNEL, "Bridge status", NotificationManager.IMPORTANCE_LOW)
        getSystemService(NotificationManager::class.java).createNotificationChannel(ch)
    }

    private fun buildNotification(): Notification {
        val st = statusOrNull()
        val sessions = st?.optJSONArray("sessions")?.length() ?: 0
        val devices = st?.optInt("devices") ?: 0
        val title = when {
            pin.isNotEmpty() && st?.optBoolean("pairing_active") == true -> "Pairing PIN: $pin"
            sessions > 0 -> "Connected to $sessions computer" + if (sessions > 1) "s" else ""
            else -> "Waiting for a computer"
        }
        val mode = if (lanMode) "Wi-Fi/LAN + ADB" else "ADB only"
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE
        )
        val stop = PendingIntent.getService(
            this, 1, Intent(this, BridgeService::class.java).setAction(ACTION_STOP), PendingIntent.FLAG_IMMUTABLE
        )
        return Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat)
            .setContentTitle(title)
            .setContentText("$mode · port $port · $devices USB device(s)")
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, "Stop", stop).build())
            .build()
    }

    override fun onDestroy() {
        instance = null
        running = false
        unregisterReceiver(usbReceiver)
        nsd.unregister()
        NativeAgent.stop()
        wakeLock?.release()
        wifiLock?.release()
        io.shutdown()
        pin = ""
        for (l in listeners) l()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "HubBridge"
        const val PORT = 47800
        const val CHANNEL = "bridge"
        const val NOTIFICATION_ID = 1
        const val ACTION_START = "com.demoz.hubbridge.START"
        const val ACTION_STOP = "com.demoz.hubbridge.STOP"
        const val ACTION_PAIR = "com.demoz.hubbridge.PAIR"
        const val ACTION_RESCAN = "com.demoz.hubbridge.RESCAN"
        const val ACTION_USB_PERMISSION = "com.demoz.hubbridge.USB_PERMISSION"
        const val EXTRA_LAN = "lan"

        @Volatile var instance: BridgeService? = null
            private set
        @Volatile var port: Int = -1
            private set
        @Volatile var lanMode: Boolean = true
            private set
        @Volatile var pin: String = ""
            private set
        @Volatile var lastError: String = ""
            private set

        /** UI listeners, called on the main thread. */
        val listeners = CopyOnWriteArraySet<() -> Unit>()

        val isRunning: Boolean get() = instance?.running == true

        fun statusOrNull(): JSONObject? {
            if (!isRunning) return null
            val s = NativeAgent.status()
            return if (s.isEmpty()) null else try { JSONObject(s) } catch (_: Exception) { null }
        }

        fun send(context: Context, action: String, lan: Boolean = lanMode) {
            val i = Intent(context, BridgeService::class.java).setAction(action).putExtra(EXTRA_LAN, lan)
            if (action == ACTION_STOP) context.startService(i) else context.startForegroundService(i)
        }
    }
}
