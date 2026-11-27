package top.ieop.clipbridge

import android.content.Context

/** 配对配置：Windows 的 EndpointId + relay 地址。 */
class Prefs(context: Context) {
    private val sp = context.getSharedPreferences("clipbridge", Context.MODE_PRIVATE)

    var peerId: String
        get() = sp.getString(KEY_PEER, "").orEmpty().ifBlank { BuildConfig.DEFAULT_PEER_ID }
        set(v) = sp.edit().putString(KEY_PEER, v.trim()).apply()

    /**
     * 固定 relay。留空 = 交给 iroh 默认地图自动挑（推荐）。
     *
     * 别钉 `aps1`：实测这台机器到 aps1 的 QAD 一直超时，拿不到自己的公网映射就打不了洞，
     * 只能全程走 relay。euc1/usw1 的 QAD 都是通的。要自建中继时把地址填这里即可。
     */
    var relay: String
        get() = sp.getString(KEY_RELAY, "").orEmpty()
        set(v) = sp.edit().putString(KEY_RELAY, v.trim()).apply()

    var autoStart: Boolean
        get() = sp.getBoolean(KEY_AUTO, true)
        set(v) = sp.edit().putBoolean(KEY_AUTO, v).apply()

    val isConfigured: Boolean get() = peerId.isNotEmpty()

    companion object {
        const val ACTION_STOP = "top.ieop.clipbridge.STOP"

        /** 默认留空 = iroh 默认地图自动选。别钉 aps1，它的 QAD 实测超时。 */
        const val DEFAULT_RELAY = ""

        private const val KEY_PEER = "peer_id"
        private const val KEY_RELAY = "relay"
        private const val KEY_AUTO = "auto_start"
    }
}

