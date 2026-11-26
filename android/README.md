# Android 端

包名 `top.ieop.clipbridge`，minSdk 26 / targetSdk 35，UI 用 Compose。

## 组成

| 组件 | 作用 |
|---|---|
| `MainActivity` | 通道视图（方向 3）：电脑节点、时间线、条目详情、连接设置 |
| `SyncService` | 常驻前台服务（`specialUse` + MIN 通知），持有 iroh 长连接 |
| `QuickSendActivity` | 快捷方式 / 分享面板 / 磁贴的统一无 UI 发送入口 |
| `ClipTileService` | 快捷设置磁贴 |
| `CopyReceiver` | 通知「复制」按钮 → 写剪贴板 |
| `CopyTrampoline` | 写失败的兜底（NoDisplay Activity） |
| `Notifier` | 三条通道：`clip_text` LOW / `clip_file` LOW / `service_alive` MIN |
| `Native` / `BridgeListener` | JNI 桥，Rust 侧回调 `onClipText` / `onState` |
| `res/xml/shortcuts.xml` | 长按图标的两项快捷方式（发剪贴板 / 发文件） |

## 编译

```powershell
# 先在仓库根目录产出 .so（不入库）
cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build -p clipbridge-android --release

cd android
.\gradlew.bat assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

电脑端 ID 默认由 `build.gradle.kts` 的 `DEFAULT_PEER_ID` 烧进 APK（装完即配对）；
要换成另一台电脑，用 `-Pclipbridge.peer=<ID>` 重新打包，或在应用内「设置」里手填。

走系统代理时 Gradle 不读 WinINET 设置，需要在 `gradle.properties` 里显式写
`systemProp.https.proxyHost=127.0.0.1` / `systemProp.https.proxyPort=7900`。

## 剪贴板策略

- 只在 `QuickSendActivity`（快捷方式 / 分享 / 磁贴）里读剪贴板，后台不读（Android 10+ 也读不到）
- 收到文本**不自动写入**剪贴板，只发通知，用户点「复制」才写
- 打开主界面不自动上传剪贴板
