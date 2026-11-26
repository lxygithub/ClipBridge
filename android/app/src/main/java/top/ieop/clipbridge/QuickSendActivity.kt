package top.ieop.clipbridge

import android.content.ClipboardManager
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity

/**
 * 所有「不打开 app」的发送入口都落到这里：
 *  - 桌面长按快捷方式（SEND_CLIP）
 *  - 分享面板（ACTION_SEND / ACTION_SEND_MULTIPLE）
 *  - 快捷设置磁贴（内部转成 SEND_CLIP）
 *
 * 之所以必须是 Activity：Android 10+ 只有获得焦点的应用才能读剪贴板。
 * 界面用 1dp 透明主题，处理完立刻 finish()，用户只看到一闪。
 */
class QuickSendActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        if (!Prefs(this).isConfigured) {
            toast("请先在 ClipBridge 里填电脑端 ID")
            finish()
            return
        }
        // 服务没在跑就顺手拉起来，发送时它会自己建连
        SyncService.start(this)

        when (intent?.action) {
            ACTION_SEND_CLIP -> sendClipboard()
            Intent.ACTION_SEND -> sendItem(intent)
            Intent.ACTION_SEND_MULTIPLE -> sendItems(intent)
            else -> finish()
        }
    }

    private fun sendClipboard() {
        val cm = getSystemService(ClipboardManager::class.java)
        val clip = cm.primaryClip
        val text = if (clip != null && clip.itemCount > 0) clip.getItemAt(0).text?.toString() else null
        if (text.isNullOrBlank()) {
            toast("剪贴板为空")
            finish()
            return
        }
        runSend(text)
    }

    private fun sendItem(intent: Intent) {
        val text = intent.getStringExtra(Intent.EXTRA_TEXT)
        val uri: Uri? = intent.getParcelableExtra(Intent.EXTRA_STREAM)
        when {
            !text.isNullOrBlank() -> runSend(text)
            uri != null -> {
                // 文件传输（FILE_OFFER 流）下一步接，这里先明确提示，不假装成功
                toast("文件传输还没接，先发文本吧")
                finish()
            }
            else -> {
                toast("没有可发送的内容")
                finish()
            }
        }
    }

    private fun sendItems(intent: Intent) {
        val uris: List<Uri> = intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM) ?: emptyList()
        toast("暂不支持多文件（${uris.size} 个）")
        finish()
    }

    /**
     * native 侧发送会阻塞（建连最多 ~8s + 等 ACK ~5s），必须离开主线程。
     * 服务刚被拉起时桥还没就绪，所以前几次失败重试一小会儿。
     */
    private fun runSend(text: String) {
        Thread({
            var ok = false
            var tries = 0
            while (!ok && tries < 12) {
                ok = runCatching { Native.nativeSendText(text) }.getOrDefault(false)
                if (!ok) {
                    tries++
                    Thread.sleep(300)
                }
            }
            runOnUiThread {
                toast(if (ok) "已发送到电脑" else "发送失败：电脑未连接")
                finish()
            }
        }, "quick-send").start()
    }

    private fun toast(msg: String) = Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()

    companion object {
        const val ACTION_SEND_CLIP = "top.ieop.clipbridge.SEND_CLIP"
    }
}
