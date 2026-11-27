package top.ieop.clipbridge

import android.app.AlarmManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import android.os.SystemClock
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
        // 服务可能先于 Activity 起来（开机自启），历史也在这里读一次
        BridgeState.attach(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // 必须第一时间 startForeground，否则 5s 内会被系统判 ANR/崩溃
        startForeground(Notifier.FG_ID, Notifier.serviceNotification(this, BridgeState.state.value))

        if (intent?.action == Prefs.ACTION_STOP) {
            Native.nativeStop()
            started = false
            cancelWake()
            stopSelf()
            return START_NOT_STICKY
        }

        if (intent?.action == ACTION_RESTART) {
            // 切到另一台电脑：先拆旧连接（nativeStop 会阻塞，放子线程），再按新配置重连
            started = false
            Thread {
                Native.nativeStop()
                val prefs = Prefs(this)
                if (prefs.isConfigured) startBridge(prefs)
            }.start()
            return START_STICKY
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
        // 15 分钟一次的兜底唤醒：服务被系统/厂商后台策略杀掉后，靠它把前台服务再拉回来。
        // inexact + ELAPSED_REALTIME_WAKEUP 不亮屏，Doze 下由系统合并，尽量少耗电。
        scheduleWake()
        return START_STICKY
    }

    private fun scheduleWake() {
        val am = getSystemService(AlarmManager::class.java)
        if (am == null) {
            Log.w(TAG, "no AlarmManager, wake-up skipped")
            return
        }
        val pi = PendingIntent.getBroadcast(
            this,
            WAKE_REQUEST,
            Intent(this, WakeReceiver::class.java).setAction(ACTION_WAKE),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        am.setInexactRepeating(
            AlarmManager.ELAPSED_REALTIME_WAKEUP,
            SystemClock.elapsedRealtime() + WAKE_INTERVAL_MS,
            WAKE_INTERVAL_MS,
            pi
        )
    }

    private fun cancelWake() {
        val am = getSystemService(AlarmManager::class.java) ?: return
        val pi = PendingIntent.getBroadcast(
            this,
            WAKE_REQUEST,
            Intent(this, WakeReceiver::class.java).setAction(ACTION_WAKE),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        am.cancel(pi)
    }

    private fun startBridge(prefs: Prefs) {
        started = true
        val dir = filesDir.absolutePath
        val peer = prefs.peerId
        val relay = prefs.relay
        // 建端点会阻塞几秒（等 relay online），不能占主线程
        Thread({
            val ok = runCatching {
                Native.nativeStart(dir, peer, relay, deviceName(), this)
            }.getOrDefault(false)
            if (!ok) {
                started = false
                BridgeState.set("error:启动失败")
                Notifier.updateService(this, "error:启动失败")
            }
        }, "bridge-start").start()
    }

    /** 电脑端要显示的设备名：厂商 + 型号，去掉「小米 小米15」这种重复前缀。 */
    private fun deviceName(): String {
        val model = Build.MODEL?.trim().orEmpty()
        val brand = Build.MANUFACTURER?.trim().orEmpty()
        if (model.isEmpty()) return "Android"
        val pretty = model.replaceFirstChar { it.uppercase() }
        return if (brand.isEmpty() || model.lowercase().startsWith(brand.lowercase())) {
            pretty
        } else {
            "$brand $pretty"
        }
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
        // 文件不再是独立列表，直接作为一条普通时间线条目（点开就能看）
        BridgeState.pushFile(name = name, size = size, path = path, outgoing = false)
        Notifier.showFile(this, name, size)
    }

    override fun onImage(seq: Long, path: String, w: Long, h: Long) {
        BridgeState.lastSeq.value = seq
        BridgeState.pushImage(path, w.toInt(), h.toInt(), outgoing = false)
        Notifier.showText(this, seq, "图片 ${w}×${h} · 已存到手机")
    }

    /** 历史快照：只填时间线，不通知（否则一次补 20 条会炸一堆通知）。 */
    override fun onHistory(json: String) {
        val obj = runCatching { org.json.JSONObject(json) }.getOrNull() ?: return
        val rows = obj.optJSONArray("items") ?: return
        val list = ArrayList<ClipItem>(rows.length())
        for (i in 0 until rows.length()) {
            val row = runCatching { rows.getJSONArray(i) }.getOrNull() ?: continue
            list += ClipItem(
                seq = row.getLong(0),
                text = row.getString(1),
                // 电脑侧给的是秒级 mtime
                ts = row.optLong(2, 0L) * 1000L
            )
        }
        BridgeState.pushHistory(list)
    }

    override fun onFileProgress(name: String, done: Long, total: Long) {
        // total == 0 是「被取消」的约定信号
        if (total <= 0) Notifier.showFileCanceled(this, name)
        else Notifier.updateFileProgress(this, name, done, total)
    }

    override fun onFileSent(name: String, ok: Boolean) {
        if (ok) {
            // 文件本体在电脑上，条目里只留流水，点开提示位置
            BridgeState.pushFile(name = name, size = 0, path = null, outgoing = true)
        } else {
            BridgeState.push(
                ClipItem(seq = 0, text = "发送失败 · $name", outgoing = true)
            )
        }
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
        const val ACTION_RESTART = "top.ieop.clipbridge.RESTART"
        const val ACTION_WAKE = "top.ieop.clipbridge.WAKE"

        /** 兜底唤醒周期：15 分钟是 AlarmManager 能用的最短周期，再短就是白耗电。 */
        private const val WAKE_INTERVAL_MS = 15L * 60L * 1000L
        private const val WAKE_REQUEST = 1001

        /** 换了目标电脑：断开旧连接、按新配置重来。 */
        fun restart(context: Context) {
            context.startService(
                Intent(context, SyncService::class.java).setAction(ACTION_RESTART)
            )
        }

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
