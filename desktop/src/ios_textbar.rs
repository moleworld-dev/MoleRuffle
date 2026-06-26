//! iOS 纯触摸文本工具条:`粘贴`(系统 UIPasteControl,零弹窗授权)+ `复制/剪切/全选`(原生
//! UILabel 叠层 + 坐标拦截)。
//!
//! 为什么这么拆:winit iOS 的 view 只实现 `UIKeyInput`、没有原生编辑菜单,纯触摸没法粘贴;
//! 而 iOS 16+ 又对“程序化读取外部 app 设置的剪贴板”做隐私拦截(`UIPasteboard.string` 返 nil)。
//! - **粘贴**:用系统 `UIPasteControl`——用户点击即授权,系统把内容交给我们设的 `target`
//!   (实现 `UIPasteConfigurationSupporting`),在其回调里读 `UIPasteboard`(此刻已授权)并经
//!   `moleruffle_core::paste_bridge` 投递给 Ruffle 的 `clipboard_content`。UIPasteControl 是可交互
//!   子视图,触摸直接给它(不经 winit),所以无需坐标拦截。
//! - **复制/剪切/全选**:不涉及隐私读取,仍用非交互 UILabel(触摸穿透)+ main.rs 按坐标命中发
//!   `TextControl`。

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_foundation::{
    CGPoint, CGRect, CGSize, MainThreadMarker, NSArray, NSItemProvider, NSString,
};
use objc2_ui_kit::{
    NSTextAlignment, UIColor, UIFont, UILabel, UIPasteConfiguration,
    UIPasteConfigurationSupporting, UIPasteControl, UIPasteControlConfiguration, UIPasteboard,
    UIView,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use moleruffle_core::paste_bridge;

/// 坐标拦截的按钮(粘贴不在此列——它由 UIPasteControl 原生处理)。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TextAction {
    Copy,
    Cut,
    SelectAll,
}

const LABELS: [(TextAction, &str); 3] = [
    (TextAction::Copy, "复制"),
    (TextAction::Cut, "剪切"),
    (TextAction::SelectAll, "全选"),
];

// 布局常量(逻辑点)。slot 0 = UIPasteControl(粘贴),slot 1..3 = 三个 label。
const BTN_W: f64 = 80.0;
const BTN_H: f64 = 42.0;
const GAP: f64 = 6.0;
const TOP: f64 = 12.0;
const SLOTS: f64 = 4.0;

fn bar_width() -> f64 {
    SLOTS * BTN_W + (SLOTS - 1.0) * GAP
}

// ───────────────────────── UIPasteControl 的粘贴目标 ─────────────────────────

declare_class!(
    /// 实现 `UIPasteConfigurationSupporting` 的对象,做 UIPasteControl 的 target。
    /// 用户点击系统粘贴按钮→系统授权→调用本对象的 `pasteItemProviders:`,在其中读已授权的
    /// 剪贴板文本并投递给 paste_bridge。无 ivars。
    struct MolePasteTarget;

    unsafe impl ClassType for MolePasteTarget {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "MolePasteTarget";
    }

    impl DeclaredClass for MolePasteTarget {}

    unsafe impl NSObjectProtocol for MolePasteTarget {}

    unsafe impl UIPasteConfigurationSupporting for MolePasteTarget {
        #[method_id(pasteConfiguration)]
        fn paste_configuration(&self) -> Option<Retained<UIPasteConfiguration>> {
            // 声明接受纯文本类型,UIPasteControl 据此判断是否可粘贴
            let types = NSArray::from_vec(vec![
                NSString::from_str("public.utf8-plain-text"),
                NSString::from_str("public.text"),
            ]);
            let cfg = unsafe {
                UIPasteConfiguration::initWithAcceptableTypeIdentifiers(
                    MainThreadMarker::new().unwrap().alloc(),
                    &types,
                )
            };
            Some(cfg)
        }

        #[method(setPasteConfiguration:)]
        fn set_paste_configuration(&self, _cfg: Option<&UIPasteConfiguration>) {}

        #[method(canPasteItemProviders:)]
        fn can_paste_item_providers(&self, _items: &NSArray<NSItemProvider>) -> bool {
            true
        }

        #[method(pasteItemProviders:)]
        fn paste_item_providers(&self, _items: &NSArray<NSItemProvider>) {
            // 此刻 UIPasteControl 已授权访问剪贴板,直接读 general pasteboard 的字符串
            let s = unsafe {
                UIPasteboard::generalPasteboard()
                    .string()
                    .map(|x| x.to_string())
            };
            tracing::info!("UIPasteControl 粘贴: {} 字符", s.as_ref().map(|x| x.len()).unwrap_or(0));
            if let Some(s) = s {
                paste_bridge::deliver(s);
            }
        }
    }
);

impl MolePasteTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send_id![mtm.alloc::<Self>(), init] }
    }
}

// ───────────────────────────────── 工具条 ─────────────────────────────────

pub struct TextBar {
    container: Retained<UIView>,
    _labels: Vec<Retained<UILabel>>,
    paste_ctl: Retained<UIPasteControl>,
    _paste_target: Retained<MolePasteTarget>, // 保活:UIPasteControl.target 可能是弱引用
    visible: bool,
    /// 复制/剪切/全选 的命中矩形(物理像素):(action, x0, y0, x1, y1)
    rects: Vec<(TextAction, f64, f64, f64, f64)>,
}

impl TextBar {
    pub fn new(window: &Window) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::UiKit(h) = handle.as_raw() else {
            return None;
        };
        // SAFETY: winit 保证窗口存活期间该指针是有效 UIView。
        let ui_view: &UIView = unsafe { &*(h.ui_view.as_ptr() as *const UIView) };

        // 非交互容器(承载 3 个 label,触摸穿透回 winit view 做坐标拦截)
        let container = UIView::initWithFrame(
            mtm.alloc(),
            CGRect {
                origin: CGPoint { x: 0.0, y: TOP },
                size: CGSize { width: bar_width(), height: BTN_H },
            },
        );
        container.setHidden(true);
        unsafe { container.setUserInteractionEnabled(false) };

        let bg = unsafe { UIColor::colorWithRed_green_blue_alpha(0.12, 0.12, 0.16, 0.94) };
        let white = unsafe { UIColor::whiteColor() };
        let font = unsafe { UIFont::boldSystemFontOfSize(16.0) };

        let mut labels = Vec::new();
        for (i, (_, title)) in LABELS.iter().enumerate() {
            let label = unsafe { UILabel::new(mtm) };
            // slot (i+1):为 UIPasteControl 留出 slot 0
            let x = (i as f64 + 1.0) * (BTN_W + GAP);
            label.setFrame(CGRect {
                origin: CGPoint { x, y: 0.0 },
                size: CGSize { width: BTN_W, height: BTN_H },
            });
            unsafe {
                label.setText(Some(&NSString::from_str(title)));
                label.setTextColor(Some(&white));
                label.setTextAlignment(NSTextAlignment::Center);
                label.setFont(Some(&font));
            }
            label.setBackgroundColor(Some(&bg));
            unsafe { container.addSubview(&label) };
            labels.push(label);
        }
        unsafe { ui_view.addSubview(&container) };

        // 系统粘贴按钮(slot 0)。可交互、置于容器之上。
        let target = MolePasteTarget::new(mtm);
        let cfg = unsafe { UIPasteControlConfiguration::new(mtm) };
        let paste_ctl = unsafe { UIPasteControl::initWithConfiguration(mtm.alloc(), &cfg) };
        paste_ctl.setHidden(true);
        unsafe {
            paste_ctl.setTarget(Some(ProtocolObject::from_ref(&*target)));
        }
        paste_ctl.setFrame(CGRect {
            origin: CGPoint { x: 0.0, y: TOP },
            size: CGSize { width: BTN_W, height: BTN_H },
        });
        unsafe { ui_view.addSubview(&paste_ctl) };

        Some(Self {
            container,
            _labels: labels,
            paste_ctl,
            _paste_target: target,
            visible: false,
            rects: Vec::new(),
        })
    }

    /// 按屏幕逻辑宽度居中布局,算出 label 命中矩形(物理像素)。
    pub fn layout(&mut self, screen_w_logical: f64, scale: f64) {
        let bw = bar_width();
        let x0 = ((screen_w_logical - bw) / 2.0).max(8.0);
        // 容器铺整条;label 在容器内 local 坐标(slot 1..3)
        self.container.setFrame(CGRect {
            origin: CGPoint { x: x0, y: TOP },
            size: CGSize { width: bw, height: BTN_H },
        });
        // 粘贴按钮在 slot 0 的绝对坐标
        self.paste_ctl.setFrame(CGRect {
            origin: CGPoint { x: x0, y: TOP },
            size: CGSize { width: BTN_W, height: BTN_H },
        });
        self.rects.clear();
        for (i, (action, _)) in LABELS.iter().enumerate() {
            let lx = x0 + (i as f64 + 1.0) * (BTN_W + GAP); // slot i+1 的绝对 x
            self.rects.push((
                *action,
                lx * scale,
                TOP * scale,
                (lx + BTN_W) * scale,
                (TOP + BTN_H) * scale,
            ));
        }
    }

    pub fn set_visible(&mut self, visible: bool) {
        if self.visible != visible {
            self.visible = visible;
            self.container.setHidden(!visible);
            self.paste_ctl.setHidden(!visible);
        }
    }

    /// 命中测试(物理像素)。隐藏时永不命中。粘贴由 UIPasteControl 原生处理,不在此。
    pub fn hit_test(&self, x: f64, y: f64) -> Option<TextAction> {
        if !self.visible {
            return None;
        }
        self.rects
            .iter()
            .find(|(_, x0, y0, x1, y1)| x >= *x0 && x <= *x1 && y >= *y0 && y <= *y1)
            .map(|(a, ..)| *a)
    }
}
