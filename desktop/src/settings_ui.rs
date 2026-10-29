//! 游戏内高级设置浮层(iOS / macOS / Windows / Linux 共用;安卓壳另做原生设置页)。
//!
//! 用 egui 画在游戏画面最上面:ruffle-fork 的渲染后端每帧提交上屏前调用宿主的叠加回调
//! (`WgpuRenderBackend::set_overlay`),这里在同一张画面上再画一层,不改游戏自身的渲染路径,
//! 也不进脏矩形的持久帧缓冲。
//!
//! 平时只画一个小齿轮(优先放在黑边里,不压游戏界面);点齿轮或按 F10 / Cmd(Ctrl)+逗号打开。
//! 打开时浮层吃掉全部输入,游戏收不到;关闭时只拦截落在齿轮上的点按。
//! 设置项按 `moleruffle_core::settings::SPECS` 自动生成,存进 settings.txt;服务器改 server.txt。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moleruffle_core as mole;
use mole::settings::{self, Group, Kind};
use winit::event::{ElementState, MouseButton, TouchPhase, WindowEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::Window;

/// 交给叠加回调去画的东西(在主线程准备,在渲染后端的提交阶段消费)。
#[derive(Default)]
struct Shared {
    renderer: Option<egui_wgpu::Renderer>,
    primitives: Vec<egui::ClippedPrimitive>,
    /// 绘制用的每点像素数(= 窗口缩放 × 渲染分辨率比例)。
    pixels_per_point: f32,
    pending_set: Vec<(egui::TextureId, egui::epaint::ImageDelta)>,
    pending_free: Vec<egui::TextureId>,
}

/// 设置界面里点了要宿主去做的事。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 立即生效画质(不用重启)。
    SetQuality(String),
    /// 立即回收内存(两轮完整 GC + 清纹理池)。
    Reclaim,
    /// 重启游戏(设置已保存)。
    Restart,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Server,
    Group(Group),
}

pub struct SettingsUi {
    ctx: egui::Context,
    state: egui_winit::State,
    shared: Arc<Mutex<Shared>>,
    pub open: bool,
    tab: Tab,
    /// 界面上正在编辑的值(未保存)。
    values: BTreeMap<String, String>,
    /// 启动时生效的值(判断"改了要重启")。
    boot_values: BTreeMap<String, String>,
    server_id: String,
    boot_server_id: String,
    fonts_loaded: bool,
    /// 齿轮按钮的区域(逻辑点)。
    gear: egui::Rect,
    /// 正在按齿轮的触摸 / 鼠标(按下在齿轮上的这一次一直归浮层,直到抬起)。
    gear_capture: Option<Option<u64>>,
    modifiers: ModifiersState,
    toast: Option<(String, Instant)>,
    confirm: Option<Confirm>,
    actions: Vec<Action>,
    /// 内存页的实时读数(每秒刷新一次)。
    stats: (Instant, String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirm {
    ClearSaves,
    ClearCache,
    Defaults,
}

impl SettingsUi {
    pub fn new(window: &Window) -> Self {
        let ctx = egui::Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let current: BTreeMap<String, String> = settings::SPECS
            .iter()
            .map(|s| (s.key.to_string(), settings::current(s)))
            .collect();
        let server_id = mole::server::selected().id.to_string();
        Self {
            ctx,
            state,
            shared: Arc::default(),
            // 调试:MOLE_SETTINGS_OPEN=1 启动即打开设置面板(截图验证界面用)。
            open: std::env::var("MOLE_SETTINGS_OPEN").as_deref() == Ok("1"),
            tab: Tab::Server,
            values: current.clone(),
            boot_values: current,
            server_id: server_id.clone(),
            boot_server_id: server_id,
            fonts_loaded: false,
            gear: egui::Rect::NOTHING,
            gear_capture: None,
            modifiers: ModifiersState::empty(),
            toast: None,
            confirm: None,
            actions: Vec::new(),
            stats: (Instant::now() - Duration::from_secs(10), String::new()),
        }
    }

    /// 叠加回调:交给 `WgpuRenderBackend::set_overlay`。
    pub fn overlay(&self, format: wgpu::TextureFormat) -> ruffle_render_wgpu::backend::OverlayFn {
        let shared = self.shared.clone();
        Box::new(move |device, queue, encoder, view, (w, h)| {
            let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            if sh.primitives.is_empty() && sh.pending_set.is_empty() {
                return;
            }
            let sh = &mut *sh;
            let renderer = sh.renderer.get_or_insert_with(|| {
                egui_wgpu::Renderer::new(
                    device,
                    format,
                    egui_wgpu::RendererOptions {
                        msaa_samples: 1,
                        depth_stencil_format: None,
                        dithering: false,
                        predictable_texture_filtering: false,
                    },
                )
            });
            for (id, delta) in sh.pending_set.drain(..) {
                renderer.update_texture(device, queue, id, &delta);
            }
            let screen = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [w, h],
                pixels_per_point: sh.pixels_per_point,
            };
            let _ = renderer.update_buffers(device, queue, encoder, &sh.primitives, &screen);
            {
                let mut pass = encoder
                    .begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("MoleRuffle 设置浮层"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view,
                            resolve_target: None,
                            depth_slice: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        ..Default::default()
                    })
                    .forget_lifetime();
                renderer.render(&mut pass, &sh.primitives, &screen);
            }
            for id in sh.pending_free.drain(..) {
                renderer.free_texture(&id);
            }
        })
    }

    /// 取走宿主要做的事。
    pub fn take_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }

    /// 先给浮层看窗口事件。返回 true = 浮层吃掉了,不要再交给游戏。
    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        if let WindowEvent::ModifiersChanged(m) = event {
            self.modifiers = m.state();
        }
        // 快捷键:F10,或 Cmd(mac)/ Ctrl + 逗号;打开时 Esc 关闭。
        if let WindowEvent::KeyboardInput { event: key, .. } = event
            && key.state == ElementState::Pressed
        {
            let cmd = if cfg!(target_os = "macos") {
                self.modifiers.super_key()
            } else {
                self.modifiers.control_key()
            };
            let hotkey = matches!(key.logical_key, Key::Named(NamedKey::F10))
                || (cmd && matches!(&key.logical_key, Key::Character(c) if c == ","));
            let escape = self.open && matches!(key.logical_key, Key::Named(NamedKey::Escape));
            if hotkey || escape {
                self.set_open(hotkey && !self.open);
                window.request_redraw();
                return true;
            }
        }
        if self.open {
            let is_input = matches!(
                event,
                WindowEvent::KeyboardInput { .. }
                    | WindowEvent::Ime(_)
                    | WindowEvent::CursorMoved { .. }
                    | WindowEvent::CursorLeft { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::Touch(_)
                    | WindowEvent::ModifiersChanged(_)
            );
            let _ = self.state.on_window_event(window, event);
            return is_input && !matches!(event, WindowEvent::ModifiersChanged(_));
        }
        // 关闭时:只拦截落在齿轮上的按下(及这次按下后续的移动 / 抬起)。
        let scale = window.scale_factor();
        let hit = |x: f64, y: f64| self.gear.contains(egui::pos2((x / scale) as f32, (y / scale) as f32));
        match event {
            WindowEvent::Touch(t) => {
                let captured = self.gear_capture == Some(Some(t.id));
                if t.phase == TouchPhase::Started && hit(t.location.x, t.location.y) {
                    self.gear_capture = Some(Some(t.id));
                    return true;
                }
                if captured {
                    if matches!(t.phase, TouchPhase::Ended) {
                        self.gear_capture = None;
                        if hit(t.location.x, t.location.y) {
                            self.set_open(true);
                            window.request_redraw();
                        }
                    } else if matches!(t.phase, TouchPhase::Cancelled) {
                        self.gear_capture = None;
                    }
                    return true;
                }
                false
            }
            WindowEvent::CursorMoved { position, .. } => {
                // 记下位置给下面的点击判断用(不吞:游戏也要光标)。
                let _ = self.state.on_window_event(window, event);
                let _ = position;
                false
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let pos = self.ctx.input(|i| i.pointer.latest_pos());
                let on_gear = pos.is_some_and(|p| self.gear.contains(p));
                match state {
                    ElementState::Pressed if on_gear => {
                        self.gear_capture = Some(None);
                        true
                    }
                    ElementState::Released if self.gear_capture == Some(None) => {
                        self.gear_capture = None;
                        if on_gear {
                            self.set_open(true);
                            window.request_redraw();
                        }
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn set_open(&mut self, open: bool) {
        if open && !self.open {
            // 每次打开都从磁盘上的设置重新读,避免显示过期的值。
            let saved = settings::load();
            for spec in settings::SPECS {
                let v = saved
                    .get(spec.key)
                    .cloned()
                    .unwrap_or_else(|| settings::current(spec));
                self.values.insert(spec.key.to_string(), v);
            }
            self.confirm = None;
        }
        self.open = open;
    }

    /// 运行一帧界面并把结果交给叠加回调。`render_scale` 是游戏的渲染分辨率比例。
    pub fn run(&mut self, window: &Window, render_scale: f64, safe: (f64, f64, f64, f64)) {
        if !self.fonts_loaded {
            self.fonts_loaded = true;
            install_cjk_font(&self.ctx);
        }
        let scale = window.scale_factor();
        let size = window.inner_size();
        let (w, h) = (size.width as f64 / scale, size.height as f64 / scale);
        self.gear = gear_rect(w, h, safe);
        let raw = self.state.take_egui_input(window);
        let output = self.ctx.clone().run(raw, |ctx| self.ui(ctx));
        self.state
            .handle_platform_output(window, output.platform_output);
        let ppp = (scale * render_scale) as f32;
        let primitives = self.ctx.tessellate(output.shapes, ppp);
        let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.primitives = primitives;
        sh.pixels_per_point = ppp;
        sh.pending_set.extend(output.textures_delta.set);
        sh.pending_free.extend(output.textures_delta.free);
    }

    /// 打开时需要持续重绘(动画、输入反馈)。
    pub fn wants_redraw(&self) -> bool {
        self.open || self.toast.is_some()
    }

    fn ui(&mut self, ctx: &egui::Context) {
        // 齿轮:半透明小圆钮(电脑上没有黑边时不画)。
        if self.gear.is_positive() {
            self.draw_gear(ctx);
        }
        self.draw_rest(ctx);
    }

    fn draw_gear(&self, ctx: &egui::Context) {
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("gear")));
        let c = self.gear.center();
        let r = self.gear.width() / 2.0;
        painter.circle_filled(c, r, egui::Color32::from_black_alpha(110));
        painter.text(
            c,
            egui::Align2::CENTER_CENTER,
            "⚙",
            egui::FontId::proportional(r * 1.3),
            egui::Color32::from_white_alpha(200),
        );
    }

    fn draw_rest(&mut self, ctx: &egui::Context) {
        if let Some((msg, at)) = &self.toast {
            if at.elapsed() > Duration::from_secs(3) {
                self.toast = None;
            } else {
                egui::Area::new(egui::Id::new("toast"))
                    .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -40.0])
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(msg.as_str()));
                    });
            }
        }
        if !self.open {
            return;
        }

        // 半透明遮罩 + 居中面板。
        let screen = ctx.content_rect();
        ctx.layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("dim")))
            .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(140));
        let panel_w = (screen.width() * 0.9).min(560.0);
        let panel_h = (screen.height() * 0.88).min(520.0);
        egui::Window::new("高级设置")
            .collapsible(false)
            .resizable(false)
            .title_bar(true)
            .fixed_size([panel_w, panel_h])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.style_mut().spacing.item_spacing.y = 8.0;
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Server, "服务器与账号");
                    for g in Group::ALL {
                        ui.selectable_value(&mut self.tab, Tab::Group(g), g.label());
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(panel_h - 110.0)
                    .show(ui, |ui| match self.tab {
                        Tab::Server => self.server_tab(ui),
                        Tab::Group(g) => self.group_tab(ui, g),
                    });
                ui.separator();
                self.bottom_bar(ui);
            });
    }

    fn server_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("选择服务器(切换后需要重启游戏):");
        for s in mole::server::ALL {
            ui.radio_value(&mut self.server_id, s.id.to_string(), s.name);
        }
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("各服务器的登录存档、资源缓存都是分开保存的,切换不会互相覆盖。")
                .small()
                .weak(),
        );
        ui.add_space(12.0);
        if ui.button("清除当前服务器的登录存档(换号用)").clicked() {
            self.confirm = Some(Confirm::ClearSaves);
        }
        self.confirm_row(ui, Confirm::ClearSaves, "会删掉记住的账号等本地存档,确定吗?", |me| {
            let dir = mole::mole_storage_dir().join(mole::server::selected().host);
            let ok = !dir.exists() || std::fs::remove_dir_all(&dir).is_ok();
            me.toast(if ok { "已清除,重启游戏后生效" } else { "清除失败" });
        });
    }

    fn group_tab(&mut self, ui: &mut egui::Ui, group: Group) {
        for spec in settings::visible_specs().filter(|s| s.group == group) {
            let mut value = self
                .values
                .get(spec.key)
                .cloned()
                .unwrap_or_else(|| spec.default.to_string());
            let before = value.clone();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(spec.label).strong());
                if spec.restart {
                    ui.label(egui::RichText::new("重启后生效").small().weak());
                }
            });
            match spec.kind {
                Kind::Bool => {
                    let mut on = value == "1";
                    let text = if on { "已开启" } else { "已关闭" };
                    ui.checkbox(&mut on, text);
                    value = if on { "1" } else { "0" }.to_string();
                }
                Kind::Choice(choices) => {
                    ui.horizontal_wrapped(|ui| {
                        for (v, label) in choices {
                            ui.radio_value(&mut value, v.to_string(), *label);
                        }
                    });
                }
                Kind::Int { min, max, step } => {
                    let mut n = value.parse::<i64>().unwrap_or(min);
                    ui.add(egui::Slider::new(&mut n, min..=max).step_by(step as f64));
                    value = n.to_string();
                }
            }
            ui.label(egui::RichText::new(spec.help).small().weak());
            ui.add_space(6.0);
            if value != before {
                if spec.key == "MOLE_QUALITY" {
                    self.actions.push(Action::SetQuality(value.clone()));
                }
                self.values.insert(spec.key.to_string(), value);
            }
        }
        match group {
            Group::Memory => self.memory_extras(ui),
            Group::Diagnostics => self.diagnostics_extras(ui),
            Group::Display => {}
        }
    }

    fn memory_extras(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        if self.stats.0.elapsed() > Duration::from_secs(1) {
            let foot = mole::mem::footprint_mb().map_or("—".to_string(), |v| format!("{v}MB"));
            let avail = mole::mem::available_mb().map_or(String::new(), |v| format!(",距系统上限还剩 {v}MB"));
            let (hits, misses, hit_bytes, _) = mole::cache::cache_summary();
            self.stats = (
                Instant::now(),
                format!(
                    "当前内存占用 {foot}{avail}\n本次资源缓存命中 {hits}/{},省下载 {}MB",
                    hits + misses,
                    hit_bytes / (1024 * 1024)
                ),
            );
        }
        ui.label(self.stats.1.as_str());
        ui.horizontal_wrapped(|ui| {
            if ui.button("立即回收内存").clicked() {
                self.actions.push(Action::Reclaim);
                self.toast("已回收");
            }
            if ui.button("清空当前服务器的资源缓存").clicked() {
                self.confirm = Some(Confirm::ClearCache);
            }
        });
        self.confirm_row(ui, Confirm::ClearCache, "下次进场景要重新下载,确定吗?", |me| {
            let dir = mole::cache::cache_dir();
            let ok = !dir.exists() || std::fs::remove_dir_all(&dir).is_ok();
            me.toast(if ok { "已清空" } else { "清空失败" });
        });
    }

    fn diagnostics_extras(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label(
            egui::RichText::new(format!(
                "版本 {}  ·  服务器 {}",
                env!("CARGO_PKG_VERSION"),
                mole::server::selected().name
            ))
            .small()
            .weak(),
        );
        if cfg!(target_os = "ios") {
            ui.label(
                egui::RichText::new("日志保存在「文件」App → 我的 iPhone → 摩尔庄园 → logs")
                    .small()
                    .weak(),
            );
        }
        if ui.button("全部恢复默认").clicked() {
            self.confirm = Some(Confirm::Defaults);
        }
        self.confirm_row(ui, Confirm::Defaults, "所有设置恢复默认(服务器不变),确定吗?", |me| {
            for spec in settings::SPECS {
                me.values.insert(spec.key.to_string(), spec.default.to_string());
            }
            me.toast("已恢复默认,记得保存");
        });
    }

    fn confirm_row(
        &mut self,
        ui: &mut egui::Ui,
        which: Confirm,
        question: &str,
        on_yes: impl FnOnce(&mut Self),
    ) {
        if self.confirm != Some(which) {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(question).color(egui::Color32::LIGHT_RED));
            if ui.button("确定").clicked() {
                self.confirm = None;
                on_yes(self);
            }
            if ui.button("取消").clicked() {
                self.confirm = None;
            }
        });
    }

    fn needs_restart(&self) -> bool {
        self.server_id != self.boot_server_id
            || settings::SPECS.iter().any(|s| {
                s.restart && self.values.get(s.key) != self.boot_values.get(s.key)
            })
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        let restart = self.needs_restart();
        ui.horizontal(|ui| {
            if ui.button(if restart { "保存并重启游戏" } else { "保存" }).clicked() {
                let saved = settings::save(&self.values).is_ok()
                    && (self.server_id == self.boot_server_id
                        || mole::server::by_id(&self.server_id)
                            .is_some_and(|s| mole::server::persist(s).is_ok()));
                if !saved {
                    self.toast("保存失败");
                } else if restart {
                    self.actions.push(Action::Restart);
                } else {
                    self.toast("已保存");
                    self.set_open(false);
                }
            }
            if ui.button("关闭").clicked() {
                self.set_open(false);
            }
            if restart {
                ui.label(egui::RichText::new("有改动需要重启才生效").small().weak());
            }
        });
    }

    fn toast(&mut self, msg: &str) {
        self.toast = Some((msg.to_string(), Instant::now()));
    }
}

/// 齿轮按钮位置(逻辑点):优先放进游戏画面外的黑边,没有黑边才贴在左上角。
/// `safe` = (左, 上, 右, 下) 安全区。
fn gear_rect(w: f64, h: f64, safe: (f64, f64, f64, f64)) -> egui::Rect {
    const SIZE: f64 = 34.0;
    const MARGIN: f64 = 8.0;
    let game = crate::pad_layout::game_rect(w, h);
    let (x, y) = if game.x >= SIZE + 2.0 * MARGIN {
        // 左侧黑边:放在左黑边顶部
        (((game.x - SIZE) / 2.0).max(safe.0 + MARGIN), safe.1 + MARGIN)
    } else if game.y >= SIZE + 2.0 * MARGIN {
        // 上方黑边
        (safe.0 + MARGIN, (game.y - SIZE) / 2.0)
    } else if h - game.bottom() >= SIZE + 2.0 * MARGIN {
        // 下方黑边
        (safe.0 + MARGIN, game.bottom() + (h - game.bottom() - SIZE) / 2.0)
    } else if cfg!(any(target_os = "ios", target_os = "android")) {
        // 手机没有黑边:左上角(手机上没有快捷键,齿轮是唯一入口)
        (safe.0 + MARGIN, safe.1 + MARGIN)
    } else {
        // 电脑没有黑边:不画齿轮,免得压住游戏界面;用 F10 / Cmd(Ctrl)+逗号打开
        return egui::Rect::NOTHING;
    };
    egui::Rect::from_min_size(egui::pos2(x as f32, y as f32), egui::vec2(SIZE as f32, SIZE as f32))
}

/// 中文字体:找到系统中文字体就放在默认字体前面(找不到时界面中文显示为方块)。
fn install_cjk_font(ctx: &egui::Context) {
    let Some((bytes, index)) = mole::cjk_font_data() else {
        tracing::warn!("设置界面:没找到系统中文字体");
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    let mut data = egui::FontData::from_owned(bytes);
    data.index = index;
    fonts.font_data.insert("cjk".into(), Arc::new(data));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        if let Some(list) = fonts.families.get_mut(&family) {
            list.insert(0, "cjk".into());
        }
    }
    ctx.set_fonts(fonts);
}
