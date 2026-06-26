//! MoleRuffle 桌面壳(Windows / macOS / Linux)
//!
//! 一个最小化的 winit + wgpu 原生窗口,内嵌 Ruffle 引擎,
//! 开箱即把 `http://mole.61.com/Client.swf` 在线加载并进入游戏。
//!
//! 关键点:
//!   - tokio 运行时手动创建并在每个事件回调里 `enter()`,
//!     否则 reqwest 网络/裸 TCP socket 的异步任务跑不起来。
//!   - winit 事件循环必须在主线程(故 `main` 不用 `#[tokio::main]`)。
//!   - 摩尔庄园专属配置(spoof / base / socket 放行 / 中文字体)全在 `moleruffle-core`。

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ruffle_core::backend::navigator::{OwnedFuture, SocketMode};
use ruffle_core::events::{ImeEvent, MouseButton as RuffleButton, MouseWheelDelta};
use ruffle_core::{FloatDuration, Player, PlayerBuilder, PlayerEvent};
use ruffle_frontend_utils::backends::audio::CpalAudioBackend;
use ruffle_frontend_utils::backends::navigator::{ExternalNavigatorBackend, FutureSpawner};
use ruffle_frontend_utils::content::{ContentDescriptor, PlayingContent};
use ruffle_render::backend::ViewportDimensions;
use ruffle_render_wgpu::backend::WgpuRenderBackend;

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
// LogicalSize 只在桌面用(iOS 不设 inner_size,见 resumed)
#[cfg(not(target_os = "ios"))]
use winit::dpi::LogicalSize;
use winit::event::{
    ElementState, Ime, Modifiers, MouseButton, MouseScrollDelta, Touch, TouchPhase, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use moleruffle_core as mole;

mod keymap;
/// iOS 纯触摸复制/粘贴工具条(原生 UIView 叠层)。
#[cfg(target_os = "ios")]
mod ios_textbar;

/// winit 自定义事件:把 Ruffle 的异步任务调度回事件循环线程执行。
enum UserEvent {
    TaskPoll(async_task::Runnable),
}

/// Ruffle 的 `FutureSpawner`:把异步任务(网络 fetch、socket 等)
/// 通过事件循环代理调度回主线程逐步 poll。
#[derive(Clone)]
struct MoleExecutor {
    proxy: EventLoopProxy<UserEvent>,
}

impl<E: std::error::Error + 'static> FutureSpawner<E> for MoleExecutor {
    fn spawn(&self, future: OwnedFuture<(), E>) {
        let future = async move {
            if let Err(e) = future.await {
                tracing::error!("async task 出错: {e}");
            }
        };
        let proxy = self.proxy.clone();
        let schedule = move |runnable| {
            let _ = proxy.send_event(UserEvent::TaskPoll(runnable));
        };
        let (runnable, task) = async_task::spawn_local(future, schedule);
        task.detach();
        runnable.schedule();
    }
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    runtime: tokio::runtime::Runtime,
    window: Option<Arc<Window>>,
    player: Option<Arc<Mutex<Player>>>,
    mouse_pos: PhysicalPosition<f64>,
    modifiers: Modifiers,
    last_tick: Instant,
    frames: u64,
    /// 上次同步给引擎的绘制尺寸(物理像素),用于检测 iOS 布局/旋转后的尺寸变化。
    viewport: (u32, u32),
    /// 软键盘请求标志(来自 MoleUiBackend)+ 当前是否已开启,用于按需 set_ime_allowed。
    kbd: Option<Arc<AtomicBool>>,
    kbd_on: bool,
    /// iOS 纯触摸复制/粘贴工具条(文本框聚焦时显示)。
    #[cfg(target_os = "ios")]
    textbar: Option<ios_textbar::TextBar>,
}

/// 进入 tokio 运行时上下文(reqwest / socket 异步依赖它)。
macro_rules! enter_runtime {
    ($self:ident) => {
        let _guard = $self.runtime.enter();
    };
}

/// 渲染表面 / 视口尺寸(物理像素)。
///
/// iOS 用 `outer_size`(整个全屏 view),**不要**用 `inner_size`(安全区,内缩且偏小):
/// winit iOS 的 `Touch.location` 是相对全屏 window 的坐标、CAMetalLayer 也是全屏;
/// 若拿安全区尺寸当 viewport,渲染会被拉伸,且触摸坐标空间(全屏)≠ viewport(安全区)
/// → 触摸偏移(越靠边偏越多)。桌面用 `inner_size`(客户区,排除标题栏)。
fn surface_dims(window: &Window) -> (u32, u32) {
    #[cfg(target_os = "ios")]
    let s = window.outer_size();
    #[cfg(not(target_os = "ios"))]
    let s = window.inner_size();
    (s.width.max(1), s.height.max(1))
}

/// 渲染降采样系数。iOS 真机渲染全屏物理像素(如 2868×1320)。注意:进游戏世界的 SIGKILL(OOM)
/// 主要是 **MSAA**(离屏滤镜/cacheAsBitmap 目标按采样数倍增显存)——已用 StageQuality::Low 关掉,
/// 那才是救命的一刀;render_scale 只缩主屏 framebuffer(占用很小、不碰离屏目标),对 OOM 贡献有限。
/// 之前 0.6 砍掉了清晰度却换不来多少内存,反而画面/字体「纸糊」。故调回 **1.0 全原生分辨率**(真高清),
/// 内存靠关 MSAA 守住。若个别机型仍紧或要更高帧率,可微降到 0.85(2438×1122,仍远超 960×560 源美术)。
#[cfg(target_os = "ios")]
const RENDER_SCALE: f64 = 1.0;
#[cfg(not(target_os = "ios"))]
const RENDER_SCALE: f64 = 1.0;

/// 喂给 wgpu surface / 引擎 viewport 的渲染尺寸(物理像素 × RENDER_SCALE)。
/// 注意:**触摸坐标也必须 ×RENDER_SCALE** 才与缩小后的 viewport 一致(见 Touch 处理),
/// 否则重蹈"viewport 与触摸坐标空间不一致 → 触摸偏移"的坑。
/// iOS 文本工具条是叠在**全屏** UIView 上的原生层,其布局/命中仍用未缩放的 `surface_dims`。
fn render_dims(window: &Window) -> (u32, u32) {
    let (w, h) = surface_dims(window);
    if RENDER_SCALE == 1.0 {
        return (w, h);
    }
    (
        ((w as f64 * RENDER_SCALE).max(1.0)) as u32,
        ((h as f64 * RENDER_SCALE).max(1.0)) as u32,
    )
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> anyhow::Result<Self> {
        Ok(Self {
            proxy,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?,
            window: None,
            player: None,
            mouse_pos: PhysicalPosition::new(0.0, 0.0),
            modifiers: Modifiers::default(),
            last_tick: Instant::now(),
            frames: 0,
            viewport: (0, 0),
            kbd: None,
            kbd_on: false,
            #[cfg(target_os = "ios")]
            textbar: None,
        })
    }

    fn build_player(&self, window: Arc<Window>) -> (Arc<Mutex<Player>>, Arc<AtomicBool>) {
        // 渲染尺寸用 render_dims(iOS 已 ×RENDER_SCALE 降采样,省显存避免 jetsam)
        let (width, height) = render_dims(&window);

        let renderer = unsafe {
            WgpuRenderBackend::for_window_unsafe(
                wgpu::SurfaceTargetUnsafe::from_window(window.as_ref())
                    .expect("创建 wgpu surface target 失败"),
                (width, height),
                wgpu::Backends::PRIMARY,
                wgpu::PowerPreference::HighPerformance,
            )
        }
        .expect("创建 wgpu 渲染后端失败");

        let content = Rc::new(PlayingContent::DirectFile(ContentDescriptor::new_remote(
            mole::game_swf_url(),
        )));

        let navigator = ExternalNavigatorBackend::new(
            mole::game_base_url(),
            None,
            None,
            MoleExecutor {
                proxy: self.proxy.clone(),
            },
            None,
            false,
            HashSet::new(),
            SocketMode::Allow, // 等价桌面版 --tcp-connections allow
            content,
            mole::MoleNavigatorInterface,
        );
        // ★ 本地资源缓存(CDN 思路):静态资源 SWF/图片缓存到磁盘,二次加载秒开且免网络,
        //   缓解 mole.61.com 慢/抖动导致的"维护"与每次重下。socket/登录动态请求不走缓存。
        let navigator = mole::CachingNavigator::new(navigator, mole::cache_dir());

        // 设备字体后端:系统字体 + 中文回退;并取软键盘标志(文本框聚焦时弹键盘)
        let ui = mole::MoleUiBackend::with_system_fonts();
        let kbd = ui.keyboard_flag();
        let mut builder = PlayerBuilder::new()
            .with_renderer(renderer)
            .with_navigator(navigator)
            .with_ui(ui);
        match CpalAudioBackend::new(None) {
            Ok(audio) => builder = builder.with_audio(audio),
            Err(e) => tracing::warn!("无音频后端: {e}"),
        }
        builder = mole::apply_mole_settings(builder);
        // ★ 磁盘存储后端:不装的话 PlayerBuilder 默认用 MemoryStorageBackend(纯内存),
        //   Flash SharedObject(登录页“记住账号”等)进程一退就丢、重启不保存。
        //   指向各端可写且重启/更新保留的数据目录(iOS=沙盒 Library/Application Support)。
        builder = mole::attach_storage(builder);

        let player = builder.build();
        {
            let mut p = player.lock().expect("player lock");
            p.set_viewport_dimensions(ViewportDimensions {
                width,
                height,
                scale_factor: window.scale_factor(),
            });
            mole::set_mole_fonts(&mut p);
            p.fetch_root_movie(mole::GAME_SWF_URL.to_string(), vec![], Box::new(|_| {}));
        }
        (player, kbd)
    }

    fn with_player<R>(&self, f: impl FnOnce(&mut Player) -> R) -> Option<R> {
        let player = self.player.as_ref()?;
        let mut guard = player.lock().expect("player lock");
        Some(f(&mut guard))
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        enter_runtime!(self);
        // ★ inner_size 只在桌面设。winit iOS 会把 inner_size 当成 UIView/UIWindow 的固定 frame
        //   (winit ios window.rs:510),设成 960x560 会让 view 不铺满全屏 → winit 上报的 viewport
        //   偏小且与真实屏幕/drawable 不一致 → Ruffle 的 ShowAll 基于错误视口计算 → 画面溢出、
        //   左右被裁、按钮点不到(实测现象)。iOS 不设 inner_size,让 winit 用全屏 screen_bounds。
        let attrs = Window::default_attributes().with_title(mole::WINDOW_TITLE);
        #[cfg(not(target_os = "ios"))]
        let attrs = attrs.with_inner_size(LogicalSize::new(mole::STAGE_WIDTH, mole::STAGE_HEIGHT));
        // ★ iOS 强制横屏:摩尔庄园是 960x560 横屏游戏,但 winit 默认 valid_orientations =
        //   LandscapeAndPortrait,会让 view controller 的 supportedInterfaceOrientations 允许竖屏,
        //   于是 app 停在竖屏、横屏内容被旋转 90° 塞进竖屏窗口。设成 Landscape 让它只报横屏,
        //   iOS 启动即旋转到横屏;顺带隐藏 home indicator 做全屏。
        #[cfg(target_os = "ios")]
        let attrs = {
            use winit::platform::ios::{ValidOrientations, WindowAttributesExtIOS};
            attrs
                .with_valid_orientations(ValidOrientations::Landscape)
                .with_prefers_home_indicator_hidden(true)
        };
        let window = Arc::new(el.create_window(attrs).expect("创建窗口失败"));
        // 软键盘/IME 不在这里无条件开启;改由文本框聚焦时(MoleUiBackend 标志)按需 set_ime_allowed,
        // 这样 iOS 不会一进来就弹软键盘,桌面 CJK 输入也只在需要时启用。
        let sz = window.inner_size();
        tracing::info!(
            "resumed: 窗口 {}x{} scale={}",
            sz.width,
            sz.height,
            window.scale_factor()
        );
        let (player, kbd) = self.build_player(window.clone());
        window.request_redraw();
        // iOS:创建纯触摸复制/粘贴工具条(挂到 UIView,初始隐藏)
        #[cfg(target_os = "ios")]
        {
            self.textbar = ios_textbar::TextBar::new(&window);
            if self.textbar.is_some() {
                tracing::info!("iOS 文本工具条已创建");
            } else {
                tracing::warn!("iOS 文本工具条创建失败");
            }
        }
        self.window = Some(window);
        self.player = Some(player);
        self.kbd = Some(kbd);
        self.last_tick = Instant::now();
        // Poll:让 about_to_wait 持续触发去 request_redraw,
        // 渲染只在 RedrawRequested 里做(iOS 要求渲染在 RedrawRequested 阶段)。
        el.set_control_flow(ControlFlow::Poll);
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, event: UserEvent) {
        enter_runtime!(self);
        match event {
            UserEvent::TaskPoll(runnable) => {
                runnable.run();
            }
        }
        // 异步任务(如 SWF 加载完成)后引导/维持 redraw 循环。
        // iOS 上 resumed 里的首次 request_redraw 可能在窗口就绪前丢失,
        // 这里在 SWF/资源加载事件后再请求,确保渲染循环被启动。
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        enter_runtime!(self);
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(_size) => {
                // 忽略事件携带的 size(iOS 上可能是安全区/全屏不一致),统一用 render_dims(已降采样)。
                if let Some(w) = &self.window {
                    let (width, height) = render_dims(w);
                    let scale = w.scale_factor();
                    let inner = w.inner_size();
                    self.viewport = (width, height);
                    tracing::info!(
                        "Resized: render={}x{} (inner={}x{}) scale={}",
                        width, height, inner.width, inner.height, scale
                    );
                    self.with_player(|p| {
                        p.set_viewport_dimensions(ViewportDimensions {
                            width,
                            height,
                            scale_factor: scale,
                        })
                    });
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                // 每帧同步真实绘制尺寸:iOS 上 resumed 时拿到的尺寸可能是布局未稳的过渡值,
                // 且旋转/布局变化不一定发干净的 Resized,这里轮询纠正,避免 UI 溢出/缩放错/触摸偏移。
                if let Some(w) = &self.window {
                    let (width, height) = render_dims(w);
                    let scale = w.scale_factor();
                    if (width, height) != self.viewport && width > 0 && height > 0 {
                        self.viewport = (width, height);
                        let inner = w.inner_size();
                        tracing::info!(
                            "viewport 同步 render={}x{} (inner={}x{}) scale={}",
                            width, height, inner.width, inner.height, scale
                        );
                        self.with_player(|p| {
                            p.set_viewport_dimensions(ViewportDimensions {
                                width,
                                height,
                                scale_factor: scale,
                            })
                        });
                    }
                }
                let now = Instant::now();
                let dt_ms = now.duration_since(self.last_tick).as_secs_f64() * 1000.0;
                self.last_tick = now;
                self.with_player(|p| {
                    p.tick(FloatDuration::from_millis(dt_ms));
                    p.render();
                });
                self.frames += 1;
                if self.frames % 60 == 1 {
                    tracing::info!("render frame #{}", self.frames);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = position;
                self.with_player(|p| {
                    p.handle_event(PlayerEvent::MouseMove {
                        x: position.x * RENDER_SCALE,
                        y: position.y * RENDER_SCALE,
                    })
                });
            }
            WindowEvent::CursorLeft { .. } => {
                self.with_player(|p| p.handle_event(PlayerEvent::MouseLeave));
            }
            // ★ 触摸(iOS/Android):winit 在触屏上发 Touch 而非鼠标事件,
            //   这里把单指触摸翻译成鼠标:按下=移动到该点+按下,移动=拖动,抬起=移动到落点+松开。
            //   没有这段,手机上点任何东西都没反应。location 是物理像素,与 ViewportDimensions 一致。
            WindowEvent::Touch(Touch {
                phase, location, ..
            }) => {
                let (x, y) = (location.x, location.y);
                self.mouse_pos = location;
                // iOS:先看是否点中纯触摸文本工具条(粘贴/复制/剪切/全选)。命中则发 TextControl
                // 并吞掉该次触摸(不当作游戏点击转发);只在按下时触发一次,Moved/Ended 一并吞掉。
                #[cfg(target_os = "ios")]
                if let Some(tb) = &self.textbar {
                    if let Some(action) = tb.hit_test(x, y) {
                        if phase == TouchPhase::Started {
                            use ios_textbar::TextAction;
                            use ruffle_core::events::TextControlCode;
                            let (code, name) = match action {
                                TextAction::Copy => (TextControlCode::Copy, "复制"),
                                TextAction::Cut => (TextControlCode::Cut, "剪切"),
                                TextAction::SelectAll => (TextControlCode::SelectAll, "全选"),
                            };
                            self.with_player(|p| {
                                p.handle_event(PlayerEvent::TextControl { code })
                            });
                            tracing::info!("文本工具条:{name}");
                        }
                        return;
                    }
                }
                // 游戏用缩放后的坐标:viewport 已 ×RENDER_SCALE,触摸(全屏物理像素)也必须同比缩,
                // 否则触摸空间(全屏)≠ viewport(缩小)→ 又触摸偏移。工具条命中已在上面用全屏坐标判过。
                let (x, y) = (x * RENDER_SCALE, y * RENDER_SCALE);
                self.with_player(|p| match phase {
                    TouchPhase::Started => {
                        p.handle_event(PlayerEvent::MouseMove { x, y });
                        p.handle_event(PlayerEvent::MouseDown {
                            x,
                            y,
                            button: RuffleButton::Left,
                            index: None,
                        });
                    }
                    TouchPhase::Moved => {
                        p.handle_event(PlayerEvent::MouseMove { x, y });
                    }
                    TouchPhase::Ended | TouchPhase::Cancelled => {
                        // 先把指针对齐到抬手落点,再松开,保证点击命中正确控件
                        p.handle_event(PlayerEvent::MouseMove { x, y });
                        p.handle_event(PlayerEvent::MouseUp {
                            x,
                            y,
                            button: RuffleButton::Left,
                        });
                        // 触摸抬起后没有持续光标,清掉 hover 状态(否则按钮一直高亮)
                        p.handle_event(PlayerEvent::MouseLeave);
                    }
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let btn = match button {
                    MouseButton::Left => RuffleButton::Left,
                    MouseButton::Right => RuffleButton::Right,
                    MouseButton::Middle => RuffleButton::Middle,
                    _ => RuffleButton::Unknown,
                };
                let (x, y) = (self.mouse_pos.x * RENDER_SCALE, self.mouse_pos.y * RENDER_SCALE);
                let ev = match state {
                    ElementState::Pressed => PlayerEvent::MouseDown {
                        x,
                        y,
                        button: btn,
                        index: None,
                    },
                    ElementState::Released => PlayerEvent::MouseUp { x, y, button: btn },
                };
                self.with_player(|p| p.handle_event(ev));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 100.0,
                };
                self.with_player(|p| {
                    p.handle_event(PlayerEvent::MouseWheel {
                        delta: MouseWheelDelta::Lines(lines),
                    })
                });
            }
            WindowEvent::ModifiersChanged(new) => {
                self.modifiers = new;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let key = keymap::winit_input_to_ruffle_key_descriptor(&event);
                let modifiers = self.modifiers;
                self.with_player(|p| match event.state {
                    ElementState::Pressed => {
                        p.handle_event(PlayerEvent::KeyDown { key });
                        if let Some(code) = keymap::winit_to_ruffle_text_control(&event, modifiers) {
                            // 复制/粘贴/剪切/全选/回车/光标移动等
                            p.handle_event(PlayerEvent::TextControl { code });
                        } else if let Some(text) = &event.text {
                            // 普通字符输入(英文/数字/符号)
                            for codepoint in text.chars() {
                                p.handle_event(PlayerEvent::TextInput { codepoint });
                            }
                        }
                    }
                    ElementState::Released => {
                        p.handle_event(PlayerEvent::KeyUp { key });
                    }
                });
            }
            WindowEvent::Ime(ime) => {
                // 输入法(中文等):预编辑 + 提交
                self.with_player(|p| match ime {
                    Ime::Preedit(text, cursor) => {
                        p.handle_event(PlayerEvent::Ime(ImeEvent::Preedit(text, cursor)));
                    }
                    Ime::Commit(text) => {
                        p.handle_event(PlayerEvent::Ime(ImeEvent::Commit(text)));
                    }
                    Ime::Enabled | Ime::Disabled => {}
                });
            }
            _ => {}
        }
    }

    // Poll 下持续触发:推进 tokio 异步 + 请求下一帧(渲染在 RedrawRequested 做)
    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        enter_runtime!(self);
        // 软键盘 + iOS 文本工具条按需开关:Flash 文本框聚焦→标志 true→弹软键盘+显示工具条;失焦→收起。
        if let Some(kbd) = &self.kbd {
            let want = kbd.load(Ordering::Relaxed);
            if want != self.kbd_on {
                self.kbd_on = want;
                if let Some(w) = &self.window {
                    w.set_ime_allowed(want);
                }
                tracing::info!("软键盘 {}", if want { "开" } else { "关" });
                // iOS:同步显示/隐藏纯触摸复制粘贴工具条(显示前按当前屏宽居中布局)
                #[cfg(target_os = "ios")]
                {
                    if let (Some(tb), Some(w)) = (self.textbar.as_mut(), self.window.as_ref()) {
                        if want {
                            let scale = w.scale_factor();
                            let (w_px, _) = surface_dims(w);
                            tb.layout(w_px as f64 / scale, scale);
                        }
                        tb.set_visible(want);
                    }
                    // 文本框失焦时清掉粘贴桥缓冲,避免下次拿到陈旧内容
                    if !want {
                        mole::paste_bridge::clear();
                    }
                }
            }
        }
        // iOS:UIPasteControl 授权投递后,这里发一次 TextControl::Paste 让 Ruffle 取 clipboard_content
        #[cfg(target_os = "ios")]
        if mole::paste_bridge::take_pending() {
            self.with_player(|p| {
                p.handle_event(PlayerEvent::TextControl {
                    code: ruffle_core::events::TextControlCode::Paste,
                })
            });
            tracing::info!("UIPasteControl→粘贴已注入");
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // winit=error:iOS 上用 Poll 驱动重绘时,winit 会在 ProcessingRedraws 阶段收到 AboutToWait,
        // 每帧刷屏式 warn("processing non RedrawRequested event ...")。这是该 winit 版本 iOS
        // 重绘模型的固有副产物(request_redraw 只能在事件阶段调,不能在 RedrawRequested 里调),
        // 无害但拖累帧率,这里压到 error 消除其每帧 {:#?} 格式化开销。
        tracing_subscriber::EnvFilter::new("warn,winit=error,ruffle=info,avm_trace=info,moleruffle=info")
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tracing::info!("MoleRuffle 桌面端启动,加载 {}", mole::GAME_SWF_URL);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy)?;
    event_loop.run_app(&mut app)?;
    Ok(())
}
