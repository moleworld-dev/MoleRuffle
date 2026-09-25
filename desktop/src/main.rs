//! MoleRuffle 桌面 / iOS 的 bin 入口 —— 薄壳,逻辑全在 lib(`moleruffle_desktop`)。
//! 安卓走 cdylib 的 `android_main`(见 lib.rs),不用 bin,故 android 上 main 留空。

#[cfg(not(target_os = "android"))]
fn main() -> anyhow::Result<()> {
    moleruffle_desktop::desktop_main()
}

#[cfg(target_os = "android")]
fn main() {}
