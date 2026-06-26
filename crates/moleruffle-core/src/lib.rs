//! MoleRuffle 跨平台共享核心
//!
//! 这里集中放“与平台无关、五端(Win/macOS/Linux/Android/iOS)共用”的东西:
//!   - 摩尔庄园网页版的固定配置(SWF 入口、base、舞台尺寸、标题)
//!   - 把一个 `PlayerBuilder` 配成“摩尔庄园专用”的设置(关键:域名守卫 spoof)
//!   - 一个对游戏放行 socket 的 `NavigatorInterface` 实现
//!   - 中文字体回退链(让动态文本也能显示中文)
//!
//! 各平台壳层(desktop / android / ios)只负责提供平台后端
//! (window/render/audio/future-spawner),其余统一调用这里。

use std::io;
use std::path::Path;
use std::sync::Arc;

use ruffle_core::backend::ui::{
    DialogResultFuture, FileFilter, FontDefinition, FullscreenError, MouseCursor,
    MultiDialogResultFuture, UiBackend,
};
use ruffle_core::config::Letterbox;
use ruffle_core::font::{DefaultFont, FontFileData, FontQuery};
use ruffle_core::{LoadBehavior, Player, PlayerBuilder};
use ruffle_frontend_utils::backends::navigator::NavigatorInterface;
use unic_langid::LanguageIdentifier;
use url::Url;

/// 摩尔庄园网页版的引导 SWF(外壳 / 加载器,AS3+Flex4)。
/// 相对路径(version/、resource/、config/、dll/)都相对它来解析。
pub const GAME_SWF_URL: &str = "http://mole.61.com/Client.swf";

/// 所有相对 fetch 的 base，回源到 mole.61.com。
pub const GAME_BASE_URL: &str = "http://mole.61.com/";

/// 窗口标题。
pub const WINDOW_TITLE: &str = "摩尔庄园 · MoleRuffle";

/// 固定舞台尺寸(Client.swf 的逻辑尺寸)。
pub const STAGE_WIDTH: u32 = 960;
pub const STAGE_HEIGHT: u32 = 560;

/// 注意:`Client.swf` 脱离正规宿主会执行 `navigateToURL("http://mole.61.com")`
/// 把自己弹回首页(实测在裸浏览器里就是这样被弹走的)。
/// 解法 = 把 SWF 的“自我认知 URL”伪装成它在官网上的地址,域名守卫就放行。
/// 这正是 [`apply_mole_settings`] 里 `with_spoofed_url` / `with_page_url` 做的事。
pub const SPOOF_URL: &str = GAME_SWF_URL;

pub fn game_swf_url() -> Url {
    Url::parse(GAME_SWF_URL).expect("GAME_SWF_URL 必须是合法 URL")
}

pub fn game_base_url() -> Url {
    Url::parse(GAME_BASE_URL).expect("GAME_BASE_URL 必须是合法 URL")
}

/// 把一个全新的 `PlayerBuilder` 配成“摩尔庄园专用”。
///
/// 这是五端共享的关键装配:平台壳层先 `with_renderer/with_audio/with_navigator`,
/// 再调本函数补齐摩尔庄园需要的设置(尤其是 spoof,缺了它进不去游戏)。
pub fn apply_mole_settings(builder: PlayerBuilder) -> PlayerBuilder {
    builder
        .with_autoplay(true)
        .with_letterbox(Letterbox::On)
        // 边下边跑:数百个资源 SWF 是运行时陆续拉的
        .with_load_behavior(LoadBehavior::Streaming)
        // ★ 域名守卫 spoof:让 Client.swf 以为自己就在官网上,不要 navigateToURL 弹走
        .with_spoofed_url(Some(SPOOF_URL.to_string()))
        .with_page_url(Some(SPOOF_URL.to_string()))
        // 伪装成较新的 Flash Player 版本(摩尔庄园按 plugin 版本判断兼容)
        .with_player_version(Some(32))
}

/// 设置中文字体回退链。
///
/// 游戏大部分 UI 用内嵌字体(实测“摩尔城堡”等已能正常显示),
/// 但动态文本(玩家名/聊天/系统提示)走 `_sans`/`_serif` 设备字体,
/// 需要给出各平台可用的中文字体名,Ruffle 才能从系统字体库里找到回退。
///
/// 真正“一定有中文字体”的保险做法是各端自带一份中文 TTF 并注册为 device font,
/// 这里先给系统字体名的回退链(macOS=PingFang / Win=YaHei / Linux·Android=Noto CJK / iOS=PingFang)。
pub fn set_mole_fonts(player: &mut Player) {
    player.set_default_font(
        DefaultFont::Sans,
        vec![
            "PingFang SC".into(),       // macOS / iOS
            "Microsoft YaHei".into(),   // Windows
            "Noto Sans CJK SC".into(),  // Linux / Android
            "Source Han Sans SC".into(),
            "Heiti SC".into(),
            "Arial".into(),
        ],
    );
    player.set_default_font(
        DefaultFont::Serif,
        vec![
            "Songti SC".into(),
            "SimSun".into(),
            "Noto Serif CJK SC".into(),
            "Times New Roman".into(),
        ],
    );
    player.set_default_font(
        DefaultFont::Typewriter,
        vec![
            "PingFang SC".into(),
            "Noto Sans CJK SC".into(),
            "Courier New".into(),
        ],
    );
}

/// 摩尔庄园专用的 `NavigatorInterface`。
///
/// - `confirm_socket`:一律放行(等价桌面版 `--tcp-connections allow`),
///   让游戏能裸 TCP 连 `123.206.131.236:1865` / `:3200` 等服务器。
/// - `navigate_to_website`:外链(充值页等),本基础实现先只记日志;
///   桌面壳可覆盖为打开系统浏览器。
/// - `open_file`:本地文件,这里直接走 std(本客户端基本不需要)。
#[derive(Clone, Default)]
pub struct MoleNavigatorInterface;

impl NavigatorInterface for MoleNavigatorInterface {
    fn navigate_to_website(&self, url: Url) {
        tracing::info!("navigate_to_website(忽略): {url}");
    }

    fn open_file(
        &self,
        path: &Path,
    ) -> impl std::future::Future<Output = io::Result<std::fs::File>> + Send {
        let path = path.to_path_buf();
        async move { std::fs::File::open(path) }
    }

    fn confirm_socket(
        &self,
        host: &str,
        port: u16,
    ) -> impl std::future::Future<Output = bool> + Send {
        tracing::info!("放行 socket: {host}:{port}");
        async move { true }
    }
}

/// 设备字体回退顺序:任何字体名都先试精确匹配,失败再依次退到这些
/// “一定带中文”的字体,保证摩尔庄园的动态文本(玩家名/聊天/系统提示)能显示中文。
const FONT_FALLBACKS: &[&str] = &[
    "PingFang SC",       // macOS / iOS
    "Microsoft YaHei",   // Windows
    "Noto Sans CJK SC",  // Linux / Android
    "Source Han Sans SC",
    "Heiti SC",
    "STHeiti",
    "Arial Unicode MS",
    "Hiragino Sans GB",
];

/// MoleRuffle 的 `UiBackend`。
///
/// 默认的 `NullUiBackend` 不提供任何设备字体,导致摩尔庄园所有动态文本
/// (`_sans`/`_serif`)“text will be missing”。这里用系统字体库(fontdb)
/// 实现 `load_device_font`:游戏要什么字体名就给什么,找不到就回退到带中文的字体。
/// 其余方法全部 no-op(本客户端不需要剪贴板/对话框等)。
#[derive(Clone)]
pub struct MoleUiBackend {
    fonts: Arc<fontdb::Database>,
}

impl MoleUiBackend {
    /// 加载系统字体(mac=PingFang / win=YaHei / iOS=PingFang / Android=Noto CJK)。
    pub fn with_system_fonts() -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        // iOS/Android 上 `load_system_fonts` 常找不到系统字体目录(返回 0),
        // 这里显式补上各平台的系统字体路径,确保有中文字体可回退。
        for dir in [
            "/System/Library/Fonts",              // iOS / macOS
            "/System/Library/Fonts/Core",         // iOS 核心字体(含 PingFang)
            "/System/Library/Fonts/Cache",        // iOS
            "/System/Library/Fonts/Supplemental", // macOS(Arial Unicode 等)
            "/system/fonts",                      // Android(Noto CJK)
            "/system/font",
            "/data/fonts",
        ] {
            db.load_fonts_dir(dir);
        }
        tracing::info!("MoleUiBackend: 载入 {} 个字体面", db.len());
        Self {
            fonts: Arc::new(db),
        }
    }

    fn try_register(
        &self,
        family: &str,
        query: &FontQuery,
        register: &mut dyn FnMut(FontDefinition),
    ) -> bool {
        let q = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            weight: if query.is_bold {
                fontdb::Weight::BOLD
            } else {
                fontdb::Weight::NORMAL
            },
            style: if query.is_italic {
                fontdb::Style::Italic
            } else {
                fontdb::Style::Normal
            },
            ..Default::default()
        };
        let Some(id) = self.fonts.query(&q) else {
            return false;
        };
        let def = self.fonts.with_face_data(id, |data, index| FontDefinition::FontFile {
            name: query.name.clone(),
            is_bold: query.is_bold,
            is_italic: query.is_italic,
            data: FontFileData::new(data.to_vec()),
            index,
        });
        if let Some(def) = def {
            register(def);
            true
        } else {
            false
        }
    }
}

impl UiBackend for MoleUiBackend {
    fn load_device_font(&self, query: &FontQuery, register: &mut dyn FnMut(FontDefinition)) {
        // 1) 先按游戏请求的确切字体名找
        if self.try_register(&query.name, query, register) {
            return;
        }
        // 2) 退到带中文的字体,保证中文不丢
        for fallback in FONT_FALLBACKS {
            if self.try_register(fallback, query, register) {
                tracing::debug!("字体 '{}' 回退到 '{}'", query.name, fallback);
                return;
            }
        }
        tracing::warn!("字体 '{}' 无可用回退", query.name);
    }

    fn mouse_visible(&self) -> bool {
        true
    }
    fn set_mouse_visible(&mut self, _visible: bool) {}
    fn set_mouse_cursor(&mut self, _cursor: MouseCursor) {}
    fn clipboard_content(&mut self) -> String {
        String::new()
    }
    fn set_clipboard_content(&mut self, _content: String) {}
    fn set_fullscreen(&mut self, _is_full: bool) -> Result<(), FullscreenError> {
        Ok(())
    }
    fn display_root_movie_download_failed_message(&self, _invalid_swf: bool, _fetch_error: String) {}
    fn message(&self, _message: &str) {}
    fn display_unsupported_video(&self, _url: Url) {}
    fn sort_device_fonts(
        &self,
        _query: &FontQuery,
        _register: &mut dyn FnMut(FontDefinition),
    ) -> Vec<FontQuery> {
        Vec::new()
    }
    fn open_virtual_keyboard(&self) {}
    fn close_virtual_keyboard(&self) {}
    fn language(&self) -> LanguageIdentifier {
        "zh-CN".parse().expect("合法 language id")
    }
    fn display_file_open_dialog(&mut self, _filters: Vec<FileFilter>) -> Option<DialogResultFuture> {
        None
    }
    fn display_file_open_dialog_multiple(
        &mut self,
        _filters: Vec<FileFilter>,
    ) -> Option<MultiDialogResultFuture> {
        None
    }
    fn close_file_dialog(&mut self) {}
    fn display_file_save_dialog(
        &mut self,
        _file_name: String,
        _domain: String,
    ) -> Option<DialogResultFuture> {
        None
    }
}
