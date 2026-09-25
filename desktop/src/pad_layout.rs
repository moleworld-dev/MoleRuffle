//! 虚拟手柄自适应布局(纯函数,不依赖 UIKit,可在桌面跑单测)。
//!
//! 旧布局把方向键死贴屏幕左下、空格贴右下,不管游戏画面落在哪。问题:
//! - 普通手机横屏(比舞台 960:560 更宽):游戏左右各有黑边,但方向键 192pt 宽,
//!   在 17 Pro Max 上仍压住游戏左下角约 90pt,正好挡住摩尔庄园底部工具栏;
//! - 折叠屏展开 / iPad(比舞台更"方"):左右没有黑边了,手柄整个压在游戏上。
//!
//! 新规则(配合 core 在移动端把舞台对齐设为贴顶,见 `apply_mole_settings`):
//! - **底栏模式**:屏幕比舞台更方时,游戏贴顶,上下余量合成底部一整条。底栏够高放两行
//!   就用键盘式倒 T(↑ 在上,← ↓ → 在下);只够一行就排成一行 ← ↑ ↓ →。空格和切换钮在右侧。
//!   代表设备:iPhone Duo 内屏(1.42)/外屏(1.46)、小米 18 Fold 内屏(1.41)、iPad。
//! - **侧栏模式**:屏幕更宽且左右黑边宽到放得下方向键十字时,方向键放左黑边、空格放右黑边。
//! - **叠放模式**:黑边太窄(比例接近 12:7,如普通手机)时退回旧的角落半透明叠放。
//!
//! 所有尺寸单位是**逻辑点**(iOS pt / 安卓 dp),与缩放无关。安卓壳的 Kotlin 版照此移植。

/// 舞台宽高比(Client.swf 固定 960×560)。
pub const STAGE_ASPECT: f64 = 960.0 / 560.0;
/// 最小触控尺寸:苹果人机界面指南 44pt,低于此值容易点错。
pub const MIN_BTN: f64 = 44.0;
/// 按钮最大边长(再大就笨重了)。
pub const MAX_BTN: f64 = 64.0;
/// 栏内边距(空间充裕时)。
const MARGIN: f64 = 10.0;
/// 栏内边距下限(空间紧张时)。
const MIN_MARGIN: f64 = 2.0;
/// 键间距(倒 T / 单排时用;十字不留缝,保持旧手感)。
const GAP: f64 = 6.0;
/// 切换钮尺寸。
const TG_W: f64 = 56.0;
const TG_H: f64 = 44.0;
/// 叠放模式沿用旧常量,保证普通手机手感不变。
const LEGACY_BTN: f64 = 64.0;
const LEGACY_EDGE: f64 = 30.0;
const LEGACY_SP_W: f64 = 150.0;
const LEGACY_GAP: f64 = 12.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f64 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }
    /// 两矩形是否有面积重叠(贴边不算)。
    pub fn overlaps(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
}

/// 安全区留白(刘海/灵动岛/底部横条/圆角)。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 底部一条,方向键为倒 T(两行)。
    BottomBarT,
    /// 底部一条,方向键单排(一行)。
    BottomBarRow,
    /// 左右黑边。
    SideBars,
    /// 叠在游戏角落(旧行为)。
    Overlay,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PadLayout {
    pub mode: Mode,
    /// 游戏画面在屏幕上的位置。
    pub game: Rect,
    pub up: Rect,
    pub down: Rect,
    pub left: Rect,
    pub right: Rect,
    pub space: Rect,
    pub toggle: Rect,
}

impl PadLayout {
    /// 全部按钮(含切换钮)。
    pub fn buttons(&self) -> [Rect; 6] {
        [self.up, self.down, self.left, self.right, self.space, self.toggle]
    }
}

/// 游戏画面在屏幕上的位置。与引擎 ShowAll + 贴顶对齐的计算一致:
/// 宽屏时高度撑满、水平居中;方屏时宽度撑满、贴顶。
pub fn game_rect(w: f64, h: f64) -> Rect {
    if w / h >= STAGE_ASPECT {
        let gw = h * STAGE_ASPECT;
        Rect::new((w - gw) / 2.0, 0.0, gw, h)
    } else {
        Rect::new(0.0, 0.0, w, w / STAGE_ASPECT)
    }
}

/// 计算手柄布局。`w`/`h` 为全屏逻辑尺寸,`safe` 为安全区留白。
pub fn compute(w: f64, h: f64, safe: Insets) -> PadLayout {
    let game = game_rect(w, h);

    if w / h < STAGE_ASPECT {
        // 方屏:底部余量 = 屏幕高 - 游戏高(贴顶后全部在下方),再扣掉底部安全区(横条手势区)。
        let avail = h - game.bottom() - safe.bottom;
        let left0 = safe.left + MARGIN;
        let right_edge = w - safe.right - MARGIN;

        // 按 rows 行排布时的(按钮边长, 上下边距)。边距随空间自适应(2~10pt),
        // 优先保证按钮不小于最小触控尺寸 —— iPhone Duo 外屏底栏扣掉横条只剩 ~50pt,
        // 固定留 10pt 边距就连一排都放不下了。
        let fit = |rows: f64| -> Option<(f64, f64)> {
            let need = rows * MIN_BTN + (rows - 1.0) * GAP;
            if avail < need + 2.0 * MIN_MARGIN {
                return None;
            }
            let m = ((avail - need) / 2.0).min(MARGIN);
            let btn = ((avail - 2.0 * m - (rows - 1.0) * GAP) / rows).min(MAX_BTN);
            Some((btn, m))
        };

        // 优先倒 T(两行,手感最接近键盘方向键)。
        if let Some((btn_t, m)) = fit(2.0) {
            let y1 = h - safe.bottom - m - btn_t; // 下排
            let y0 = y1 - GAP - btn_t; // 上排
            let step = btn_t + GAP;
            return bottom_bar(
                Mode::BottomBarT,
                game,
                Rect::new(left0 + step, y0, btn_t, btn_t),
                Rect::new(left0 + step, y1, btn_t, btn_t),
                Rect::new(left0, y1, btn_t, btn_t),
                Rect::new(left0 + 2.0 * step, y1, btn_t, btn_t),
                btn_t,
                y1,
                right_edge,
            );
        }
        // 只够一行:← ↑ ↓ →。
        if let Some((btn_r, m)) = fit(1.0) {
            let y = h - safe.bottom - m - btn_r;
            let step = btn_r + GAP;
            return bottom_bar(
                Mode::BottomBarRow,
                game,
                Rect::new(left0 + step, y, btn_r, btn_r),
                Rect::new(left0 + 2.0 * step, y, btn_r, btn_r),
                Rect::new(left0, y, btn_r, btn_r),
                Rect::new(left0 + 3.0 * step, y, btn_r, btn_r),
                btn_r,
                y,
                right_edge,
            );
        }
    } else {
        // 宽屏:两侧黑边宽度(左右对称;安全区取较大一侧,保证两边都放得下)。
        let bar_w = game.x - safe.left.max(safe.right) - 2.0 * MARGIN;
        let btn = (bar_w / 3.0).min(MAX_BTN);
        if btn >= MIN_BTN {
            // 方向键十字:左黑边内水平居中,贴底。
            let cx = safe.left + MARGIN + (bar_w - 3.0 * btn) / 2.0;
            let cy = h - safe.bottom - MARGIN - 3.0 * btn;
            // 右黑边:切换钮贴底,空格在它上方,宽度占满可用栏宽。
            let rx = game.right() + MARGIN;
            let toggle = Rect::new(rx + (bar_w - TG_W).max(0.0) / 2.0, h - safe.bottom - MARGIN - TG_H, TG_W.min(bar_w), TG_H);
            let space = Rect::new(rx, toggle.y - GAP - btn, bar_w, btn);
            return PadLayout {
                mode: Mode::SideBars,
                game,
                up: Rect::new(cx + btn, cy, btn, btn),
                left: Rect::new(cx, cy + btn, btn, btn),
                right: Rect::new(cx + 2.0 * btn, cy + btn, btn, btn),
                down: Rect::new(cx + btn, cy + 2.0 * btn, btn, btn),
                space,
                toggle,
            };
        }
    }

    // 叠放:沿用旧布局(方向键十字左下、空格与切换钮右下),普通手机手感不变。
    let gx = LEGACY_EDGE;
    let gy = h - LEGACY_EDGE - 3.0 * LEGACY_BTN;
    let tg_x = w - LEGACY_EDGE - TG_W;
    let tg_y = h - LEGACY_EDGE - TG_H;
    PadLayout {
        mode: Mode::Overlay,
        game,
        up: Rect::new(gx + LEGACY_BTN, gy, LEGACY_BTN, LEGACY_BTN),
        left: Rect::new(gx, gy + LEGACY_BTN, LEGACY_BTN, LEGACY_BTN),
        right: Rect::new(gx + 2.0 * LEGACY_BTN, gy + LEGACY_BTN, LEGACY_BTN, LEGACY_BTN),
        down: Rect::new(gx + LEGACY_BTN, gy + 2.0 * LEGACY_BTN, LEGACY_BTN, LEGACY_BTN),
        space: Rect::new(tg_x - LEGACY_GAP - LEGACY_SP_W, h - LEGACY_EDGE - LEGACY_BTN, LEGACY_SP_W, LEGACY_BTN),
        toggle: Rect::new(tg_x, tg_y, TG_W, TG_H),
    }
}

/// 底栏右侧:切换钮贴右,空格在它左边,与方向键下排同一基线。
#[allow(clippy::too_many_arguments)]
fn bottom_bar(
    mode: Mode,
    game: Rect,
    up: Rect,
    down: Rect,
    left: Rect,
    right: Rect,
    btn: f64,
    row_y: f64,
    right_edge: f64,
) -> PadLayout {
    let tg_h = TG_H.min(btn);
    let toggle = Rect::new(right_edge - TG_W, row_y + (btn - tg_h) / 2.0, TG_W, tg_h);
    let sp_w = (btn * 2.4).max(120.0);
    let space = Rect::new(toggle.x - GAP * 2.0 - sp_w, row_y, sp_w, btn);
    PadLayout { mode, game, up, down, left, right, space, toggle }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 各机型横屏逻辑尺寸 + 大致安全区。
    const IPHONE_17PM: (f64, f64, Insets) =
        (956.0, 440.0, Insets { left: 62.0, right: 62.0, top: 0.0, bottom: 21.0 });
    /// iPhone Duo 内屏 1878×2670 px @3x,横屏 890×626pt(官方规格,2026-09)。
    const DUO_INNER: (f64, f64, Insets) =
        (890.0, 626.0, Insets { left: 0.0, right: 0.0, top: 0.0, bottom: 20.0 });
    /// iPhone Duo 外屏 1398×2034 px @3x,横屏 678×466pt。
    const DUO_OUTER: (f64, f64, Insets) =
        (678.0, 466.0, Insets { left: 44.0, right: 44.0, top: 0.0, bottom: 20.0 });
    /// 小米 18 Fold 内屏 2364×1672 px,约 2.625 倍 → 900×637dp。
    const XIAOMI_FOLD_INNER: (f64, f64, Insets) =
        (900.0, 637.0, Insets { left: 0.0, right: 0.0, top: 0.0, bottom: 16.0 });
    /// iPad Pro 11" 横屏 1194×834pt(比例 1.43,是折叠屏内屏的现成替身)。
    const IPAD_11: (f64, f64, Insets) =
        (1194.0, 834.0, Insets { left: 0.0, right: 0.0, top: 24.0, bottom: 20.0 });
    /// 超宽屏(21:9 安卓机横屏约 960×411dp;以及更宽的桌面比例)。
    const ULTRAWIDE: (f64, f64, Insets) =
        (1400.0, 500.0, Insets { left: 0.0, right: 0.0, top: 0.0, bottom: 0.0 });

    fn check_invariants(name: &str, (w, h, safe): (f64, f64, Insets)) -> PadLayout {
        let l = compute(w, h, safe);
        for b in l.buttons() {
            // 不出屏幕
            assert!(b.x >= 0.0 && b.y >= 0.0 && b.right() <= w + 0.01 && b.bottom() <= h + 0.01,
                "{name}: 按钮出界 {b:?}");
        }
        if l.mode != Mode::Overlay {
            for b in l.buttons() {
                // 非叠放模式:任何按钮都不压游戏画面
                assert!(!b.overlaps(&l.game), "{name} {:?}: 按钮 {b:?} 压住了游戏 {:?}", l.mode, l.game);
                // 不进底部安全区(横条手势区)
                assert!(b.bottom() <= h - safe.bottom + 0.01, "{name}: 按钮进了底部安全区 {b:?}");
            }
            for b in [l.up, l.down, l.left, l.right] {
                assert!(b.w >= MIN_BTN && b.h >= MIN_BTN, "{name}: 方向键小于最小触控尺寸 {b:?}");
            }
        }
        // 按钮之间互不重叠
        let bs = l.buttons();
        for i in 0..bs.len() {
            for j in (i + 1)..bs.len() {
                assert!(!bs[i].overlaps(&bs[j]), "{name}: 按钮 {i} 与 {j} 重叠 {:?} {:?}", bs[i], bs[j]);
            }
        }
        l
    }

    #[test]
    fn 普通手机_黑边太窄_退回旧叠放布局_手感不变() {
        let l = check_invariants("17PM", IPHONE_17PM);
        assert_eq!(l.mode, Mode::Overlay);
        assert_eq!(l.left, Rect::new(30.0, 440.0 - 30.0 - 128.0, 64.0, 64.0));
    }

    #[test]
    fn iphone_duo_内屏_游戏贴顶_手柄进底栏_不压画面() {
        let l = check_invariants("Duo 内屏", DUO_INNER);
        assert!(matches!(l.mode, Mode::BottomBarRow | Mode::BottomBarT), "{:?}", l.mode);
        assert_eq!(l.game.y, 0.0, "游戏应贴顶");
    }

    #[test]
    fn iphone_duo_外屏_同样进底栏() {
        let l = check_invariants("Duo 外屏", DUO_OUTER);
        assert!(matches!(l.mode, Mode::BottomBarRow | Mode::BottomBarT), "{:?}", l.mode);
    }

    #[test]
    fn 小米18fold_内屏_进底栏() {
        let l = check_invariants("小米 Fold 内屏", XIAOMI_FOLD_INNER);
        assert!(matches!(l.mode, Mode::BottomBarRow | Mode::BottomBarT), "{:?}", l.mode);
    }

    #[test]
    fn ipad_底栏够高_用倒t() {
        let l = check_invariants("iPad 11", IPAD_11);
        assert_eq!(l.mode, Mode::BottomBarT);
        // 倒 T:↑ 在 ↓ 正上方
        assert_eq!(l.up.x, l.down.x);
        assert!(l.up.bottom() <= l.down.y);
    }

    #[test]
    fn 超宽屏_手柄进左右黑边() {
        let l = check_invariants("超宽", ULTRAWIDE);
        assert_eq!(l.mode, Mode::SideBars);
        assert!(l.left.right() <= l.game.x && l.space.x >= l.game.right());
    }

    #[test]
    fn 折叠与展开来回切换_布局都成立() {
        // 模拟 iPhone Duo 合上(外屏)→ 展开(内屏)→ 再合上
        for dims in [DUO_OUTER, DUO_INNER, DUO_OUTER, XIAOMI_FOLD_INNER, IPHONE_17PM] {
            check_invariants("切换", dims);
        }
    }

    #[test]
    fn game_rect_与引擎_showall_贴顶一致() {
        // 宽屏:高度撑满,水平居中
        let g = game_rect(1200.0, 560.0);
        assert!((g.h - 560.0).abs() < 1e-9 && (g.x - 120.0).abs() < 1e-9 && g.y == 0.0);
        // 方屏:宽度撑满,贴顶
        let g = game_rect(960.0, 800.0);
        assert!((g.w - 960.0).abs() < 1e-9 && (g.h - 560.0).abs() < 1e-9 && g.y == 0.0);
    }
}

