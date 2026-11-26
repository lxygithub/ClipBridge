package top.ieop.clipbridge

/**
 * JNI 桥：对应 `android/jni/src/lib.rs` 里的导出函数。
 *
 * 所有 native 调用都可能阻塞（建连、等 ACK），**不要在主线程调用**。
 */
object Native {
    init {
        System.loadLibrary("clipbridge_android")
    }

    /** 启动长连接会话；listener 需实现 [BridgeListener]。 */
    external fun nativeStart(dir: String, peer: String, relay: String, listener: Any): Boolean

    external fun nativeStop()

    /** 发一条文本到 PC，返回是否收到 ACK。 */
    external fun nativeSendText(text: String): Boolean

    /** `"<id>|<relay>|<state>"` */
    external fun nativeSelfInfo(): String

    external fun nativeAckedSeq(): Long
}

/** Rust 侧通过 JNI 回调这两个方法，方法名与签名必须保持一致。 */
interface BridgeListener {
    fun onClipText(seq: Long, text: String)
    fun onState(state: String)
}
