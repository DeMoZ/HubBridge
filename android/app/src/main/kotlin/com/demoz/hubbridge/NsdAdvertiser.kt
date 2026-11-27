package com.demoz.hubbridge

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.util.Log

/** Announces the agent as `_hubbridge._tcp` so `hubbridge discover` finds it. */
class NsdAdvertiser(context: Context) {
    private val nsd = context.getSystemService(NsdManager::class.java)
    private var listener: NsdManager.RegistrationListener? = null

    fun register(port: Int, agentId: String, name: String) {
        unregister()
        val info = NsdServiceInfo().apply {
            serviceName = "HubBridge-" + name.replace(Regex("[^A-Za-z0-9-]"), "-").take(40)
            serviceType = SERVICE_TYPE
            setPort(port)
            setAttribute("id", agentId)
            setAttribute("v", PROTO_VERSION)
            setAttribute("name", name)
        }
        val l = object : NsdManager.RegistrationListener {
            override fun onServiceRegistered(s: NsdServiceInfo) {
                Log.i(TAG, "NSD registered ${s.serviceName}")
            }
            override fun onRegistrationFailed(s: NsdServiceInfo, code: Int) {
                Log.w(TAG, "NSD register failed: $code")
            }
            override fun onServiceUnregistered(s: NsdServiceInfo) {
                Log.i(TAG, "NSD unregistered")
            }
            override fun onUnregistrationFailed(s: NsdServiceInfo, code: Int) {
                Log.w(TAG, "NSD unregister failed: $code")
            }
        }
        listener = l
        nsd?.registerService(info, NsdManager.PROTOCOL_DNS_SD, l)
    }

    fun unregister() {
        listener?.let { l ->
            try {
                nsd?.unregisterService(l)
            } catch (_: IllegalArgumentException) {
                // Not registered (yet).
            }
        }
        listener = null
    }

    companion object {
        private const val TAG = "HubBridge"
        /** Must match hubbridge_proto::NSD_SERVICE_TYPE and PROTO_VERSION. */
        const val SERVICE_TYPE = "_hubbridge._tcp"
        const val PROTO_VERSION = "1"
    }
}
