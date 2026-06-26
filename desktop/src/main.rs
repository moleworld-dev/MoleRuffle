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
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ruffle_core::backend::navigator::{OwnedFuture, SocketMode};
use ruffle_core::events::{MouseButton as RuffleButton, MouseWheelDelta};
use ruffle_core::{FloatDuration, Player, PlayerBuilder, PlayerEvent};
use ruffle_frontend_utils::backends::audio::CpalAudioBackend;
use ruffle_frontend_utils::backends::navigator::{ExternalNavigatorBackend, FutureSpawner};
use ruffle_frontend_utils::content::{ContentDescriptor, PlayingContent};
use ruffle_render::backend::ViewportDimensions;
use ruffle_render_wgpu::backend::WgpuRenderBackend;

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use moleruffle_core as mole;

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
    last_tick: Instant,
    frames: u64,
}

/// 进入 tokio 运行时上下文(reqwest / socket 异步依赖它)。
macro_rules! enter_runtime {
    ($self:ident) => {
        let _guard = $self.runtime.enter();
    };
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
            last_tick: Instant::now(),
            frames: 0,
        })
    }

    fn build_player(&self, window: Arc<Window>) -> Arc<Mutex<Player>> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

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

        let mut builder = PlayerBuilder::new()
            .with_renderer(renderer)
            .with_navigator(navigator)
            // 设备字体后端:用系统字体 + 中文回退,解决动态文本缺字问题
            .with_ui(mole::MoleUiBackend::with_system_fonts());
        match CpalAudioBackend::new(None) {
            Ok(audio) => builder = builder.with_audio(audio),
            Err(e) => tracing::warn!("无音频后端: {e}"),
        }
        builder = mole::apply_mole_settings(builder);

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
        player
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
        let attrs = Window::default_attributes()
            .with_title(mole::WINDOW_TITLE)
            .with_inner_size(LogicalSize::new(mole::STAGE_WIDTH, mole::STAGE_HEIGHT));
        let window = Arc::new(el.create_window(attrs).expect("创建窗口失败"));
        let sz = window.inner_size();
        tracing::info!(
            "resumed: 窗口 {}x{} scale={}",
            sz.width,
            sz.height,
            window.scale_factor()
        );
        let player = self.build_player(window.clone());
        window.request_redraw();
        self.window = Some(window);
        self.player = Some(player);
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
            WindowEvent::Resized(size) => {
                let scale = self
                    .window
                    .as_ref()
                    .map(|w| w.scale_factor())
                    .unwrap_or(1.0);
                self.with_player(|p| {
                    p.set_viewport_dimensions(ViewportDimensions {
                        width: size.width.max(1),
                        height: size.height.max(1),
                        scale_factor: scale,
                    })
                });
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
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
                        x: position.x,
                        y: position.y,
                    })
                });
            }
            WindowEvent::CursorLeft { .. } => {
                self.with_player(|p| p.handle_event(PlayerEvent::MouseLeave));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let btn = match button {
                    MouseButton::Left => RuffleButton::Left,
                    MouseButton::Right => RuffleButton::Right,
                    MouseButton::Middle => RuffleButton::Middle,
                    _ => RuffleButton::Unknown,
                };
                let (x, y) = (self.mouse_pos.x, self.mouse_pos.y);
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
            _ => {}
        }
    }

    // Poll 下持续触发:推进 tokio 异步 + 请求下一帧(渲染在 RedrawRequested 做)
    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        enter_runtime!(self);
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("warn,ruffle=info,avm_trace=info,moleruffle=info")
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tracing::info!("MoleRuffle 桌面端启动,加载 {}", mole::GAME_SWF_URL);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy)?;
    event_loop.run_app(&mut app)?;
    Ok(())
}
