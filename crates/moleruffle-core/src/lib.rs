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

use ruffle_core::config::Letterbox;
use ruffle_core::font::DefaultFont;
use ruffle_core::{LoadBehavior, Player, PlayerBuilder};
use ruffle_frontend_utils::backends::navigator::NavigatorInterface;
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
