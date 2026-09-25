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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ruffle_core::backend::ui::{
    DialogResultFuture, FileFilter, FontDefinition, FullscreenError, MouseCursor,
    MultiDialogResultFuture, UiBackend,
};
use ruffle_core::config::Letterbox;
use ruffle_core::font::{DefaultFont, FontFileData, FontQuery};
use ruffle_core::{LoadBehavior, Player, PlayerBuilder, StageScaleMode};
use ruffle_render::quality::StageQuality;
use ruffle_frontend_utils::backends::navigator::NavigatorInterface;
use ruffle_frontend_utils::backends::storage::DiskStorageBackend;
use unic_langid::LanguageIdentifier;
use url::Url;

pub mod cache;
pub use cache::{cache_dir, CachingNavigator, PENDING_BIG_LOAD_TRIM};
pub mod mem;
pub mod server;
pub mod workers;
pub use server::{base_url as game_base_url, swf_url as game_swf_url, ServerConfig};

/// 固定舞台尺寸(Client.swf 的逻辑尺寸)。
pub const STAGE_WIDTH: u32 = 960;
pub const STAGE_HEIGHT: u32 = 560;

/// 窗口标题。非官方服会带上服务器名,避免玩家分不清自己连的是哪个服。
pub fn window_title() -> String {
    let s = server::selected();
    if s.id == server::OFFICIAL.id {
        "摩尔庄园 · MoleRuffle".to_string()
    } else {
        format!("摩尔庄园 · MoleRuffle（{}）", s.name)
    }
}

/// 把一个全新的 `PlayerBuilder` 配成“摩尔庄园专用”。
///
/// 这是五端共享的关键装配:平台壳层先 `with_renderer/with_audio/with_navigator`,
/// 再调本函数补齐摩尔庄园需要的设置(尤其是 spoof,缺了它进不去游戏)。
pub fn apply_mole_settings(builder: PlayerBuilder) -> PlayerBuilder {
    // ★家园/场景灰屏根治(#1010 族,ABC-A1):被加载 SWF(如家园默认背景 160030.swf)主时间轴
    // 第1帧的嵌套多帧 MovieClip(mc2/door_mc)与命名按钮 btn,在 Ruffle 单趟 construct_frame 里
    // 不被构造 → 游戏加载回调访问 goodsMC.mc2.getChildAt(0) / owner.parent["btn"] 拿到 undefined
    // → #1010/#1009 → 家园背景初始化夭折灰屏。启用 fork 里已实现的 eager-construct 递归补齐
    // (loader.rs:2112 门控 + :2421 递归):只【补上】Flash 派发 complete 前本就会做的构造,从不
    // 删改重排(loader.rs:2101-2104),且只对调用本函数的摩尔庄园路径生效(摩尔勇士 hero.61.com
    // 不调此函数,拿逐字节上游行为,隔离成立)。live 读 env,启动期 set_var 立即生效。
    // SAFETY: 本函数在客户端启动期(SWF 加载前)单线程调用一次,无并发 env 读写竞争。
    unsafe { std::env::set_var("MOLE_LOADER_EAGER_CONSTRUCT", "1"); }
    // 影片背景色生效前用黑色清屏(fork 默认白色):iOS 启动屏是黑的,等 Client.swf 那几秒
    // 整屏闪白很刺眼。影片自己的背景色一旦生效照常使用。见 ruffle-fork player.rs。
    // SAFETY: 同上,启动期单线程调用。
    unsafe { std::env::set_var("MOLE_DEFAULT_BG_BLACK", "1"); }

    // 画质/MSAA:iOS 真机关 MSAA(Low=1x)。Apple GPU 最大 4x MSAA,High8x8 在真机被钳到 4x、
    // 仍要按全屏物理像素(~2868×1320)分配 ~90MB+ MSAA framebuffer,且乘进每个滤镜/cacheAsBitmap
    // 离屏目标 → 进游戏世界叠纹理超 iOS jetsam 内存上限被 SIGKILL(实测真机闪退)。关 MSAA + 壳层
    // render_scale 降采样后显存大降。摩尔庄园源美术仅 960×560,关 MSAA 视觉几乎无感。桌面窗口小,
    // 保留 High8x8 高画质。
    #[cfg(target_os = "ios")]
    let default_quality = StageQuality::Low;
    #[cfg(not(target_os = "ios"))]
    let default_quality = StageQuality::High8x8;
    // 实验开关:MOLE_QUALITY=low|medium|high|high8x8 运行时选画质/MSAA(测 MSAA 对高帧率 GPU 成本)。
    // 未设=各端默认。采样数:low=1x medium=2x high=4x high8x8=8x(Metal 钳 4x)。
    let quality = match std::env::var("MOLE_QUALITY").as_deref() {
        Ok("low") => StageQuality::Low,
        Ok("medium") => StageQuality::Medium,
        Ok("high") => StageQuality::High,
        Ok("high8x8") => StageQuality::High8x8,
        _ => default_quality,
    };
    // ★舞台贴顶(仅移动端)★:折叠屏展开 / iPad 这类比舞台 960:560 更"方"的屏幕上,
    //   ShowAll 默认把游戏垂直居中,上下各留一条窄黑边,虚拟手柄只能压在游戏上。贴顶后
    //   两条窄边合成底部一整条,手柄放进去完全不挡画面(见 desktop/src/pad_layout.rs)。
    //   普通手机横屏比舞台更宽,垂直方向没有余量(build_matrices 里 height_delta=0),
    //   贴顶与居中结果完全一样,所以对普通手机零影响;且对齐只含 TOP,水平仍居中。
    //   强制(force)防止 SWF 自己改 stage.align 把它挪回去。桌面窗口没有手柄,保持居中。
    //   ★配套★:上游只在对齐为空时画黑边,强制贴顶会让黑边消失、两侧露出白色舞台背景和舞台外
    //   内容(实测 iPhone 两侧白边)。MOLE_LETTERBOX_FORCED_ALIGN 让 fork 在宿主强制对齐时照样画黑边。
    #[cfg(any(target_os = "ios", target_os = "android"))]
    let builder = {
        // SAFETY: 同上,启动期单线程调用。
        unsafe { std::env::set_var("MOLE_LETTERBOX_FORCED_ALIGN", "1"); }
        builder.with_align(ruffle_core::StageAlign::TOP, true)
    };

    builder
        .with_autoplay(true)
        .with_letterbox(Letterbox::On)
        .with_quality(quality)
        // ★ 强制 ShowAll:摩尔庄园舞台固定 960x560 且用 NoScale(老 Flash 游戏惯例),
        //   在手机/缩放窗口上会溢出屏幕。强制 ShowAll(force=true)把整个舞台等比缩放
        //   letterbox 适配任意屏幕尺寸,无视 SWF 自设的 scaleMode。
        .with_scale_mode(StageScaleMode::ShowAll, true)
        // 边下边跑:数百个资源 SWF 是运行时陆续拉的
        .with_load_behavior(LoadBehavior::Streaming)
        // ★ 宿主伪装 spoof:让 Client.swf 以为自己就在官网上(playerType/ExternalInterface 判定),
        //   不要 navigateToURL 弹走。★必须跟着当前服务器变★——spoof 决定引擎眼里 root movie 的
        //   URL,而 SharedObject(.sol)落盘路径是 {movie_host}/{local_path}/{name}.sol,spoof 不
        //   跟着换 = 官方服与平行服的存档写进同一棵目录互相覆盖(登录框看到别服的米米号,静默污染)。
        //   跟着换则分服是引擎白送的,mole_storage_dir() 不用动。详见 server.rs 头注释。
        .with_spoofed_url(Some(server::selected().spoof_url.to_string()))
        .with_page_url(Some(server::selected().spoof_url.to_string()))
        // 伪装成较新的 Flash Player 版本(摩尔庄园按 plugin 版本判断兼容)
        .with_player_version(Some(32))
        // 实验开关:MOLE_FPS=60 强制覆盖 SWF 的 24fps(用来测摩尔庄园是"帧基"还是"时间基")。
        // 默认(未设/解析失败)= None → 用 SWF 自带 24fps,零影响。帧基游戏提帧率会整体加速,
        // 时间基则只变顺不变快。forced_frame_rate 由 with_frame_rate(Some) 自动置真、覆盖 SWF 头。
        .with_frame_rate(std::env::var("MOLE_FPS").ok().and_then(|s| s.parse::<f64>().ok()))
}

/// 摩尔庄园本地存储(Flash `SharedObject` / `.sol`)的磁盘根目录。
///
/// 没有它,`PlayerBuilder` 默认装的是 `MemoryStorageBackend`(纯内存,见 ruffle
/// `core/src/player.rs:2975`),进程一退所有 `SharedObject` 全丢——登录页“记住账号”
/// 勾了也白勾,重启就没了。这里给出各端**可写、且重启/更新后仍保留**的目录:
///
///   - 桌面:`dirs::data_local_dir()`(mac=`~/Library/Application Support`,
///     win=`%LOCALAPPDATA%`,linux=`~/.local/share`)/MoleRuffle/SharedObjects
///   - iOS:`dirs::data_local_dir()` 在沙盒里就是 `$HOME/Library/Application Support`
///     ($HOME = app 容器根);Application Support 不对用户可见、不进 iCloud 文档,
///     是放 app 私有数据的标准位置,App 更新保留、仅卸载时清除(符合预期)。
///   - Android:`~/.local/share`(壳层若拿到 app filesDir 可改传绝对路径,见
///     [`attach_storage_at`])。
///
/// `DiskStorageBackend` 会在此目录下按 `{host}/{swf}/{name}.sol` 落盘
/// (摩尔庄园即 `mole.61.com/Client.swf/<名字>.sol`),目录不存在会自动创建。
pub fn mole_storage_dir() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| {
        // 极少数环境拿不到标准数据目录时,退到 HOME(再退到当前目录),保证仍是磁盘持久化。
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    });
    base.join("MoleRuffle").join("SharedObjects")
}

/// 给 `PlayerBuilder` 装上指向 `dir` 的磁盘存储后端([`DiskStorageBackend`])。
///
/// 平台壳如果能拿到更合适的可写目录(如 Android 的 app `filesDir`),
/// 直接传进来即可;否则用 [`attach_storage`] 走默认目录。
pub fn attach_storage_at(builder: PlayerBuilder, dir: PathBuf) -> PlayerBuilder {
    tracing::info!("SharedObject 存储目录: {}", dir.display());
    builder.with_storage(Box::new(DiskStorageBackend::new(dir)))
}

/// 给 `PlayerBuilder` 装上磁盘存储后端,目录用 [`mole_storage_dir`](各端默认数据目录)。
///
/// 五端壳层在 `apply_mole_settings` 之后(或之前皆可,`with_storage` 只是覆盖默认)
/// 调用一次,`SharedObject`(记住账号/各类本地存档)就会持久化到磁盘,重启不丢。
pub fn attach_storage(builder: PlayerBuilder) -> PlayerBuilder {
    attach_storage_at(builder, mole_storage_dir())
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

/// 已知**只含拉丁字形、无中文**的系统字体名。摩尔庄园里由 `new TextField()` +
/// `new TextFormat()`(未设 `.font`)动态创建的文本(如世界地图弹窗里各地图的
/// 任务/游戏/购物提示行、"任务"/"游戏"等分区标题)会用 Flash 默认字体名
/// **"Times New Roman"**;引擎把它当 `_serif` 设备字体解析,`load_device_font`
/// 第 1 步在 macOS/Windows 上会精确命中系统里**真正的** Times New Roman(纯拉丁),
/// 于是中文字形缺失、只剩 ASCII 的 `"* "` 和分隔号 `"--"` 被渲染 → 用户看到 "* --"。
/// 对这些名字跳过第 1 步精确匹配,直接落到下面的中文回退链 / 打包字体,即可补上中文。
const LATIN_ONLY_DEVICE_FONTS: &[&str] = &[
    "Times New Roman",
    "Times",
    "Arial",
    "Helvetica",
    "Tahoma",
    "Verdana",
    "Georgia",
    "Courier New",
    "Courier",
    "Consolas",
];

/// MoleRuffle opt-in:`MOLE_CJK_DEVICE_FONTS=1` 时,对 `LATIN_ONLY_DEVICE_FONTS`
/// 里的纯拉丁字体名跳过系统精确匹配,改用带中文的回退字体解析,修复世界地图弹窗
/// 提示行等动态文本 "* --"(中文缺字)问题。默认关闭 = 保持上游/现状行为
/// (与共用此 fork 的“摩尔勇士”同事一致,不影响他们)。
fn mole_cjk_device_fonts() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOLE_CJK_DEVICE_FONTS").as_deref() == Ok("1"))
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

/// 打包进二进制的中文+拉丁兜底字体:**PingFang SC Regular(苹方,iOS 原生高清字体)** 子集
/// (ASCII+标点+全部常用汉字 U+4E00-9FFF+假名,CFF 轮廓,~8.5MB)。iOS 真机沙盒读不到系统
/// 中文字体(/System/Library/Fonts/Core 不可读)、fontdb 按名查 PingFang 全失败时用它兜底,
/// 保证动态文本(玩家名/聊天/输入)不缺字且是 iOS 原生观感。ttf_parser 0.25 的 outline_glyph
/// 支持 CFF;`include_bytes!` 编进 .rodata,`FontFileData::new(BUNDLED_FONT)` 对 &'static 零拷贝。
const BUNDLED_FONT: &[u8] = include_bytes!("../assets/molefont.ttf");

/// MoleRuffle 的 `UiBackend`。
///
/// 默认的 `NullUiBackend` 不提供任何设备字体,导致摩尔庄园所有动态文本
/// (`_sans`/`_serif`)“text will be missing”。这里用系统字体库(fontdb)
/// 实现 `load_device_font`:游戏要什么字体名就给什么,找不到就回退到带中文的字体。
/// 其余方法全部 no-op(本客户端不需要剪贴板/对话框等)。
#[derive(Clone)]
pub struct MoleUiBackend {
    fonts: Arc<fontdb::Database>,
    /// 应用内剪贴板兜底(移动端无系统剪贴板时用;桌面也作镜像)。
    clip: Arc<std::sync::Mutex<String>>,
    /// 是否需要弹出软键盘:Flash 文本框聚焦时引擎调 open_virtual_keyboard 置 true,
    /// 失焦置 false。平台壳轮询此标志去 set_ime_allowed(显示/隐藏 iOS 软键盘)。
    kbd: Arc<std::sync::atomic::AtomicBool>,
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
            clip: Arc::new(std::sync::Mutex::new(String::new())),
            kbd: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// 平台壳取这个标志:为 true 时该弹软键盘(set_ime_allowed(true)),false 时收起。
    pub fn keyboard_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.kbd.clone()
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
        // 0) opt-in(MOLE_CJK_DEVICE_FONTS=1):游戏请求的是已知纯拉丁字体名
        //    (Times New Roman / Arial / Tahoma …)时,跳过第 1 步的系统精确匹配
        //    ——否则 macOS/Windows 会命中真正的纯拉丁字体,导致中文缺字(世界地图
        //    提示行显示 "* --")。直接落到下面的中文回退链 / 打包字体来补中文。
        let skip_exact = mole_cjk_device_fonts()
            && LATIN_ONLY_DEVICE_FONTS
                .iter()
                .any(|n| n.eq_ignore_ascii_case(query.name.trim()));

        // 1) 先按游戏请求的确切字体名找
        if !skip_exact && self.try_register(&query.name, query, register) {
            return;
        }
        // 2) 退到系统里带中文的字体(macOS/Windows 通常命中)
        for fallback in FONT_FALLBACKS {
            if self.try_register(fallback, query, register) {
                tracing::debug!("字体 '{}' 回退到 '{}'", query.name, fallback);
                return;
            }
        }
        // 3) ★ 最终兜底:用**打包进二进制**的 CJK+拉丁字体。iOS 真机沙盒读不到
        //    /System/Library/Fonts/Core(PingFang 等),fontdb 按名查全失败 → 上面两步都不命中,
        //    动态文本(玩家名/聊天/系统提示)会缺字/渲染异常。打包字体保证任何 device font 请求
        //    (含 'Tahoma' 等)都有可用字形。FontFileData::new 对 &'static 切片零拷贝(11MB 留在
        //    .rodata,只包一个小 Arc 指针),所有名字共享同一份字节。
        register(FontDefinition::FontFile {
            name: query.name.clone(),
            is_bold: query.is_bold,
            is_italic: query.is_italic,
            data: FontFileData::new(BUNDLED_FONT),
            index: 0,
        });
        tracing::debug!("字体 '{}' 用打包字体兜底", query.name);
    }

    fn mouse_visible(&self) -> bool {
        true
    }
    fn set_mouse_visible(&mut self, _visible: bool) {}
    fn set_mouse_cursor(&mut self, _cursor: MouseCursor) {}
    fn clipboard_content(&mut self) -> String {
        // 桌面:读系统剪贴板(arboard);iOS:读系统剪贴板(UIPasteboard);
        // 都失败/Android:用应用内兜底。
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
        {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                if let Ok(text) = cb.get_text() {
                    return text;
                }
            }
        }
        #[cfg(target_os = "ios")]
        {
            // 1) UIPasteControl 授权后投递的内容优先(绕过 iOS16+ 隐私拦截)
            if let Some(text) = paste_bridge::peek() {
                return text;
            }
            // 2) 同 app 内复制的内容(.string 不受隐私限制)
            if let Some(text) = ios_clipboard::get() {
                return text;
            }
        }
        self.clip.lock().map(|s| s.clone()).unwrap_or_default()
    }
    fn set_clipboard_content(&mut self, content: String) {
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
        {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(content.clone());
            }
        }
        #[cfg(target_os = "ios")]
        {
            ios_clipboard::set(&content);
        }
        // 应用内镜像兜底(系统剪贴板不可用时仍能应用内复制粘贴)
        if let Ok(mut s) = self.clip.lock() {
            *s = content;
        }
    }
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
    fn open_virtual_keyboard(&self) {
        // Flash 文本框聚焦:请求弹软键盘(平台壳轮询 keyboard_flag 去 set_ime_allowed)
        self.kbd.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn close_virtual_keyboard(&self) {
        self.kbd.store(false, std::sync::atomic::Ordering::Relaxed);
    }
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

/// iOS 粘贴桥:UIPasteControl(系统授权的粘贴按钮)点击后,平台壳把读到的文本经此送进来,
/// `clipboard_content` 优先返回它,从而绕过 iOS16+ 对程序化读剪贴板的隐私拦截(不弹窗)。
#[cfg(target_os = "ios")]
pub mod paste_bridge {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    static BUF: Mutex<Option<String>> = Mutex::new(None);
    static PENDING: AtomicBool = AtomicBool::new(false);

    /// UIPasteControl 授权后投递的文本(置缓冲 + 置“待粘贴”标志)。
    pub fn deliver(text: String) {
        if let Ok(mut b) = BUF.lock() {
            *b = Some(text);
        }
        PENDING.store(true, Ordering::Relaxed);
    }

    /// 平台壳轮询:为 true 时该发一次 TextControl::Paste(消费标志)。
    pub fn take_pending() -> bool {
        PENDING.swap(false, Ordering::Relaxed)
    }

    /// clipboard_content 偷看缓冲(不消费;粘贴的 gate 与实际插入会各读一次)。
    pub fn peek() -> Option<String> {
        BUF.lock().ok().and_then(|b| b.clone())
    }

    /// 文本框失焦时清掉,避免下次同 app 粘贴拿到陈旧内容。
    pub fn clear() {
        if let Ok(mut b) = BUF.lock() {
            *b = None;
        }
        PENDING.store(false, Ordering::Relaxed);
    }
}

/// iOS 系统剪贴板(UIPasteboard)。让游戏内的复制/粘贴与 iOS 系统剪贴板互通——
/// 比如登录时可粘贴从密码管理器复制的账号密码。
///
/// 调用发生在 winit 主线程的 player tick 内(单线程事件循环),满足 UIKit 主线程要求。
/// 读剪贴板会触发 iOS 的「X 从 Y 粘贴」提示横幅,属系统正常行为。
#[cfg(target_os = "ios")]
mod ios_clipboard {
    use objc2_foundation::NSString;
    use objc2_ui_kit::UIPasteboard;

    pub fn get() -> Option<String> {
        // SAFETY: 主线程调用;generalPasteboard/string 是标准只读 API。
        // 注意:iOS 16+ 对“外部 app 设置的内容”做隐私保护——程序化读 .string 会被系统拦截
        //   返回 nil(hasStrings 仍为 true),需走系统授权(真机会弹“允许粘贴”;模拟器静默拒绝)。
        //   同 app 内复制的内容不受此限。读不到时上层会回退到应用内镜像 self.clip。
        unsafe {
            let pb = UIPasteboard::generalPasteboard();
            pb.string().map(|s| s.to_string())
        }
    }

    pub fn set(text: &str) {
        // SAFETY: 主线程调用;setString 接受可空 NSString。
        unsafe {
            let pb = UIPasteboard::generalPasteboard();
            let ns = NSString::from_str(text);
            pb.setString(Some(&ns));
        }
    }
}
