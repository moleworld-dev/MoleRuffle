//! 桌面窗口:记住大小和位置(下次启动恢复)、最小尺寸、全屏切换。
//!
//! 状态文件 `<数据目录>/MoleRuffle/window.txt`,一行 `x y 宽 高`(位置为物理像素,大小为逻辑点)。
//! 位置只在落在某个现存显示器上时才恢复(拔掉外接屏后不会把窗口放到看不见的地方)。

use std::path::PathBuf;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Fullscreen, Window, WindowAttributes};

/// 窗口最小尺寸(逻辑点):再小游戏文字就看不清了(舞台 960×560 的一半)。
const MIN_SIZE: (f64, f64) = (480.0, 280.0);

fn state_file() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("MoleRuffle").join("window.txt"))
}

/// 给窗口属性套上:上次的大小与位置、最小尺寸、启动全屏。
pub fn apply(attrs: WindowAttributes, el: &ActiveEventLoop) -> WindowAttributes {
    let mut attrs = attrs.with_min_inner_size(LogicalSize::new(MIN_SIZE.0, MIN_SIZE.1));
    if let Some((x, y, w, h)) = load() {
        attrs = attrs.with_inner_size(LogicalSize::new(w.max(MIN_SIZE.0), h.max(MIN_SIZE.1)));
        let on_screen = el.available_monitors().any(|m| {
            let (p, s) = (m.position(), m.size());
            x >= p.x && y >= p.y && x < p.x + s.width as i32 && y < p.y + s.height as i32
        });
        if on_screen {
            attrs = attrs.with_position(PhysicalPosition::new(x, y));
        }
    }
    if std::env::var("MOLE_START_FULLSCREEN").as_deref() == Ok("1") {
        attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
    }
    attrs
}

fn load() -> Option<(i32, i32, f64, f64)> {
    let text = std::fs::read_to_string(state_file()?).ok()?;
    let mut it = text.split_whitespace();
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    let w = it.next()?.parse().ok()?;
    let h = it.next()?.parse().ok()?;
    Some((x, y, w, h))
}

/// 记下当前窗口的大小和位置(全屏或最小化时不记,免得下次启动成全屏尺寸的普通窗口)。
pub fn save(window: &Window) {
    if window.fullscreen().is_some() || window.is_minimized() == Some(true) {
        return;
    }
    let Some(path) = state_file() else { return };
    let Ok(pos) = window.outer_position() else { return };
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{} {} {:.0} {:.0}", pos.x, pos.y, size.width, size.height));
}

/// 全屏 / 退出全屏。
pub fn toggle_fullscreen(window: &Window) {
    let full = window.fullscreen().is_some();
    window.set_fullscreen(if full { None } else { Some(Fullscreen::Borderless(None)) });
    tracing::info!("{}", if full { "退出全屏" } else { "进入全屏" });
}
