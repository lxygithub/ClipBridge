package top.ieop.clipbridge

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log

/**
 * 兜底唤醒 + 开机自启：只做一件事——把常驻的 [SyncService] 拉起来。
 *
 * 服务自己活着的时候，这次 start 走到 onStartCommand 里 `started == true` 就直接返回，
 * 不会重连也不会重连抖动，代价可以忽略；服务被系统/厂商策略杀掉了，这一下就把它救回来，
 * 之后 Rust 侧的 supervisor 会自己连上电脑。手机重启后也要靠它把服务重新立起来
 * （AlarmManager 的定时唤醒在重启后就没了，不重建一次就永远等不到下一次 15 分钟）。
 *
 * Android 12+ 对「后台启动前台服务」有限制，这里包了 try/catch：真被拦了也不会崩，
 * 用户下次打开 app 时 MainActivity.onResume 会再抢救一次。
 */
class WakeReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val prefs = Prefs(context)
        if (!prefs.isConfigured) return
        runCatching { SyncService.start(context) }
            .onFailure {
                Log.w("WakeReceiver", "wake start blocked: ${it.javaClass.simpleName}")
            }
    }
}
