package top.ieop.clipbridge

import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/** 从 content Uri 里取一个能当文件名的显示名。 */
private fun queryName(ctx: Context, uri: android.net.Uri): String {
    val fallback = "file-" + System.currentTimeMillis()
    val name = runCatching {
        ctx.contentResolver.query(uri, null, null, null, null)?.use { c ->
            val idx = c.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
            if (idx >= 0 && c.moveToFirst()) c.getString(idx) else null
        }
    }.getOrNull()
    return (name ?: fallback).replace(Regex("[\\\\/:*?\"<>|]"), "_")
}

/** 方向 3「一条通道」的色板。 */
object Palette {
    val void = Color(0xFF070E15)
    val panel = Color(0xFF0B141C)
    val border = Color(0xFF1B2836)
    val ink = Color(0xFFE4EBF2)
    val dim = Color(0xFF96A4B3)
    val faint = Color(0xFF8493A3)
    val amber = Color(0xFFFFB454)
    val teal = Color(0xFF4FD1C5)
}

private val clockFormat = SimpleDateFormat("HH:mm:ss", Locale.getDefault())

/**
 * 主界面：一条通道。
 *
 * 上面是电脑节点，下面是本机；条目挂在中间那条线上，越新越靠上。
 * 断线时线变灰、节点变红，不用点进设置才知道。
 */
class MainActivity : ComponentActivity() {

    private lateinit var prefs: Prefs

    private val notifPerm =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        prefs = Prefs(this)
        Notifier.ensureChannels(this)
        askNotificationPermission()

        // 已配对就顺手把常驻服务拉起来；只是建立连接，不会去读剪贴板
        if (prefs.isConfigured) SyncService.start(this)

        setContent {
            ChannelScreen(
                peerId = prefs.peerId,
                onSave = { peer, relay ->
                    prefs.peerId = peer
                    prefs.relay = relay
                    SyncService.start(this)
                },
                onStop = { SyncService.stop(this) }
            )
        }
    }

    private fun askNotificationPermission() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            notifPerm.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }
}

@Composable
private fun ChannelScreen(
    peerId: String,
    onSave: (String, String) -> Unit,
    onStop: () -> Unit
) {
    val ctx = LocalContext.current
    val state by BridgeState.state.collectAsState()
    val items by BridgeState.items.collectAsState()
    val sent by BridgeState.sentCount.collectAsState()

    var statsOpen by remember { mutableStateOf(false) }
    var settingsOpen by remember { mutableStateOf(false) }
    var selected by remember { mutableStateOf<ClipItem?>(null) }

    val connected = state == "connected"
    val accent = if (connected) Palette.teal else Color(0xFF8795A4)

    // 手机 → 电脑发文件：选一个文档，先复制到应用缓存再交给 native
    val pickFile = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        Thread {
            val name = queryName(ctx, uri)
            val tmp = File(ctx.cacheDir, name)
            val okCopy = runCatching {
                ctx.contentResolver.openInputStream(uri)!!.use { input ->
                    tmp.outputStream().use { input.copyTo(it) }
                }
                true
            }.getOrDefault(false)
            if (!okCopy) {
                Handler(Looper.getMainLooper()).post { toast(ctx, "读取文件失败") }
                return@Thread
            }
            val ok = runCatching { Native.nativeSendFile(tmp.absolutePath) }.getOrDefault(false)
            tmp.delete()
            Handler(Looper.getMainLooper()).post {
                toast(ctx, if (ok) "已发送，等待电脑校验" else "发送失败：未连接")
            }
        }.start()
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(Palette.void)
            .padding(horizontal = 18.dp)
    ) {
        Spacer(Modifier.height(14.dp))

        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                "CLIPBRIDGE / 通道",
                color = Palette.faint,
                fontSize = 9.sp,
                letterSpacing = 2.sp
            )
            Spacer(Modifier.weight(1f))
            TextButton(onClick = { settingsOpen = true }) {
                Text("设置", color = Palette.dim, fontSize = 12.sp)
            }
        }

        NodeCard(connected = connected, peerId = peerId, state = state, accent = accent) {
            statsOpen = !statsOpen
        }

        if (statsOpen) {
            Spacer(Modifier.height(10.dp))
            StatsCard(
                total = items.size,
                down = items.count { !it.outgoing },
                up = sent,
                state = state
            )
        }

        Spacer(Modifier.height(14.dp))
        Row {
            Text("时间 · 方向 · 内容", color = Palette.faint, fontSize = 9.sp, letterSpacing = 1.4.sp)
            Spacer(Modifier.weight(1f))
            Text("共 ${items.size} 条", color = Palette.faint, fontSize = 9.sp)
        }
        Spacer(Modifier.height(4.dp))

        Box(Modifier.weight(1f).fillMaxWidth()) {
            if (items.isEmpty()) {
                Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.Center) {
                    Text(
                        "通道已建立，还没有条目。\n电脑上复制的东西会落在这里。",
                        color = Palette.faint,
                        fontSize = 12.sp,
                        lineHeight = 22.sp
                    )
                }
            } else {
                LazyColumn(Modifier.fillMaxSize()) {
                    items(items, key = { "${it.seq}-${it.ts}" }) { item ->
                        TimelineRow(item) { selected = item }
                    }
                    item {
                        Text(
                            "更早的条目在电脑端历史里",
                            color = Palette.faint,
                            fontSize = 9.sp,
                            letterSpacing = 1.sp,
                            modifier = Modifier.padding(start = 34.dp, top = 4.dp, bottom = 12.dp)
                        )
                    }
                }
            }
        }

        FlowBar(down = items.count { !it.outgoing }, up = sent)
        Spacer(Modifier.height(10.dp))

        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlineAction("发送当前剪贴板", Palette.teal, Modifier.weight(1f)) {
                val text = readClipboard(ctx)
                if (text.isNullOrBlank()) {
                    toast(ctx, "剪贴板为空")
                } else {
                    Thread {
                        val ok = runCatching { Native.nativeSendText(text) }.getOrDefault(false)
                        if (ok) BridgeState.markSent(text)
                        (ctx as? ComponentActivity)?.runOnUiThread {
                            toast(ctx, if (ok) "已发送到电脑" else "发送失败：未连接")
                        }
                    }.start()
                }
            }
            OutlineAction("发文件", Palette.amber, Modifier.width(88.dp)) {
                pickFile.launch(arrayOf("*/*"))
            }
            OutlineAction("断开", Palette.dim, Modifier.width(64.dp)) {
                onStop()
                toast(ctx, "已断开")
            }
        }
        Spacer(Modifier.height(22.dp))
    }

    selected?.let { item ->
        AlertDialog(
            onDismissRequest = { selected = null },
            containerColor = Palette.panel,
            title = {
                Text(
                    if (item.outgoing) "发往电脑" else "来自电脑",
                    color = Palette.ink,
                    fontSize = 14.sp
                )
            },
            text = {
                Column {
                    Text(item.text, color = Palette.ink, fontSize = 14.sp, lineHeight = 22.sp)
                    Spacer(Modifier.height(10.dp))
                    Text(
                        "${clockFormat.format(Date(item.ts))} · ${item.text.length} 字" +
                            if (item.seq > 0) " · #${item.seq}" else "",
                        color = Palette.faint,
                        fontSize = 10.sp
                    )
                    Spacer(Modifier.height(14.dp))
                    Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        DetailAction("发回电脑") {
                            Thread {
                                val ok = runCatching { Native.nativeSendText(item.text) }.getOrDefault(false)
                                if (ok) BridgeState.markSent(item.text)
                                (ctx as? ComponentActivity)?.runOnUiThread {
                                    toast(ctx, if (ok) "已发回电脑" else "发送失败：未连接")
                                }
                            }.start()
                            selected = null
                        }
                        DetailAction("从列表移除") {
                            BridgeState.items.value = BridgeState.items.value.filterNot { it.ts == item.ts }
                            selected = null
                        }
                    }
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    writeClipboard(ctx, item.text)
                    toast(ctx, "已复制到手机剪贴板")
                    selected = null
                }) { Text("复制", color = Palette.teal) }
            },
            dismissButton = {
                TextButton(onClick = { selected = null }) {
                    Text("关闭", color = Palette.dim)
                }
            }
        )
    }

    if (settingsOpen) {
        SettingsDialog(peerId, onClose = { settingsOpen = false }) { peer, relay ->
            onSave(peer, relay)
            settingsOpen = false
        }
    }
}

@Composable
private fun NodeCard(
    connected: Boolean,
    peerId: String,
    state: String,
    accent: Color,
    onClick: () -> Unit
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = 10.dp)
            .background(Palette.panel, RoundedCornerShape(6.dp))
            .border(1.dp, Palette.border, RoundedCornerShape(6.dp))
            .clickable { onClick() }
            .padding(horizontal = 14.dp, vertical = 13.dp),
        verticalAlignment = Alignment.CenterVertically
    ) {
        Box(Modifier.size(8.dp).background(accent, CircleShape))
        Spacer(Modifier.width(10.dp))
        Column(Modifier.weight(1f)) {
            Text(
                "PC · ${peerId.take(8)}…",
                color = Palette.ink,
                fontSize = 13.sp,
                fontFamily = FontFamily.Monospace
            )
            Text(
                Notifier.stateText(state),
                color = Palette.dim,
                fontSize = 10.sp,
                modifier = Modifier.padding(top = 2.dp)
            )
        }
        Text(
            if (connected) "点开看统计" else "重试中",
            color = Palette.faint,
            fontSize = 9.sp
        )
    }
}

@Composable
private fun StatsCard(total: Int, down: Int, up: Int, state: String) {
    Column(
        Modifier
            .fillMaxWidth()
            .background(Palette.panel, RoundedCornerShape(4.dp))
            .border(1.dp, Palette.border, RoundedCornerShape(4.dp))
            .padding(14.dp)
    ) {
        Text("通道统计", color = Palette.faint, fontSize = 9.sp, letterSpacing = 1.6.sp)
        Spacer(Modifier.height(8.dp))
        StatLine("连接状态", Notifier.stateText(state))
        StatLine("本次界面条数", "$total")
        StatLine("下行 / 上行", "$down / $up")
        StatLine("同步水位", "#${BridgeState.lastSeq.value}")
    }
}

@Composable
private fun StatLine(label: String, value: String) {
    Row(Modifier.fillMaxWidth().padding(vertical = 3.dp)) {
        Text(label, color = Palette.dim, fontSize = 11.sp)
        Spacer(Modifier.weight(1f))
        Text(value, color = Palette.ink, fontSize = 11.sp, fontFamily = FontFamily.Monospace)
    }
}

@Composable
private fun TimelineRow(item: ClipItem, onClick: () -> Unit) {
    val accent = if (item.outgoing) Palette.amber else Palette.teal
    Row(Modifier.fillMaxWidth().height(IntrinsicSize.Min)) {
        Box(Modifier.width(30.dp).fillMaxHeight()) {
            Box(
                Modifier
                    .align(Alignment.Center)
                    .width(1.dp)
                    .fillMaxHeight()
                    .background(Color(0xFF1E2C39))
            )
            Box(
                Modifier
                    .align(Alignment.TopCenter)
                    .padding(top = 9.dp)
                    .size(7.dp)
                    .background(accent.copy(alpha = 0.18f), CircleShape)
                    .border(1.dp, accent, CircleShape)
            )
        }
        Column(Modifier.weight(1f).padding(top = 4.dp, bottom = 8.dp)) {
            Text(
                clockFormat.format(Date(item.ts)) + if (item.outgoing) " · 发往电脑" else " · 来自电脑",
                color = Palette.faint,
                fontSize = 9.sp,
                fontFamily = FontFamily.Monospace,
                letterSpacing = 0.6.sp
            )
            Spacer(Modifier.height(4.dp))
            Row(
                Modifier
                    .fillMaxWidth()
                    .background(Palette.panel, RoundedCornerShape(3.dp))
                    .border(1.dp, Palette.border, RoundedCornerShape(3.dp))
                    .clickable { onClick() }
                    .height(IntrinsicSize.Min)
            ) {
                Box(Modifier.width(2.dp).fillMaxHeight().background(accent))
                Column(Modifier.padding(horizontal = 11.dp, vertical = 9.dp)) {
                    Text(
                        item.text,
                        color = Palette.ink,
                        fontSize = 12.sp,
                        lineHeight = 19.sp,
                        maxLines = 3,
                        overflow = TextOverflow.Ellipsis
                    )
                    Spacer(Modifier.height(6.dp))
                    Text(
                        when {
                            item.seq > 0 -> "#${item.seq} · ${item.text.length} 字"
                            item.text.contains('·') && (item.text.startsWith("文件") || item.text.startsWith("已发送") || item.text.startsWith("发送失败")) -> "文件"
                            else -> "已送达 · ${item.text.length} 字"
                        },
                        color = Palette.faint,
                        fontSize = 8.sp,
                        letterSpacing = 0.8.sp
                    )
                }
            }
        }
    }
}

@Composable
private fun FlowBar(down: Int, up: Int) {
    Row(
        Modifier
            .fillMaxWidth()
            .background(Palette.panel, RoundedCornerShape(4.dp))
            .border(1.dp, Palette.border, RoundedCornerShape(4.dp))
            .padding(horizontal = 12.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically
    ) {
        Text("下行", color = Palette.faint, fontSize = 9.sp, letterSpacing = 1.sp)
        Spacer(Modifier.width(8.dp))
        Box(Modifier.weight(1f).height(2.dp).background(Palette.teal.copy(alpha = 0.5f)))
        Spacer(Modifier.width(8.dp))
        Text("$down", color = Palette.teal, fontSize = 10.sp, fontFamily = FontFamily.Monospace)
        Spacer(Modifier.width(14.dp))
        Text("上行", color = Palette.faint, fontSize = 9.sp, letterSpacing = 1.sp)
        Spacer(Modifier.width(8.dp))
        Box(Modifier.weight(1f).height(2.dp).background(Palette.amber.copy(alpha = 0.5f)))
        Spacer(Modifier.width(8.dp))
        Text("$up", color = Palette.amber, fontSize = 10.sp, fontFamily = FontFamily.Monospace)
    }
}

@Composable
private fun OutlineAction(
    label: String,
    color: Color,
    modifier: Modifier = Modifier,
    onClick: () -> Unit
) {
    Box(
        modifier = modifier
            .height(42.dp)
            .border(1.dp, color, RoundedCornerShape(3.dp))
            .clickable { onClick() },
        contentAlignment = Alignment.Center
    ) {
        Text(label, color = color, fontSize = 12.sp, letterSpacing = 0.6.sp)
    }
}

@Composable
private fun SettingsDialog(
    defaultPeer: String,
    onClose: () -> Unit,
    onSave: (String, String) -> Unit
) {
    var peer by remember { mutableStateOf(defaultPeer) }
    var relay by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onClose,
        containerColor = Palette.panel,
        title = { Text("连接设置", color = Palette.ink, fontSize = 14.sp) },
        text = {
            Column {
                OutlinedTextField(
                    value = peer,
                    onValueChange = { peer = it },
                    label = { Text("电脑端 EndpointId") },
                    modifier = Modifier.fillMaxWidth(),
                    textStyle = androidx.compose.ui.text.TextStyle(
                        color = Palette.ink,
                        fontSize = 12.sp,
                        fontFamily = FontFamily.Monospace
                    )
                )
                Spacer(Modifier.height(10.dp))
                OutlinedTextField(
                    value = relay,
                    onValueChange = { relay = it },
                    label = { Text("Relay 地址（留空自动发现）") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                    textStyle = androidx.compose.ui.text.TextStyle(
                        color = Palette.ink,
                        fontSize = 12.sp
                    )
                )
            }
        },
        confirmButton = {
            TextButton(onClick = {
                if (peer.isNotBlank()) onSave(peer, relay)
            }) { Text("保存并连接", color = Palette.teal) }
        },
        dismissButton = {
            TextButton(onClick = onClose) { Text("取消", color = Palette.dim) }
        }
    )
}

@Composable
private fun DetailAction(label: String, onClick: () -> Unit) {
    Box(
        Modifier
            .height(32.dp)
            .border(1.dp, Palette.border, RoundedCornerShape(3.dp))
            .clickable { onClick() }
            .padding(horizontal = 12.dp),
        contentAlignment = Alignment.Center
    ) {
        Text(label, color = Palette.dim, fontSize = 11.sp)
    }
}

private fun readClipboard(ctx: Context): String? {
    val clip = ctx.getSystemService(ClipboardManager::class.java)?.primaryClip ?: return null
    if (clip.itemCount == 0) return null
    return clip.getItemAt(0).text?.toString()
}

private fun writeClipboard(ctx: Context, text: String) {
    val cm = ctx.getSystemService(ClipboardManager::class.java) ?: return
    cm.setPrimaryClip(ClipData.newPlainText("ClipBridge", text))
}

private fun toast(ctx: Context, msg: String) =
    Toast.makeText(ctx, msg, Toast.LENGTH_SHORT).show()

