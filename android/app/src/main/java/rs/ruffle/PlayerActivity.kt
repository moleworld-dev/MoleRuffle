package rs.ruffle

import android.annotation.SuppressLint
import android.app.ActivityManager
import android.content.Context
import android.content.Intent
import android.content.res.Configuration
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.Uri
import android.os.Build
import android.os.Build.VERSION_CODES
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import android.view.Menu
import android.view.MenuItem
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.view.Window
import android.view.WindowManager
import android.widget.Button
import android.widget.PopupMenu
import android.widget.TextView
import androidx.constraintlayout.widget.ConstraintLayout
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.google.androidgamesdk.GameActivity
import java.io.DataInputStream
import java.io.File
import java.io.IOException

class PlayerActivity : GameActivity() {
    @Suppress("unused")
    // Used by Rust
    private val swfBytes: ByteArray?
        get() {
            val uri = intent.data
            if (uri?.scheme == "content") {
                try {
                    contentResolver.openInputStream(uri).use { inputStream ->
                        if (inputStream == null) {
                            return null
                        }
                        val bytes = ByteArray(inputStream.available())
                        val dataInputStream = DataInputStream(inputStream)
                        dataInputStream.readFully(bytes)
                        return bytes
                    }
                } catch (ignored: IOException) {
                }
            }
            return null
        }

    @Suppress("unused")
    // Used by Rust
    private val swfUri: String?
        get() {
            return intent.dataString
        }

    @Suppress("unused")
    // Used by Rust
    private val traceOutput: String?
        get() {
            return intent.getStringExtra("traceOutput")
        }

    @Suppress("unused")
    // Used by Rust
    private fun navigateToUrl(url: String?) {
        startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
    }

    private var loc = IntArray(2)

    @Suppress("unused")
    // Handle of an EventLoopProxy over in rust-land
    private val eventLoopHandle: Long = 0

    @Suppress("unused")
    // Used by Rust
    private val locInWindow: IntArray
        get() {
            mSurfaceView.getLocationInWindow(loc)
            return loc
        }

    @Suppress("unused")
    // Used by Rust
    private val surfaceWidth: Int
        get() = mSurfaceView.width

    @Suppress("unused")
    // Used by Rust
    private val surfaceHeight: Int
        get() = mSurfaceView.height

    private external fun keydown(keyTag: String)
    private external fun keyup(keyTag: String)
    private external fun requestContextMenu()
    private external fun runContextMenuCallback(index: Int)
    private external fun clearContextMenu()

    // MoleRuffle:显/隐系统自带软键盘。
    private external fun toggleSoftKeyboard()

    // MoleRuffle:性能 HUD 的 Rust 侧数据(FPS/帧时间/后端/分辨率)。
    private external fun getPerfStats(): String

    // MoleRuffle:性能 HUD 500ms 轮询。
    private val hudHandler = Handler(Looper.getMainLooper())
    private var perfHud: TextView? = null
    private val hudRunnable = object : Runnable {
        override fun run() {
            updatePerfHud()
            hudHandler.postDelayed(this, 500)
        }
    }

    @Suppress("unused")
    // Used by Rust
    private fun showContextMenu(items: Array<String>) {
        runOnUiThread {
            val popup = PopupMenu(this, findViewById(R.id.button_toggle))
            val menu = popup.menu
            if (Build.VERSION.SDK_INT >= VERSION_CODES.P) {
                menu.setGroupDividerEnabled(true)
            }
            var group = 1
            for (i in items.indices) {
                val elements = items[i].split(" ".toRegex(), limit = 4).toTypedArray()
                val enabled = elements[0].toBoolean()
                val separatorBefore = elements[1].toBoolean()
                val checked = elements[2].toBoolean()
                val caption = elements[3]
                if (separatorBefore) group += 1
                val item = menu.add(group, i, Menu.NONE, caption)
                item.setEnabled(enabled)
                if (checked) {
                    item.setCheckable(true)
                    item.setChecked(true)
                }
            }
            val exitItemId: Int = items.size
            menu.add(group, exitItemId, Menu.NONE, "Exit")
            popup.setOnMenuItemClickListener { item: MenuItem ->
                if (item.itemId == exitItemId) {
                    finish()
                } else {
                    runContextMenuCallback(item.itemId)
                }
                true
            }
            popup.setOnDismissListener { clearContextMenu() }
            popup.show()
        }
    }

    @Suppress("unused")
    // Used by Rust
    private fun getAndroidDataStorageDir(): String {
        // TODO It can also be placed in an external storage path in the future to share archived content
        val storageDirPath = "${filesDir.absolutePath}/ruffle/shared_objects"
        val storageDir = File(storageDirPath)
        if (!storageDir.exists()) {
            storageDir.mkdirs()
        }
        return storageDirPath
    }

    override fun onCreateSurfaceView() {
        val inflater = layoutInflater

        @SuppressLint("InflateParams")
        val layout = inflater.inflate(R.layout.keyboard, null) as ConstraintLayout

        contentViewId = View.generateViewId()
        layout.id = contentViewId
        setContentView(layout)
        mSurfaceView = InputEnabledSurfaceView(this)

        mSurfaceView.contentDescription = "Ruffle Player"

        val placeholder = findViewById<View>(R.id.placeholder)
        val pars = placeholder.layoutParams as ConstraintLayout.LayoutParams
        val parent = placeholder.parent as ViewGroup
        val index = parent.indexOfChild(placeholder)
        parent.removeView(placeholder)
        parent.addView(mSurfaceView, index)
        mSurfaceView.setLayoutParams(pars)
        // MoleRuffle:虚拟手柄按钮(↑↓←→/空格)—— 复用官方按键注入通道 keydown/keyup(tag)。
        //   按下→KeyDown、抬起/取消→KeyUp,天然支持按住持续走;ConstraintLayout 默认 splitMotionEvents
        //   开启,两指按两个按钮各自独立成流 → 多点触控(如 ← + 空格 同时)可用。
        val gamepadKeys = gatherAllDescendantsOfType<Button>(
            layout.getViewById(R.id.gamepad_panel),
            Button::class.java
        )
        for (b in gamepadKeys) {
            b.setOnTouchListener { view: View, motionEvent: MotionEvent ->
                val tag = view.tag as? String ?: return@setOnTouchListener false
                when (motionEvent.actionMasked) {
                    MotionEvent.ACTION_DOWN -> keydown(tag)
                    MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> keyup(tag)
                }
                view.performClick()
                // 返回 false:让 Button 自身也处理这次触摸(呈现按下高亮),同时保证后续 UP 仍投递到本按钮。
                false
            }
        }

        // MoleRuffle:手柄显隐开关。
        layout.findViewById<View>(R.id.button_toggle).setOnClickListener {
            val panel = layout.findViewById<View>(R.id.gamepad_panel)
            panel.visibility =
                if (panel.visibility == View.VISIBLE) View.GONE else View.VISIBLE
        }

        // MoleRuffle:系统自带软键盘开关(登录/聊天打字,交给原生调 AndroidApp.show_soft_input)。
        layout.findViewById<View>(R.id.button_ime).setOnClickListener {
            toggleSoftKeyboard()
        }

        // MoleRuffle:性能监视 HUD —— 开始 500ms 轮询刷新。
        perfHud = layout.findViewById(R.id.perf_hud)
        hudHandler.removeCallbacks(hudRunnable)
        hudHandler.post(hudRunnable)

        layout.requestLayout()
        layout.requestFocus()
        mSurfaceView.holder.addCallback(this)
        ViewCompat.setOnApplyWindowInsetsListener(mSurfaceView, this)
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        // MoleRuffle:已强制横屏且移除官方 QWERTY 键盘,虚拟手柄由约束布局自适应,无需在此处理。
    }

    // MoleRuffle:刷新性能 HUD —— Rust 侧给 FPS/帧时间/后端/分辨率,Kotlin 补内存/温度/网络。
    private fun updatePerfHud() {
        val hud = perfHud ?: return
        val native = try {
            getPerfStats()
        } catch (t: Throwable) {
            "FPS -"
        }
        val mb = 1024L * 1024L
        val rt = Runtime.getRuntime()
        val usedMb = (rt.totalMemory() - rt.freeMemory()) / mb
        var availMb = 0L
        var totalMb = 0L
        try {
            val am = getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
            val mi = ActivityManager.MemoryInfo()
            am.getMemoryInfo(mi)
            availMb = mi.availMem / mb
            totalMb = mi.totalMem / mb
        } catch (_: Throwable) {
        }
        hud.text = buildString {
            append(native)
            append("\n内存 用").append(usedMb).append(" / 余").append(availMb)
                .append(" / 总").append(totalMb).append(" MB")
            append("\n温度 ").append(thermalText()).append("   网络 ").append(networkText())
        }
    }

    private fun thermalText(): String {
        return try {
            val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
            when (pm.currentThermalStatus) {
                PowerManager.THERMAL_STATUS_NONE -> "正常"
                PowerManager.THERMAL_STATUS_LIGHT -> "微热"
                PowerManager.THERMAL_STATUS_MODERATE -> "偏热"
                PowerManager.THERMAL_STATUS_SEVERE -> "过热"
                PowerManager.THERMAL_STATUS_CRITICAL -> "严重"
                PowerManager.THERMAL_STATUS_EMERGENCY,
                PowerManager.THERMAL_STATUS_SHUTDOWN -> "危急"
                else -> "未知"
            }
        } catch (_: Throwable) {
            "?"
        }
    }

    private fun networkText(): String {
        return try {
            val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
            val nw = cm.activeNetwork ?: return "离线"
            val caps = cm.getNetworkCapabilities(nw) ?: return "离线"
            when {
                caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> "WiFi"
                caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> "蜂窝"
                caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> "有线"
                else -> "在线"
            }
        } catch (_: Throwable) {
            "?"
        }
    }

    override fun onStart() {
        super.onStart()
        // MoleRuffle:回前台恢复 HUD 轮询(perfHud 已就绪时)。
        if (perfHud != null) {
            hudHandler.removeCallbacks(hudRunnable)
            hudHandler.post(hudRunnable)
        }
    }

    override fun onStop() {
        // MoleRuffle:进后台停 HUD 轮询,避免无谓唤醒主线程/耗电(遵循失焦即暂停)。
        hudHandler.removeCallbacks(hudRunnable)
        super.onStop()
    }

    override fun onDestroy() {
        hudHandler.removeCallbacks(hudRunnable)
        super.onDestroy()
    }

    private fun hideSystemUI() {
        // This will put the game behind any cutouts and waterfalls on devices which have
        // them, so the corresponding insets will be non-zero.
        if (Build.VERSION.SDK_INT >= VERSION_CODES.R) {
            window.attributes.layoutInDisplayCutoutMode =
                WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        }
        // From API 30 onwards, this is the recommended way to hide the system UI, rather than
        // using View.setSystemUiVisibility.
        val decorView = window.decorView
        val controller = WindowInsetsControllerCompat(
            window,
            decorView
        )
        controller.hide(WindowInsetsCompat.Type.systemBars())
        controller.hide(WindowInsetsCompat.Type.displayCutout())
        controller.systemBarsBehavior =
            WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        nativeInit { message ->
            Log.e("ruffle", "Handling panic: $message")
            startActivity(
                Intent(this, PanicActivity::class.java).apply {
                    putExtra("message", message)
                }
            )
        }
        // When true, the app will fit inside any system UI windows.
        // When false, we render behind any system UI windows.
        WindowCompat.setDecorFitsSystemWindows(window, false)
        hideSystemUI()
        // You can set IME fields here or in native code using GameActivity_setImeEditorInfoFields.
        // We set the fields in native_engine.cpp.
        // super.setImeEditorInfoFields(InputType.TYPE_CLASS_TEXT,
        //     IME_ACTION_NONE, IME_FLAG_NO_FULLSCREEN );
        requestNoStatusBarFeature()
        supportActionBar?.hide()
        super.onCreate(savedInstanceState)
    }

    // Used by Rust
    @Suppress("unused")
    val isGooglePlayGames: Boolean
        get() {
            val pm = packageManager
            return pm.hasSystemFeature("com.google.android.play.feature.HPE_EXPERIENCE")
        }

    private fun requestNoStatusBarFeature() {
        // Hiding the status bar this way makes it see through when pulled down
        requestWindowFeature(Window.FEATURE_NO_TITLE)
        WindowInsetsControllerCompat(
            window,
            mSurfaceView
        ).hide(WindowInsetsCompat.Type.statusBars())
    }

    companion object {
        init {
            // load the native activity
            System.loadLibrary("ruffle_android")
        }

        @JvmStatic
        private external fun nativeInit(crashCallback: CrashCallback)

        private fun <T> gatherAllDescendantsOfType(v: View, t: Class<*>): List<T> {
            val result: MutableList<T> = ArrayList()
            @Suppress("UNCHECKED_CAST")
            if (t.isInstance(v)) result.add(v as T)
            if (v is ViewGroup) {
                for (i in 0 until v.childCount) {
                    result.addAll(gatherAllDescendantsOfType(v.getChildAt(i), t))
                }
            }
            return result
        }
    }

    fun interface CrashCallback {
        fun onCrash(message: String)
    }
}
