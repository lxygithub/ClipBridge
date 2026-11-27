package top.ieop.clipbridge

import android.content.ClipboardManager
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts
import java.io.ByteArrayOutputStream
import java.io.File

/**
 * 所有「不打开 app」的发送入口都落到这里：
 *  - 桌面长按快捷方式（SEND_CLIP / SEND_FILE）
 *  - 分享面板（ACTION_SEND / ACTION_SEND_MULTIPLE：文本、图片、任意文件）
 *  - 快捷设置磁贴（内部转成 SEND_CLIP）
 *
 * 之所以必须是 Activity：Android 10+ 只有**获得焦点**的应用才能读剪贴板。
 * 界面用透明主题，处理完立刻 finish()，用户只看到一闪。
 *
 * 踩过的坑（都别再改回去）：
 *  1. 剪贴板不能在 onCreate 里读 —— 那时还没有窗口焦点，`primaryClip` 一定返回 null，
 *     表现就是「提示剪贴板为空」。必须等 onWindowFocusChanged(true)，再重试几帧。
 *  2. SEND_FILE 快捷方式原来没进 when 分支，直接走 else -> finish()，
 *     所以「发送文件到电脑」点了没任何反应。现在会拉起系统文件选择器。
 */
class QuickSendActivity : ComponentActivity() {

    /** SEND_CLIP 要等焦点；已经处理过就不再重复 */
    private var clipHandled = false
    private var wantClipboard = false

    private val pickFile =
        registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
            if (uri == null) {
                finish()
            } else {
                runSendFile(listOf(uri))
            }
        }

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
            ACTION_SEND_CLIP -> wantClipboard = true
            ACTION_SEND_FILE -> pickFile.launch(arrayOf("*/*"))
            Intent.ACTION_SEND -> sendItem(intent)
            Intent.ACTION_SEND_MULTIPLE -> sendItems(intent)
            else -> finish()
        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && wantClipboard && !clipHandled) {
            clipHandled = true
            // 焦点刚拿到的那一帧，系统可能还没放行剪贴板读取：延后一帧再试
            window.decorView.post { sendClipboard(0) }
        }
    }

    /**
     * 读剪贴板并发送。读不到就小幅重试（150ms x 4），都失败才提示为空——
     * 这样「刚复制完立刻点快捷方式」也不会误报。
     */
    private fun sendClipboard(attempt: Int) {
        val text = readClip()
        if (!text.isNullOrBlank()) {
            runSend(text)
            return
        }
        // 没文字再当图片试一次：截图后「复制图片」是手机上最常见的用法
        val png = readClipImage()
        if (png != null) {
            runSendImage(png)
            return
        }
        if (attempt < 4) {
            window.decorView.postDelayed({ sendClipboard(attempt + 1) }, 150)
        } else {
            toast("剪贴板为空")
            finish()
        }
    }

    private fun readClip(): String? = runCatching {
        val cm = getSystemService(ClipboardManager::class.java)
        val clip = cm?.primaryClip
        if (clip == null || clip.itemCount == 0) null
        else clip.getItemAt(0).text?.toString()
    }.getOrNull()

    /** 剪贴板里的图（content Uri）→ 压好的 PNG 字节。 */
    private fun readClipImage(): ByteArray? = runCatching {
        val clip = getSystemService(ClipboardManager::class.java)?.primaryClip ?: return null
        if (clip.itemCount == 0) return null
        val uri = clip.getItemAt(0).uri ?: return null
        val bmp: Bitmap = contentResolver.openInputStream(uri)?.use {
            BitmapFactory.decodeStream(it)
        } ?: return null
        val long = maxOf(bmp.width, bmp.height)
        val scaled = if (long > 2000) {
            val s = 2000f / long
            Bitmap.createScaledBitmap(
                bmp,
                (bmp.width * s).toInt().coerceAtLeast(1),
                (bmp.height * s).toInt().coerceAtLeast(1),
                true
            )
        } else bmp
        ByteArrayOutputStream().use { out ->
            scaled.compress(Bitmap.CompressFormat.PNG, 100, out)
            out.toByteArray()
        }
    }.getOrNull()

    private fun runSendImage(png: ByteArray) {
        Thread({
            var ok = false
            var tries = 0
            while (!ok && tries < 12) {
                ok = runCatching { Native.nativeSendImage(png) }.getOrDefault(false)
                if (!ok) {
                    tries++
                    Thread.sleep(300)
                }
            }
            runOnUiThread {
                toast(if (ok) "已发送图片到电脑" else "发送失败：电脑未连接")
                finish()
            }
        }, "quick-send-image").start()
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
            if (uris.isEmpty()) {
                runOnUiThread { finish() }
                return@Thread
            }
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
        const val ACTION_SEND_FILE = "top.ieop.clipbridge.SEND_FILE"
    }
}
