package top.ieop.clipbridge

import android.content.ClipData
import android.content.ClipboardManager
import android.os.Bundle
import androidx.activity.ComponentActivity

/**
 * 兜底入口：个别 ROM 会拦截后台写剪贴板时使用。
 *
 * 注意：Android 10+ 对「后台启动 Activity」有全局限制，从 BroadcastReceiver 直接拉起
 * 可能被系统拦截。M5 阶段验证真机行为后，若确实被拦，改为
 * 「常驻通知 + 用户手动点击」的方式，不做无谓的激进保活。
 */
class CopyTrampoline : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        intent.getStringExtra(CopyReceiver.EXTRA_TEXT)?.let { text ->
            getSystemService(ClipboardManager::class.java)
                .setPrimaryClip(ClipData.newPlainText("ClipBridge", text))
        }
        finish()
    }
}
