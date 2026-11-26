package top.ieop.clipbridge

import android.content.Intent
import android.os.Build
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import android.widget.Toast

/**
 * 快捷设置磁贴：下拉一点，把当前手机剪贴板发到电脑。
 *
 * 磁贴自身没有焦点，读不了剪贴板，所以点击后必须拉起 QuickSendActivity。
 */
class ClipTileService : TileService() {

    override fun onStartListening() {
        super.onStartListening()
        // TODO(M1): 根据连接状态更新磁贴
        qsTile?.apply {
            state = Tile.STATE_INACTIVE
            label = "发到电脑"
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) subtitle = "未配对"
            updateTile()
        }
    }

    override fun onClick() {
        super.onClick()
        val intent = Intent(this, QuickSendActivity::class.java).apply {
            action = QuickSendActivity.ACTION_SEND_CLIP
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startActivityAndCollapse(
                android.app.PendingIntent.getActivity(
                    this, 0, intent,
                    android.app.PendingIntent.FLAG_IMMUTABLE or android.app.PendingIntent.FLAG_UPDATE_CURRENT
                )
            )
        } else {
            @Suppress("DEPRECATION")
            startActivityAndCollapse(intent)
        }
    }
}
