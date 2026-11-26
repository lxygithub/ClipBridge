# 剪贴板 / 文件跨端同步 —— 架构设计

> 平台：Windows（常驻） ↔ Android（常驻前台服务，可选省电降级）
> 传输：iroh（QUIC + NAT 打洞 + 官方 relay 中继）
> 标识：包名 `top.ieop.clipbridge`，Windows 可执行 `clipbridge.exe`
> 落盘：`D:\ClipBridge\inbox`
> 版本：v2（2026-11-26，需求已确认）

---

## 0. 已确认决策

| 项目 | 结论 |
|---|---|
| relay | **官方 relay**（`RelayMode::Default`），自建留作可选扩展 |
| 图片同步 | **纳入第一版** |
| 手机收到文本 | **不自动写剪贴板**，只发通知，用户点按钮 |
| 打开 app | **不自动上传剪贴板**（只保留快捷方式 / 分享面板触发） |
| 文件策略 | 不弹确认、自动接收；**可暂停 / 恢复 / 取消** |
| 单文件上限 | 2GB（软上限，仅做校验拒绝） |
| 多设备 | 支持，白名单内多台手机可同时连接 |
| 历史 | **全量保留**，提供管理功能（搜索/筛选/删除/清空） |
| Windows 自启 | 默认开启，托盘可关 |
| 传输限速 | 不做，交给 QUIC 拥塞控制 |
| 来源窗口标题 | **默认不采集**（隐私敏感，可选开关） |
| 包名 / exe | `top.ieop.clipbridge` / `clipbridge.exe` |
| 落盘目录 | `D:\ClipBridge\inbox`（设置里可改） |
| 分享面板接收 | **要做**：`ACTION_SEND` 文本 / 图片 / 文件，任意 app 内直接发 |
| 快捷设置磁贴 | **要做**：下拉一点即发当前剪贴板 |
| Windows 全局快捷键 | **要做**：默认 `Ctrl+Alt+H` 唤出历史窗口，可在设置改键 |

---

## 1. 需求与约束

| 需求 | 推导出的设计决策 |
|---|---|
| Windows 端监听剪贴板并同步到手机 | Windows 端必须是**用户会话内的托盘进程**，不能是 Windows Service（session 0 读不到交互式剪贴板） |
| 手机端常驻保活，要求即时同步 | **推送模型**：长连接常驻，Windows 主动下发；队列退化为断连兜底 |
| 手机→PC 的剪贴板同步仍受限 | Android 10+ 后台读不到剪贴板，必须有一个获得焦点的界面存在 |
| 手机→PC 要不打开 app | 桌面图标长按快捷方式 → 透明 Activity 读剪贴板即发，用户无感 |
| 手机端不装第三方软件 | 网络栈内嵌在自己 app 里，iroh Rust core 编成 `.so`，零外部依赖 |
| 文本通知可直接复制，不打开 app | 通知 action → `BroadcastReceiver` 直接写剪贴板，失败用 `Theme.NoDisplay` Activity 兜底 |
| 服务端架在 Windows | Windows 持有 iroh Endpoint 长期在线；relay 只做地址发布与打洞失败时的转发 |
| 不用 IM、不用厂商推送 | 不接任何第三方消息通道；`PushTransport` 抽象保留，未来可插厂商 SDK |

**明确不做**：厂商推送（企业资质门槛）、IM、公有云网盘中转、iOS、传输限速。

### 1.1 同步模型：推送优先，队列兜底

- **常态**：前台服务维持一条 iroh 长连接，Windows 捕获剪贴板后立即下发，手机秒级收到通知
- **队列角色**：不再是"打开 app 才拉"的主路径，只在断连、重启、进程被冻结期间兜底；重连后按 `last_seq` 补齐
- **单向即时**：PC→手机 是推送；手机→PC 的剪贴板靠桌面长按快捷方式 / 分享面板触发，用户不必进入 app 主界面

---

## 2. 总体架构

```
┌───────────────────────────┐                        ┌──────────────────────────────┐
│ Android                   │   QUIC（双向多流）      │ Windows                      │
│ ┌───────────────────────┐ │ ◄──── 打洞直连 ──────► │ ┌──────────────────────────┐ │
│ │ Kotlin + Compose UI   │ │                        │ │ tray-ui（托盘/配对/设置） │ │
│ ├───────────────────────┤ │ ◄──── 打洞失败 ──────► │ ├──────────────────────────┤ │
│ │ Rust core (.so)       │ │      走 relay          │ │ clip-watcher（Win32 剪贴板）│ │
│ │  · endpoint / 协议     │ │                        │ ├──────────────────────────┤ │
│ │  · 队列 / 文件         │ │                        │ │ core（Endpoint/协议/队列） │ │
│ └───────────────────────┘ │                        │ ├──────────────────────────┤ │
│ ┌───────────────────────┐ │                        │ │ SQLite（items/peers/xfer）│ │
│ │ FGS 实时模式 / WM 省电 │ │                        │ │ D:\ClipBridge\inbox      │ │
│ └───────────────────────┘ │                        │ └──────────────────────────┘ │
└─────────────┬─────────────┘                        └───────────────┬──────────────┘
              │                                                      │
              └────────────► iroh relay（官方）◄──────────────────────┘
                    （地址发布 + 打洞失败时转发，只处理 TLS 密文）
```

要点：

- **双方都是 peer**，Windows 不开监听端口、不需要公网 IP
- 连接方向固定为「手机发起」；Windows 只负责 accept + 主动 push
- Windows 的 endpoint 常驻在线，手机冷启动即可凭 `EndpointId` 直连
- 多设备：Windows 同时持有 N 条连接（白名单内每台手机一条），互不干扰
- 数据面与控制面分离：控制面一条长驻流，每个文件一条独立流

---

## 3. 组件划分

### 3.1 仓库结构

```
transformer/
├── core/          Rust lib：endpoint、协议、队列、文件传输、配对
├── windows/       Rust bin：clipbridge.exe（托盘 + 剪贴板 + 历史窗口）
├── android/       Kotlin app（top.ieop.clipbridge）+ JNI/uniffi 绑定
├── relay/         可选：自建 iroh-relay 部署配置（默认不用）
└── docs/
```

### 3.2 Windows 端（单进程单 exe）

| 模块 | 职责 |
|---|---|
| `clip-watcher` | `AddClipboardFormatListener` + 隐藏窗口消息循环，处理 `WM_CLIPBOARDUPDATE`；取 `CF_UNICODETEXT` / `CF_DIB` / `CF_HDROP` |
| `core` | iroh Endpoint、控制面/数据面、白名单、多设备连接管理、待发队列 |
| `store` | SQLite（rusqlite）：items / peers / transfers / settings |
| `tray-ui` | 托盘菜单、配对二维码、目标目录、自启开关、**历史窗口** |
| `autostart` | `HKCU\...\Run` 注册 `clipbridge.exe`，默认开启 |
| `hotkey` | `RegisterHotKey` 注册全局热键（默认 `Ctrl+Alt+H`），在托盘消息循环线程处理 `WM_HOTKEY` |

三块同进程，用 tokio channel 通信，不做 IPC 拆分。

### 3.3 Android 端

| 模块 | 职责 |
|---|---|
| `App` | `Application.onCreate` 后台线程初始化 core（bind endpoint 需 1~3s） |
| `MainActivity` | 连接状态、待复制列表、**历史管理**、发送文件、配对扫码 |
| `ClipboardGateway` | `onResume` 读剪贴板（Android 10+ 仅前台可读）；写入走统一入口 |
| `SyncService`（默认） | `startForeground` type=`specialUse`，持有连接、收消息、发通知 |
| `SyncWorker`（省电降级） | `WorkManager` 15min 周期探活 |
| `CopyReceiver` | 静态注册，通知按钮 → 写剪贴板 → 回 ACK |
| `CopyTrampoline` | `Theme.NoDisplay` Activity，仅当 receiver 写失败时兜底 |
| `QuickSendActivity` | **统一的无 UI 发送入口**：桌面快捷方式 / 分享面板 / 磁贴三类 Intent 都落到这里 |
| `ClipTileService` | 快捷设置磁贴：点击即发当前剪贴板（内部仍走 `QuickSendActivity` 以拿到焦点） |
| `shortcuts.xml` | 静态快捷方式：发送剪贴板 / 发送文件（未配对时 `disableShortcuts`） |

---

## 4. 技术选型

| 选型点 | 方案 | 理由 |
|---|---|---|
| 传输 | **iroh 1.0.3** | 已有原型验证；QUIC + 打洞 + relay + 身份一体 |
| 设备身份 | iroh `SecretKey` → `EndpointId` | 天然设备指纹，无需自建账号体系 |
| 地址发现 | 二维码传 `EndpointId` + relay 提示（`presets::Minimal`） | 不依赖第三方发现服务 |
| 剪贴板（Win） | Win32 `AddClipboardFormatListener` | 比 `WinRT Clipboard` 少一层依赖 |
| 图片转换 | `image` crate（DIB → PNG） | 纯 Rust，无系统依赖 |
| 存储 | SQLite | 单文件、事务可靠、支持 FTS |
| Android UI | Compose | 你的既有技术栈 |
| Android↔Rust | uniffi 优先，退化为 JNI | uniffi 省样板代码 |
| Relay | 官方 relay | 免运维，加密流量 |

---

## 5. 协议设计

### 5.1 帧格式

```
frame = [type u8][payload_len u32 LE][payload]
```

- **控制面**：一条长驻双向流，payload 为 UTF-8 JSON
- **数据面**：每个文件一条新双向流，流首 16 字节 transfer id，其后为裸字节流

ALPN：`clipbridge/1`

### 5.2 消息类型

| type | 名称 | 方向 | payload |
|---|---|---|---|
| `0x01` | `HELLO` | 双向 | `{ver, caps, last_seq, device_name}` |
| `0x02` | `CLIP_TEXT` | W→A | `{seq, ts, text}`（≤64KB） |
| `0x03` | `CLIP_IMAGE_META` | W→A | `{seq, ts, size, sha256, w, h}`，随后开数据流 |
| `0x04` | `CLIP_FILES` | W→A | `{seq, ts, names[]}`，只提示，不自动传 |
| `0x10` | `FILE_OFFER` | 双向 | `{id, name, size, sha256, mime}` |
| `0x11` | `FILE_ACCEPT` | 双向 | `{id, offset}`，offset 支持断点续传 |
| `0x12` | `FILE_REJECT` | 双向 | `{id, reason}` |
| `0x13` | `FILE_COMPLETE` | 双向 | `{id, ok, sha256}` |
| `0x14` | `FILE_PAUSE` | 双向 | `{id, offset}`，接收方落盘后停写 |
| `0x15` | `FILE_RESUME` | 双向 | `{id, offset}`，新开流从 offset 续传 |
| `0x16` | `FILE_CANCEL` | 双向 | `{id}`，双方删 `.part` 并清理状态 |
| `0x17` | `FILE_DATA` | 双向 | 数据面流首帧，payload = transfer id 的 hex（32 字符），其后是裸文件字节。M2 实现时补的编号，不改动 0x01–0x16 |
| `0x20` | `ACK` | 双向 | `{seq}` / `{id, ok}` |
| `0x21` | `PING` / `PONG` | 双向 | 空 |
| `0x30` | `SYNC_REQ` | A→W | `{since_seq}`，请求补发 |
| `0x31` | `SYNC_END` | W→A | `{last_seq}` |

**关键约束**

- 控制面严禁塞大 payload；超过 64KB 的文本 Windows 侧不采集
- 文件不做自研分块：QUIC 流自带流控与重传，直接 `copy`；断点靠 offset + 新流
- 单文件 ≤ 2GB，`FILE_OFFER` 阶段校验，超限直接 `REJECT`
- 每个文件 SHA256 校验，失败重试一次
- 多设备时 `seq` 全局单调，`ACK` 按设备粒度假推进

### 5.3 文件传输状态机

```
OFFER → ACCEPTED → TRANSFERRING ⇄ PAUSED
                      │              │
                      ├→ COMPLETED   └→ CANCELED
                      └→ FAILED（校验失败，可重试一次）
```

- 暂停时：发送方停写但**保留流**最多 10 分钟，期间恢复可原地续传
- 超过 10 分钟或连接断开：转 `PAUSED` 持久化状态，恢复时新开流从 `offset` 续传
- 落盘统一先写 `name.part`，`FILE_COMPLETE` 校验通过后 `rename` 为正式名
- 取消时删除 `.part`，并把 `offset` 记录清空

### 5.4 时序

**配对（一次性）**

```
Windows 托盘生成二维码 {endpoint_id, relay_url, one_time_token}
→ 手机扫码 → 建连 → HELLO(token) → Windows 校验并写入白名单 → 手机存下 endpoint_id
```

**文本下行（常态）**

```
clip-watcher 捕获 → 400ms 防抖 + hash 去重 → 入 items(seq)
→ 控制面 CLIP_TEXT → 手机 SyncService → 发通知
→ 用户点「复制」→ CopyReceiver 写剪贴板 → ACK 回传 → 该设备水位线推进
```

**手机上传文件**

```
选文件 → FILE_OFFER → Windows 校验大小/空间 → FILE_ACCEPT
→ 手机开数据流 → Windows 流式写 .part → SHA256 校验 → rename → FILE_COMPLETE → ACK
```

**断连补发**

```
手机连上 → HELLO(last_seq) → 控制面逐条推 seq > last_seq → 手机 ACK 逐条推进
```

补发只在连接恢复时触发，日常路径不经过它。

---

## 6. 数据模型（Windows SQLite）

```sql
CREATE TABLE peers (
  device_id   TEXT PRIMARY KEY,   -- EndpointId
  name        TEXT,
  paired_at   INTEGER,
  last_seen   INTEGER,
  acked_seq   INTEGER DEFAULT 0   -- 该设备已确认到的水位线
);

CREATE TABLE items (              -- 全量保留，不自动淘汰
  seq          INTEGER PRIMARY KEY AUTOINCREMENT,
  kind         TEXT,              -- text | image | files
  text         TEXT,
  blob_path    TEXT,
  size         INTEGER,
  sha256       TEXT,
  source_win   TEXT,              -- 来源窗口标题，默认 NULL
  created_at   INTEGER,
  deleted_at   INTEGER            -- 软删除，NULL 表示有效
);

CREATE TABLE transfers (          -- 可续传的文件传输
  id           TEXT PRIMARY KEY,
  peer_id      TEXT,
  direction    TEXT,              -- up | down
  name         TEXT,
  size         INTEGER,
  offset       INTEGER DEFAULT 0,
  sha256       TEXT,
  part_path    TEXT,
  state        TEXT,              -- offering|transferring|paused|completed|canceled|failed
  updated_at   INTEGER
);

CREATE TABLE settings (k TEXT PRIMARY KEY, v TEXT);
```

**历史保留策略**：全量保留、软删除。UI 显示当前占用大小，并提供「清理 30 天前」按钮（需二次确认）。

---

## 7. 剪贴板交互细节

### 7.1 Windows

1. 只注册一次 listener，`WM_CLIPBOARDUPDATE` 到达后 400ms 防抖
2. 取内容优先级：`CF_HDROP` > `CF_DIB` > `CF_UNICODETEXT`
3. **跳过敏感标记**：含 `ExcludeClipboardContentFromMonitorProcessing` 或 `CanIncludeInClipboardHistory=0` 的内容不采集（密码管理器会打这两个标记）
4. **防回环**：自己写剪贴板时记录 `hash + 时间窗(2s)`，窗口内同 hash 事件丢弃
5. 文本 ≤ 64KB、图片 ≤ 50MB，超过不采集

### 7.2 来源窗口标题（可选，默认关）

指复制内容那一刻**前台窗口的标题**，例如浏览器标签名、Word 文档名、终端里的命令。采集方式：捕获剪贴板时立即 `GetForegroundWindow()` + `GetWindowTextW()`。

风险很实际：窗口标题经常包含文件名、URL、客户名，属于敏感信息，且会跟着通知推到手机上。因此**默认关闭**，仅在设置里给出开关，开启后 `items.source_win` 才会写值。

### 7.3 Android

- **读**：只在 `onResume` 与快捷方式入口的透明 Activity 里读，后台不读
- **写**：通知按钮直接写；`setPrimaryClip` 在 13+ 会弹系统提示，无法规避
- **兜底**：receiver 写失败（部分国产 ROM）时改指 `Theme.NoDisplay` Activity
- **不自动写**：收到文本只发通知，等用户点「复制」
- **回环**：手机写剪贴板时记录 hash，避免 `onResume` 把 PC 刚推来的内容再传回去

### 7.4 手机→PC 的入口（都不打开 app）

| 入口 | 交互 | 出现什么界面 | 定位 |
|---|---|---|---|
| 桌面图标长按 → 发送剪贴板 | 长按 → 点一项 | 透明 Activity 闪一下 | **主入口** |
| 桌面图标长按 → 发送文件 | 长按 → 点一项 → 系统选择器 | 只有系统选择器 | 传文件 |
| 分享面板（文本） | 任意 app 内「分享 → 发到电脑」 | 透明 Activity 闪一下 | 最常用 |
| 分享面板（图片/文件） | 相册/文件管理器内分享 | 同上 | 传内容 |
| 快捷设置磁贴 | 下拉 → 点磁贴 | 透明 Activity 闪一下 | 高频快发 |
| 常驻通知按钮 | 下拉 → 点 | 无（不读剪贴板时不需焦点） | 需通知为 LOW |

**统一入口：`QuickSendActivity`**

三类入口都只是 Intent 不同，落到同一个透明 Activity 处理，避免三套逻辑：

```xml
<!-- AndroidManifest -->
<activity android:name=".QuickSendActivity"
    android:theme="@style/Theme.Transparent"
    android:excludeFromRecents="true"
    android:noHistory="true"
    android:taskAffinity=""
    android:exported="true">

    <!-- 桌面快捷方式 / 磁贴 -->
    <intent-filter>
        <action android:name="top.ieop.clipbridge.SEND_CLIP"/>
    </intent-filter>

    <!-- 分享面板：文本 -->
    <intent-filter>
        <action android:name="android.intent.action.SEND"/>
        <category android:name="android.intent.category.DEFAULT"/>
        <data android:mimeType="text/plain"/>
    </intent-filter>

    <!-- 分享面板：单个文件（含图片） -->
    <intent-filter>
        <action android:name="android.intent.action.SEND"/>
        <category android:name="android.intent.category.DEFAULT"/>
        <data android:mimeType="*/*"/>
    </intent-filter>

    <!-- 分享面板：多文件 -->
    <intent-filter>
        <action android:name="android.intent.action.SEND_MULTIPLE"/>
        <category android:name="android.intent.category.DEFAULT"/>
        <data android:mimeType="*/*"/>
    </intent-filter>
</activity>
```

处理顺序：

1. `ACTION_SEND` + `EXTRA_TEXT` → 直接当文本入队
2. `ACTION_SEND` + `EXTRA_STREAM` → 用 `ContentResolver.openInputStream(uri)` **流式上传**，不先复制到私有目录（大视频复制一遍既慢又占空间）
3. `ACTION_SEND_MULTIPLE` + `EXTRA_STREAM`（`ArrayList<Uri>`）→ 逐个排队
4. 自定义 action `SEND_CLIP` → 读剪贴板

**为什么必须有一个 Activity**：Android 10+ 要求应用聚焦才能读剪贴板，后台/服务都读不到。分享面板的 `EXTRA_TEXT` 虽然不涉及剪贴板，但为了统一处理与"发完即走"的体验，也走同一个 Activity。

实现要点：

1. `Theme.Transparent` + `windowIsTranslucent` + `windowNoTitle`，尺寸 1dp
2. `excludeFromRecents` / `noHistory` / `taskAffinity=""`
3. 复用 `SyncService` 的 core 实例，不重新建连
4. 800ms 内没连上 → 入待发队列，Toast「已排队，连上自动发送」
5. 多设备时：只有一个已配对设备直接发；多设备弹一个底部选择条（仍不进入主界面）
6. 部分 ROM 需「允许后台弹出界面」权限，写进引导页

**分享面板的呈现细节**

- 应用名在分享列表里显示为「发到电脑」，靠 `android:label` 设置，不要用完整产品名
- 图片分享走 `SEND` + `image/*`，Windows 侧落盘为原图，不做压缩
- 第三方 app 给的 Uri 权限是临时的，必须在 `onCreate` 内立即开始读取，不能延后到队列延迟发送——所以**先落盘到临时 .part 或建立传输任务再 finish**

**快捷设置磁贴**

```kotlin
class ClipTileService : TileService() {
    override fun onClick() {
        // 磁贴自身是后台状态，读剪贴板仍需焦点 → 拉起 QuickSendActivity
        startActivityAndCollapse(pendingIntentToQuickSend())
    }
}
```

- Manifest 声明 `android:exported="true"` + `android.service.quicksettings.action.QS_TILE`
- `onStartListening` 时根据连接状态 `updateTile()`：已连接显示设备名，未连接显示「未配对」并置灰
- 长按磁贴默认进系统设置，可在 app 内提供「添加磁贴」引导

**快捷方式状态管理**

- 桌面最多展示 4 个（含静态），只放「发剪贴板」「发文件」两个
- 未配对时 `disableShortcuts(ids, "先与电脑配对")`，配对后恢复
- 图标提供自适应图标，`rank=0/1` 固定顺序

### 7.5 Windows 全局快捷键

- 默认 `Ctrl+Alt+H` → 唤出并聚焦历史窗口；设置里可改键
- 用 `RegisterHotKey(hwnd, id, MOD_CONTROL|MOD_ALT, 'H')`，必须注册在**托盘消息循环所在线程**，`WM_HOTKEY` 也从该窗口过程收
- 返回 `ERROR_HOTKEY_ALREADY_REGISTERED`（热键被其他程序占用）时：不静默失败，托盘提示「Ctrl+Alt+H 已被占用，请换一组」，并引导进设置
- 键位存 `settings` 表，启动时注册、退出时 `UnregisterHotKey`
- 附加一个可选热键 `Ctrl+Alt+Shift+H`：把当前剪贴板立刻重推到所有在线设备（用于自动推送被关掉或设备离线重连的场景）

---

## 8. 通知设计

| 渠道 id | importance | 用途 |
|---|---|---|
| `clip_text` | `LOW`（无声、状态栏有图标） | 文本到达 |
| `clip_file` | `LOW` | 文件到达 / 传输进度 |
| `service_alive` | `MIN`（静默折叠） | 前台服务常驻通知 |

- 文本通知：`BigTextStyle` + actions `[复制] [忽略]`；同设备 `setGroup` 合并
- 图片：`BigPictureStyle` 缩略图，点击进 app 查看/保存
- 文件：进度通知带 `[暂停] [取消]`，完成后变「已保存到 D:\ClipBridge\inbox」
- **importance 创建后不可由代码修改**，首次上线就要定对
- 节流在 Windows 端做（防抖 + 去重 + 连续短文本合并）

---

## 9. 可靠性与保活

| 机制 | 说明 |
|---|---|
| 心跳 | QUIC `keep_alive_interval` 30s；空闲断开阈值 120s |
| 网络切换 | `ConnectivityManager.NetworkCallback` → 立即重建连接 |
| 重连退避 | 1s → 2s → 4s … 上限 30s |
| Doze 兜底 | `WorkManager` 15min 周期探活（省电模式主路径） |
| 传输韧性 | 断线后 `transfers` 保留 offset，重连自动续传 |
| 用户引导 | 自启动、电池优化白名单、后台锁定、允许后台弹窗 |
| Windows 侧 | endpoint 常驻；异常退出由托盘守护/开机自启恢复 |

默认实时模式，省电模式作为降级开关保留：

- **实时模式（默认）**：前台服务常驻，秒级送达，代价是常驻通知 + 1~3%/天耗电
- **省电模式（可选）**：无常驻通知，打开 app 时同步 + 15min 粒度探活

---

## 10. 多设备支持

- 白名单表（`peers`）支持多台手机；Windows 同时维持多条连接，每条独立控制面
- `seq` 全局单调递增，`acked_seq` 每设备独立，互不影响
- 推送时遍历在线设备：在线即时发，离线等下次 `HELLO` 补齐
- 手机端首页显示已配对电脑列表、当前连接状态、各自历史
- 同一内容会广播给所有在线设备（剪贴板语义如此），文件只发给发起方

---

## 11. 历史管理

**Windows 端**：托盘菜单 → 历史窗口

- 列表：时间、类型、内容预览、来源设备
- 操作：复制到剪贴板、打开落盘文件、删除单条、按设备/类型筛选、清空
- 顶部显示存储占用，提供「清理 30 天前」

**Android 端**：历史页

- 搜索（`items.text` 做 FTS 可选）、按类型/设备筛选
- 单条：复制、重新发送、删除
- 批量：多选删除、清空（二次确认）
- 图片以缩略图网格展示，点击全屏

删除走软删除（`deleted_at`），物理清理由「清理」操作统一执行。

---

## 12. 安全与隐私

- 身份：iroh `SecretKey`；Windows 存 DPAPI/凭据管理器，Android 存 Keystore
- 配对：一次性 token 二维码；白名单外 `EndpointId` 直接拒绝握手
- 传输：QUIC + TLS 1.3，relay 只见密文
- 本地：文本不写日志；blob 落可配置目录；退出可一键清空
- 来源窗口标题默认不采集
- 敏感过滤：可选「长文本/含关键词不自动推送」，改为手动确认

---

## 13. 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| M0 | 改造原型为常驻 serve，手填 ID，CLI 双端互发 | 两端稳定互连 10 分钟不断 |
| M1 | Windows 剪贴板监听（文本 + 图片）+ Android 最小 app | 复制→通知→复制回手机剪贴板闭环，图片可看 |
| M2 | items 队列 + seq/ACK + 多设备连接 + 断线重连 | 两台手机同时在线，各自不漏不重 |
| M3 | 文件双向传输 + 暂停/恢复/取消 + 断点续传 | 2GB 文件中断后可续，SHA256 一致 |
| M4 | 二维码配对 + 白名单 + 托盘 UI + 桌面快捷方式 + 分享面板 + 磁贴 | 长按/分享/磁贴三种入口都能发，未配对设备无法连接 |
| M5 | 前台服务实时模式 + 通知动作（复制/暂停/取消） | 主流国产 ROM 白名单下 24h 不掉线 |
| M6 | 双端历史管理（搜索/筛选/删除/清空）+ Windows 全局热键 | 万条历史下列表不卡，`Ctrl+Alt+H` 唤出窗口 |
| M7（可选） | 自建 relay 替换官方 | 打洞失败场景仍可传文本 |

---

## 14. 风险清单

| 风险 | 影响 | 应对 |
|---|---|---|
| 国产 ROM 杀前台服务 | 默认模式失效，退化为补发 | 白名单引导 + WorkManager 兜底 + 省电模式 |
| UAC 提权窗口的剪贴板不可见 | 管理员程序里的复制收不到 | 系统行为，文档说明，无法规避 |
| iroh 在 Android 的绑定成熟度 | M1 卡壳 | 先官方 FFI，不行手写 JNI |
| iroh 冷启动 1~3s | 打开 app 后要等 | `Application.onCreate` 预 bind，UI 显示「连接中」 |
| 大文件内存 | 崩溃 | 全程流式，禁止 `fs::read` 整文件 |
| 中文文件名 | 落盘乱码 | 统一 UTF-8，Windows 用宽字符 API |
| 后台写剪贴板被 ROM 拦截 | 通知按钮失效 | `NoDisplay` Activity 兜底 |
| 历史全量保留 | 磁盘无限增长 | 显示占用 + 一键清理 30 天前 |
| 2GB 文件 + 手机端后台被杀 | 传输中断 | offset 持久化，重连自动续传 |

---

## 15. 明确不做的扩展

- 自建 relay：官方 relay 已满足需求，除非官方不可用再评估（原 M7 保留为可选项）
- 厂商推送：资质门槛 + 内容过第三方
- iOS 端、跨平台 Web 端
- 传输限速、断点加密分片等进阶特性

---

## 16. M1 实现修订（2026-11-26 实测后回填）

实现过程中偏离原设计的点，以及原因：

| 项 | 原设计 | 实际实现 | 原因 |
|---|---|---|---|
| 地址发现 | 手填 `relay` URL | `presets::N0`（n0 DNS/pkarr 发现），relay 可留空 | 实测同一台机器拿到过 `euc1` 和 `usw1` 两个 relay 区域，写死地址必然对不上；交给发现服务解析对端当前家庭 relay |
| 队列存储 | SQLite | 每条文本一个文件 + `seq` 水位线（`items/<seq:012>.txt`） | M1 只需要"断连不丢 + 能按 seq 补发"，目录扫描足够；SQLite 留给文件传输阶段 |
| 托盘/热键/历史 | 排在 M4/M6 | 与 M1 一起做 | 没有托盘的常驻进程无法停止/查看，用户实际不可用 |
| GUI 栈 | 未指定 | 纯 Win32 内置控件类（EDIT/LISTBOX/BUTTON/STATIC） | 单文件 exe、无运行时依赖；托盘与热键本来就要 Win32 消息循环，一体化后不需要事件循环适配 |
| 自写回环 | 未提及 | `SELF_WRITE_HASH` 抑制：历史窗口「复制」写入剪贴板后，轮询不再把它当新内容回推 | 否则 PC 上查历史会反复给手机推送同一条 |
| 开机自启 | 待定 | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，`clipbridge autostart --off` 关闭 | 无需管理员权限，符合"用户会话内托盘进程"的定位 |
| 手机端 relay 字段 | 必填 | 可留空 | 与发现服务配套；老手填地址仍兼容 |

M1 已完成 / 未完成的准确清单见 `README.md`。当前尚未实现：文件传输、图片剪贴板、手机端历史页、手机配多台电脑。
