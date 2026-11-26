package top.ieop.clipbridge

import android.content.Context

/** 配对配置：Windows 的 EndpointId + relay 地址。 */
class Prefs(context: Context) {
    private val sp = context.getSharedPreferences("clipbridge", Context.MODE_PRIVATE)

    var peerId: String
        get() = sp.getString(KEY_PEER, "").orEmpty().ifBlank { BuildConfig.DEFAULT_PEER_ID }
        set(v) = sp.edit().putString(KEY_PEER, v.trim()).apply()

    /**
     * 固定 relay。留空 = 交给 iroh 默认地图自动挑最近节点。
     *
     * 实测北京会自动挑到 euc1（法兰克福），跨网传大文件只有 ~70KB/s 还常被 idle 掐断；
     * 默认改钉 aps1（新加坡），两端一致更稳。自建中继时把地址填这里即可。
     */
    var relay: String
        get() = sp.getString(KEY_RELAY, DEFAULT_RELAY).orEmpty()
        set(v) = sp.edit().putString(KEY_RELAY, v.trim()).apply()

    var autoStart: Boolean
        get() = sp.getBoolean(KEY_AUTO, true)
        set(v) = sp.edit().putBoolean(KEY_AUTO, v).apply()

    val isConfigured: Boolean get() = peerId.isNotEmpty()

    companion object {
        const val ACTION_STOP = "top.ieop.clipbridge.STOP"

        /** n0 官方亚太 relay（新加坡）。 */
        const val DEFAULT_RELAY = "https://aps1-1.relay.n0.iroh.link/"

        private const val KEY_PEER = "peer_id"
        private const val KEY_RELAY = "relay"
        private const val KEY_AUTO = "auto_start"
    }
}

