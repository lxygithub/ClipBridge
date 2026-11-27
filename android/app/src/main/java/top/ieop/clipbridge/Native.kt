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

    /** 启动长连接会话；deviceName 会随 HELLO 发给电脑，电脑端显示「已连接 <名字>」。 */
    external fun nativeStart(dir: String, peer: String, relay: String, deviceName: String, listener: Any): Boolean

    external fun nativeStop()

    /** 发一条文本到 PC，返回是否收到 ACK。 */
    external fun nativeSendText(text: String): Boolean

    /** 发一个文件到 PC；返回是否把数据写完了（校验结果走 onFileSent）。 */
    external fun nativeSendFile(path: String): Boolean

    /** 发一张图（PNG 字节，Kotlin 侧已编码）；返回是否收到 ACK。 */
    external fun nativeSendImage(png: ByteArray): Boolean

    /** 取消当前正在接收的传输；返回是否真的有一笔在跑。 */
    external fun nativeCancelTransfer(): Boolean

    /** `"<id>|<relay>|<state>"` */
    external fun nativeSelfInfo(): String

    external fun nativeAckedSeq(): Long

    /** 当前链路：`direct|12`（直连 12ms）/ `relay|312` / 空串（没连上） */
    external fun nativePathStatus(): String

    /** 手机收到的文件 + 手机发出去的文件，JSON 数组：[{name,size,mtime,path,outgoing}] */
    external fun nativeListFiles(): String

    /** 电脑的显示名（来自电脑发的 HELLO），没连过就是空串 */
    external fun nativePeerName(): String
}

/** Rust 侧通过 JNI 回调这些方法，方法名与签名必须保持一致。 */
interface BridgeListener {
    fun onClipText(seq: Long, text: String)
    fun onState(state: String)

    /** 文件落地完成（当前落在 app 私有目录 inbox/ 下）。 */
    fun onFile(name: String, path: String, size: Long) {}

    /** 电脑发来一张图：PNG 已落在 inbox/，UI 用来显示缩略图。 */
    fun onImage(seq: Long, path: String, w: Long, h: Long) {}

    /** 文件接收进度，用于进度通知。 */
    fun onFileProgress(name: String, done: Long, total: Long) {}

    /** 手机发出去的文件，电脑回报校验结果。 */
    fun onFileSent(name: String, ok: Boolean) {}
}
