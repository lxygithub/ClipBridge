package top.ieop.clipbridge

import android.content.ClipboardManager
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.widget.Toast
import androidx.activity.ComponentActivity
import java.io.File

/**
 * 所有「不打开 app」的发送入口都落到这里：
 *  - 桌面长按快捷方式（SEND_CLIP）
 *  - 分享面板（ACTION_SEND / ACTION_SEND_MULTIPLE：文本、图片、任意文件）
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
        val uris = incomingUris(intent)
        when {
            uris.isNotEmpty() -> runSendFile(uris)
            !text.isNullOrBlank() -> runSend(text)
            else -> {
                toast("没有可发送的内容")
                finish()
            }
        }
    }

    private fun sendItems(intent: Intent) {
        val text = intent.getStringExtra(Intent.EXTRA_TEXT)
        val uris = incomingUris(intent)
        when {
            uris.isNotEmpty() -> runSendFile(uris)
            !text.isNullOrBlank() -> runSend(text)
            else -> {
                toast("没有可发送的内容")
                finish()
            }
        }
    }

    /**
     * 取分享进来的文件 Uri。各家 app 传法不统一：`EXTRA_STREAM` 可能是 Uri、字符串，
     * 也可能塞在 `ClipData` 里，三种都兜住，否则「分享过来没反应」。
     */
    private fun incomingUris(intent: Intent): List<Uri> {
        val out = mutableListOf<Uri>()
        when (val raw = intent.extras?.get(Intent.EXTRA_STREAM)) {
            is Uri -> out += raw
            is String -> Uri.parse(raw)?.let { out += it }
            is ArrayList<*> -> raw.forEach {
                when (it) {
                    is Uri -> out += it
                    is String -> Uri.parse(it)?.let { u -> out += u }
                }
            }
        }
        if (out.isEmpty()) {
            val clip = intent.clipData
            for (i in 0 until (clip?.itemCount ?: 0)) {
                clip?.getItemAt(i)?.uri?.let { out += it }
            }
        }
        return out.distinct()
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

    /**
     * 别的 app 分享过来的文件：先落到自己的缓存目录（Rust 侧只认本地路径），
     * 发完就删。多个文件按顺序发，全部处理完才提示。
     */
    private fun runSendFile(uris: List<Uri>) {
        Thread({
            var sent = 0
            for (uri in uris) {
                val name = displayName(uri)
                val tmp = File(cacheDir, name)
                val copied = runCatching {
                    contentResolver.openInputStream(uri)!!.use { input ->
                        tmp.outputStream().use { input.copyTo(it) }
                    }
                    true
                }.getOrDefault(false)
                if (!copied) continue

                var ok = false
                var tries = 0
                while (!ok && tries < 12) {
                    ok = runCatching { Native.nativeSendFile(tmp.absolutePath) }.getOrDefault(false)
                    if (!ok) {
                        tries++
                        Thread.sleep(300)
                    }
                }
                tmp.delete()
                if (ok) sent++
            }
            runOnUiThread {
                toast(
                    when {
                        sent == 0 -> "发送失败：电脑未连接"
                        sent == uris.size -> "已发送 $sent 个文件到电脑"
                        else -> "已发送 $sent/${uris.size} 个文件"
                    }
                )
                finish()
            }
        }, "quick-send-file").start()
    }

    /** 从 content Uri 里取一个能当文件名的显示名。 */
    private fun displayName(uri: Uri): String {
        val fallback = "shared-${System.currentTimeMillis()}"
        return runCatching {
            contentResolver.query(uri, null, null, null, null)?.use { c ->
                if (c.moveToFirst()) {
                    val idx = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                    if (idx >= 0) c.getString(idx)?.takeIf { it.isNotBlank() } ?: fallback else fallback
                } else {
                    fallback
                }
            } ?: uri.lastPathSegment ?: fallback
        }.getOrDefault(fallback)
    }

    private fun toast(msg: String) = Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()

    companion object {
        const val ACTION_SEND_CLIP = "top.ieop.clipbridge.SEND_CLIP"
    }
}
