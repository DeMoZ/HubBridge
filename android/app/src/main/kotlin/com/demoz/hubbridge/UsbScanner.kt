package com.demoz.hubbridge

import android.content.Context
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/**
 * Describes the attached USB devices as JSON for [NativeAgent.setDevices].
 *
 * Without permission Android shows only what `UsbDevice` knows (ids, class, interfaces,
 * endpoints). With permission we also read the serial and the raw descriptors
 * (device + all configuration descriptors) through a short-lived connection.
 */
class UsbScanner(context: Context) {
    private val usb = context.getSystemService(UsbManager::class.java)

    fun devices(): Collection<UsbDevice> = usb?.deviceList?.values ?: emptyList()

    fun find(id: Int): UsbDevice? = devices().firstOrNull { it.deviceId == id }

    fun scanJson(): String {
        val arr = JSONArray()
        for (d in devices()) {
            try {
                arr.put(describe(d))
            } catch (e: Exception) {
                Log.w(TAG, "skip ${d.deviceName}: $e")
            }
        }
        return arr.toString()
    }

    private fun describe(d: UsbDevice): JSONObject {
        val perm = usb?.hasPermission(d) == true
        var serial: String? = null
        var raw: String? = null
        if (perm) {
            serial = try { d.serialNumber } catch (_: SecurityException) { null }
            usb?.openDevice(d)?.let { conn ->
                try {
                    raw = conn.rawDescriptors?.toHex()
                } finally {
                    conn.close()
                }
            }
        }
        val configs = JSONArray()
        for (i in 0 until d.configurationCount) {
            val c = d.getConfiguration(i)
            // UsbConfiguration has no getAttributes(): rebuild bmAttributes.
            var attrs = 0x80
            if (c.isSelfPowered) attrs = attrs or 0x40
            if (c.isRemoteWakeup) attrs = attrs or 0x20
            val intfs = JSONArray()
            for (j in 0 until c.interfaceCount) {
                val intf = c.getInterface(j)
                val eps = JSONArray()
                for (k in 0 until intf.endpointCount) {
                    val e = intf.getEndpoint(k)
                    eps.put(
                        JSONObject()
                            .put("address", e.address)
                            .put("attributes", e.attributes)
                            .put("max_packet_size", e.maxPacketSize)
                            .put("interval", e.interval)
                    )
                }
                intfs.put(
                    JSONObject()
                        .put("number", intf.id)
                        .put("alternate", intf.alternateSetting)
                        .put("class", intf.interfaceClass)
                        .put("subclass", intf.interfaceSubclass)
                        .put("protocol", intf.interfaceProtocol)
                        .put("name", intf.name ?: JSONObject.NULL)
                        .put("endpoints", eps)
                )
            }
            configs.put(
                JSONObject()
                    .put("value", c.id)
                    .put("name", c.name ?: JSONObject.NULL)
                    .put("attributes", attrs)
                    .put("max_power_ma", c.maxPower)
                    .put("interfaces", intfs)
            )
        }
        return JSONObject()
            .put("id", d.deviceId)
            .put("path", d.deviceName)
            .put("vendor_id", d.vendorId)
            .put("product_id", d.productId)
            .put("class", d.deviceClass)
            .put("subclass", d.deviceSubclass)
            .put("protocol", d.deviceProtocol)
            .put("manufacturer", d.manufacturerName ?: JSONObject.NULL)
            .put("product", d.productName ?: JSONObject.NULL)
            .put("serial", serial ?: JSONObject.NULL)
            .put("has_permission", perm)
            .put("configurations", configs)
            .put("raw_descriptors_hex", raw ?: JSONObject.NULL)
    }

    companion object {
        private const val TAG = "HubBridge"

        private fun ByteArray.toHex(): String {
            val sb = StringBuilder(size * 2)
            for (b in this) sb.append(String.format("%02x", b.toInt() and 0xff))
            return sb.toString()
        }
    }
}
