package top.ieop.clipbridge

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject

/** 配对过的电脑：id 是 EndpointId，name 是电脑自己报上来的显示名（还没连过时为空）。 */
data class KnownPeer(val id: String, val name: String) {
    val label: String get() = name.ifBlank { "PC · ${id.take(8)}…" }
}

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

    /**
     * M4 多台电脑配对：记得住每一次配对成功的电脑，设置页里可以随时切。
     * 连接成功后 UI 会把电脑报上来的名字补进来（同一个 id 只留一条）。
     */
    fun knownPeers(): List<KnownPeer> {
        val raw = sp.getString(KEY_PEERS, "").orEmpty()
        if (raw.isBlank()) {
            val self = peerId
            return if (self.isBlank()) emptyList() else listOf(KnownPeer(self, ""))
        }
        return runCatching {
            val arr = JSONArray(raw)
            (0 until arr.length()).mapNotNull { i ->
                val o = arr.getJSONObject(i)
                val id = o.optString("id").trim()
                if (id.isEmpty()) null else KnownPeer(id, o.optString("name"))
            }
        }.getOrDefault(emptyList())
    }

    /** 记住/更新一台电脑；`name` 为空时保留旧名字。 */
    fun rememberPeer(id: String, name: String) {
        val clean = id.trim()
        if (clean.isEmpty()) return
        val list = knownPeers().toMutableList()
        val idx = list.indexOfFirst { it.id == clean }
        val peer = KnownPeer(clean, name.trim().ifEmpty { if (idx >= 0) list[idx].name else "" })
        if (idx >= 0) list[idx] = peer else list.add(0, peer)
        val arr = JSONArray()
        list.take(10).forEach { p ->
            arr.put(JSONObject().apply {
                put("id", p.id)
                put("name", p.name)
            })
        }
        sp.edit().putString(KEY_PEERS, arr.toString()).apply()
    }

    fun forgetPeer(id: String) {
        val arr = JSONArray()
        knownPeers().filterNot { it.id == id.trim() }.take(10).forEach { p ->
            arr.put(JSONObject().apply {
                put("id", p.id)
                put("name", p.name)
            })
        }
        sp.edit().putString(KEY_PEERS, arr.toString()).apply()
    }

    companion object {
        const val ACTION_STOP = "top.ieop.clipbridge.STOP"

        /** 默认留空 = iroh 默认地图自动选。别钉 aps1，它的 QAD 实测超时。 */
        const val DEFAULT_RELAY = ""

        private const val KEY_PEER = "peer_id"
        private const val KEY_RELAY = "relay"
        private const val KEY_AUTO = "auto_start"
        private const val KEY_PEERS = "peers"
    }
}

