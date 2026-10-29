//! 玩家可调的高级设置:一张设置项表 + `settings.txt` 读写。
//!
//! 设置文件:`<数据目录>/MoleRuffle/settings.txt`(mac 为 ~/Library/Application Support/MoleRuffle),
//! 每行 `键=值`,`#` 开头为注释。启动时在 [`crate::init_process_env`](单线程期)把每一项作为
//! 同名环境变量的**默认值**设进去 —— 已有代码本来就读这些环境变量,所以设置项不用逐处改代码;
//! 手动设的环境变量仍然优先(调试 / 对照用)。界面([`SPECS`])按表自动生成。
//!
//! 服务器不在这里:沿用 `server.txt`(见 [`crate::server`]),界面直接改它。
//! `MOLE_FPS` 这类会改变游戏速度的开关**绝不**放进来(帧基游戏提帧率会整体加速)。

use std::collections::BTreeMap;
use std::path::PathBuf;

/// 设置项分组(界面上的标签页)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Display,
    Memory,
    Diagnostics,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Display, Group::Memory, Group::Diagnostics];

    pub fn label(self) -> &'static str {
        match self {
            Group::Display => "画质与显示",
            Group::Memory => "内存与缓存",
            Group::Diagnostics => "安全与诊断",
        }
    }
}

/// 取值类型。
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// 开关:存 `1` / `0`。
    Bool,
    /// 几个固定选项:(存储值, 显示名)。
    Choice(&'static [(&'static str, &'static str)]),
    /// 整数范围(含两端)与步长。
    Int { min: i64, max: i64, step: i64 },
}

/// 一个设置项。
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    /// 对应的环境变量名,也是 settings.txt 里的键。
    pub key: &'static str,
    pub label: &'static str,
    /// 一句话说明(界面上显示在名称下面)。
    pub help: &'static str,
    pub group: Group,
    pub kind: Kind,
    /// 默认值(各平台不同的,取当前平台的)。
    pub default: &'static str,
    /// 改了要重启游戏才生效。
    pub restart: bool,
    /// 只在这些平台显示(空 = 全部)。
    pub platforms: &'static [Platform],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Desktop,
    Mobile,
}

/// 当前平台。
pub fn platform() -> Platform {
    if cfg!(any(target_os = "ios", target_os = "android")) {
        Platform::Mobile
    } else {
        Platform::Desktop
    }
}

const QUALITY_CHOICES: &[(&str, &str)] = &[
    ("low", "低(手机默认,最省电)"),
    ("medium", "中"),
    ("high", "高(4 倍抗锯齿)"),
    ("high8x8", "最高(8 倍抗锯齿,电脑默认)"),
];

const ON_OFF_DEFAULT_ON: &str = "1";

/// 全部设置项。界面按顺序显示;分组见 [`Group`]。
pub const SPECS: &[Spec] = &[
    // ───────── 画质与显示 ─────────
    Spec {
        key: "MOLE_QUALITY",
        label: "画质",
        help: "抗锯齿档位。越高越清晰,也越费电、越占显存。",
        group: Group::Display,
        kind: Kind::Choice(QUALITY_CHOICES),
        default: if cfg!(target_os = "ios") { "low" } else { "high8x8" },
        restart: false,
        platforms: &[],
    },
    Spec {
        key: "MOLE_RENDER_SCALE",
        label: "渲染分辨率(%)",
        help: "按屏幕分辨率的百分之几来画。调低更省电、更省内存,画面略糊。",
        group: Group::Display,
        kind: Kind::Int { min: 50, max: 100, step: 5 },
        default: if cfg!(target_os = "ios") { "80" } else { "100" },
        restart: true,
        platforms: &[],
    },
    Spec {
        key: "MOLE_INTERP",
        label: "平滑插值(60 帧观感)",
        help: "游戏逻辑仍是 24 帧,画面按 60 帧在两帧之间插值,移动更顺,不会加速游戏。",
        group: Group::Display,
        kind: Kind::Bool,
        default: "0",
        restart: true,
        platforms: &[Platform::Desktop],
    },
    Spec {
        key: "MOLE_DIRTY_RECT",
        label: "局部重画",
        help: "画面只有一部分变化时只重画那一块,大幅省电。出现画面残留时可关掉。",
        group: Group::Display,
        kind: Kind::Bool,
        default: ON_OFF_DEFAULT_ON,
        restart: true,
        platforms: &[],
    },
    // ───────── 内存与缓存 ─────────
    Spec {
        key: "MOLE_POOL_BUDGET_MB",
        label: "离屏纹理池上限(MB)",
        help: "滤镜、缓存位图用的临时纹理最多保留多少。调低省内存,切场景时可能稍卡。",
        group: Group::Memory,
        kind: Kind::Int { min: 64, max: 1024, step: 32 },
        default: if cfg!(target_os = "ios") { "288" } else { "512" },
        restart: true,
        platforms: &[],
    },
    Spec {
        key: "MOLE_CACHE_BUDGET_MB",
        label: "资源缓存上限(MB)",
        help: "下载过的场景、图片存在本机,下次不用再下载。超过上限时删最旧的。",
        group: Group::Memory,
        kind: Kind::Int { min: 256, max: 8192, step: 256 },
        default: "1536",
        restart: true,
        platforms: &[],
    },
    Spec {
        key: "MOLE_BITMAP_PREDECODE",
        label: "位图后台预解码",
        help: "进场景前用其它 CPU 核提前解码图片,减少进场景时的卡顿,会多占一点内存。",
        group: Group::Memory,
        kind: Kind::Bool,
        default: ON_OFF_DEFAULT_ON,
        restart: true,
        platforms: &[],
    },
    Spec {
        key: "MOLE_CAB_MEMO",
        label: "动画缓存记忆",
        help: "发光、闪烁这类循环动画画过一次就记住,省电;会多占约 48MB 显存。",
        group: Group::Memory,
        kind: Kind::Bool,
        default: ON_OFF_DEFAULT_ON,
        restart: true,
        platforms: &[],
    },
    // ───────── 安全与诊断 ─────────
    Spec {
        key: "MOLE_DT_CLAMP",
        label: "防切后台掉线",
        help: "从后台回来时不让游戏一口气补跑积压的时间,避免服务器因时间跳变断开连接。",
        group: Group::Diagnostics,
        kind: Kind::Bool,
        default: ON_OFF_DEFAULT_ON,
        restart: true,
        platforms: &[],
    },
    Spec {
        key: "MOLE_PREFETCH",
        label: "启动预取",
        help: "启动时提前并行下载主程序与版本清单,进登录页更快。",
        group: Group::Diagnostics,
        kind: Kind::Bool,
        default: ON_OFF_DEFAULT_ON,
        restart: true,
        platforms: &[],
    },
    Spec {
        key: "MOLE_NET_LOG",
        label: "网络日志",
        help: "把每个请求的耗时写进日志,排查加载慢时用。",
        group: Group::Diagnostics,
        kind: Kind::Bool,
        default: "0",
        restart: true,
        platforms: &[],
    },
    Spec {
        key: "MOLE_FAST_PATHS",
        label: "渲染加速总开关",
        help: "各项渲染加速一起开关。画面出现异常时先关掉它确认是不是加速引起的。",
        group: Group::Diagnostics,
        kind: Kind::Bool,
        default: ON_OFF_DEFAULT_ON,
        restart: true,
        platforms: &[],
    },
];

/// 当前平台要显示的设置项。
pub fn visible_specs() -> impl Iterator<Item = &'static Spec> {
    let p = platform();
    SPECS
        .iter()
        .filter(move |s| s.platforms.is_empty() || s.platforms.contains(&p))
}

pub fn spec(key: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.key == key)
}

/// 取值是否合法。
pub fn valid(spec: &Spec, value: &str) -> bool {
    match spec.kind {
        Kind::Bool => matches!(value, "0" | "1"),
        Kind::Choice(choices) => choices.iter().any(|(v, _)| *v == value),
        Kind::Int { min, max, .. } => value.parse::<i64>().is_ok_and(|v| (min..=max).contains(&v)),
    }
}

/// 设置文件路径。
pub fn settings_file() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("MoleRuffle").join("settings.txt"))
}

/// 读设置文件(只收合法的已知项)。
pub fn load() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Some(text) = settings_file().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return map;
    };
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if let Some(spec) = spec(key)
            && valid(spec, value)
        {
            map.insert(key.to_string(), value.to_string());
        }
    }
    map
}

/// 写设置文件(只写与默认值不同的项;写临时文件再改名,不会写出半截文件)。
pub fn save(values: &BTreeMap<String, String>) -> std::io::Result<()> {
    let Some(path) = settings_file() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = String::from("# MoleRuffle 高级设置(由游戏内设置界面生成,也可以手动编辑)\n");
    for spec in SPECS {
        if let Some(v) = values.get(spec.key)
            && v != spec.default
            && valid(spec, v)
        {
            text.push_str(&format!("{}={v}\n", spec.key));
        }
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

/// 当前生效的值:环境变量(已含设置文件与默认值)> 默认值。
pub fn current(spec: &Spec) -> String {
    std::env::var(spec.key)
        .ok()
        .filter(|v| valid(spec, v))
        .unwrap_or_else(|| spec.default.to_string())
}

/// 启动时调用(单线程期):设置文件里的每一项作为环境变量默认值(已设的不覆盖)。
/// 返回生效的项,供日志打印。
pub(crate) fn apply_at_startup(set_default: impl Fn(&str, &str)) -> Vec<String> {
    let values = load();
    for (k, v) in &values {
        set_default(k, v);
    }
    values.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// 渲染分辨率比例(0.5~1.0)。取 `MOLE_RENDER_SCALE`(百分比),没有就用平台默认。
pub fn render_scale() -> f64 {
    let spec = spec("MOLE_RENDER_SCALE").expect("表里必须有渲染分辨率");
    current(spec).parse::<f64>().map_or(1.0, |p| p / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_are_consistent() {
        for s in SPECS {
            assert!(valid(s, s.default), "{} 的默认值不合法", s.key);
            assert_eq!(SPECS.iter().filter(|o| o.key == s.key).count(), 1, "{} 重复", s.key);
            assert_ne!(s.key, "MOLE_FPS", "MOLE_FPS 会让游戏加速,不能放进设置");
        }
        let q = spec("MOLE_QUALITY").unwrap();
        assert!(valid(q, "medium") && !valid(q, "ultra"));
        let r = spec("MOLE_RENDER_SCALE").unwrap();
        assert!(valid(r, "75") && !valid(r, "200") && !valid(r, "abc"));
    }
}
