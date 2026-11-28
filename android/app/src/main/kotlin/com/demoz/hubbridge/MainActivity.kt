package com.demoz.hubbridge

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.os.Build
import android.os.Bundle
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import java.net.Inet4Address

/** Minimal control screen: start/stop, pairing PIN, USB devices, paired computers. */
class MainActivity : Activity() {

    private lateinit var status: TextView
    private lateinit var pinView: TextView
    private lateinit var devices: TextView
    private lateinit var hosts: LinearLayout
    private lateinit var startLan: Button
    private lateinit var startAdb: Button
    private lateinit var stop: Button
    private lateinit var pair: Button
    private val refresh: () -> Unit = { render() }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val pad = (16 * resources.displayMetrics.density).toInt()
        val col = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
        }
        fun button(text: String, onClick: () -> Unit) = Button(this).apply {
            this.text = text
            isAllCaps = false
            setOnClickListener { onClick() }
            col.addView(this)
        }
        fun label(sizeSp: Float, bold: Boolean = false) = TextView(this).apply {
            textSize = sizeSp
            if (bold) typeface = Typeface.DEFAULT_BOLD
            setPadding(0, pad / 2, 0, pad / 2)
            setTextIsSelectable(true)
            col.addView(this)
        }

        label(22f, bold = true).text = "HubBridge"
        status = label(15f)
        startLan = button("Start: Wi-Fi/LAN + ADB") { ensureNotifications(); BridgeService.send(this, BridgeService.ACTION_START, lan = true) }
        startAdb = button("Start: ADB only (localhost)") { ensureNotifications(); BridgeService.send(this, BridgeService.ACTION_START, lan = false) }
        pair = button("Pair a new computer") { BridgeService.send(this, BridgeService.ACTION_PAIR) }
        pinView = label(34f, bold = true).apply { typeface = Typeface.MONOSPACE }
        stop = button("Stop") { BridgeService.send(this, BridgeService.ACTION_STOP) }
        label(18f, bold = true).text = "USB devices"
        devices = label(13f).apply { typeface = Typeface.MONOSPACE }
        button("Rescan USB") { BridgeService.send(this, BridgeService.ACTION_RESCAN) }
        label(18f, bold = true).text = "Paired computers"
        hosts = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        col.addView(hosts)

        setContentView(ScrollView(this).apply { addView(col) })
        handleUsbIntent(intent)
        // Not again when the activity is recreated.
        if (savedInstanceState == null) handleSpikeIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleUsbIntent(intent)
        handleSpikeIntent(intent)
    }

    /** `am start … --es spike <route>` (design D1). The only way into the spike: debuggable builds only. */
    private fun handleSpikeIntent(intent: Intent?) {
        if (applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE == 0) return
        // Reopening from Recents replays the old intent.
        if (intent == null || intent.flags and Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY != 0) return
        val route = intent.getStringExtra("spike") ?: return
        BridgeService.sendSpike(this, route)
    }

    /** Opened by USB_DEVICE_ATTACHED ("open HubBridge for this device?"): permission is now granted. */
    private fun handleUsbIntent(intent: Intent?) {
        if (intent?.action == "android.hardware.usb.action.USB_DEVICE_ATTACHED" && BridgeService.isRunning) {
            BridgeService.send(this, BridgeService.ACTION_RESCAN)
        }
    }

    override fun onStart() {
        super.onStart()
        BridgeService.listeners.add(refresh)
        render()
    }

    override fun onStop() {
        BridgeService.listeners.remove(refresh)
        super.onStop()
    }

    private fun ensureNotifications() {
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
    }

    private fun localIpv4(): String? {
        val cm = getSystemService(ConnectivityManager::class.java)
        val lp: LinkProperties = cm.getLinkProperties(cm.activeNetwork) ?: return null
        return lp.linkAddresses.map { it.address }.firstOrNull { it is Inet4Address }?.hostAddress
    }

    private fun render() {
        val running = BridgeService.isRunning
        val st = BridgeService.statusOrNull()
        startLan.visibility = if (running) View.GONE else View.VISIBLE
        startAdb.visibility = if (running) View.GONE else View.VISIBLE
        stop.visibility = if (running) View.VISIBLE else View.GONE
        pair.visibility = if (running) View.VISIBLE else View.GONE

        status.text = buildString {
            if (!running) {
                append("Stopped.")
                if (BridgeService.lastError.isNotEmpty()) append("\n").append(BridgeService.lastError)
                return@buildString
            }
            val port = BridgeService.port
            if (BridgeService.lanMode) {
                append("Listening on ${localIpv4() ?: "?"}:$port (Wi-Fi/LAN) and via ADB.\n")
            } else {
                append("Listening on 127.0.0.1:$port — reach it with `hubbridge adb`.\n")
            }
            val sessions = st?.optJSONArray("sessions")
            if (sessions == null || sessions.length() == 0) {
                append("No computer connected.")
            } else {
                for (i in 0 until sessions.length()) {
                    val s = sessions.getJSONObject(i)
                    val via = if (s.optBoolean("via_adb")) "ADB" else s.optString("peer")
                    append("Connected: ${s.optString("host_name")} ($via)\n")
                }
            }
            if (!packageManager.hasSystemFeature(PackageManager.FEATURE_USB_HOST)) {
                append("\nThis phone reports no USB host support.")
            }
        }

        val pairing = st?.optBoolean("pairing_active") == true && BridgeService.pin.isNotEmpty()
        pinView.visibility = if (pairing) View.VISIBLE else View.GONE
        if (pairing) {
            val left = st?.optLong("pairing_seconds_left") ?: 0
            pinView.text = "${BridgeService.pin}\nhubbridge pair <target> --pin ${BridgeService.pin}  (${left}s)"
            pinView.textSize = 28f
        }

        devices.text = if (!running) "—" else UsbScanner(this).devices().joinToString("\n") { d ->
            val perm = if (getSystemService(android.hardware.usb.UsbManager::class.java).hasPermission(d)) "✓" else "no permission"
            "[%d] %04x:%04x %s (%s)".format(d.deviceId, d.vendorId, d.productId, d.productName ?: "?", perm)
        }.ifEmpty { "No USB devices. Connect a hub or a device with an OTG adapter." }

        hosts.removeAllViews()
        val paired = st?.optJSONArray("paired_hosts")
        if (paired == null || paired.length() == 0) {
            hosts.addView(TextView(this).apply { text = if (running) "None yet." else "—" })
        } else {
            for (i in 0 until paired.length()) {
                val h = paired.getJSONObject(i)
                val id = h.optString("host_id")
                hosts.addView(Button(this).apply {
                    isAllCaps = false
                    text = "Forget ${h.optString("name")} (${id.take(8)})"
                    setOnClickListener { NativeAgent.forgetHost(id); render() }
                })
            }
        }
    }
}
