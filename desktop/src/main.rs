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
use std::time::{Duration, Instant};

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
/// iOS 绿色多行调试 HUD(原生 UILabel 叠层,不吃触摸)。
#[cfg(target_os = "ios")]
#[allow(dead_code)]
mod ios_debug_hud;

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
    /// 上次主动重置纹理池的时刻,限频用(避免反复重建 surface 卡顿)。
    last_pool_trim: Instant,
    /// 上次内存观测/守卫评估的时刻。锁帧后 RedrawRequested 稀疏,故守卫改时间驱动(在 about_to_wait)。
    last_mem_check: Instant,
    /// 上次同步给引擎的绘制尺寸(物理像素),用于检测 iOS 布局/旋转后的尺寸变化。
    viewport: (u32, u32),
    /// 软键盘请求标志(来自 MoleUiBackend)+ 当前是否已开启,用于按需 set_ime_allowed。
    kbd: Option<Arc<AtomicBool>>,
    kbd_on: bool,
    /// iOS 纯触摸复制/粘贴工具条(文本框聚焦时显示)。
    #[cfg(target_os = "ios")]
    textbar: Option<ios_textbar::TextBar>,
    /// 渲染后端 + GPU 名(build_player 时从 wgpu adapter 抓,给 HUD 显示)。
    render_api: String,
    /// 上次 HUD 取样时的渲染帧数,用于算实时 FPS。
    last_hud_frames: u64,
    /// 上次 HUD 取样时的累计逐出数,用于算每秒逐出数。
    last_evict: u64,
    /// iOS 绿色调试 HUD(左上角实时 FPS/内存/网络/渲染API/内核/温度)。
    #[cfg(target_os = "ios")]
    hud: Option<ios_debug_hud::DebugHud>,
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

/// 渲染降采样系数。**1.0 = 全原生分辨率(最高清)**,iOS 也用 1.0。
///
/// 内存真相(真机 JetsamEvent 实测):进游戏世界/切场景被 signal 9 杀,死因 `per-process-limit`、
/// 死时足迹 **3386MB**——撞的是 iOS 默认单 app 内存墙(12GB 机型默认仅给 ~3.4GB,跟 8GB 机型差不多)。
/// render_scale **确实**会放大 cacheAsBitmap/滤镜/blend 离屏目标(地图等),显存随面积平方增长,
/// 所以 1.0 比 0.7 更吃内存——但根因是内存墙太低 + 纹理池只进不出累积,不是分辨率本身。
///
/// 解法不再靠砍分辨率(0.6/0.7 换来的内存有限,画面却「纸糊」),而是:
/// ① `increased-memory-limit` entitlement 把墙抬到 ~9GB(见 ios/MoleRuffle.entitlements),1.0 峰值塞得下;
/// ② 运行时观测内存 + 余量过低时主动重置纹理池回收显存(见 RedrawRequested 里的 mem 逻辑)。
/// 故全平台统一 1.0 真高清。若极端机型仍紧,可临时降到 0.85(2438×1122,仍远超 960×560 源美术)。
const RENDER_SCALE: f64 = 1.0;

/// 自适应内存守卫阈值(iOS)。研究结论:~5GB 足迹里 ~3-4GB 是 Ruffle 不压缩的解码位图(动不了),
/// ~1GB 是纹理池(只进不出累积,**唯一可回收的大头**)。故守卫=按"距 jetsam 墙余量"主动清纹理池:
/// 余量本身已反映各设备墙高(iPhone 6143 / iPad 8191 / 老机型更低),同一套绝对阈值即自适应。
///   - SOFT:余量跌到这就提前回收(随累积上涨触发;大内存设备余量常年高于它→极少触发→几乎不卡)。
///   - URGENT:更紧急,缩短回收间隔抢救,逼近墙也能稳住、永不 OOM。
#[cfg(target_os = "ios")]
const MEM_SOFT_FLOOR_MB: u64 = 1500;
#[cfg(target_os = "ios")]
const MEM_URGENT_MB: u64 = 700;

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

/// 内嵌的 Ruffle 引擎 git rev(pin 在 Cargo.toml;ruffle_core 不导出库版本常量,故硬编码)。给 HUD 显示。
const RUFFLE_REV: &str = "Ruffle @304a3c9";

/// wgpu Backend → 友好字符串(给 HUD"渲染 API"行)。
#[cfg(target_os = "ios")]
fn backend_str(b: wgpu::Backend) -> &'static str {
    match b {
        wgpu::Backend::Metal => "Metal",
        wgpu::Backend::Vulkan => "Vulkan",
        wgpu::Backend::Dx12 => "Direct3D 12",
        wgpu::Backend::Gl => "OpenGL/GLES",
        wgpu::Backend::BrowserWebGpu => "WebGPU",
        wgpu::Backend::Noop => "Noop",
    }
}
#[cfg(not(target_os = "ios"))]
fn backend_str(b: wgpu::Backend) -> String {
    format!("{b:?}")
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
            last_pool_trim: Instant::now(),
            last_mem_check: Instant::now(),
            viewport: (0, 0),
            kbd: None,
            kbd_on: false,
            #[cfg(target_os = "ios")]
            textbar: None,
            render_api: String::from("?"),
            last_hud_frames: 0,
            last_evict: 0,
            #[cfg(target_os = "ios")]
            hud: None,
        })
    }

    fn build_player(&self, window: Arc<Window>) -> (Arc<Mutex<Player>>, Arc<AtomicBool>, String) {
        // 渲染尺寸用 render_dims(iOS 已 ×RENDER_SCALE 降采样,省显存避免 jetsam)
        let (width, height) = render_dims(&window);

        // 自建 wgpu device(复刻 ruffle for_window_unsafe + request_device,只改两处省内存):
        //   ① memory_hints: MemoryUsage —— 让 Metal 分配器更紧凑,省 ~150-400MB 冗余(默认 Performance 偏大块预分配);
        //   ② max_texture_dimension_2d 封顶 4096 —— 摩尔庄园源美术仅 960×560,足够;挡病态超大纹理分配。
        // ruffle 把 wgpu 及 create_wgpu_instance / Descriptors::new / SwapChainTarget::new 都 pub 出来了,无需 fork。
        let (renderer, render_api) = {
            use ruffle_render_wgpu::backend::create_wgpu_instance;
            use ruffle_render_wgpu::descriptors::Descriptors;
            use ruffle_render_wgpu::target::SwapChainTarget;

            let instance = create_wgpu_instance(wgpu::Backends::PRIMARY, wgpu::BackendOptions::default());
            let surface = unsafe {
                instance
                    .create_surface_unsafe(
                        wgpu::SurfaceTargetUnsafe::from_window(window.as_ref())
                            .expect("创建 wgpu surface target 失败"),
                    )
                    .expect("创建 wgpu surface 失败")
            };
            let adapter = futures::executor::block_on(instance.request_adapter(
                &wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                },
            ))
            .expect("无可用 GPU 适配器");

            // 抓渲染后端 + GPU 名给 HUD(必须在 adapter 被 move 进 Descriptors::new 之前)。
            let info = adapter.get_info();
            let render_api = format!("{} · {}", backend_str(info.backend), info.name);

            // limits:复刻 ruffle request_device(GLES3 起步 → 抬到适配器上限),仅多封顶纹理尺寸。
            let mut limits = wgpu::Limits::downlevel_webgl2_defaults();
            limits = limits.using_resolution(adapter.limits());
            limits = limits.using_alignment(adapter.limits());
            limits.max_uniform_buffer_binding_size = adapter.limits().max_uniform_buffer_binding_size;
            limits.max_inter_stage_shader_components = adapter.limits().max_inter_stage_shader_components;
            limits.max_color_attachments = 4;
            limits.max_texture_dimension_2d = limits.max_texture_dimension_2d.min(4096);

            let mut features = wgpu::Features::empty();
            for feature in [
                wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
                wgpu::Features::TEXTURE_COMPRESSION_BC,
                wgpu::Features::FLOAT32_FILTERABLE,
            ] {
                if adapter.features().contains(feature) {
                    features |= feature;
                }
            }

            let (device, queue) = futures::executor::block_on(adapter.request_device(
                &wgpu::DeviceDescriptor {
                    label: None,
                    required_features: features,
                    required_limits: limits,
                    memory_hints: wgpu::MemoryHints::MemoryUsage, // ★ 省分配器冗余
                    trace: wgpu::Trace::Off,
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                },
            ))
            .expect("创建 wgpu device 失败");

            let descriptors = Descriptors::new(instance, adapter, device, queue);
            let target =
                SwapChainTarget::new(surface, &descriptors.adapter, (width, height), &descriptors.device);
            let backend = WgpuRenderBackend::new(std::sync::Arc::new(descriptors), target)
                .expect("创建 wgpu 渲染后端失败");
            (backend, render_api)
        };

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
        (player, kbd, render_api)
    }

    fn with_player<R>(&self, f: impl FnOnce(&mut Player) -> R) -> Option<R> {
        let player = self.player.as_ref()?;
        let mut guard = player.lock().expect("player lock");
        Some(f(&mut guard))
    }

    /// iOS 内存观测 + 自适应守卫 + 刷新调试 HUD(时间驱动,在 about_to_wait 每 ~0.5s 调一次)。
    /// `since` = 距上次调用的间隔,用于算实时 FPS(渲染帧数差 / 间隔)。
    /// 守卫:余量跌破 SOFT/URGENT 阈值就主动重置纹理池(set_viewport 同尺寸 → TexturePool::new()
    /// 释放累积的 ~1GB 池纹理),把进程锁在墙下。
    #[cfg(target_os = "ios")]
    fn mem_hud_tick(&mut self, since: Duration) {
        let avail = mole::mem::available_mb().unwrap_or(0);
        let foot = mole::mem::footprint_mb().unwrap_or(0);
        let total = mole::mem::total_ram_mb();

        // 实时 FPS:这段时间真正渲染的帧数 / 间隔(锁帧后挂机近 0、有动画约 24-30)。
        let dframes = self.frames.saturating_sub(self.last_hud_frames);
        self.last_hud_frames = self.frames;
        let fps = if since.as_secs_f64() > 0.0 {
            (dframes as f64 / since.as_secs_f64()).round() as u32
        } else {
            0
        };

        // 常驻库位图纹理数/显存 + 每秒逐出数(来自 fork 的 ruffle_render::evict 计数器)。
        use std::sync::atomic::Ordering::Relaxed;
        let res_tex = ruffle_render::evict::RESIDENT_TEXTURES.load(Relaxed);
        let res_mb = ruffle_render::evict::RESIDENT_BYTES.load(Relaxed) / (1024 * 1024);
        let ev_total = ruffle_render::evict::EVICTIONS_TOTAL.load(Relaxed);
        let ev_s = (ev_total.saturating_sub(self.last_evict)) as f64 / since.as_secs_f64().max(0.001);
        self.last_evict = ev_total;

        // 刷新 HUD 文本。
        if let Some(hud) = &self.hud {
            let text = format!(
                "FPS {fps}\n内存 软件{foot} / 系统{total} MB\n余量 {avail} MB  温度 {temp}\n渲染 {api}\n常驻 {res_tex} 张 / {res_mb} MB  逐出 {ev_s:.0}/s\n网络 {net}   内核 {rev}",
                temp = ios_debug_hud::thermal_state(),
                api = self.render_api,
                net = ios_debug_hud::network_status(),
                rev = RUFFLE_REV,
            );
            hud.set_text(&text);
        }

        // 自适应内存守卫:余量低就主动回收纹理池。
        if avail > 0 {
            let urgent = avail < MEM_URGENT_MB;
            let min_gap = if urgent { 2 } else { 6 };
            if avail < MEM_SOFT_FLOOR_MB && self.last_pool_trim.elapsed().as_secs() >= min_gap {
                if let Some(w) = self.window.clone() {
                    let (width, height) = render_dims(&w);
                    let scale_factor = w.scale_factor();
                    self.with_player(|p| {
                        p.set_viewport_dimensions(ViewportDimensions {
                            width,
                            height,
                            scale_factor,
                        });
                    });
                    self.last_pool_trim = Instant::now();
                    tracing::warn!(
                        "内存守卫{}:余量 {avail}MB → 重置纹理池回收(清前足迹 {foot}MB)",
                        if urgent { "(急)" } else { "" }
                    );
                }
            }
        }
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
        let (player, kbd, render_api) = self.build_player(window.clone());
        self.render_api = render_api;
        window.request_redraw();
        // iOS:创建纯触摸复制/粘贴工具条(挂到 UIView,初始隐藏)+ 绿色调试 HUD(挂到 UIView,常显)
        #[cfg(target_os = "ios")]
        {
            self.textbar = ios_textbar::TextBar::new(&window);
            if self.textbar.is_some() {
                tracing::info!("iOS 文本工具条已创建");
            } else {
                tracing::warn!("iOS 文本工具条创建失败");
            }
            self.hud = ios_debug_hud::DebugHud::new(&window);
            tracing::info!("iOS 调试 HUD {}", if self.hud.is_some() { "已创建" } else { "创建失败" });
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
                // 只渲染当前帧。tick 已移到 about_to_wait,按 SWF 帧率(摩尔庄园 ~24-30fps)推进,
                // 且仅在 needs_render 时才会走到这里——不再按屏幕 60Hz 过度渲染,GPU/发热砍约一半。
                self.with_player(|p| p.render());
                self.frames += 1;
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

    // 锁帧驱动:推进 tokio 异步 + 按 SWF 帧率 tick + 仅脏时请求重绘 + 睡到下一帧(渲染在 RedrawRequested 做)
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
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
        // 帧率锁定 + 脏标记:按 SWF 帧率 tick,只在内容真变化(needs_render)时请求重绘,然后睡到下一帧。
        // 摩尔庄园 ~24-30fps,屏幕 60Hz 时这砍掉约一半 GPU/发热;挂机/对话框近乎 0 重绘。
        let now = Instant::now();
        let dt_ms = now.duration_since(self.last_tick).as_secs_f64() * 1000.0;
        self.last_tick = now;
        let (needs_render, til) = self
            .with_player(|p| {
                p.tick(FloatDuration::from_millis(dt_ms));
                (p.needs_render(), p.time_til_next_frame())
            })
            .unwrap_or((false, Duration::from_millis(16)));
        if needs_render {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }

        // 内存观测 + 自适应守卫 + 刷新调试 HUD:时间驱动(锁帧后空闲帧计数会停,故按 ~0.5s 间隔评估)。
        #[cfg(target_os = "ios")]
        {
            let since = self.last_mem_check.elapsed();
            if since.as_millis() >= 500 {
                self.last_mem_check = now;
                self.mem_hud_tick(since);
            }
        }

        // 睡到下一 SWF 帧(或被输入/异步事件提前唤醒),替代原来的 Poll 满速空转。
        el.set_control_flow(ControlFlow::WaitUntil(now + til));
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

    // 纹理逐出开关:读 MOLE_TEXTURE_EVICT(Phase 1 不设=保持关=行为同现状)。
    ruffle_render::evict::init_from_env();

    tracing::info!("MoleRuffle 桌面端启动,加载 {}", mole::GAME_SWF_URL);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy)?;
    event_loop.run_app(&mut app)?;
    Ok(())
}
