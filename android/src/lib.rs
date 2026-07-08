mod audio;
mod custom_event;
mod java;
mod keycodes;
mod navigator;
mod trace;
mod ui_backend;

use custom_event::RuffleEvent;

use jni::{
    objects::{JObject, JString},
    sys::{self, jint, jobject},
    JNIEnv, JavaVM,
};
use keycodes::{android_key_event_to_ruffle_key_descriptor, key_tag_to_key_descriptor};
use std::any::Any;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{mpsc, MutexGuard};
use std::time::Duration;
use std::{
    panic,
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};
use wgpu::rwh::{AndroidDisplayHandle, HasWindowHandle, RawDisplayHandle};

use android_activity::input::{InputEvent, KeyAction, MotionAction};
use android_activity::{AndroidApp, AndroidAppWaker, InputStatus, MainEvent, PollEvent};
use backtrace::Backtrace;
use jni::objects::JClass;

use audio::AAudioAudioBackend;
use url::Url;

use ruffle_common::duration::FloatDuration;
use ruffle_core::{
    backend::navigator::OwnedFuture,
    events::{LogicalKey, MouseButton, PlayerEvent},
    tag_utils::SwfMovie,
    Player, PlayerBuilder, StageScaleMode, ViewportDimensions,
};

// MoleRuffle:摩尔庄园固定加载点 + 域名 spoof(缺 spoof 会被 Client.swf navigateToURL 弹走进不去)。
const MOLE_SWF_URL: &str = "http://mole.61.com/Client.swf";
use ruffle_frontend_utils::backends::storage::DiskStorageBackend;
use ruffle_frontend_utils::content::PlayingContent;
use ruffle_frontend_utils::{
    backends::navigator::{ExternalNavigatorBackend, FutureSpawner},
    content::ContentDescriptor,
};

use crate::navigator::AndroidNavigatorInterface;
use crate::trace::FileLogBackend;
use java::JavaInterface;
use ruffle_render_wgpu::{backend::WgpuRenderBackend, target::SwapChainTarget};

// MoleRuffle:性能监视 HUD 的全局状态。渲染循环(android_main 线程)写入,
//   JNI getPerfStats(UI 线程)读取。用原子/Mutex 免加锁竞争。
mod perf {
    use std::sync::atomic::AtomicU32;
    use std::sync::Mutex;

    pub static FPS: AtomicU32 = AtomicU32::new(0);
    /// 最近一帧 tick 间隔,微秒(帧时间)。
    pub static FRAME_US: AtomicU32 = AtomicU32::new(0);
    pub static SURFACE_W: AtomicU32 = AtomicU32::new(0);
    pub static SURFACE_H: AtomicU32 = AtomicU32::new(0);
    /// 渲染后端名(Vulkan / Gl 等),渲染器创建时写一次。
    pub static BACKEND: Mutex<String> = Mutex::new(String::new());
}

/// A unique identifier for a given `Player` instance.
/// Used to track which player any currently executing future is bound to.
#[derive(Copy, Clone, Eq, PartialEq)]
struct PlayerId(i64);

impl PlayerId {
    fn new() -> Self {
        use std::sync::atomic::{AtomicI64, Ordering};

        static NEXT: AtomicI64 = AtomicI64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(id >= 0, "PlayerId overflowed!");
        Self(id)
    }
}

/// A `Player`-bound future that is currently running.
pub struct PlayerRunnable(async_task::Runnable<PlayerId>);

/// Represents a current Player and any associated state with that player,
/// which may be lost when this Player is closed (dropped)
struct ActivePlayer {
    id: PlayerId,
    player: Arc<Mutex<Player>>,
}

#[derive(Clone)]
pub struct EventSender {
    sender: Sender<RuffleEvent>,
    waker: AndroidAppWaker,
}

impl EventSender {
    pub fn send(&self, event: RuffleEvent) {
        if self.sender.send(event).is_ok() {
            self.waker.wake();
        }
    }
}

/// A bare-bones executor that schedules tasks on the winit event loop.
struct AndroidExecutor {
    event_loop: EventSender,
    player_id: PlayerId,
}

impl<E: std::error::Error + 'static> FutureSpawner<E> for AndroidExecutor {
    fn spawn(&self, future: OwnedFuture<(), E>) {
        // Discard any errors.
        let future = async {
            if let Err(e) = future.await {
                tracing::error!("Async error: {}", e);
            }
        };

        let event_loop = self.event_loop.clone();
        let scheduler = move |task| {
            let event = RuffleEvent::TaskPoll(PlayerRunnable(task));
            event_loop.send(event)
        };

        let (runnable, task) = async_task::Builder::new()
            .metadata(self.player_id)
            .spawn_local(|_| future, scheduler);

        // The future should run in the background.
        task.detach();
        // Immediately schedule the future to be polled for the first time.
        runnable.schedule();
    }
}

#[tokio::main]
async fn run(app: AndroidApp) {
    let mut last_frame_time = Instant::now();
    let mut next_frame_time = Some(Instant::now());
    let mut quit = false;
    let (sender, receiver) = mpsc::channel::<RuffleEvent>();
    let mut native_window: Option<ndk::native_window::NativeWindow> = None;
    let mut playerbox: Option<ActivePlayer> = None;
    // MoleRuffle:性能 HUD 采样(每 500ms 算一次 FPS)。
    let mut frame_count: u64 = 0;
    let mut last_hud_time = Instant::now();
    let mut last_hud_frames: u64 = 0;
    // MoleRuffle:系统软键盘状态 + 上一次 IME 文本(用于增删 diff)。
    let mut soft_kb_visible = false;
    let mut last_ime_text = String::new();
    let sender = EventSender {
        sender,
        waker: app.create_waker(),
    };

    log::info!("Starting event loop...");
    let trace_output;
    let android_storage_dir;

    unsafe {
        let vm = JavaVM::from_raw(app.vm_as_ptr() as *mut sys::JavaVM).expect("JVM must exist");
        let activity = JObject::from_raw(app.activity_as_ptr() as jobject);
        let mut jni_env = vm.get_env().unwrap();
        trace_output = JavaInterface::get_trace_output(&mut jni_env, &activity);
        android_storage_dir = JavaInterface::get_android_data_storage_dir(&mut jni_env, &activity);
        let _ = jni_env.set_rust_field(activity, "eventLoopHandle", sender.clone());
    }

    while !quit {
        let mut needs_redraw = false;
        app.poll_events(
            Some(
                next_frame_time
                    .and_then(|next| next.checked_duration_since(last_frame_time))
                    .unwrap_or_else(|| Duration::from_millis(100)),
            ),
            |event| {
                match event {
                    PollEvent::Main(event) => match event {
                        MainEvent::Destroy => {
                            if let Some(player) = playerbox.as_ref() {
                                let mut player_lock = player.player.lock().unwrap();
                                player_lock.flush_shared_objects();
                            }
                            quit = true;
                        }
                        MainEvent::WindowResized { .. } => {
                            if let Some(player) = playerbox.as_ref() {
                                let mut player_lock = player.player.lock().unwrap();
                                let window = native_window
                                    .as_ref()
                                    .expect("native_window should be Some for a WindowResized");
                                log::info!(
                                    "WindowResized: {} x {}",
                                    window.width(),
                                    window.height()
                                );
                                let viewport_scale_factor = app
                                    .config()
                                    .density()
                                    .map(|dpi| dpi as f64 / 160.0)
                                    .unwrap_or(1.0);
                                let dimensions = ViewportDimensions {
                                    width: window.width() as u32,
                                    height: window.height() as u32,
                                    scale_factor: viewport_scale_factor,
                                };
                                player_lock.set_viewport_dimensions(dimensions);
                                // MoleRuffle:旋转/尺寸变化后刷新 HUD 分辨率(否则停留在首帧旧值)。
                                perf::SURFACE_W
                                    .store(dimensions.width, std::sync::atomic::Ordering::Relaxed);
                                perf::SURFACE_H
                                    .store(dimensions.height, std::sync::atomic::Ordering::Relaxed);
                                needs_redraw = true;
                            }
                        }
                        MainEvent::Resume { .. } => {
                            if let Some(player) = playerbox.as_ref() {
                                if let Some(window) = native_window.as_ref() {
                                    // [NA] For some reason we can get negative sizes during a resume...
                                    if window.width() > 0 && window.height() > 0 {
                                        unsafe {
                                            let mut player = player
                                                .player
                                                .lock()
                                                .unwrap();

                                            let renderer = <dyn Any>::downcast_mut::<WgpuRenderBackend<SwapChainTarget>>(
                                                player.renderer_mut(),
                                            )
                                            .unwrap();

                                            renderer.recreate_surface_unsafe(
                                                wgpu::SurfaceTargetUnsafe::RawHandle {
                                                    raw_display_handle:
                                                        RawDisplayHandle::Android(
                                                            AndroidDisplayHandle::new(),
                                                        ),
                                                    raw_window_handle: window
                                                        .window_handle()
                                                        .unwrap()
                                                        .into(),
                                                },
                                                (window.width() as u32, window.height() as u32),
                                            )
                                            .unwrap();
                                        }
                                    }
                                }
                            }
                        }
                        MainEvent::InitWindow { .. } => {
                            native_window = app.native_window();
                            let window = native_window
                                .as_ref()
                                .expect("native_window should be Some after InitWindow");
                            let viewport_scale_factor = app
                                .config()
                                .density()
                                .map(|dpi| dpi as f64 / 160.0)
                                .unwrap_or(1.0);
                            let dimensions = ViewportDimensions {
                                width: window.width() as u32,
                                height: window.height() as u32,
                                scale_factor: viewport_scale_factor,
                            };
                            log::info!(
                                "Init window: {} x {} (is existing: {})",
                                window.width(),
                                window.height(),
                                playerbox.is_some()
                            );
                            // MoleRuffle:窗口(重)建后刷新 HUD 分辨率,覆盖后台回前台/重建路径。
                            perf::SURFACE_W
                                .store(dimensions.width, std::sync::atomic::Ordering::Relaxed);
                            perf::SURFACE_H
                                .store(dimensions.height, std::sync::atomic::Ordering::Relaxed);

                            if let Some(activeplayer) = &playerbox {
                                let mut player_lock = activeplayer.player.lock().unwrap();
                                unsafe {
                                    let renderer = <dyn Any>::downcast_mut::<WgpuRenderBackend<SwapChainTarget>>(
                                        player_lock.renderer_mut(),
                                    )
                                    .unwrap();

                                    renderer.recreate_surface_unsafe(
                                        wgpu::SurfaceTargetUnsafe::RawHandle {
                                            raw_display_handle: RawDisplayHandle::Android(
                                                AndroidDisplayHandle::new(),
                                            ),
                                            raw_window_handle: window
                                                .window_handle()
                                                .unwrap()
                                                .into(),
                                        },
                                        (window.width() as u32, window.height() as u32),
                                    )
                                    .unwrap();
                                }
                                player_lock.set_is_playing(true);
                            } else {
                                let renderer = unsafe {
                                    // TODO: make this take an Arc<Window> instead?
                                    WgpuRenderBackend::for_window_unsafe(
                                        wgpu::SurfaceTargetUnsafe::RawHandle {
                                            raw_display_handle: RawDisplayHandle::Android(
                                                AndroidDisplayHandle::new(),
                                            ),
                                            raw_window_handle: window
                                                .window_handle()
                                                .unwrap()
                                                .into(),
                                        },
                                        (dimensions.width, dimensions.height),
                                        // MoleRuffle:现代安卓(本机 Vulkan 可用)优先 Vulkan;GL/EGL 在本机
                                        // 报 "can present but not natively",可能白屏。Vulkan 失败再回退 GL。
                                        wgpu::Backends::VULKAN | wgpu::Backends::GL,
                                        wgpu::PowerPreference::HighPerformance,
                                    )
                                    .unwrap()
                                };
                                // MoleRuffle:记录真实后端名 + 渲染分辨率给性能 HUD 显示。
                                {
                                    let info = renderer.descriptors().adapter.get_info();
                                    if let Ok(mut b) = perf::BACKEND.lock() {
                                        *b = format!("{:?}", info.backend);
                                    }
                                    perf::SURFACE_W.store(
                                        dimensions.width,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                    perf::SURFACE_H.store(
                                        dimensions.height,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                    log::info!(
                                        "MoleRuffle: 渲染后端 = {:?} ({})",
                                        info.backend,
                                        info.name
                                    );
                                }
                                // MoleRuffle:base URL 指向 mole.61.com,好让 Client.swf 里的相对资源正确解析。
                                let movie_url = Url::parse(MOLE_SWF_URL).unwrap();
                                let player_id = PlayerId::new();

                                let future_spawner = AndroidExecutor {
                                    event_loop: sender.clone(),
                                    player_id,
                                };

                                let navigator = ExternalNavigatorBackend::new(
                                    movie_url.clone(),
                                    None,
                                    None,
                                    future_spawner,
                                    None,
                                    // MoleRuffle:upgrade_to_https=false —— 摩尔庄园服务器只有 http,
                                    //   升 https 会连到 443 假 IP 代理不处理 → "Domain resolution failure"。
                                    false,
                                    Default::default(),
                                    ruffle_core::backend::navigator::SocketMode::Allow,
                                    Rc::new(PlayingContent::DirectFile(ContentDescriptor::new_remote(movie_url))),
                                    AndroidNavigatorInterface,
                                );

                                playerbox = Some(ActivePlayer {
                                    id: player_id,
                                    player: PlayerBuilder::new()
                                            .with_renderer(renderer)
                                            .with_audio(AAudioAudioBackend::new().unwrap())
                                            .with_storage(Box::new(DiskStorageBackend::new(android_storage_dir.clone())))
                                            .with_navigator(navigator)
                                            .with_log(FileLogBackend::new(trace_output.as_deref()))
                                            .with_video(
                                                ruffle_video_software::backend::SoftwareVideoBackend::new(),
                                            )
                                            // MoleRuffle:自定义 UI 后端 —— 缺失设备字体(SimSun/Arial 等)回退到
                                            //   系统 CJK 字体,游戏内中文才渲染得出来。
                                            .with_ui(ui_backend::AndroidUiBackend::new())
                                            // MoleRuffle:域名 spoof(让 Client.swf 以为在官网,不 navigateToURL 弹走)+
                                            //   ShowAll 等比缩放铺满屏(摩尔庄园舞台固定 960x560)。
                                            .with_spoofed_url(Some(MOLE_SWF_URL.to_string()))
                                            .with_scale_mode(StageScaleMode::ShowAll, true)
                                            .with_autoplay(true)
                                        .build(),
                                    }
                                );

                                let player = &playerbox.as_ref().unwrap().player;
                                let mut player_lock = player.lock().unwrap();
                                let (jvm, activity) = get_jvm().unwrap();
                                let mut env = jvm.attach_current_thread().unwrap();
                                // MoleRuffle:不走 Java 文件选择器,固定从网络加载摩尔庄园 Client.swf。
                                let _ = (&mut env, &activity); // 保留 jvm/activity 绑定,后续 JNI 会用
                                let url = MOLE_SWF_URL.to_string();
                                log::warn!("MoleRuffle: fetch_root_movie 开始: {url}");
                                player_lock.fetch_root_movie(
                                    url,
                                    Vec::new(),
                                    // 该回调仅在 SWF 头解析成功(=fetch 成功)时触发;失败由 ruffle loader 内部记日志。
                                    Box::new(|_header| {
                                        log::warn!("MoleRuffle: 根影片 SWF 头到达 —— fetch 成功、开始加载内容");
                                    }),
                                );
                                player_lock.set_is_playing(true); // Desktop player will auto-play.

                                player_lock.set_letterbox(ruffle_core::config::Letterbox::On);

                                player_lock.set_viewport_dimensions(dimensions);

                                // MoleRuffle:内置默认字体 _sans/_serif/_typewriter 也路由到 CJK
                                //   (catch-all UI 后端会把任意名字解析成系统 CJK 字体)。
                                player_lock.set_default_font(
                                    ruffle_core::font::DefaultFont::Sans,
                                    vec!["_MoleCJK".to_string()],
                                );
                                player_lock.set_default_font(
                                    ruffle_core::font::DefaultFont::Serif,
                                    vec!["_MoleCJK".to_string()],
                                );
                                player_lock.set_default_font(
                                    ruffle_core::font::DefaultFont::Typewriter,
                                    vec!["_MoleCJK".to_string()],
                                );

                                last_frame_time = Instant::now();
                                next_frame_time = Some(Instant::now());

                                log::info!("MOVIE STARTED");
                            }
                        }
                        MainEvent::TerminateWindow { .. }  => {
                            let player = &playerbox.as_ref().unwrap().player;
                            let mut player_lock = player.lock().unwrap();
                            player_lock.set_is_playing(false);
                        }
                        MainEvent::InputAvailable => {
                            if let Ok(mut inputs) = app.input_events_iter() {
                                while inputs.next(|input| match input {
                                    InputEvent::MotionEvent(event) => {
                                        let window = native_window.as_ref().unwrap();
                                        let pointer = event.pointer_index();
                                        let pointer = event.pointer_at_index(pointer);
                                        let coords: (i32, i32) = get_loc_in_window();
                                        let mut x = pointer.x() as f64 - coords.0 as f64;
                                        let mut y = pointer.y() as f64 - coords.1 as f64;
                                        let view_size = get_view_size().unwrap();
                                        x = x * window.width() as f64 / view_size.0 as f64;
                                        y = y * window.height() as f64 / view_size.1 as f64;
                                        let ruffle_event = match event.action() {
                                            MotionAction::Down | MotionAction::PointerDown | MotionAction::ButtonPress => {
                                                PlayerEvent::MouseDown {
                                                    x,
                                                    y,
                                                    button: MouseButton::Left, // TODO
                                                    index: None, // TODO
                                                }
                                            }
                                            MotionAction::Up | MotionAction::PointerUp | MotionAction::ButtonRelease => {
                                                PlayerEvent::MouseUp {
                                                    x,
                                                    y,
                                                    button: MouseButton::Left, // TODO
                                                }
                                            }
                                            MotionAction::Move => PlayerEvent::MouseMove { x, y },
                                            _ => return InputStatus::Unhandled,
                                        };

                                        if let Some(player) = playerbox.as_ref() {
                                            player
                                                .player
                                                .lock()
                                                .unwrap()
                                                .handle_event(ruffle_event);
                                        }

                                        InputStatus::Handled
                                    }
                                    InputEvent::KeyEvent(event) => {
                                        if let Some(player) = playerbox.as_ref() {
                                            let Some(key_descriptor) =
                                                android_key_event_to_ruffle_key_descriptor(event)
                                            else {
                                                return InputStatus::Unhandled;
                                            };
                                            let down;
                                            let ruffle_event = match event.action() {
                                                KeyAction::Down => {
                                                    down = true;
                                                    PlayerEvent::KeyDown {
                                                        key: key_descriptor,
                                                    }
                                                }
                                                KeyAction::Up => {
                                                    down = false;
                                                    PlayerEvent::KeyUp { key: key_descriptor }
                                                }
                                                _ => return InputStatus::Unhandled,
                                            };
                                            player
                                                .player
                                                .lock()
                                                .unwrap()
                                                .handle_event(ruffle_event);

                                            // TODO: Use `KeyEvent.unicode_char` when it's available:
                                            // https://github.com/rust-mobile/android-activity/issues/183
                                            if down {
                                                if let LogicalKey::Character(c) = key_descriptor.logical_key {
                                                    let event = PlayerEvent::TextInput { codepoint: c };
                                                    player.player.lock().unwrap().handle_event(event);
                                                }
                                            };

                                            needs_redraw = true;
                                        }

                                        InputStatus::Handled
                                    }
                                    _ => InputStatus::Unhandled,
                                }) {}
                            }
                        }
                        _ => {} // Something else happened but it's probably not important for now.
                    },
                    PollEvent::Wake => {} // A task tried to wake us, we'll recv it below
                    PollEvent::Timeout => {} // No events happened, we'll tick as normal below
                    _ => {}               // Unknown future event
                }
            },
        );

        match receiver.try_recv() {
            Err(_) => {}
            Ok(RuffleEvent::TaskPoll(task)) => {
                // Only run the task if it matches our current player;
                // otherwise it is stale, and should be cancelled (which
                // happens implicitly on drop).
                if let Some(player) = playerbox.as_ref() {
                    if *task.0.metadata() == player.id {
                        task.0.run();
                    }
                }
            }
            Ok(RuffleEvent::VirtualKeyEvent {
                down,
                key_descriptor,
            }) => {
                if let Some(player) = playerbox.as_ref() {
                    let event = if down {
                        PlayerEvent::KeyDown {
                            key: key_descriptor,
                        }
                    } else {
                        PlayerEvent::KeyUp {
                            key: key_descriptor,
                        }
                    };
                    player.player.lock().unwrap().handle_event(event);

                    if down {
                        // TODO: Add shift/capslock and pass in uppercase characters accordingly
                        if let LogicalKey::Character(c) = key_descriptor.logical_key {
                            let event = PlayerEvent::TextInput { codepoint: c };
                            player.player.lock().unwrap().handle_event(event);
                        }
                    }
                }
            }
            Ok(RuffleEvent::RunContextMenuCallback(index)) => {
                if let Some(player) = playerbox.as_ref() {
                    player
                        .player
                        .lock()
                        .unwrap()
                        .run_context_menu_callback(index);
                }
            }
            Ok(RuffleEvent::ClearContextMenu) => {
                if let Some(player) = playerbox.as_ref() {
                    player.player.lock().unwrap().clear_custom_menu_items();
                }
            }
            Ok(RuffleEvent::RequestContextMenu) => {
                if let Some(player) = playerbox.as_ref() {
                    log::warn!("preparing context menu!");
                    let items = player.player.lock().unwrap().prepare_context_menu();
                    let (jvm, activity) = get_jvm().unwrap();
                    let mut env = jvm.attach_current_thread().unwrap();
                    JavaInterface::show_context_menu(&mut env, &activity, &items);
                }
            }
            Ok(RuffleEvent::ToggleSoftKeyboard) => {
                // MoleRuffle:摩尔庄园登录/聊天需要打字 —— 调系统自带软键盘(而非官方那套占屏 View 键盘)。
                soft_kb_visible = !soft_kb_visible;
                if soft_kb_visible {
                    // 复位 IME 文本基准,避免上次残留触发假 diff。
                    last_ime_text.clear();
                    app.set_text_input_state(android_activity::input::TextInputState {
                        text: String::new(),
                        selection: android_activity::input::TextSpan { start: 0, end: 0 },
                        compose_region: None,
                    });
                    app.show_soft_input(true);
                    log::info!("MoleRuffle: 显示系统软键盘");
                } else {
                    app.hide_soft_input(false);
                    log::info!("MoleRuffle: 隐藏系统软键盘");
                }
            }
        }

        // MoleRuffle:软键盘开着时轮询 IME 文本,做增删 diff 注入到聚焦的 Flash 文本框。
        //   android-activity(game-activity 后端)没有专用文本事件,靠轮询 text_input_state。
        if soft_kb_visible {
            let new_text = app.text_input_state().text;
            if new_text != last_ime_text {
                let common = last_ime_text
                    .chars()
                    .zip(new_text.chars())
                    .take_while(|(a, b)| a == b)
                    .count();
                let deletes = last_ime_text.chars().count().saturating_sub(common);
                let added: String = new_text.chars().skip(common).collect();
                if let Some(player) = playerbox.as_ref() {
                    if let Ok(mut p) = player.player.lock() {
                        if let Some(bs) = key_tag_to_key_descriptor("BACKSPACE") {
                            for _ in 0..deletes {
                                p.handle_event(PlayerEvent::KeyDown { key: bs });
                                p.handle_event(PlayerEvent::KeyUp { key: bs });
                            }
                        }
                        for c in added.chars() {
                            p.handle_event(PlayerEvent::TextInput { codepoint: c });
                        }
                    }
                }
                last_ime_text = new_text;
                needs_redraw = true;
            }
        }

        let new_time = Instant::now();
        let dt = new_time.duration_since(last_frame_time).as_micros();
        if dt > 0 {
            last_frame_time = new_time;
            // MoleRuffle:帧时间(µs)给性能 HUD。
            perf::FRAME_US.store(dt.min(u32::MAX as u128) as u32, std::sync::atomic::Ordering::Relaxed);
            if let Some(player) = playerbox.as_ref() {
                if let Ok(mut player) = player.player.lock() {
                    player.tick(FloatDuration::from_millis(dt as f64 / 1000.0));
                    next_frame_time = Some(new_time + player.time_til_next_frame());
                    needs_redraw = player.needs_render();
                    let audio =
                        <dyn Any>::downcast_mut::<AAudioAudioBackend>(player.audio_mut()).unwrap();
                    audio.recreate_stream_if_needed();
                }
            } else {
                next_frame_time = None;
            }
        }

        if needs_redraw {
            if let Some(player) = playerbox.as_ref() {
                if let Ok(mut player) = player.player.lock() {
                    player.render();
                    // MoleRuffle:数真正渲染出的帧(HUD 的 FPS 用)。
                    frame_count += 1;
                }
            }
        }

        // MoleRuffle:每 500ms 采样一次 FPS 写入全局,供 JNI getPerfStats 读取。
        let hud_elapsed = last_hud_time.elapsed();
        if hud_elapsed.as_millis() >= 500 {
            let dframes = frame_count.saturating_sub(last_hud_frames);
            let secs = hud_elapsed.as_secs_f64();
            let fps = if secs > 0.0 {
                (dframes as f64 / secs).round() as u32
            } else {
                0
            };
            perf::FPS.store(fps, std::sync::atomic::Ordering::Relaxed);
            last_hud_frames = frame_count;
            last_hud_time = Instant::now();
        }
    }

    unsafe {
        let vm = JavaVM::from_raw(app.vm_as_ptr() as *mut sys::JavaVM).expect("JVM must exist");
        let activity = JObject::from_raw(app.activity_as_ptr() as jobject);
        // Ensure that we take the EventSender back, or we'll leak it
        let _: Result<EventSender, _> = vm
            .get_env()
            .unwrap()
            .take_rust_field(activity, "eventLoopHandle");
    }
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_keydown(
    mut env: JNIEnv,
    this: JObject,
    key_tag: JString,
) {
    let tag: String = env
        .get_string(&key_tag)
        .expect("Couldn't get java string!")
        .into();

    let event_loop: MutexGuard<Sender<RuffleEvent>> =
        env.get_rust_field(this, "eventLoopHandle").unwrap();
    if let Some(desc) = key_tag_to_key_descriptor(&tag) {
        let _ = event_loop.send(RuffleEvent::VirtualKeyEvent {
            down: true,
            key_descriptor: desc,
        });
    }
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_keyup(
    mut env: JNIEnv,
    this: JObject,
    key_tag: JString,
) {
    let tag: String = env
        .get_string(&key_tag)
        .expect("Couldn't get java string!")
        .into();

    let event_loop: MutexGuard<Sender<RuffleEvent>> =
        env.get_rust_field(this, "eventLoopHandle").unwrap();
    if let Some(desc) = key_tag_to_key_descriptor(&tag) {
        let _ = event_loop.send(RuffleEvent::VirtualKeyEvent {
            down: false,
            key_descriptor: desc,
        });
    }
}

pub fn get_jvm<'a>() -> Result<(jni::JavaVM, JObject<'a>), Box<dyn std::error::Error>> {
    // Create a VM for executing Java calls
    let context = ndk_context::android_context();
    let activity = unsafe { JObject::from_raw(context.context().cast()) };
    let vm = unsafe { jni::JavaVM::from_raw(context.vm().cast()) }?;

    Ok((vm, activity))
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_toggleSoftKeyboard(
    mut env: JNIEnv,
    this: JObject,
) {
    // MoleRuffle:⌨ 按钮 → 显/隐系统自带软键盘(交给主循环处理,那里能拿到 AndroidApp)。
    let event_loop: MutexGuard<Sender<RuffleEvent>> =
        env.get_rust_field(this, "eventLoopHandle").unwrap();
    let _ = event_loop.send(RuffleEvent::ToggleSoftKeyboard);
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_getPerfStats(
    env: JNIEnv,
    _this: JObject,
) -> jni::sys::jstring {
    // MoleRuffle:性能 HUD 的 Rust 侧数据(FPS/帧时间/后端/分辨率),Kotlin 再补内存/温度/网络。
    use std::sync::atomic::Ordering::Relaxed;
    let fps = perf::FPS.load(Relaxed);
    let frame_us = perf::FRAME_US.load(Relaxed);
    let w = perf::SURFACE_W.load(Relaxed);
    let h = perf::SURFACE_H.load(Relaxed);
    let backend = perf::BACKEND
        .lock()
        .map(|b| b.clone())
        .unwrap_or_default();
    let ms = frame_us as f64 / 1000.0;
    let text = format!("FPS {fps}   帧 {ms:.1}ms\n渲染 {backend}   {w}x{h}");
    match env.new_string(text) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_requestContextMenu(
    mut env: JNIEnv,
    this: JObject,
) {
    let event_loop: MutexGuard<Sender<RuffleEvent>> =
        env.get_rust_field(this, "eventLoopHandle").unwrap();
    let _ = event_loop.send(RuffleEvent::RequestContextMenu);
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_runContextMenuCallback(
    mut env: JNIEnv,
    this: JObject,
    index: jint,
) {
    let event_loop: MutexGuard<Sender<RuffleEvent>> =
        env.get_rust_field(this, "eventLoopHandle").unwrap();
    let _ = event_loop.send(RuffleEvent::RunContextMenuCallback(index as usize));
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_clearContextMenu(
    mut env: JNIEnv,
    this: JObject,
) {
    let event_loop: MutexGuard<Sender<RuffleEvent>> =
        env.get_rust_field(this, "eventLoopHandle").unwrap();
    let _ = event_loop.send(RuffleEvent::ClearContextMenu);
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_nativeInit(
    mut env: JNIEnv,
    class: JClass,
    crash_callback: JObject,
) {
    let crash_callback = env.new_global_ref(crash_callback).unwrap();
    let jvm = env.get_java_vm().unwrap();

    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("ruffle")
            .with_filter(
                android_logger::FilterBuilder::new()
                    .parse("warn,ruffle=info")
                    .build(),
            ),
    );

    panic::set_hook(Box::new(move |info| {
        let backtrace = Backtrace::new();
        let thread = thread::current();
        let thread = thread.name().unwrap_or("<unnamed>");
        let message = match info.payload().downcast_ref::<&'static str>() {
            Some(s) => *s,
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => &**s,
                None => "Box<Any>",
            },
        };

        let full = match info.location() {
            Some(location) => format!(
                "thread '{}' panicked at '{}': {}:{}\n{:?}",
                thread,
                message,
                location.file(),
                location.line(),
                backtrace
            ),
            None => format!(
                "thread '{}' panicked at '{}'\n{:?}",
                thread, message, backtrace
            ),
        };
        log::error!(target: "panic","{}", full);

        let mut env = jvm.attach_current_thread().unwrap();
        if env.exception_check().unwrap() {
            // There's a pending exception, java will discover this on their own
        } else {
            let java_message = env.new_string(full).unwrap();
            let crash_callback = env.new_global_ref(&crash_callback).unwrap();
            env.call_method(
                crash_callback,
                "onCrash",
                "(Ljava/lang/String;)V",
                &[(&java_message).into()],
            )
            .unwrap();
        }
    }));

    JavaInterface::init(&mut env, &class)
}

fn get_loc_in_window() -> (i32, i32) {
    let (jvm, activity) = get_jvm().unwrap();
    let mut env = jvm.attach_current_thread().unwrap();

    // no worky :(
    //ndk_glue::native_activity().show_soft_input(true);

    JavaInterface::get_loc_in_window(&mut env, &activity)
}

fn get_view_size() -> Result<(i32, i32), Box<dyn std::error::Error>> {
    let (jvm, activity) = get_jvm()?;
    let mut env = jvm.attach_current_thread()?;

    let width = JavaInterface::get_surface_width(&mut env, &activity);
    let height = JavaInterface::get_surface_height(&mut env, &activity);

    Ok((width, height))
}

#[no_mangle]
fn android_main(app: AndroidApp) {
    log::info!("Starting android_main...");
    run(app);
}
