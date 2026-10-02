package com.displayswarm.client

import android.content.Context
import android.content.pm.PackageManager
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.p2p.WifiP2pManager
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONArray
import org.json.JSONObject

/** A host found through mDNS (`_displayswarm._tcp`). */
data class HostInfo(
    val name: String,
    val host: String,
    val port: Int,
    /** First 16 hex digits of the host certificate fingerprint (TXT `id`). */
    val idPrefix: String,
    val protocol: Int,
    val pairingRequired: Boolean
) {
    companion object {
        const val SERVICE_TYPE = "_displayswarm._tcp."

        /** Builds a [HostInfo] from a resolved service's TXT attributes; null if it is not a usable host. */
        fun fromTxt(instance: String, host: String, port: Int, txt: Map<String, ByteArray?>): HostInfo? {
            fun text(k: String) = txt[k]?.toString(Charsets.UTF_8)
            val protocol = text("v")?.toIntOrNull() ?: return null
            if (host.isBlank() || port !in 1..65535) return null
            return HostInfo(
                name = text("name")?.takeIf { it.isNotBlank() } ?: instance,
                host = host,
                port = port,
                idPrefix = text("id").orEmpty().lowercase(),
                protocol = protocol,
                pairingRequired = text("pair") != "0"
            )
        }
    }
}

/** A paired host: the pinned certificate and the token that skips the PIN. */
data class KnownHost(
    val name: String,
    val host: String,
    val port: Int,
    val fingerprintHex: String,
    val tokenHex: String,
    val lastSeen: Long = 0
) {
    val fingerprint: ByteArray get() = Pairing.fromHex(fingerprintHex) ?: ByteArray(0)
    val token: ByteArray? get() = Pairing.fromHex(tokenHex)?.takeIf { it.isNotEmpty() }

    fun matches(h: HostInfo) = h.idPrefix.isNotEmpty() && fingerprintHex.startsWith(h.idPrefix)
}

/** Persists [KnownHost]s as JSON text (SharedPreferences in the app, memory in tests). */
class KnownHostStore(private val load: () -> String?, private val save: (String) -> Unit) {
    fun all(): List<KnownHost> = try {
        val arr = JSONArray(load() ?: "[]")
        (0 until arr.length()).map {
            val o = arr.getJSONObject(it)
            KnownHost(
                o.getString("name"), o.getString("host"), o.getInt("port"),
                o.getString("fp"), o.getString("token"), o.optLong("seen")
            )
        }
    } catch (_: Exception) {
        emptyList()
    }

    fun put(h: KnownHost) {
        val list = all().filter { it.fingerprintHex != h.fingerprintHex } + h
        save(JSONArray(list.map {
            JSONObject().put("name", it.name).put("host", it.host).put("port", it.port)
                .put("fp", it.fingerprintHex).put("token", it.tokenHex).put("seen", it.lastSeen)
        }).toString())
    }

    fun forget(fingerprintHex: String) {
        save(JSONArray(all().filter { it.fingerprintHex != fingerprintHex }.map {
            JSONObject().put("name", it.name).put("host", it.host).put("port", it.port)
                .put("fp", it.fingerprintHex).put("token", it.tokenHex).put("seen", it.lastSeen)
        }).toString())
    }
}

/** The discovered host that should be connected to automatically, if any. */
fun pickAutoConnect(found: List<HostInfo>, known: List<KnownHost>, enabled: Boolean): Pair<HostInfo, KnownHost>? {
    if (!enabled) return null
    for (h in found) {
        val k = known.firstOrNull { it.matches(h) } ?: continue
        return h to k
    }
    return null
}

/**
 * Discovery and memory of Wi-Fi hosts. The connect screen renders [hosts] and
 * [known], and connects with [securityFor] / [securityForQr] passed to
 * [NetworkClient].
 */
class WifiHosts(context: Context) {
    private val app = context.applicationContext
    private val prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
    private val store = KnownHostStore(
        { prefs.getString(KEY_KNOWN, null) },
        { prefs.edit().putString(KEY_KNOWN, it).apply() }
    )
    private val nsd = app.getSystemService(Context.NSD_SERVICE) as NsdManager

    private val _hosts = MutableStateFlow<List<HostInfo>>(emptyList())
    /** Hosts currently visible on the network. */
    val hosts: StateFlow<List<HostInfo>> = _hosts.asStateFlow()

    private val _known = MutableStateFlow(store.all())
    /** Hosts this phone is paired with. */
    val known: StateFlow<List<KnownHost>> = _known.asStateFlow()

    /** Connect automatically when a known host shows up. */
    var autoConnect: Boolean
        get() = prefs.getBoolean(KEY_AUTO, false)
        set(v) = prefs.edit().putBoolean(KEY_AUTO, v).apply()

    /** Development: plain TCP with no TLS or pairing (needs `DISPLAYSWARM_INSECURE_TCP=1` on the host). */
    var plainTcpForDev: Boolean
        get() = prefs.getBoolean(KEY_PLAIN, false)
        set(v) = prefs.edit().putBoolean(KEY_PLAIN, v).apply()

    /** The known host that should be connected now, if auto-connect is on. */
    fun autoConnectCandidate(): Pair<HostInfo, KnownHost>? = pickAutoConnect(_hosts.value, _known.value, autoConnect)

    private var listener: NsdManager.DiscoveryListener? = null
    private val resolveQueue = ArrayDeque<NsdServiceInfo>()
    private var resolving = false
    private val byInstance = LinkedHashMap<String, HostInfo>()

    @Synchronized
    fun startDiscovery() {
        if (listener != null) return
        val l = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(serviceType: String) {}
            override fun onServiceFound(info: NsdServiceInfo) = enqueue(info)
            override fun onServiceLost(info: NsdServiceInfo) = lost(info.serviceName)
            override fun onDiscoveryStopped(serviceType: String) {}
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "mDNS discovery failed: $errorCode")
                listener = null
            }
            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}
        }
        listener = l
        try {
            nsd.discoverServices(HostInfo.SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, l)
        } catch (e: Exception) {
            Log.w(TAG, "mDNS discovery not started: ${e.message}")
            listener = null
        }
    }

    @Synchronized
    fun stopDiscovery() {
        listener?.let { try { nsd.stopServiceDiscovery(it) } catch (_: Exception) {} }
        listener = null
        byInstance.clear()
        _hosts.value = emptyList()
    }

    @Synchronized
    private fun enqueue(info: NsdServiceInfo) {
        resolveQueue.addLast(info)
        resolveNext()
    }

    // NsdManager resolves one service at a time.
    @Synchronized
    @Suppress("DEPRECATION")
    private fun resolveNext() {
        if (resolving) return
        val next = resolveQueue.removeFirstOrNull() ?: return
        resolving = true
        try {
            nsd.resolveService(next, object : NsdManager.ResolveListener {
                override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) = done()
                override fun onServiceResolved(info: NsdServiceInfo) {
                    val host = pickAddress(info)?.let { preferIpv4(it, info.attributes) }
                    val h = if (host == null) null else HostInfo.fromTxt(info.serviceName, host, info.port, info.attributes)
                    synchronized(this@WifiHosts) {
                        if (h != null) {
                            byInstance[info.serviceName] = h
                            _hosts.value = byInstance.values.toList()
                            touch(h)
                        }
                    }
                    done()
                }

                private fun done() = synchronized(this@WifiHosts) {
                    resolving = false
                    resolveNext()
                }
            })
        } catch (e: Exception) {
            resolving = false
        }
    }

    /** When only an IPv6 address came back, use an IPv4 one from the host's TXT `ip` (the one on our subnet if we can tell). */
    private fun preferIpv4(host: String, txt: Map<String, ByteArray?>): String {
        if (!host.contains(':')) return host
        val ips = txt["ip"]?.toString(Charsets.UTF_8)?.split(',')?.map { it.trim() }?.filter { it.isNotEmpty() }.orEmpty()
        if (ips.isEmpty()) return host
        val mine = try {
            java.net.NetworkInterface.getNetworkInterfaces().toList().filter { it.isUp && !it.isLoopback }
                .flatMap { it.interfaceAddresses }.filter { it.address is java.net.Inet4Address }
        } catch (_: Exception) { emptyList() }
        fun same(ip: String, ia: java.net.InterfaceAddress): Boolean {
            val t = java.net.InetAddress.getByName(ip).address
            val a = ia.address.address
            return (0 until ia.networkPrefixLength.toInt()).all { i ->
                ((a[i / 8].toInt() shr (7 - i % 8)) and 1) == ((t[i / 8].toInt() shr (7 - i % 8)) and 1)
            }
        }
        return ips.firstOrNull { ip -> mine.any { same(ip, it) } } ?: ips.first()
    }

    /**
     * Prefer an IPv4 address: a link-local IPv6 one (`fe80::...`) only connects with its
     * interface scope, and stripping the scope made every connect fail with EINVAL.
     */
    private fun pickAddress(info: NsdServiceInfo): String? {
        val all = if (android.os.Build.VERSION.SDK_INT >= 34) info.hostAddresses else listOfNotNull(info.host)
        val a = all.firstOrNull { it is java.net.Inet4Address } ?: all.firstOrNull() ?: return null
        return a.hostAddress
    }

    @Synchronized
    private fun lost(instance: String) {
        byInstance.remove(instance)
        _hosts.value = byInstance.values.toList()
    }

    /** Refreshes the address of a paired host that was just seen. */
    private fun touch(h: HostInfo) {
        val k = _known.value.firstOrNull { it.matches(h) } ?: return
        store.put(k.copy(host = h.host, port = h.port, name = h.name, lastSeen = System.currentTimeMillis()))
        _known.value = store.all()
    }

    fun forget(k: KnownHost) {
        store.forget(k.fingerprintHex)
        _known.value = store.all()
    }

    /**
     * Security for connecting to [host]:[port] (a discovered or typed-in host).
     * A paired host is pinned and reconnects without a PIN; anything else asks
     * for the PIN through [pinProvider] and remembers the host on success.
     */
    fun securityFor(
        host: String,
        port: Int,
        name: String,
        idPrefix: String = "",
        pinProvider: suspend (String) -> String?
    ): NetSecurity {
        // By address first; by certificate id when the host's address changed (DHCP, hotspot),
        // otherwise a known host would ask for a PIN nobody can see.
        val k = _known.value.firstOrNull { it.host == host && it.port == port }
            ?: idPrefix.takeIf { it.isNotEmpty() }?.let { id -> _known.value.firstOrNull { it.fingerprintHex.startsWith(id) } }
        return NetSecurity(
            deviceId = DeviceIdentity.id(app),
            deviceName = android.os.Build.MODEL ?: "Android",
            pinnedFingerprint = k?.fingerprint?.takeIf { it.size == 32 },
            token = k?.token,
            pinProvider = pinProvider,
            onAuthenticated = { fp, newToken -> remember(host, port, name, fp, newToken, k) }
        )
    }

    /** Security for a scanned (or pasted) QR pairing code. */
    fun securityForQr(qr: QrPairing): NetSecurity = NetSecurity(
        deviceId = DeviceIdentity.id(app),
        deviceName = android.os.Build.MODEL ?: "Android",
        pinnedFingerprint = qr.fingerprint,
        qrSecret = qr.secret,
        onAuthenticated = { fp, newToken -> remember(qr.host, qr.port, qr.hostName, fp, newToken, null) }
    )

    private fun remember(host: String, port: Int, name: String, fp: ByteArray, newToken: ByteArray?, old: KnownHost?) {
        val token = newToken?.let { Pairing.toHex(it) } ?: old?.tokenHex ?: return
        store.put(KnownHost(name, host, port, Pairing.toHex(fp), token, System.currentTimeMillis()))
        _known.value = store.all()
    }

    companion object {
        private const val TAG = "DisplaySwarmWifi"
        private const val PREFS = "displayswarm_wifi"
        private const val KEY_KNOWN = "known_hosts"
        private const val KEY_AUTO = "auto_connect"
        private const val KEY_PLAIN = "plain_tcp_dev"
    }
}

/** Wi-Fi Direct is hidden in the UI when this is false. */
object WifiDirectSupport {
    fun isSupported(context: Context): Boolean {
        val pm = context.packageManager
        return pm.hasSystemFeature(PackageManager.FEATURE_WIFI_DIRECT) &&
            context.getSystemService(Context.WIFI_P2P_SERVICE) is WifiP2pManager
    }
}
