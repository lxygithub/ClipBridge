package top.ieop.clipbridge

import android.content.Context

/** 配对配置：Windows 的 EndpointId + relay 地址。 */
class Prefs(context: Context) {
    private val sp = context.getSharedPreferences("clipbridge", Context.MODE_PRIVATE)

    var peerId: String
        get() = sp.getString(KEY_PEER, "").orEmpty().ifBlank { BuildConfig.DEFAULT_PEER_ID }
        set(v) = sp.edit().putString(KEY_PEER, v.trim()).apply()

    /** 留空 = 交给 iroh 发现服务（n0 DNS）解析对端当前 relay，避免写死的区域 relay 失效。 */
    var relay: String
        get() = sp.getString(KEY_RELAY, "").orEmpty()
        set(v) = sp.edit().putString(KEY_RELAY, v.trim()).apply()

    var autoStart: Boolean
        get() = sp.getBoolean(KEY_AUTO, true)
        set(v) = sp.edit().putBoolean(KEY_AUTO, v).apply()

    val isConfigured: Boolean get() = peerId.isNotEmpty()

    companion object {
        const val ACTION_STOP = "top.ieop.clipbridge.STOP"
        private const val KEY_PEER = "peer_id"
        private const val KEY_RELAY = "relay"
        private const val KEY_AUTO = "auto_start"
    }
}

