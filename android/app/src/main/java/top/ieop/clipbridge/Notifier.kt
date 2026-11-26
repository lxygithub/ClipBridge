package top.ieop.clipbridge

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent

/**
 * 通知渠道与通知构造。
 *
 * 文本消息用 LOW（无声、不弹窗），避免来回同步把手机震成筛子；
 * 常驻通知用 MIN（折叠进「其他通知」，尽量不打扰）。
 */
object Notifier {
    const val CH_TEXT = "clip_text"
    const val CH_FILE = "clip_file"
    const val CH_SERVICE = "service_alive"
    const val FG_ID = 1
    const val FILE_ID = 2

    private const val TAG_TEXT = "clip_text"

    fun ensureChannels(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CH_SERVICE, "同步服务", NotificationManager.IMPORTANCE_MIN).apply {
                description = "保持与电脑的连接"
                setShowBadge(false)
            }
        )
        nm.createNotificationChannel(
            NotificationChannel(CH_TEXT, "来自电脑的文本", NotificationManager.IMPORTANCE_LOW).apply {
                description = "电脑剪贴板同步过来的文本"
                setShowBadge(true)
            }
        )
        nm.createNotificationChannel(
            NotificationChannel(CH_FILE, "来自电脑的文件", NotificationManager.IMPORTANCE_LOW).apply {
                description = "电脑传来的文件"
            }
        )
    }

    /** 文本通知：点正文回 app，点「复制」直接写剪贴板。 */
    fun showText(ctx: Context, seq: Long, text: String) {
        val body = text.take(4000)
        val open = PendingIntent.getActivity(
            ctx, 0,
            Intent(ctx, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        val copy = PendingIntent.getBroadcast(
            ctx,
            (seq and 0xffff).toInt().coerceAtLeast(1),
            Intent(ctx, CopyReceiver::class.java)
                .setAction(TAG_TEXT)
                .putExtra(CopyReceiver.EXTRA_TEXT, text),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )

        val n = Notification.Builder(ctx, CH_TEXT)
            .setSmallIcon(R.drawable.ic_shortcut_clip)
            .setContentTitle("来自电脑的文本")
            .setContentText(body.lineSequence().first().take(80))
            .setStyle(Notification.BigTextStyle().bigText(body))
            .setContentIntent(open)
            .addAction(Notification.Action.Builder(null, "复制", copy).build())
            .setAutoCancel(true)
            .build()

        ctx.getSystemService(NotificationManager::class.java)
            .notify(10_000 + (seq % 500).toInt(), n)
    }

    /** 文件到达：进度通知（LOW），完成后改成「已保存」。 */
    fun updateFileProgress(ctx: Context, name: String, done: Long, total: Long) {
        val pct = if (total > 0) (done * 100 / total).toInt() else 0
        val open = PendingIntent.getActivity(
            ctx, 2,
            Intent(ctx, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        val n = Notification.Builder(ctx, CH_FILE)
            .setSmallIcon(R.drawable.ic_shortcut_file)
            .setContentTitle("接收文件 · $name")
            .setContentText("$pct% · ${human(done)} / ${human(total)}")
            .setProgress(100, pct, total <= 0)
            .setContentIntent(open)
            .setOngoing(true)
            .build()
        ctx.getSystemService(NotificationManager::class.java).notify(FILE_ID, n)
    }

    fun showFile(ctx: Context, name: String, size: Long) {
        val open = PendingIntent.getActivity(
            ctx, 2,
            Intent(ctx, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        val n = Notification.Builder(ctx, CH_FILE)
            .setSmallIcon(R.drawable.ic_shortcut_file)
            .setContentTitle("文件已保存 · $name")
            .setContentText(human(size) + " · 在应用内查看路径")
            .setContentIntent(open)
            .setAutoCancel(true)
            .build()
        ctx.getSystemService(NotificationManager::class.java).notify(FILE_ID, n)
    }

    /** 常驻通知：显示连接状态。 */
    fun serviceNotification(ctx: Context, state: String = "启动中"): Notification {
        val open = PendingIntent.getActivity(
            ctx, 1,
            Intent(ctx, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        return Notification.Builder(ctx, CH_SERVICE)
            .setSmallIcon(R.drawable.ic_shortcut_clip)
            .setContentTitle("ClipBridge")
            .setContentText(stateText(state))
            .setContentIntent(open)
            .setOngoing(true)
            .build()
    }

    fun updateService(ctx: Context, state: String) {
        ctx.getSystemService(NotificationManager::class.java)
            .notify(FG_ID, serviceNotification(ctx, state))
    }

    fun stateText(state: String): String = when {
        state == "connected" -> "已连接电脑"
        state == "connecting" -> "正在连接电脑…"
        state == "disconnected" -> "未连接（自动重试中）"
        state.startsWith("error") -> "连接异常：${state.removePrefix("error:")}"
        else -> state
    }

    /** 1.2 MB / 340 KB 这类可读体积。 */
    fun human(bytes: Long): String = when {
        bytes >= 1024L * 1024 * 1024 -> String.format("%.2f GB", bytes / 1024.0 / 1024 / 1024)
        bytes >= 1024L * 1024 -> String.format("%.1f MB", bytes / 1024.0 / 1024)
        bytes >= 1024 -> String.format("%.0f KB", bytes / 1024.0)
        else -> "$bytes B"
    }
}
