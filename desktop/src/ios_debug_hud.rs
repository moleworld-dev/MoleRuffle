//! iOS 绿色多行调试 HUD 叠层。
//!
//! 在 winit/wgpu(Metal)app 的左上角叠一层半透明黑底、绿色等宽字的 `UILabel`,
//! 多行、置顶、**不吃触摸**(`userInteractionEnabled = false`,触摸穿透回 winit view)。
//!
//! 为什么叠在 winit 的 `UIView` 之上而不是 key window:winit iOS 把 `CAMetalLayer`
//! 挂在它自己的那个 `UIView` 的 `.layer` 上,wgpu 往这一层画。CAMetalLayer 是该 view
//! 的**根层**,任何作为该 view **兄弟子视图(subview)**后加进去的 UIView 都在根层之上,
//! 因此可见。直接加到 key window 也行(window 是 view 的祖先),但复用 `ios_textbar.rs`
//! 同法——从 `raw_window_handle` 拿 winit 的 `ui_view` 再 `addSubview`——最稳、和现有叠层
//! 同一坐标系,故采用之。
//!
//! 线程:UIKit 全部对象必须在主线程创建/更新,靠 `MainThreadMarker` 在编译期/运行期把关。

use objc2::rc::Retained;
use objc2_foundation::{CGPoint, CGRect, CGSize, MainThreadMarker, NSString};
// 注意:UIEdgeInsets 由 objc2-ui-kit 提供(不是 foundation),需 "UIGeometry" feature;
// UIFontWeightRegular / monospacedSystemFontOfSize_weight 需 "UIFontDescriptor" feature。
use objc2_ui_kit::{
    NSTextAlignment, UIColor, UIEdgeInsets, UIFont, UIFontWeightRegular, UILabel, UIView,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// 左上角绿色多行调试 HUD。
///
/// 持有 `Retained<UILabel>` 保活——只要 `DebugHud` 在,label 就不会被释放、不会从父
/// view 掉下来。Drop 时(连带 Retained 引用计数归零)label 自动从父视图移除并销毁。
pub struct DebugHud {
    label: Retained<UILabel>,
    /// 主线程标记,保证后续 `set_text` 也在主线程(本类型因此 !Send/!Sync,符合 UIKit 约束)。
    mtm: MainThreadMarker,
    /// 顶部安全区内边距(逻辑点),布局时避开刘海/灵动岛。
    top_inset: f64,
}

impl DebugHud {
    /// 创建并挂载 HUD。必须在主线程调用。失败(非主线程 / 拿不到 winit UIView)返回 `None`。
    pub fn new(window: &Window) -> Option<Self> {
        // 主线程把关:非主线程直接 None,绝不在后台线程碰 UIKit。
        let mtm = MainThreadMarker::new()?;

        // 从 winit 拿它的 UIView(CAMetalLayer 就挂在这个 view 上)。
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::UiKit(h) = handle.as_raw() else {
            return None;
        };
        // SAFETY: winit 保证窗口存活期间该指针是有效 UIView。
        let ui_view: &UIView = unsafe { &*(h.ui_view.as_ptr() as *const UIView) };

        // 顶部安全区:有刘海/灵动岛时用 safeAreaInsets.top,否则给个保底 60 点避开状态栏。
        let insets: UIEdgeInsets = ui_view.safeAreaInsets();
        let top_inset = if insets.top > 0.0 { insets.top } else { 60.0 };

        let label = unsafe { UILabel::new(mtm) };

        unsafe {
            // 绿色文字。
            label.setTextColor(Some(&UIColor::greenColor()));
            // 等宽字体(常规字重),13 点。等宽利于对齐数值列。
            let font = UIFont::monospacedSystemFontOfSize_weight(13.0, UIFontWeightRegular);
            label.setFont(Some(&font));
            // 多行:0 = 不限行数。
            label.setNumberOfLines(0);
            // 左对齐。
            label.setTextAlignment(NSTextAlignment::Left);
            // 不吃触摸——触摸穿透回 winit view(否则会挡住游戏输入)。
            label.setUserInteractionEnabled(false);
        }

        // 半透明黑底(α=0.45),让绿字在任意画面上都看得清。
        let bg = unsafe { UIColor::colorWithRed_green_blue_alpha(0.0, 0.0, 0.0, 0.45) };
        label.setBackgroundColor(Some(&bg));

        // 初始 frame:左上角,避开刘海。宽度给个保守值,稍后 set_text 会按内容布局。
        label.setFrame(initial_frame(top_inset));

        // 加到 winit 的 UIView 之上(CAMetalLayer 之上),HUD 即可见。
        unsafe { ui_view.addSubview(&label) };

        Some(Self {
            label,
            mtm,
            top_inset,
        })
    }

    /// 更新 HUD 文本。必须在主线程调用(由 `self.mtm` 在类型层面保证调用者持有标记)。
    ///
    /// 每次按内容重新计算高度并贴回左上角,文本变长/变短都能自适应。
    pub fn set_text(&self, text: &str) {
        let _ = self.mtm; // 显式说明:本方法依赖 self 仍处于主线程上下文。
        let ns = NSString::from_str(text);
        unsafe { self.label.setText(Some(&ns)) };

        // 按内容自适应高度:sizeToFit 后,把 frame 钉回左上角(避开刘海),并加内边距。
        unsafe { self.label.sizeToFit() };
        let fitted: CGRect = self.label.frame();
        const PAD_X: f64 = 8.0;
        const PAD_Y: f64 = 4.0;
        self.label.setFrame(CGRect {
            origin: CGPoint {
                x: 8.0,
                y: self.top_inset,
            },
            size: CGSize {
                width: fitted.size.width + PAD_X * 2.0,
                height: fitted.size.height + PAD_Y * 2.0,
            },
        });
    }

    /// 显示 / 隐藏 HUD(不销毁,随时可再显示)。
    pub fn set_visible(&self, visible: bool) {
        self.label.setHidden(!visible);
    }
}

/// HUD 初始 frame:左上角,y 避开刘海,给个保守宽高(随后 set_text 会按内容覆盖)。
fn initial_frame(top_inset: f64) -> CGRect {
    CGRect {
        origin: CGPoint { x: 8.0, y: top_inset },
        size: CGSize {
            width: 320.0,
            height: 120.0,
        },
    }
}

// ───────────────────────── 设备热状态(温度的公开代理)─────────────────────────
// iOS 不暴露真实摄氏度,公开 API 只有 NSProcessInfo.thermalState(正常/偏热/过热/严重)。
// 用 objc2 的 msg_send FFI 直取,免给 objc2-foundation 加 NSProcessInfo feature。任意线程可调。
pub fn thermal_state() -> &'static str {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    let state: isize = unsafe {
        let pi: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
        msg_send![pi, thermalState]
    };
    match state {
        0 => "正常", // Nominal
        1 => "偏热", // Fair
        2 => "过热", // Serious
        3 => "严重", // Critical
        _ => "未知",
    }
}

// ───────────────────────── 网络状态(在线/WiFi/蜂窝)─────────────────────────
// SystemConfiguration 的 SCNetworkReachability(同步、可轮询)。8.8.8.8 纯地址查询,
// 不触发本地网络隐私弹窗、无需 entitlement。模拟器恒为 WiFi/离线(无蜂窝)。
mod net {
    use std::os::raw::{c_char, c_int, c_void};
    use std::ptr;

    type SCNetworkReachabilityRef = *const c_void;
    type Flags = u32;

    const REACHABLE: Flags = 1 << 1;
    const CONNECTION_REQUIRED: Flags = 1 << 2;
    const CONNECTION_ON_TRAFFIC: Flags = 1 << 3;
    const INTERVENTION_REQUIRED: Flags = 1 << 4;
    const CONNECTION_ON_DEMAND: Flags = 1 << 5;
    const IS_WWAN: Flags = 1 << 18; // 仅 iOS:蜂窝

    #[link(name = "SystemConfiguration", kind = "framework")]
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn SCNetworkReachabilityCreateWithName(
            allocator: *const c_void,
            nodename: *const c_char,
        ) -> SCNetworkReachabilityRef;
        fn SCNetworkReachabilityGetFlags(target: SCNetworkReachabilityRef, flags: *mut Flags) -> u8;
        fn CFRelease(cf: *const c_void);
    }

    fn parse(flags: Flags) -> &'static str {
        let reachable = flags & REACHABLE != 0;
        let needs_conn = flags & CONNECTION_REQUIRED != 0;
        let auto_conn = (flags & (CONNECTION_ON_TRAFFIC | CONNECTION_ON_DEMAND)) != 0
            && (flags & INTERVENTION_REQUIRED) == 0;
        if !(reachable && (!needs_conn || auto_conn)) {
            return "离线";
        }
        if flags & IS_WWAN != 0 {
            "蜂窝"
        } else {
            "WiFi"
        }
    }

    /// 当前网络状态:"离线" | "蜂窝" | "WiFi"。内部 CFRelease,无泄漏。
    pub fn status() -> &'static str {
        let host = b"8.8.8.8\0".as_ptr() as *const c_char;
        unsafe {
            let r = SCNetworkReachabilityCreateWithName(ptr::null(), host);
            if r.is_null() {
                return "离线";
            }
            let mut flags: Flags = 0;
            let ok = SCNetworkReachabilityGetFlags(r, &mut flags as *mut _);
            CFRelease(r as *const c_void);
            let _ = c_int::from(0);
            if ok == 0 {
                "离线"
            } else {
                parse(flags)
            }
        }
    }
}

pub fn network_status() -> &'static str {
    net::status()
}
