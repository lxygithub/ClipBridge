package top.ieop.clipbridge

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/** 一条历史条目（M1 只存内存 + 原生 side 的 seq 水位线，落盘历史留给后续里程碑）。 */
data class ClipItem(
    val seq: Long,
    val text: String,
    val ts: Long = System.currentTimeMillis(),
    val outgoing: Boolean = false,
    /** 图片条目：本地 PNG 路径；文本条目为 null */
    val imagePath: String? = null,
    val imageW: Int = 0,
    val imageH: Int = 0,
    /** 文件条目：收下来的本地路径；发出去的文件本体在电脑上，为 null */
    val filePath: String? = null,
    val fileSize: Long = 0L,
    /** 这条是文件（不是文本/图片）；发出去的文件没有本地路径，靠它区分 */
    val isFile: Boolean = false
)

object BridgeState {
    private var historyFile: File? = null

    val state = MutableStateFlow("idle")
    val lastSeq = MutableStateFlow(0L)

    /** 界面时间线，新的在最上面 */
    val items = MutableStateFlow<List<ClipItem>>(emptyList())
    val sentCount = MutableStateFlow(0)

    /** 链路：`direct|12` / `relay|312` / 空串，Rust 侧每 2 秒刷新一次 */
    val link = MutableStateFlow("")

    /** 电脑的显示名（来自电脑的 HELLO），列表里「发往 xxx / 来自 xxx」用它 */
    val peerName = MutableStateFlow("")

    fun set(newState: String) {
        state.value = newState
    }

    /**
     * M4 历史持久化：时间线原来只活在内存里，杀进程就清空。
     * 落到 `filesDir/history.json`（最多 200 条，和内存里的上限一致）。
     */
    fun attach(ctx: Context) {
        if (historyFile != null) return
        val f = File(ctx.filesDir, "history.json")
        historyFile = f
        runCatching {
            if (!f.exists()) return@runCatching
            val arr = JSONArray(f.readText())
            val list = ArrayList<ClipItem>(arr.length())
            for (i in 0 until arr.length()) {
                val o = arr.getJSONObject(i)
                val path = o.optString("imagePath").ifBlank { null }
                // 图片被清掉了就只留文字记录，不要把空路径塞回列表
                if (path != null && !File(path).exists()) continue
                val fpath = o.optString("filePath").ifBlank { null }
                // 收到的文件被删了也保留一条记录，只是点不开
                list += ClipItem(
                    seq = o.optLong("seq", 0),
                    text = o.optString("text"),
                    ts = o.optLong("ts", System.currentTimeMillis()),
                    outgoing = o.optBoolean("outgoing", false),
                    imagePath = path,
                    imageW = o.optInt("imageW", 0),
                    imageH = o.optInt("imageH", 0),
                    filePath = fpath?.takeIf { File(it).exists() },
                    fileSize = o.optLong("fileSize", 0),
                    isFile = o.optBoolean("isFile", false)
                )
            }
            items.value = list.take(200)
        }
    }

    private fun persist() {
        val f = historyFile ?: return
        runCatching {
            val arr = JSONArray()
            items.value.take(200).forEach { it ->
                arr.put(JSONObject().apply {
                    put("seq", it.seq)
                    put("text", it.text)
                    put("ts", it.ts)
                    put("outgoing", it.outgoing)
                    it.imagePath?.let { p ->
                        put("imagePath", p)
                        put("imageW", it.imageW)
                        put("imageH", it.imageH)
                    }
                    it.filePath?.let { p -> put("filePath", p) }
                    if (it.isFile) put("isFile", true)
                    if (it.fileSize > 0) put("fileSize", it.fileSize)
                })
            }
            f.writeText(arr.toString())
        }
    }

    fun push(item: ClipItem) {
        items.value = (listOf(item) + items.value).take(200)
        if (item.outgoing) sentCount.value += 1
        if (item.seq > lastSeq.value) lastSeq.value = item.seq
        persist()
    }

    /**
     * 历史快照合并：按 seq 覆盖，按时间倒序排。
     * 这是「手机本地历史为空但 last_seq 已经很靠前」的兜底——不这样做时间线会一直空着。
     */
    fun pushHistory(list: List<ClipItem>) {
        if (list.isEmpty()) return
        val merged = LinkedHashMap<Long, ClipItem>()
        for (it in items.value) {
            if (it.seq > 0) merged[it.seq] = it
        }
        for (it in list) {
            // 文本以电脑侧为准（本地那条可能是没落盘的半截）
            merged[it.seq] = it
        }
        val localOnly = items.value.filter { it.seq <= 0L || it.imagePath != null }
        val sorted = (merged.values + localOnly)
            .sortedByDescending { it.ts }
            .take(200)
        items.value = sorted
        lastSeq.value = maxOf(lastSeq.value, list.maxOf { it.seq })
        persist()
    }

    fun markSent(text: String) {
        push(ClipItem(seq = 0, text = text, outgoing = true, ts = System.currentTimeMillis()))
    }

    /** 图片条目：`path` 是本地 PNG（收到的落 inbox，发出去的落 sent_images）。 */
    fun pushImage(path: String, w: Int, h: Int, outgoing: Boolean) {
        push(
            ClipItem(
                seq = 0,
                text = "图片 · ${w}×${h}",
                outgoing = outgoing,
                imagePath = path,
                imageW = w,
                imageH = h
            )
        )
    }

    /** 文件条目：和图片一样进同一条时间线，不再单开「文件」列表。 */
    fun pushFile(name: String, size: Long, path: String?, outgoing: Boolean) {
        push(
            ClipItem(
                seq = 0,
                text = name,
                outgoing = outgoing,
                filePath = path,
                fileSize = size,
                isFile = true,
                ts = System.currentTimeMillis()
            )
        )
    }
}
