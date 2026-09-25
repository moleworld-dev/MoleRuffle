//! 服务器配置(官方服 / 平行服)。
//!
//! ★为什么必须绑成一个不可拆的整体★
//!
//! `Client.swf` 与服务器的版本清单是**强绑定**的:它内部 `TaomeeVersionManager.VERSION` 是编译进
//! ABC 常量池的字面量,启动时拉 `version/zzz_config.txt` 拿到清单版本号做相等校验,不等就 throw。
//! 实测(反编译两服 Client.swf 逐字节比对):
//!   - 官方服 `mole.61.com`      VERSION = 20140805,清单 = 1452075536 → version1452075536.swf 头 = 20140805
//!   - 平行服 `mole.61player.com` VERSION = 20250211,清单 = 1739349872 → version1739349872.swf 头 = 20250211
//! 两个 SWF 的 ABC 常量池里**各自只有自己那个版本号**(官方包里 find(20250211) = -1)。
//!
//! 所以「只改 base_url 不换 SWF」= 版本闸 100% 抛异常。更坑的是:该异常抛在 URLStream 的
//! COMPLETE 事件处理器里,而 fork 对事件监听器内的未捕获 AVM2 异常是**隔离**的(只打一行
//! `Error dispatching event`,不崩不弹窗)→ 游戏永远停在 `请稍候,正在进入摩尔庄园...`,零线索。
//!
//! 因此这里把 swf/base/spoof/host/id **绑成一个 const,不导出任何单独的 URL 常量**,
//! 从类型上杜绝"改了一半"。
//!
//! ★spoof 必须跟着服务器变★
//! spoof URL 决定引擎眼里 root movie 的 URL,而 `SharedObject`(.sol 存档)的落盘路径是
//! `{movie_host}/{local_path}/{name}.sol`(见 ruffle `avm2/globals/flash/net/shared_object.rs`)。
//! spoof 不跟着换 = 两个服的存档写进同一棵 `mole.61.com/` 目录互相覆盖(玩家会在官方服登录框
//! 看到平行服的米米号,且完全静默无法自查)。反过来只要 spoof 跟着走,**分服是引擎白送的**,
//! [`crate::mole_storage_dir`] 一行都不用改。

use std::sync::OnceLock;

use url::Url;

/// 一个可连的摩尔庄园服务器。五个字段同源同变,不可拆开使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConfig {
    /// 稳定短 id:用作缓存分目录名与配置文件里的持久化标识。改了会让老缓存变孤儿。
    pub id: &'static str,
    /// 给人看的名字(选服 UI / 窗口标题)。
    pub name: &'static str,
    /// root `Client.swf` 的绝对 URL(壳层 `fetch_root_movie` 用)。
    pub swf_url: &'static str,
    /// 相对资源(`resource/`、`version/`、`module/`…)的解析 base。
    /// **结尾斜杠不能省** —— `ExternalNavigatorBackend` 解析相对路径时会 pop 掉最后一段。
    pub base_url: &'static str,
    /// 喂给 `with_spoofed_url` / `with_page_url` 的 URL,决定 .sol 存档分层。通常同 `swf_url`。
    pub spoof_url: &'static str,
    /// 主机名。缓存白名单按它**精确匹配** `Url::host_str()`(不是子串匹配)。
    pub host: &'static str,
}

/// 官方服(淘米官方,`Client.swf` 自 2022-12 起未再更新)。
pub const OFFICIAL: ServerConfig = ServerConfig {
    id: "official",
    name: "官方服",
    swf_url: "http://mole.61.com/Client.swf",
    base_url: "http://mole.61.com/",
    spoof_url: "http://mole.61.com/Client.swf",
    host: "mole.61.com",
};

/// 平行摩尔(饭制服,与官方站同构;实测仍在活跃迭代,`Cache-Control: no-cache`)。
pub const PARALLEL: ServerConfig = ServerConfig {
    id: "parallel",
    name: "平行摩尔",
    swf_url: "https://mole.61player.com/Client.swf",
    base_url: "https://mole.61player.com/",
    spoof_url: "https://mole.61player.com/Client.swf",
    host: "mole.61player.com",
};

/// 所有内置服务器(选服 UI 遍历用)。
pub const ALL: &[ServerConfig] = &[OFFICIAL, PARALLEL];

/// 按 id 找服务器。
pub fn by_id(id: &str) -> Option<ServerConfig> {
    ALL.iter().copied().find(|s| s.id.eq_ignore_ascii_case(id.trim()))
}

static SELECTED: OnceLock<ServerConfig> = OnceLock::new();

/// 持久化选服文件:`<数据目录>/MoleRuffle/server.txt`,内容就是一行 id。
///
/// 为什么用文件而不是命令行参数:**iOS / Android 壳拿不到命令行参数**,env 在移动端也设不了,
/// 只有可写文件是五端通用的底座。桌面另支持 `MOLE_SERVER` env 覆盖(调试方便)。
pub fn server_file() -> std::path::PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("MoleRuffle")
        .join("server.txt")
}

/// 把选择持久化(下次启动生效)。选服 UI / 壳层调用。
pub fn persist(cfg: ServerConfig) -> std::io::Result<()> {
    let path = server_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, cfg.id)?;
    tracing::info!("已记住服务器选择: {} → {}", cfg.name, path.display());
    Ok(())
}

/// 显式指定本进程使用的服务器。**必须在任何 [`selected`] 调用之前**(即 build_player 之前)调用,
/// 否则返回 `Err(已锁定的配置)`——因为 root movie / navigator base / 缓存目录都已按旧值装配,
/// 运行时换服必须重建整个 Player,不在本函数职责内。
pub fn select(cfg: ServerConfig) -> Result<(), ServerConfig> {
    SELECTED.set(cfg).map_err(|_| *SELECTED.get().expect("已初始化"))
}

/// 本进程使用的服务器。首次调用时按 **env `MOLE_SERVER` > 持久化文件 > 官方服** 的顺序锁定。
pub fn selected() -> ServerConfig {
    *SELECTED.get_or_init(|| {
        // 1) env(桌面调试用;移动端设不了,自然跳过)
        if let Ok(v) = std::env::var("MOLE_SERVER") {
            if let Some(cfg) = by_id(&v) {
                tracing::info!("服务器(来自 MOLE_SERVER): {}", cfg.name);
                return cfg;
            }
            tracing::warn!("MOLE_SERVER='{v}' 不是已知服务器 id,忽略");
        }
        // 2) 持久化文件(五端通用)
        if let Ok(s) = std::fs::read_to_string(server_file()) {
            if let Some(cfg) = by_id(&s) {
                tracing::info!("服务器(来自 server.txt): {}", cfg.name);
                return cfg;
            }
        }
        // 3) 默认官方服
        OFFICIAL
    })
}

/// root `Client.swf` 的 URL(已按当前服务器解析)。
pub fn swf_url() -> Url {
    Url::parse(selected().swf_url).expect("内置 swf_url 必须是合法 URL")
}

/// 相对资源解析 base(已按当前服务器解析)。
pub fn base_url() -> Url {
    Url::parse(selected().base_url).expect("内置 base_url 必须是合法 URL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 所有内置服务器的_url_都合法且_base_带结尾斜杠() {
        for s in ALL {
            let swf = Url::parse(s.swf_url).expect("swf_url 合法");
            let base = Url::parse(s.base_url).expect("base_url 合法");
            Url::parse(s.spoof_url).expect("spoof_url 合法");
            // base 必须以 / 结尾,否则 ExternalNavigatorBackend 解析相对路径会 pop 掉最后一段
            assert!(s.base_url.ends_with('/'), "{} 的 base_url 必须以 / 结尾", s.id);
            // host 必须与 swf/base 的真实 host 一致(缓存白名单靠它精确匹配)
            assert_eq!(swf.host_str(), Some(s.host), "{} 的 host 与 swf_url 不符", s.id);
            assert_eq!(base.host_str(), Some(s.host), "{} 的 host 与 base_url 不符", s.id);
        }
    }

    #[test]
    fn id_唯一且能被_by_id_找到() {
        for s in ALL {
            assert_eq!(by_id(s.id).map(|c| c.id), Some(s.id));
        }
        let n = ALL.len();
        let uniq: std::collections::HashSet<_> = ALL.iter().map(|s| s.id).collect();
        assert_eq!(uniq.len(), n, "服务器 id 必须唯一(缓存按 id 分目录)");
    }

    #[test]
    fn by_id_忽略大小写与空白_未知_id_返回_none() {
        assert_eq!(by_id("  OFFICIAL \n").map(|c| c.id), Some("official"));
        assert!(by_id("nope").is_none());
    }
}
