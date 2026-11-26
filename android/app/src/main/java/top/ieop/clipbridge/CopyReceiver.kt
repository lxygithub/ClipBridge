package top.ieop.clipbridge

import android.content.BroadcastReceiver
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.util.Log
import android.widget.Toast

/**
 * 通知栏「复制」按钮的落点。
 *
 * 关键点：Android 只限制「后台读剪贴板」，写入是允许的，
 * 所以这里可以直接写，不需要打开 app。13+ 会由系统弹出「已复制」提示，无法规避。
 * 若个别 ROM 静默失败（可通过回读校验发现），改用 CopyTrampoline（NoDisplay 透明 Activity）兜底。
 */
class CopyReceiver : BroadcastReceiver() {

    override fun onReceive(context: Context, intent: Intent) {
        val text = intent.getStringExtra(EXTRA_TEXT) ?: return
        val cm = context.getSystemService(ClipboardManager::class.java)
        cm.setPrimaryClip(ClipData.newPlainText("ClipBridge", text))

        val ok = cm.primaryClip?.getItemAt(0)?.text?.toString() == text
        if (ok) {
            // TODO(M2): 回 ACK 给 Windows，推进 acked_seq
            Toast.makeText(context, "已复制", Toast.LENGTH_SHORT).show()
        } else {
            Log.w(TAG, "clipboard write failed, fallback to trampoline activity")
            context.startActivity(
                Intent(context, CopyTrampoline::class.java)
                    .putExtra(EXTRA_TEXT, text)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            )
        }
    }

    companion object {
        private const val TAG = "CopyReceiver"
        const val EXTRA_TEXT = "extra_text"
    }
}
