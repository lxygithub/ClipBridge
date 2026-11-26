package top.ieop.clipbridge

import kotlinx.coroutines.flow.MutableStateFlow

/** 一条历史条目（M1 只存内存 + 原生 side 的 seq 水位线，落盘历史留给后续里程碑）。 */
data class ClipItem(
    val seq: Long,
    val text: String,
    val ts: Long = System.currentTimeMillis(),
    val outgoing: Boolean = false
)

/** 跨界面共享的连接状态（Rust 回调线程写，UI 读）。 */
object BridgeState {
    val state = MutableStateFlow("idle")
    val lastSeq = MutableStateFlow(0L)

    /** 界面时间线，新的在最上面 */
    val items = MutableStateFlow<List<ClipItem>>(emptyList())
    val sentCount = MutableStateFlow(0)

    fun set(newState: String) {
        state.value = newState
    }

    fun push(item: ClipItem) {
        items.value = (listOf(item) + items.value).take(200)
        if (item.outgoing) sentCount.value += 1
        if (item.seq > lastSeq.value) lastSeq.value = item.seq
    }

    fun markSent(text: String) {
        push(ClipItem(seq = 0, text = text, outgoing = true, ts = System.currentTimeMillis()))
    }
}
