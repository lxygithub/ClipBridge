package top.ieop.clipbridge

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
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
import androidx.compose.foundation.layout.statusBarsPadding
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
import androidx.compose.runtime.LaunchedEffect
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
import androidx.compose.foundation.layout.heightIn
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import java.io.ByteArrayOutputStream
import androidx.core.content.FileProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
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
        // M4：把上次的历史读回时间线（最多 200 条）
        BridgeState.attach(this)
        askNotificationPermission()

        // 已配对就顺手把常驻服务拉起来；只是建立连接，不会去读剪贴板
        if (prefs.isConfigured) SyncService.start(this)

        setContent {
            ChannelScreen(
                peerId = prefs.peerId,
                onSave = { peer, relay ->
                    val changed = peer.trim() != prefs.peerId
                    prefs.peerId = peer
                    prefs.relay = relay
                    prefs.rememberPeer(peer, "")
                    if (changed) SyncService.restart(this) else SyncService.start(this)
                }
            )
        }
    }

    /**
     * 打开 app 就是最强的恢复信号：进程要是刚被系统杀过，这时候链路一定是断的。
     * 不等 supervisor 那套退避，直接把会话拆了重建，抢在用户动手之前连上。
     * 15 秒内只做一次，避免来回切前台把连接反复掐断。
     */
    override fun onResume() {
        super.onResume()
        // 前台节奏：更勤的探活，并且立刻踢一次（2.5s 探不通就断开重连，不等一分钟）
        runCatching { Native.nativeSetForeground(true) }
        if (!prefs.isConfigured) return
        // 只有「很久很久没确认活着」才重建会话：重建要重开 endpoint，比让 supervisor
        // 直接重连慢得多（实测能差十几秒），所以常规快恢复交给上面的探活。
        val idle = runCatching { Native.nativeIdleMs() }.getOrDefault(-1L)
        if (idle < RECOVER_IDLE_MS) return
        val now = System.currentTimeMillis()
        if (now - prefs.lastRecover < RECOVER_GAP_MS) return
        prefs.lastRecover = now
        SyncService.restart(this)
    }

    override fun onPause() {
        super.onPause()
        // 后台把心跳放慢（20s、三次失败才断），省电优先
        runCatching { Native.nativeSetForeground(false) }
    }

    private companion object {
        /** 两次「进前台就重建会话」之间的最小间隔 */
        const val RECOVER_GAP_MS = 15_000L

        /** 超过这么久没确认过链路活着，才值得推翻重来（-1 表示还没连过） */
        const val RECOVER_IDLE_MS = 45_000L
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
    onSave: (String, String) -> Unit
) {
    val ctx = LocalContext.current
    val state by BridgeState.state.collectAsState()
    val items by BridgeState.items.collectAsState()
    val link by BridgeState.link.collectAsState()
    val files by BridgeState.files.collectAsState()
    val peerName by BridgeState.peerName.collectAsState()

    // 链路状态、对端名字和文件列表都来自 Rust 侧，2 秒拉一次（读 mutex/扫目录，开销可忽略）
    val prefs = remember { Prefs(ctx) }
    LaunchedEffect(Unit) {
        while (true) {
            withContext(Dispatchers.IO) {
                BridgeState.link.value = runCatching { Native.nativePathStatus() }.getOrDefault("")
                val name = runCatching { Native.nativePeerName() }.getOrDefault("")
                BridgeState.peerName.value = name
                // 连上以后把电脑名字记进配对表，设置页里就是「我的主机」而不是一串 hex
                if (name.isNotBlank() && peerId.isNotBlank()) prefs.rememberPeer(peerId, name)
                val raw = runCatching { Native.nativeListFiles() }.getOrDefault("[]")
                BridgeState.files.value = parseFiles(raw)
            }
            delay(2000)
        }
    }

    var settingsOpen by remember { mutableStateOf(false) }
    var selected by remember { mutableStateOf<ClipItem?>(null) }

    val connected = state == "connected"
    val accent = if (connected) Palette.teal else Color(0xFF8795A4)
    // 电脑的显示名：没收到过就用短 id 兜底
    val pcName = peerName.ifBlank { "PC · ${peerId.take(8)}…" }

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
            // 状态栏底下留出安全距离：原先标题贴着状态栏，字也太小
            .statusBarsPadding()
            .padding(horizontal = 18.dp)
    ) {
        Spacer(Modifier.height(10.dp))

        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                "CLIPBRIDGE / 通道",
                color = Palette.dim,
                fontSize = 12.sp,
                letterSpacing = 2.sp
            )
            Spacer(Modifier.weight(1f))
            // 设置按钮做成 48dp 热区：原来是 12sp 文本 + TextButton 的窄内边距，很难点中
            Box(
                modifier = Modifier
                    .size(48.dp)
                    .clickable { settingsOpen = true },
                contentAlignment = Alignment.Center
            ) {
                Text("设置", color = Palette.dim, fontSize = 13.sp)
            }
        }
        Spacer(Modifier.height(6.dp))

        NodeCard(
            connected = connected,
            title = pcName,
            state = state,
            accent = accent,
            link = link
        )

        if (files.isNotEmpty()) {
            Spacer(Modifier.height(12.dp))
            FilesSection(
                peerName = pcName,
                files = files,
                onShare = { shareFile(ctx, File(it.path)) },
                onOpen = { f ->
                    // 发出去的文件只留在电脑上，手机上点开没有意义
                    if (f.outgoing || f.path.isBlank()) {
                        toast(ctx, "这个文件已经在「$pcName」上")
                    } else {
                        openFile(ctx, File(f.path))
                    }
                }
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
                        "通道已建立，还没有条目。\n电脑窗口里粘贴或发文件才会到这里；\n连上后会自动拉一次电脑端历史。",
                        color = Palette.faint,
                        fontSize = 12.sp,
                        lineHeight = 22.sp
                    )
                }
            } else {
                LazyColumn(Modifier.fillMaxSize()) {
                    items(items, key = { "${it.seq}-${it.ts}" }) { item ->
                        TimelineRow(item, pcName) { selected = item }
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

        Spacer(Modifier.height(12.dp))

        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlineAction("发送当前剪贴板", Palette.teal, Modifier.weight(1f)) {
                // 先文字，没文字再看是不是一张图（手机截图后直接「复制图片」很常见）
                val text = readClipboard(ctx)
                if (!text.isNullOrBlank()) {
                    Thread {
                        val ok = runCatching { Native.nativeSendText(text) }.getOrDefault(false)
                        if (ok) BridgeState.markSent(text)
                        (ctx as? ComponentActivity)?.runOnUiThread {
                            toast(ctx, if (ok) "已发送到电脑" else "发送失败：未连接")
                        }
                    }.start()
                    return@OutlineAction
                }
                val bmp = readClipboardImage(ctx)
                if (bmp == null) {
                    toast(ctx, "剪贴板为空")
                    return@OutlineAction
                }
                Thread {
                    val png = bitmapToPng(bmp)
                    val ok = runCatching { Native.nativeSendImage(png) }.getOrDefault(false)
                    if (ok) {
                        // 发出去的图留一份本地副本，时间线里才有缩略图可看
                        val dir = File(ctx.filesDir, "sent_images").apply { mkdirs() }
                        val f = File(dir, "out_${System.currentTimeMillis()}.png")
                        runCatching { f.writeBytes(png) }
                        BridgeState.pushImage(f.absolutePath, bmp.width, bmp.height, outgoing = true)
                    }
                    (ctx as? ComponentActivity)?.runOnUiThread {
                        toast(
                            ctx,
                            if (ok) "已发送图片 ${bmp.width}×${bmp.height}"
                            else "发送失败：未连接"
                        )
                    }
                }.start()
            }
            OutlineAction("发文件", Palette.amber, Modifier.weight(1f)) {
                pickFile.launch(arrayOf("*/*"))
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
                    if (item.outgoing) "发往 $pcName" else "来自 $pcName",
                    color = Palette.ink,
                    fontSize = 14.sp
                )
            },
            text = {
                Column {
                    item.imagePath?.let { path ->
                        var bmp by remember(path) { mutableStateOf<Bitmap?>(null) }
                        LaunchedEffect(path) {
                            bmp = withContext(Dispatchers.IO) { BitmapFactory.decodeFile(path) }
                        }
                        bmp?.let { b ->
                            Image(
                                b.asImageBitmap(),
                                contentDescription = null,
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .heightIn(max = 300.dp),
                                contentScale = ContentScale.Fit
                            )
                            Spacer(Modifier.height(8.dp))
                        }
                    }
                    Text(item.text, color = Palette.ink, fontSize = 14.sp, lineHeight = 22.sp)
                    Spacer(Modifier.height(10.dp))
                    Text(
                        "${clockFormat.format(Date(item.ts))} · " +
                            if (item.imagePath != null) {
                                "${item.imageW}×${item.imageH} 像素" +
                                    if (item.seq > 0) " · #${item.seq}" else ""
                            } else {
                                "${item.text.length} 字" +
                                    if (item.seq > 0) " · #${item.seq}" else ""
                            },
                        color = Palette.faint,
                        fontSize = 10.sp
                    )
                    Spacer(Modifier.height(14.dp))
                    Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        DetailAction("发回电脑") {
                            val img = item.imagePath
                            Thread {
                                val ok = if (img != null) {
                                    val png = runCatching { File(img).readBytes() }.getOrNull()
                                    if (png == null) false
                                    else runCatching { Native.nativeSendImage(png) }.getOrDefault(false)
                                } else {
                                    runCatching { Native.nativeSendText(item.text) }.getOrDefault(false)
                                }
                                if (ok) {
                                    if (img != null) {
                                        BridgeState.pushImage(img, item.imageW, item.imageH, outgoing = true)
                                    } else {
                                        BridgeState.markSent(item.text)
                                    }
                                }
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
                val img = item.imagePath
                if (img != null) {
                    TextButton(onClick = {
                        openFile(ctx, File(img))
                        selected = null
                    }) { Text("用别的应用打开", color = Palette.teal) }
                } else {
                    TextButton(onClick = {
                        writeClipboard(ctx, item.text)
                        toast(ctx, "已复制到手机剪贴板")
                        selected = null
                    }) { Text("复制", color = Palette.teal) }
                }
            },
            dismissButton = {
                TextButton(onClick = { selected = null }) {
                    Text("关闭", color = Palette.dim)
                }
            }
        )
    }

    if (settingsOpen) {
        SettingsDialog(peerId, prefs.knownPeers(), onClose = { settingsOpen = false }) { peer, relay ->
            onSave(peer, relay)
            settingsOpen = false
        }
    }
}

@Composable
private fun NodeCard(
    connected: Boolean,
    title: String,
    state: String,
    accent: Color,
    link: String
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = 10.dp)
            .background(Palette.panel, RoundedCornerShape(6.dp))
            .border(1.dp, Palette.border, RoundedCornerShape(6.dp))
            .padding(horizontal = 14.dp, vertical = 13.dp),
        verticalAlignment = Alignment.CenterVertically
    ) {
        Box(Modifier.size(8.dp).background(accent, CircleShape))
        Spacer(Modifier.width(10.dp))
        Column(Modifier.weight(1f)) {
            Text(
                // 电脑的显示名（来自电脑的 HELLO）；没连上时 URL 里只有短 id 也不丑
                title,
                color = Palette.ink,
                fontSize = 14.sp,
                fontWeight = FontWeight.Medium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis
            )
            Text(
                if (connected) "已连接 · ${Notifier.stateText(state)}" else Notifier.stateText(state),
                color = Palette.dim,
                fontSize = 10.sp,
                modifier = Modifier.padding(top = 2.dp)
            )
        }
        PathChip(link)
    }
}

/// 链路徽标：直连/中继 + RTT；颜色按 60ms / 160ms 分档（与桌面端同一套语义）。
private data class LinkChip(val text: String, val fg: Color, val bg: Color)

private fun linkChip(link: String, connected: Boolean): LinkChip {
    if (!connected) {
        return LinkChip("未连接", Color(0xFFA49587), Color(0xFF2A2220))
    }
    if (link.isBlank()) {
        return LinkChip("连接中", Palette.dim, Color(0xFF16212B))
    }
    val parts = link.split('|')
    val direct = parts.getOrNull(0) == "direct"
    val ms = parts.getOrNull(1)?.toLongOrNull()
    val head = if (direct) "直连" else "中继"
    val tail = ms?.let { " · " + (if (it >= 1000) "%.1f s".format(it / 1000.0) else "$it ms") } ?: ""
    val fg = when {
        ms == null -> Palette.dim
        ms <= 60 -> Color(0xFF7BE3A6)
        ms <= 160 -> Color(0xFFF5B95B)
        else -> Color(0xFFFF9A9A)
    }
    val bg = when {
        ms == null -> Color(0xFF16212B)
        ms <= 60 -> Color(0xFF14432B)
        ms <= 160 -> Color(0xFF4A3212)
        else -> Color(0xFF4A1D1D)
    }
    return LinkChip(head + tail, fg, bg)
}

@Composable
private fun PathChip(link: String) {
    val chip = linkChip(link, connected = BridgeState.state.value == "connected")
    Box(
        Modifier
            .background(chip.bg, RoundedCornerShape(20.dp))
            .padding(horizontal = 10.dp, vertical = 5.dp)
    ) {
        Text(chip.text, color = chip.fg, fontSize = 11.sp, fontWeight = FontWeight.Medium)
    }
}

@Composable
private fun TimelineRow(item: ClipItem, peer: String, onClick: () -> Unit) {
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
                clockFormat.format(Date(item.ts)) +
                    if (item.outgoing) " · 发往 $peer" else " · 来自 $peer",
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
                    item.imagePath?.let { path ->
                        var bmp by remember(path) { mutableStateOf<Bitmap?>(null) }
                        LaunchedEffect(path) {
                            bmp = withContext(Dispatchers.IO) { BitmapFactory.decodeFile(path) }
                        }
                        bmp?.let { b ->
                            Image(
                                b.asImageBitmap(),
                                contentDescription = null,
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .heightIn(max = 150.dp),
                                contentScale = ContentScale.Fit
                            )
                            Spacer(Modifier.height(6.dp))
                        }
                    }
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
    peers: List<KnownPeer>,
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
                // M4 多机配对：配对过的电脑一键切，不必每次重新粘 64 位 id
                if (peers.isNotEmpty()) {
                    Text("已配对 · ${peers.size}", color = Palette.faint, fontSize = 9.sp, letterSpacing = 1.2.sp)
                    Spacer(Modifier.height(6.dp))
                    peers.forEach { p ->
                        val current = p.id == defaultPeer
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(vertical = 3.dp)
                                .background(Palette.void, RoundedCornerShape(3.dp))
                                .border(
                                    1.dp,
                                    if (current) Palette.teal else Palette.border,
                                    RoundedCornerShape(3.dp)
                                )
                                .clickable { peer = p.id }
                                .padding(horizontal = 10.dp, vertical = 8.dp),
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Text(p.label, color = if (current) Palette.teal else Palette.ink, fontSize = 12.sp)
                            Spacer(Modifier.weight(1f))
                            Text(
                                if (current) "当前" else p.id.take(8),
                                color = Palette.faint,
                                fontSize = 8.sp,
                                fontFamily = FontFamily.Monospace
                            )
                        }
                    }
                    Spacer(Modifier.height(12.dp))
                }
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
                // 保活：允许忽略电池优化，系统才不会在待机几分钟后把前台服务冻住
                Spacer(Modifier.height(14.dp))
                val ctx = LocalContext.current
                val ignoring = runCatching {
                    ctx.getSystemService(PowerManager::class.java)
                        ?.isIgnoringBatteryOptimizations(ctx.packageName) == true
                }.getOrDefault(false)
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .clickable { if (!ignoring) requestIgnoreBattery(ctx) }
                        .padding(vertical = 4.dp),
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    Text(
                        if (ignoring) "后台保活 · 已允许（忽略电池优化）" else "后台保活 · 未允许，点这里申请",
                        color = if (ignoring) Palette.teal else Palette.dim,
                        fontSize = 10.sp
                    )
                }
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

/// 文件区：收（inbox 里落地的）和发（发出去的流水）都列，行里标「发往 xxx / 来自 xxx」。
/// 收到的：点一下用系统播放器打开，长按分享；发出去的文件本体在电脑上，点了只提示位置。
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun FilesSection(
    peerName: String,
    files: List<RecvFile>,
    onShare: (RecvFile) -> Unit,
    onOpen: (RecvFile) -> Unit
) {
    Column {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                "文件 · ${files.size}",
                color = Palette.faint,
                fontSize = 9.sp,
                letterSpacing = 1.4.sp
            )
            Spacer(Modifier.weight(1f))
            Text(
                "收 ${files.count { !it.outgoing }} · 发 ${files.count { it.outgoing }}",
                color = Palette.faint,
                fontSize = 9.sp
            )
        }
        Spacer(Modifier.height(4.dp))
        files.take(6).forEach { f ->
            val accent = if (f.outgoing) Palette.amber else Palette.teal
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(vertical = 3.dp)
                    .background(Palette.panel, RoundedCornerShape(3.dp))
                    .border(1.dp, Palette.border, RoundedCornerShape(3.dp))
                    .combinedClickable(
                        onClick = { onOpen(f) },
                        onLongClick = { if (!f.outgoing && f.path.isNotBlank()) onShare(f) }
                    )
                    .height(IntrinsicSize.Min)
            ) {
                // 方向色条：和文本条目同一套语义（发=琥珀、收=青）
                Box(Modifier.width(2.dp).fillMaxHeight().background(accent))
                Column(Modifier.padding(horizontal = 11.dp, vertical = 9.dp)) {
                    Text(
                        f.name,
                        color = Palette.ink,
                        fontSize = 12.sp,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis
                    )
                    Spacer(Modifier.height(3.dp))
                    Text(
                        // 「发往 / 来自」带上设备名，才知道这份文件归谁
                        (if (f.outgoing) "发往 $peerName" else "来自 $peerName") +
                            " · " + humanSize(f.size),
                        color = Palette.faint,
                        fontSize = 9.sp,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis
                    )
                }
            }
        }
        if (files.size > 6) {
            Text(
                "还有 ${files.size - 6} 个文件",
                color = Palette.faint,
                fontSize = 9.sp,
                modifier = Modifier.padding(top = 4.dp, start = 2.dp)
            )
        }
    }
}

private fun parseFiles(raw: String): List<RecvFile> = runCatching {
    val arr = org.json.JSONArray(raw)
    (0 until arr.length()).map { i ->
        val o = arr.getJSONObject(i)
        RecvFile(
            name = o.optString("name"),
            size = o.optLong("size"),
            path = o.optString("path"),
            mtime = o.optLong("mtime"),
            outgoing = o.optBoolean("outgoing", false)
        )
    }
}.getOrDefault(emptyList())

private fun humanSize(n: Long): String = when {
    n < 1024 -> "$n B"
    n < 1024 * 1024 -> "%.1f KB".format(n / 1024.0)
    n < 1024L * 1024 * 1024 -> "%.1f MB".format(n / 1024.0 / 1024.0)
    else -> "%.2f GB".format(n / 1024.0 / 1024.0 / 1024.0)
}

private fun fileMime(file: File): String {
    val ext = file.extension.lowercase()
    return when (ext) {
        "jpg", "jpeg" -> "image/jpeg"
        "png" -> "image/png"
        "gif" -> "image/gif"
        "mp4" -> "video/mp4"
        "mp3" -> "audio/mpeg"
        "pdf" -> "application/pdf"
        "txt", "md", "log" -> "text/plain"
        "apk" -> "application/vnd.android.package-archive"
        "zip" -> "application/zip"
        else -> "application/octet-stream"
    }
}

/** 长按分享：把 app 私有目录里的文件通过 FileProvider 授权给别的 app。 */
private fun shareFile(ctx: Context, file: File) {
    runCatching {
        val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.files", file)
        val send = Intent(Intent.ACTION_SEND).apply {
            type = fileMime(file)
            putExtra(Intent.EXTRA_STREAM, uri)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        val chooser = Intent.createChooser(send, "分享 ${file.name}")
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        ctx.startActivity(chooser)
    }.onFailure { toast(ctx, "分享失败：${it.message}") }
}

private fun openFile(ctx: Context, file: File) {
    runCatching {
        val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.files", file)
        val view = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, fileMime(file))
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        ctx.startActivity(view)
    }.onFailure { toast(ctx, "没有能打开它的 app") }
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

/**
 * 剪贴板里的图：Android 10+ 复制图片走的是 content Uri（`ClipData.Item.getUri`），
 * 只有部分厂商会直接给 Bitmap，所以这里按 Uri 解码。
 */
private fun readClipboardImage(ctx: Context): Bitmap? {
    val clip = ctx.getSystemService(ClipboardManager::class.java)?.primaryClip ?: return null
    if (clip.itemCount == 0) return null
    val uri = clip.getItemAt(0).uri ?: return null
    return runCatching {
        ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it) }
    }.getOrNull()
}

/** 编成 PNG 上链路：长边压到 2000 以内，帧上限 16MB 兜得住。 */
private fun bitmapToPng(src: Bitmap): ByteArray {
    val max = 2000
    val long = maxOf(src.width, src.height)
    val bmp = if (long > max) {
        val s = max.toFloat() / long
        Bitmap.createScaledBitmap(
            src,
            (src.width * s).toInt().coerceAtLeast(1),
            (src.height * s).toInt().coerceAtLeast(1),
            true
        )
    } else src
    val out = ByteArrayOutputStream()
    bmp.compress(Bitmap.CompressFormat.PNG, 100, out)
    return out.toByteArray()
}

private fun toast(ctx: Context, msg: String) =
    Toast.makeText(ctx, msg, Toast.LENGTH_SHORT).show()

/**
 * 申请「忽略电池优化」：厂商的省电策略最容易杀掉的就是这种常驻前台服务，
 * 加了白名单之后待机几个小时也不会被冻住。老系统直接跳到电池优化列表页。
 */
private fun requestIgnoreBattery(ctx: Context) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return
    val pm = ctx.getSystemService(PowerManager::class.java)
    if (pm?.isIgnoringBatteryOptimizations(ctx.packageName) == true) return
    val direct = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS)
        .setData(Uri.parse("package:${ctx.packageName}"))
    runCatching { ctx.startActivity(direct) }.onFailure {
        runCatching {
            ctx.startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
        }
    }
}

