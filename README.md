# ClipBridge

Windows ↔ Android 的剪贴板 / 文件直传工具。传输走 [iroh](https://iroh.computer)（QUIC + NAT 打洞 + 官方 relay 兜底），
**不经过任何 IM、不依赖厂商推送、不落公有云中转**。电脑是监听方，手机主动拨号，两端一条长连接常驻。

- 电脑端**只做手动发送**：粘贴一段文本 / 选一个文件 / 拖进窗口，才发给手机（不偷看你的剪贴板）
- 手机端四个入口：桌面快捷方式「发剪贴板」「发文件」、任意 app 的分享面板、快捷设置磁贴、App 内按钮
- 文本、文件都双向；文件带 `.part` 断点续传 + SHA256 校验 + 同名不覆盖
- 单列表双向时间线：文本行点一下即复制，文件行右侧有「打开所在目录」；行边框按方向着色，**发 = 琥珀 `#F5B95B`，收 = 青 `#7BD3C0`**（与手机端同一套语义）
- 两端都显示设备名：电脑标题下 `已连接 · 小米 15`，手机节点卡显示电脑主机名，列表行标 `发给 xxx / 来自 xxx`

## 当前能力

| 能力 | 状态 |
|---|---|
| 文本双向、离线补发（`seq` 水位线）、多设备广播 | ✅ |
| 电脑端自绘单窗口：粘贴/选择文件卡片（悬浮即收 `Ctrl+V`）、单列表、日志同窗翻页、复制提示 | ✅ |
| 手机端界面：节点卡（设备名 + 直连/中继徽标）、文件区（收发都列）、时间线；无统计卡/无上下行条 | ✅ |
| 设备名双向显示（HELLO 携带机型 / COMPUTERNAME，电脑连上后反向发 HELLO） | ✅ |
| 文件：电脑 → 手机（`outbox/` 投递即推、落盘、校验回包、进度/完成通知） | ✅ |
| 文件：手机 → 电脑（快捷方式选文件、分享任意文件、App 内「发文件」；发完在电脑 `inbox/`） | ✅ |
| 文件列表两端一致：电脑扫 `outbox/`、`outbox/sent/`、`inbox/`；手机列 `inbox/` + 已发流水 | ✅ |
| 传输可中断（删 outbox 源文件 / 手机通知「取消」）、单写者、续传 offset 对齐 | ✅ |
| Windows 常驻：托盘、`Ctrl+Alt+H`、开机自启、`>=5MB` 日志轮转 | ✅ |
| 手机常驻前台服务（`specialUse`，MIN 通知），20s 心跳探活 + 3s 退避重连 | ✅ |
| 直连/中继徽标（RTT 按 60/160ms 分档）+ `info --diag` NAT 诊断 | ✅ |
| 手机端历史持久化、多台电脑配对、图片剪贴板 | ⬜ |
| 自建 relay（跨网大文件只有几十 KB/s） | ⬜ 可选 |

## 架构

```
Android (SyncService + Rust core .so)  ──iroh/QUIC──►  Windows (clipbridge.exe serve)
        │  主动拨号，长连接常驻                          │  监听 4242 + 托盘 + 自绘窗口
        └─ 收到推送 → LOW 通知 → 点「复制」               └─ 手动粘贴/选文件 → 入队(seq) → 广播
```

- **控制面**：一条常驻双向流，帧格式 `[type u8][len u32 LE][payload]`
  - `HELLO 0x01`：`v1|<last_seq>|<device_name>` —— 手机用它补发 + 报名字；电脑连上后反向发 `v1|0|<COMPUTERNAME>` 让手机显示电脑名
  - `CLIP_TEXT 0x02` / `ACK 0x20` / `PING 0x21`
  - `FILE_OFFER 0x10` / `ACCEPT 0x11` / `REJECT 0x12` / `COMPLETE 0x13` / `PAUSE 0x14` / `RESUME 0x15` / `CANCEL 0x16` / `DATA 0x17`
- **发现**：`presets::N0`（`dns.iroh.link`），只凭 `EndpointId` 就能连；relay 区域会变，不写死
- **不丢**：电脑端每条文本落 `store/items/<seq>.txt`，手机 `HELLO` 带 `last_seq` 触发补发
- **不回推**：写剪贴板时用自实现 `OpenClipboard/SetClipboardData`，不参与自身发送循环

完整设计（需求矩阵、协议表、文件传输状态机、风险清单）见 [`docs/architecture.md`](docs/architecture.md)；
UI 方向稿见 [`design/design-demos/`](design/design-demos/)。

## 目录

```
core/          Rust lib：端点身份、协议帧、peer 解析、传输原语
windows/       Rust bin：clipbridge.exe（serve / 托盘 / 自绘窗口 / 队列）
android/       Kotlin app（top.ieop.clipbridge）+ Compose 界面
android/jni/   Rust cdylib：JNI 桥，产出 libclipbridge_android.so
dist/          双端打包产物（见下）
design/        UI 方向稿
docs/          架构文档
```

## 直接用打包好的产物

`dist/` 里就是当前代码的构建结果，免编译：

| 文件 | 说明 |
|---|---|
| `dist/clipbridge.exe` | Windows x64，单文件无运行时依赖，约 16 MB |
| `dist/clipbridge.apk` | Android arm64 debug 包，约 40 MB，覆盖安装用 `adb install -r` |

> debug 包带调试符号、体积偏大；正式自用不影响功能。注意 APK 换了签名/包名之外的任何东西都不用重配对，
> 但重新编译后 `EndpointId` 保持不变（身份在 `endpoint.key` 里，不在 APK 里）。

## 快速上手

1. 电脑端跑一次 `clipbridge.exe info --dir D:\ClipBridge`，抄下 `ID:`
   （想同时看打洞条件就加 `--diag`）
2. 电脑端常驻：`clipbridge.exe serve --dir D:\ClipBridge`（零参数也行，默认就是 serve）
3. 手机装 `dist/clipbridge.apk`，首次打开在「设置」里填电脑端 `ID`；Relay 留空＝官方默认地图自动挑最近
4. 电脑端：粘贴文本 / 点右半卡片选文件 / 直接把文件拖进窗口；`Ctrl+Alt+H` 打开主窗口
5. 手机端：长按桌面图标（发剪贴板 / 发文件）、任意 app 分享、下拉磁贴、或 App 内按钮

## 命令速查

```powershell
clipbridge.exe info  --dir D:\ClipBridge           # ID / relay / 已发布候选地址
clipbridge.exe info  --dir D:\ClipBridge --diag    # + NAT 类型、公网映射、relay 时延、结论
clipbridge.exe serve --dir D:\ClipBridge           # 常驻（托盘 + 热键），默认端口 4242
clipbridge.exe serve --dir D:\ClipBridge --headless --bind-ip 192.168.50.87 --port 4242
clipbridge.exe send-file --peer <ID> --file D:\path\to\big.zip
clipbridge.exe recv  --peer <ID>                   # 模拟手机端，观察广播与补发
clipbridge.exe autostart --off                     # 关开机自启（托盘菜单也能关）
```

数据目录 `D:\ClipBridge`：

```
store/items/<seq>.txt   文本队列（也是补发源）
inbox/                  收到的文件（电脑端落地）
outbox/                 投递即推；对端 COMPLETE 后才移到 outbox/sent/
outbox/sent/            已发送（仍会列在主界面里）
endpoint.key            设备身份（== EndpointId，丢了就要重配对）
serve.log               常驻日志（>5MB 轮转到 serve.log.1）
```

## 关于直连与中继

默认走官方 relay（`euc1-1` 北京实测 145ms，`usw1` 219ms）。打洞成功会新增一条 IP 路径并切过去，relay 退成兜底，
标题栏/节点卡上的徽标会从「中继」变成「直连」。

能不能直连取决于网络拓扑，不是代码开关：

| 场景 | 结果 |
|---|---|
| 同 Wi-Fi | 内网直连（曾实测 `direct(192.168.50.17:54839) rtt=6ms`） |
| 蜂窝 + 对称 NAT（`mapping_varies_v4=true`） | IPv4 打洞基本无解，只能中继 |
| 双端都有公网 IPv6 | IPv6 直连可用（本项目默认双栈 bind，`[::]:4242`） |
| PC 无公网 v6、手机在对称 NAT 后 | 必然中继，自建 relay 只降延迟不解决直连 |

排查入口：`serve.log` 里每次连接会打两组快照（`+2s` / `+8s`），带 `[peer+2s] 候选地址` 和 `[path+8s]` 实际路径。
打洞要 1–4 秒，只看建连瞬间的快照会误判成「对端没发布 IP」。

## 构建

### Windows

```powershell
cargo build --release -p clipbridge --offline   # 依赖已缓存时可离线
# 产物：target/release/clipbridge.exe
Copy-Item target\release\clipbridge.exe dist\clipbridge.exe -Force
```

`serve` 会在 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 登记开机自启，托盘菜单可关。

### Android

`.so` 不入库，需先编（arm64 为真机必需）：

```powershell
$env:ANDROID_NDK_HOME="D:\Sdk\ndk\27.0.12077973"
cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build -p clipbridge-android --release --offline
cd android
$env:JAVA_HOME="D:\Android\Android Studio\jbr"
.\gradlew.bat assembleDebug --offline
Copy-Item app\build\outputs\apk\debug\app-debug.apk ..\dist\clipbridge.apk -Force
```

Gradle 不读 Windows 系统代理，走代理时在 `android/gradle.properties` 里加
`systemProp.https.proxyHost=127.0.0.1` / `systemProp.https.proxyPort=7900`。

## 已知限制

- Android 10+ 后台读不到剪贴板，所以手机侧发送只能由用户动作触发（设计如此）。快捷方式的剪贴板入口要等窗口焦点，
  实现上是 `onWindowFocusChanged` 后读 + 150ms 重试，别改回 `onCreate`（那时 `primaryClip` 恒为 null）
- 快捷方式「发文件」需要一个可恢复的 Activity 承接文件选择器，**不能给 `QuickSendActivity` 加 `noHistory`**，
  否则跳去选择器时自己就被销毁，结果回不来
- 国产 ROM 可能杀后台，需要给「无省电限制 / 自启动」白名单
- 单条文本超过 64KB 不采集（控制面不塞大 payload）
- 未做额外鉴权：能连上就信任（`EndpointId` 即身份，白名单待做）
- 官方 relay 跨网带宽很低（北京↔手机实测 25–70 KB/s），打洞没成时大文件只能靠反复续传慢慢补齐（数据不会错，但慢）

## 开发

```powershell
cargo test                                   # core
cargo run -p clipbridge -- info --diag       # NAT / relay 体检
```

提交约定：一个功能一个 commit，信息写清「做了什么 + 为什么」；不改用户数据目录里的内容。

## 路线图

- [x] M1 文本双向 + Windows 常驻
- [x] M2 文件双向（`.part` 断点、SHA256、≤2GB、同名去重）
- [x] M3 双端 UI 改版（手动发送、单列表、日志同窗、设备名、方向配色）
- [ ] M4 手机端历史持久化、多台电脑配对
- [ ] M5 图片剪贴板（DIB → PNG）
- [ ] M6 自建 relay（可选，改善跨网带宽）

## License

MIT
