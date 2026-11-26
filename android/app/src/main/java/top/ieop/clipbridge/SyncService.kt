package top.ieop.clipbridge

import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.IBinder
import android.util.Log

/**
 * 实时模式常驻服务：持有到 Windows 的 iroh 长连接，收到推送就发通知。
 *
 * 生命周期：MainActivity / 开机自启 / 快捷方式入口都可以拉起它；
 * Rust 侧自带 supervisor，连接断了会 3s 退避重连，所以这里不做重连逻辑。
 */
class SyncService : Service(), BridgeListener {

    @Volatile
    private var started = false

    override fun onCreate() {
        super.onCreate()
        Notifier.ensureChannels(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // 必须第一时间 startForeground，否则 5s 内会被系统判 ANR/崩溃
        startForeground(Notifier.FG_ID, Notifier.serviceNotification(this, BridgeState.state.value))

        if (intent?.action == Prefs.ACTION_STOP) {
            Native.nativeStop()
            started = false
            stopSelf()
            return START_NOT_STICKY
        }

        if (intent?.action == ACTION_CANCEL_TRANSFER) {
            // 通知栏「取消」：Rust 侧会中断接收、删掉半截文件并让 PC 停止重试
            Thread { Native.nativeCancelTransfer() }.start()
            return START_STICKY
        }

        val prefs = Prefs(this)
        if (!prefs.isConfigured) {
            Log.w(TAG, "no peer configured, service stops")
            stopSelf()
            return START_NOT_STICKY
        }

        if (!started) startBridge(prefs)
        return START_STICKY
    }

    private fun startBridge(prefs: Prefs) {
        started = true
        val dir = filesDir.absolutePath
        val peer = prefs.peerId
        val relay = prefs.relay
        // 建端点会阻塞几秒（等 relay online），不能占主线程
        Thread({
            val ok = runCatching { Native.nativeStart(dir, peer, relay, this) }.getOrDefault(false)
            if (!ok) {
                started = false
                BridgeState.set("error:启动失败")
                Notifier.updateService(this, "error:启动失败")
            }
        }, "bridge-start").start()
    }

    override fun onClipText(seq: Long, text: String) {
        BridgeState.lastSeq.value = seq
        BridgeState.push(ClipItem(seq = seq, text = text))
        Notifier.showText(this, seq, text)
    }

    override fun onState(state: String) {
        BridgeState.set(state)
        Notifier.updateService(this, state)
    }

    override fun onFile(name: String, path: String, size: Long) {
        BridgeState.push(
            ClipItem(seq = 0, text = "文件 · $name", ts = System.currentTimeMillis())
        )
        Notifier.showFile(this, name, size)
    }

    override fun onFileProgress(name: String, done: Long, total: Long) {
        // total == 0 是「被取消」的约定信号
        if (total <= 0) Notifier.showFileCanceled(this, name)
        else Notifier.updateFileProgress(this, name, done, total)
    }

    override fun onFileSent(name: String, ok: Boolean) {
        BridgeState.push(
            ClipItem(
                seq = 0,
                text = if (ok) "已发送 · $name" else "发送失败 · $name",
                ts = System.currentTimeMillis(),
                outgoing = true
            )
        )
        Notifier.showFileSent(this, name, ok)
    }

    override fun onDestroy() {
        Native.nativeStop()
        started = false
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val TAG = "SyncService"

        const val ACTION_CANCEL_TRANSFER = "top.ieop.clipbridge.CANCEL_TRANSFER"

        /** 通知栏「取消」按钮用：不新起服务，直接投给已运行的服务。 */
        fun cancelTransfer(context: Context) {
            context.startService(
                Intent(context, SyncService::class.java).setAction(ACTION_CANCEL_TRANSFER)
            )
        }

        fun start(context: Context) {
            context.startForegroundService(Intent(context, SyncService::class.java))
        }

        fun stop(context: Context) {
            context.startService(
                Intent(context, SyncService::class.java).setAction(Prefs.ACTION_STOP)
            )
        }
    }
}
