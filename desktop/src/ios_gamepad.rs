//! iOS 屏幕虚拟手柄:左下方向键(↑↓←→)+ 右下空格 + 右下角常驻"⌨"切换钮。
//!
//! 摩尔庄园靠方向键走路、空格交互。iOS 真机无实体键盘,故叠一层原生 UILabel 按钮。
//! 复用 `ios_textbar` 同法:**非交互**叠层(userInteractionEnabled=false,触摸穿透回 winit),
//! 由 main.rs 的 Touch 处理按坐标命中 → 按下发 KeyDown、抬起发 KeyUp(支持按住持续走 + 多指)。
//! 切换钮常驻;方向键/空格面板默认隐藏,点切换钮显示/隐藏。

use objc2::rc::Retained;
use objc2_foundation::{CGPoint, CGRect, CGSize, MainThreadMarker, NSString};
use objc2_ui_kit::{NSTextAlignment, UIColor, UIFont, UILabel, UIView};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::keymap::GamepadKey;

/// 命中结果:方向/空格键,或切换钮。
#[derive(Clone, Copy, PartialEq)]
pub enum GamepadHit {
    Key(GamepadKey),
    Toggle,
}

// 布局常量(逻辑点)。
const BTN: f64 = 64.0; // 方向键单格边长
const EDGE: f64 = 30.0; // 距屏幕边距
const SP_W: f64 = 150.0; // 空格键宽
const SP_H: f64 = 64.0; // 空格键高
const TG_W: f64 = 56.0; // 切换钮宽
const TG_H: f64 = 44.0; // 切换钮高
const GAP: f64 = 12.0;

/// 一个按钮的标签 + 其命中目标。
struct Btn {
    hit: GamepadHit,
    label: Retained<UILabel>,
}

pub struct GamePad {
    /// 方向键 + 空格面板(整体显隐)。
    panel: Retained<UIView>,
    /// 切换钮(常驻显示)。
    _toggle: Retained<UILabel>,
    /// 面板内按钮(方向/空格)。
    pad_btns: Vec<Btn>,
    /// 切换钮命中矩形(物理像素)。
    toggle_rect: (f64, f64, f64, f64),
    /// 面板内按钮命中矩形(物理像素):(hit, x0, y0, x1, y1)。
    pad_rects: Vec<(GamepadHit, f64, f64, f64, f64)>,
    /// 面板是否可见。
    visible: bool,
}

fn make_label(mtm: MainThreadMarker, text: &str, sz: f64, bg: &UIColor) -> Retained<UILabel> {
    let label = unsafe { UILabel::new(mtm) };
    let white = unsafe { UIColor::whiteColor() };
    let font = unsafe { UIFont::boldSystemFontOfSize(sz) };
    unsafe {
        label.setText(Some(&NSString::from_str(text)));
        label.setTextColor(Some(&white));
        label.setTextAlignment(NSTextAlignment::Center);
        label.setFont(Some(&font));
        label.setUserInteractionEnabled(false);
    }
    label.setBackgroundColor(Some(bg));
    label
}

impl GamePad {
    pub fn new(window: &Window) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::UiKit(h) = handle.as_raw() else {
            return None;
        };
        // SAFETY: winit 保证窗口存活期间该指针是有效 UIView。
        let ui_view: &UIView = unsafe { &*(h.ui_view.as_ptr() as *const UIView) };

        let bg = unsafe { UIColor::colorWithRed_green_blue_alpha(0.10, 0.10, 0.14, 0.55) };

        // 面板(铺整屏的非交互容器,内部按绝对屏幕坐标放按钮;触摸穿透)。
        let panel = unsafe { UIView::new(mtm) };
        panel.setHidden(true);
        unsafe { panel.setUserInteractionEnabled(false) };

        let mut pad_btns = Vec::new();
        for (hit, glyph, sz) in [
            (GamepadHit::Key(GamepadKey::Up), "↑", 30.0),
            (GamepadHit::Key(GamepadKey::Down), "↓", 30.0),
            (GamepadHit::Key(GamepadKey::Left), "←", 30.0),
            (GamepadHit::Key(GamepadKey::Right), "→", 30.0),
            (GamepadHit::Key(GamepadKey::Space), "空格", 20.0),
        ] {
            let label = make_label(mtm, glyph, sz, &bg);
            unsafe { panel.addSubview(&label) };
            pad_btns.push(Btn { hit, label });
        }
        unsafe { ui_view.addSubview(&panel) };

        // 切换钮(常驻,稍亮)。
        let tg_bg = unsafe { UIColor::colorWithRed_green_blue_alpha(0.16, 0.16, 0.22, 0.85) };
        let toggle = make_label(mtm, "⌨", 24.0, &tg_bg);
        unsafe { ui_view.addSubview(&toggle) };

        Some(Self {
            panel,
            _toggle: toggle.clone(),
            pad_btns,
            toggle_rect: (0.0, 0.0, 0.0, 0.0),
            pad_rects: Vec::new(),
            visible: false,
        })
    }

    /// 按屏幕逻辑尺寸布局,算出各按钮 frame(逻辑点)与命中矩形(物理像素)。
    pub fn layout(&mut self, screen_w: f64, screen_h: f64, scale: f64) {
        // 方向键十字(左下,3×3 网格,中心空)。
        let gx = EDGE;
        let gy = screen_h - EDGE - 3.0 * BTN;
        let dpad = |hit: GamepadHit| -> (f64, f64) {
            match hit {
                GamepadHit::Key(GamepadKey::Up) => (gx + BTN, gy),
                GamepadHit::Key(GamepadKey::Left) => (gx, gy + BTN),
                GamepadHit::Key(GamepadKey::Right) => (gx + 2.0 * BTN, gy + BTN),
                GamepadHit::Key(GamepadKey::Down) => (gx + BTN, gy + 2.0 * BTN),
                _ => (0.0, 0.0),
            }
        };
        // 切换钮(右下角,常驻)。
        let tg_x = screen_w - EDGE - TG_W;
        let tg_y = screen_h - EDGE - TG_H;
        // 空格(切换钮左侧)。
        let sp_x = tg_x - GAP - SP_W;
        let sp_y = screen_h - EDGE - SP_H;

        self.pad_rects.clear();
        for btn in &self.pad_btns {
            let (x, y, w, h) = if btn.hit == GamepadHit::Key(GamepadKey::Space) {
                (sp_x, sp_y, SP_W, SP_H)
            } else {
                let (x, y) = dpad(btn.hit);
                (x, y, BTN, BTN)
            };
            btn.label.setFrame(CGRect {
                origin: CGPoint { x, y },
                size: CGSize { width: w, height: h },
            });
            self.pad_rects
                .push((btn.hit, x * scale, y * scale, (x + w) * scale, (y + h) * scale));
        }

        self._toggle.setFrame(CGRect {
            origin: CGPoint { x: tg_x, y: tg_y },
            size: CGSize {
                width: TG_W,
                height: TG_H,
            },
        });
        self.toggle_rect = (
            tg_x * scale,
            tg_y * scale,
            (tg_x + TG_W) * scale,
            (tg_y + TG_H) * scale,
        );
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        self.panel.setHidden(!self.visible);
    }

    /// 命中测试(物理像素)。切换钮恒可命中;方向/空格仅面板可见时命中。
    pub fn hit_test(&self, x: f64, y: f64) -> Option<GamepadHit> {
        let (tx0, ty0, tx1, ty1) = self.toggle_rect;
        if x >= tx0 && x <= tx1 && y >= ty0 && y <= ty1 {
            return Some(GamepadHit::Toggle);
        }
        if !self.visible {
            return None;
        }
        self.pad_rects
            .iter()
            .find(|(_, x0, y0, x1, y1)| x >= *x0 && x <= *x1 && y >= *y0 && y <= *y1)
            .map(|(hit, ..)| *hit)
    }
}
