//! 本地资源缓存(CDN 思路):把摩尔庄园从 mole.61.com 流式拉取的**静态资源**
//! (几百个 SWF / 图片)缓存到磁盘。二次加载直接读本地——秒开、且不依赖网络,
//! 缓解 mole.61.com 慢/抖动导致的资源拉取失败("可能服务器在维护")与每次重下的低效。
//!
//! 实现:包一层 [`CachingNavigator`] 在 `ExternalNavigatorBackend` 外面,只重写 `fetch`:
//!   - 可缓存(GET、无 body、host=mole.61.com、无 query/非动态)→ 命中读盘、未命中走内层
//!     网络后存盘;只缓存 HTTP 200。
//!   - 其余(登录 account.61.com / POST / socket)原样透传给内层,绝不缓存动态内容。
//!
//! socket(游戏服 123.206.131.236:1865 的实时协议)走 `connect_socket`,天然不经此缓存。

use std::borrow::Cow;
use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use encoding_rs::Encoding;
use indexmap::IndexMap;
use ruffle_core::backend::navigator::{
    ErrorResponse, NavigationMethod, NavigatorBackend, OwnedFuture, Request, SuccessResponse,
};
use ruffle_core::loader::Error;
use ruffle_core::socket::{SocketAction, SocketHandle};
use url::{ParseError, Url};

use crate::{server, workers};

/// 失败重试次数(总尝试 = RETRIES + 1)。只对幂等 GET 生效。抖动网络下瞬时超时重试一次往往就成。
const RETRIES: u32 = 2;

/// ★对冲请求★:发往游戏服务器的 GET 如果【停滞】了(一段时间内既没收到响应头、也没收到任何新数据),
/// 就并发再发一次,谁先完整成功用谁(其余的随 future 被丢弃)。
///
/// 为什么:这台服务器的典型病症是"同一个请求这次卡几十秒、紧接着再请求只要 0.6 秒"
/// (实测 163 字节的 XML 0.6~7.6 秒,20KB 的 Client.swf 0.6~36 秒)。顺序重试只在【失败】后才触发,
/// 对"卡着不回"无能为力,而 HTTP 层只设了连接超时、没有整体超时(怕误杀慢但正常的大文件)。
///
/// 判据是"停滞"而不是"总耗时":慢网络上正在稳定传输的大文件不会被重复下载(第一版按总耗时触发,
/// 会把 1MB 的资源复制 2~4 份,审查指出)。时间表按"距最近一次发起"计,每档对冲独立计数,
/// 失败重试不占对冲名额;主线程卡顿或切后台恢复后最多补发一个,不会把剩下几档一次全发出去。
const STALL: Duration = Duration::from_millis(2500);
/// 是否打印每个网络尝试的首字节时间(env `MOLE_NET_LOG=1`)。
fn net_log_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MOLE_NET_LOG").as_deref() == Ok("1"))
}

/// 硬停滞上限:所有在途尝试这么久都没有任何进展(响应头或新数据),就全部放弃重来;连续两次则报错。
/// 没有它的话,"原请求成了死连接、副本又都很快失败"或"副本回 4xx 后不再对冲"时会永远等下去
/// (切网、断网时真实可达,审查实测),主 SWF 碰上就永远白屏,也触发不了退避重试。
const HARD_STALL: Duration = Duration::from_secs(15);

/// 第 1/2/3 次对冲距上一次发起的最短间隔(还要同时满足"已停滞 STALL")。
const HEDGE_GAP: [Duration; 3] = [
    Duration::from_millis(2500),
    Duration::from_millis(3500),
    Duration::from_secs(6),
];

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 缓存运行时统计(全局):命中数 / 未命中数 / 命中读盘字节 / 写盘字节。
/// 供各端(如桌面 perf 日志)读取,量化缓存实际加速,避免盲调。
pub static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
pub static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
pub static CACHE_HIT_BYTES: AtomicU64 = AtomicU64::new(0);
pub static CACHE_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);
/// 触发过对冲(停滞请求并发重发)的次数,观测用(见 [`hedge_summary`])。
pub static HEDGED_REQUESTS: AtomicU64 = AtomicU64::new(0);

/// 累计对冲次数(桌面/iOS 的 [perf] 行打印)。
pub fn hedge_summary() -> u64 {
    HEDGED_REQUESTS.load(Ordering::Relaxed)
}

/// MoleRuffle:大加载信号(P0 内存事故缓解)。navigator 一看到 .swf 请求(切场景/魔灵等
/// 小游戏)就置位;壳的内存守卫下个 tick 消费它,抢在资源解码落地前 force_gc+清池腾余量。
/// 命中磁盘缓存也置位——内存峰值来自解码与注册,与走不走网络无关。
pub static PENDING_BIG_LOAD_TRIM: AtomicBool = AtomicBool::new(false);

/// (命中, 未命中, 命中读盘字节, 写盘字节)。
pub fn cache_summary() -> (u64, u64, u64, u64) {
    (
        CACHE_HITS.load(Ordering::Relaxed),
        CACHE_MISSES.load(Ordering::Relaxed),
        CACHE_HIT_BYTES.load(Ordering::Relaxed),
        CACHE_WRITE_BYTES.load(Ordering::Relaxed),
    )
}

/// 给 `ExternalNavigatorBackend` 套上本地资源缓存 + GET 重试。
/// 内层包 `Rc<RefCell<N>>`:重试需要在异步过程里重新发起请求,借此绕开 `&self` 的生命周期约束
/// (借用只在同步的 `fetch()`/`borrow_mut()` 调用期间持有,绝不跨 await)。
pub struct CachingNavigator<N> {
    inner: Rc<RefCell<N>>,
    cache_dir: PathBuf,
    /// 预取中的请求:(匹配键, 是否忽略查询串, 已启动的请求)。匹配的下一次 fetch 直接接手,
    /// 见 [`Self::prefetch`]。
    prefetched: RefCell<Vec<(String, bool, Prefetch, std::time::Instant)>>,
}

/// 预取的匹配键:完整网址,或忽略查询串时的"去掉查询串与片段的网址"。
fn prefetch_key(abs: &Url, ignore_query: bool) -> String {
    if ignore_query {
        let mut u = abs.clone();
        u.set_query(None);
        u.set_fragment(None);
        u.to_string()
    } else {
        abs.to_string()
    }
}

/// 预取请求的状态。
enum Prefetch {
    /// 请求已发出,并交给事件循环在后台继续驱动(对冲、重试照常进行);完成后结果从这里取。
    Pending(futures::channel::oneshot::Receiver<Result<Box<dyn SuccessResponse>, ErrorResponse>>),
    /// 第一次轮询就完成了(例如命中磁盘缓存)。
    Ready(Result<Box<dyn SuccessResponse>, ErrorResponse>),
}

impl<N> CachingNavigator<N> {
    /// `cache_dir` 用各端缓存目录(iOS=沙盒 Library/Caches,可被系统按需清理,正合缓存语义)。
    pub fn new(inner: N, cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        tracing::info!("资源缓存目录: {}", cache_dir.display());
        Self {
            inner: Rc::new(RefCell::new(inner)),
            cache_dir,
            prefetched: RefCell::new(Vec::new()),
        }
    }

    /// ★预取★:立刻对 `url` 发起 GET(与之后引擎的请求走完全相同的路径:对冲、重试、读 body),
    /// 并把这个已经在路上的请求留着;稍后同一 URL 的 fetch 直接接手,不再另发。
    ///
    /// 为什么:主 SWF 原本要等渲染器(冷缓存时着色器编译约 0.5 秒)、字体库、音频都初始化完,
    /// 引擎才发出请求;而白屏的主体是网络等待(剖析:加载窗口里主线程只忙 6~12%)。宿主在创建
    /// 渲染器之前调用它,让网络与这些初始化并行。
    ///
    /// 必须在 tokio 运行时上下文里调用(底层请求在第一次轮询时就 spawn 到 tokio 上)。
    /// 这里用空唤醒器同步轮询一次,只为把请求推出去;之后接手的 fetch 会用真正的唤醒器继续轮询。
    pub fn prefetch(&self, url: &str)
    where
        N: NavigatorBackend,
    {
        self.prefetch_with(url, false);
    }

    /// 同 [`Self::prefetch`],但之后匹配时【忽略查询串】:给"网址带每次随机的防缓存串、
    /// 而服务器其实忽略它"的文件用。只应用于确认过这一点的文件(版本清单
    /// `version/zzz_config.txt?<随机数>`:实测不同随机串的内容与 ETag 完全相同)。
    /// 内容同样是本次启动刚取到的,新鲜度与不预取相同,只是提前到和主 SWF 并行。
    pub fn prefetch_ignoring_query(&self, url: &str)
    where
        N: NavigatorBackend,
    {
        self.prefetch_with(url, true);
    }

    fn prefetch_with(&self, url: &str, ignore_query: bool)
    where
        N: NavigatorBackend,
    {
        let Ok(abs) = self.inner.borrow().resolve_url(url) else {
            return;
        };
        let mut fut = self.fetch_uncached_prefetch(Request::get(url.to_string()));
        // 先同步轮询一次:把底层请求立刻推上 tokio(此时事件循环还没开始跑)。
        let waker = futures::task::noop_waker_ref();
        let mut cx = std::task::Context::from_waker(waker);
        let state = match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(result) => Prefetch::Ready(result),
            std::task::Poll::Pending => {
                // 之后交给事件循环在后台继续驱动:被接手之前,对冲/重试/硬停滞上限照常工作
                // (原先接手前没人轮询,预取卡住时要等到接手才开始对冲)。
                let (sender, receiver) = futures::channel::oneshot::channel();
                self.inner.borrow_mut().spawn_future(Box::pin(async move {
                    let _ = sender.send(fut.await);
                    Ok(())
                }));
                Prefetch::Pending(receiver)
            }
        };
        tracing::info!("预取: {abs}");
        self.prefetched.borrow_mut().push((
            prefetch_key(&abs, ignore_query),
            ignore_query,
            state,
            std::time::Instant::now(),
        ));
    }

    /// 预取用的 fetch:与 [`NavigatorBackend::fetch`] 相同,只是绕开"接手预取"那一步(防止递归)。
    fn fetch_uncached_prefetch(
        &self,
        request: Request,
    ) -> OwnedFuture<Box<dyn SuccessResponse>, ErrorResponse>
    where
        N: NavigatorBackend,
    {
        self.fetch_inner(request)
    }

    /// URL → 缓存文件路径(对完整 URL 取稳定 hash,按前两位分桶,避免单目录文件过多)。
    fn cache_path(&self, url: &str) -> PathBuf {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut h);
        let hex = format!("{:016x}", h.finish());
        self.cache_dir
            .join(&hex[0..2])
            .join(format!("{hex}.swfcache"))
    }
}

/// 不可变媒体资产后缀白名单:只缓存这些确定"版本内不变"的静态资源。
/// 动态脚本(.php/.jsp)、配置/文本(.xml/.txt 可能无版本号却会变)一律不在白名单 → 不缓存。
const STATIC_EXT: &[&str] = &[
    ".swf", ".jpg", ".jpeg", ".png", ".gif", ".mp3", ".wav", ".bin", ".dat", ".fla", ".ttf",
];

/// 哪些请求可缓存。★入参必须是【已解析的绝对 URL】★,见 [`CachingNavigator::fetch`] 的说明。
///
/// 判定三条:
/// 1. host **精确等于**当前服务器的 host(不是子串匹配 —— 旧代码 `url.contains("mole.61.com")`
///    既漏掉平行服 `mole.61player.com`,又会把 `evil.example/x.swf?ref=mole.61.com` 误判为可缓存);
/// 2. 路径后缀属媒体白名单(**不因含 `?` 就拒**:Flash 惯例给静态资源加 `?v=` 版本串,
///    按"去 query 后的路径后缀"判,而缓存 key 用**完整绝对 URL(含 query)** 的 hash
///    → 版本号一变即换 key、自动 cache-bust,绝不拿陈旧);
/// 3. **root `Client.swf` 显式拉黑** —— 它是版本闸的另一半(见 server.rs 头注释):
///    SWF 内嵌的 VERSION 必须与服务端 `version/zzz_config.txt` 清单匹配。缓存它 = 服务端一更新
///    客户端,本地还拿旧 SWF → 旧 VERSION vs 新清单 → 永久卡在"请稍候",且无 TTL 自愈不了
///    (iOS 玩家连删缓存目录都做不到)。官方服四年没动 SWF 才没暴露,平行服在活跃迭代必然踩。
///    代价仅是每次启动多下 ~20KB。
fn is_cacheable(abs: &Url, has_body: bool) -> bool {
    if has_body {
        return false;
    }
    if abs.host_str() != Some(server::selected().host) {
        return false;
    }
    let path = abs.path().to_ascii_lowercase();
    if path.eq_ignore_ascii_case("/client.swf") {
        return false; // ★ root 版本闸,必须每次拿最新
    }
    STATIC_EXT.iter().any(|ext| path.ends_with(ext)) || is_versioned_text(abs)
}

/// 带版本串的配置/文本文件(4. 条):`.xml` / `.txt`,且查询串正好是游戏版本清单派生的版本号
/// `i` + 6~10 位小写字母数字(如 `config/Server.xml?i6ooed60`)。资源 SWF 用的是同一种版本串
/// (`LoginHome.swf?ij2o74r4`),文件一变清单就给新串、网址就变,所以和 SWF 一样可以按完整网址缓存。
///
/// 为什么要缓存:登录路径是一条串行链 —— 主 SWF → 版本闸 → loadingWord.xml → Server.xml → ext.xml,
/// 这台服务器上每个小文件要 0.4~2.7 秒(实测),三个 XML 加起来每次登录白等 2~6 秒。
/// 版本闸 `version/zzz_config.txt?<纯数字随机串>` 不匹配,照旧每次取最新。
/// 保险:命中缓存的同时在后台重新下载一次,内容变了就更新缓存(最多旧一次启动),见 revalidate。
fn is_versioned_text(abs: &Url) -> bool {
    // MOLE_CACHE_TEXT=0 关掉(对照/回退用)。只对官方服启用:官方服的这些文件自 2022 年起没变过
    // (Last-Modified),平行服还在活跃更新,可能出现"内容变了而版本串没变"(审查指出)。
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var("MOLE_CACHE_TEXT").as_deref() == Ok("0"))
        || server::selected().id != "official"
    {
        return false;
    }
    let path = abs.path().to_ascii_lowercase();
    if !(path.ends_with(".xml") || path.ends_with(".txt")) {
        return false;
    }
    let Some(query) = abs.query() else {
        return false;
    };
    let token = query.as_bytes();
    token.len() >= 7
        && token.len() <= 11
        && token[0] == b'i'
        && token[1..]
            .iter()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
}

/// 后台复核一个命中缓存的带版本文本:重新下载,内容和缓存不同就覆盖缓存(下次启动生效)。
/// 不影响本次返回;失败静默。每个网址每次启动只复核一次。
fn revalidate<N: NavigatorBackend>(
    inner: &Rc<RefCell<N>>,
    url: String,
    path: Option<PathBuf>,
    cached: Vec<u8>,
) {
    let Some(path) = path else {
        return;
    };
    {
        let mut done = REVALIDATED.lock().unwrap_or_else(|e| e.into_inner());
        if done.contains(&url) {
            return;
        }
        done.push(url.clone());
    }
    let fut = inner.borrow().fetch(Request::get(url.clone()));
    let task = Box::pin(async move {
        let Ok(resp) = fut.await else {
            return Ok(());
        };
        if resp.status() != 200 || resp.text_encoding().is_some() {
            return Ok(());
        }
        let expected = resp.expected_length().ok().flatten();
        let Ok(fresh) = resp.body().await else {
            return Ok(());
        };
        if fresh != cached && text_looks_complete(&url, &fresh, expected) {
            tracing::info!("带版本文本在服务器上已变化,更新缓存(下次启动生效): {url}");
            let _ = workers::offload(move || write_cache_atomic(&path, &fresh)).await;
        }
        Ok(())
    });
    inner.borrow_mut().spawn_future(task);
}

/// 文本内容看起来是完整的正常文件:非空;已知长度时长度一致;`.xml` 去掉 BOM 与空白后以 `<` 开头。
/// 用来挡住维护期/运营商劫持返回的 200 错误页、无长度时被截断的正文,免得它们进缓存。
fn text_looks_complete(url: &str, bytes: &[u8], expected: Option<u64>) -> bool {
    if bytes.is_empty() || expected.is_some_and(|n| n != bytes.len() as u64) {
        return false;
    }
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if path.ends_with(".xml") {
        let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        return body.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<');
    }
    true
}

/// 本进程里已经后台复核过的带版本文本(每个网址每次启动只复核一次)。
static REVALIDATED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// 确定性失败(4xx):重试毫无意义,只会把一次 404 放大成 3 次请求 + 3 倍延迟。
/// 5xx / 网络抖动 / 超时才值得重试(摩尔服务器抖起来确实靠重试救回)。
fn is_worth_retry(err: &ErrorResponse) -> bool {
    // 408(请求超时)、425、429(限流)是暂时性的 4xx。
    !matches!(err.error, Error::HttpNotOk(_, status, ..)
        if (400..500).contains(&status) && !matches!(status, 408 | 425 | 429))
}

/// 版本清单 `version/zzz_config.txt`(带每次随机的防缓存串,服务器忽略该串)。
fn is_version_gate(abs: &Url) -> bool {
    abs.path().eq_ignore_ascii_case("/version/zzz_config.txt")
}

/// 把 zlib 压缩的 SWF(`CWS`)解成未压缩的 `FWS`,供引擎直接解析。在后台线程调用。
///
/// 为什么:引擎拿到 CWS 会在主线程(winit 事件循环)上解压,切场景一次几十个资源 SWF,
/// 解压全压在主线程上变成可感知的卡顿。缓存层反正要在后台读盘/写盘,顺手在那里解好,
/// 主线程只做解析。FWS 头:签名 `FWS` + 版本 + 文件总长(含 8 字节头),正文即解压后的数据,
/// 与引擎 `swf::decompress_swf` 对 CWS 解出的内容逐字节一致(见单测)。
///
/// 非 CWS(已是 FWS、LZMA 压缩的 ZWS、非 SWF)原样返回;解压出错也**原样返回压缩数据**,
/// 交给引擎按原路径处理 —— 引擎对损坏流是"解多少用多少"的容错逻辑,这样行为与以前完全一致。
pub fn cws_to_fws(bytes: Vec<u8>) -> Vec<u8> {
    use std::io::Read;
    if bytes.len() < 8 || &bytes[0..3] != b"CWS" || bytes[3] == 0 {
        return bytes;
    }
    let declared = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    // 声明长度离谱(<8 或 >256MB)就不碰,交回引擎
    if !(8..=256 << 20).contains(&declared) {
        return bytes;
    }
    let mut out = Vec::with_capacity(declared);
    out.extend_from_slice(b"FWS");
    out.push(bytes[3]);
    out.extend_from_slice(&[0; 4]);
    let mut dec = flate2::read::ZlibDecoder::new(&bytes[8..]);
    if dec.read_to_end(&mut out).is_err() || out.len() > u32::MAX as usize {
        return bytes;
    }
    let total = out.len() as u32;
    out[4..8].copy_from_slice(&total.to_le_bytes());
    out
}

fn write_cache_atomic(path: &PathBuf, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 写临时文件再 rename,保证不会出现半截的损坏缓存。
    // ★tmp 名必须唯一★:老代码用固定的 `path.with_extension("tmp")`,同一 URL 并发写会互相
    // 踩踏(两个写入交错 → rename 出半截文件)。加进程内递增序号隔离。
    // 残留的 .tmp(写到一半被 jetsam/断电打断)由 trim_cache_in_background 启动时收走。
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{seq}"));
    if std::fs::write(&tmp, bytes).is_ok() {
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// 游戏自己的统计打点 `misc.js?…&sstid=首页素材加载&item=加载成功` = 登录页素材全部到齐(游戏自己的定义)。
fn is_home_loaded_ping(url: &str) -> bool {
    let Some((path, query)) = url.split_once('?') else {
        return false;
    };
    if !path.ends_with("misc.js") {
        return false;
    }
    let pairs: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).collect();
    let has = |key: &str, value: &str| pairs.iter().any(|(k, v)| k == key && v == value);
    has("sstid", "首页素材加载") && has("item", "加载成功")
}

impl<N: NavigatorBackend> NavigatorBackend for CachingNavigator<N> {
    fn fetch(&self, request: Request) -> OwnedFuture<Box<dyn SuccessResponse>, ErrorResponse> {
        // 发出请求时就记(不等统计服务器响应):桌面对照脚本 perf-ab / startup-ab 拿它当"登录页就绪"。
        if is_home_loaded_ping(request.url()) {
            tracing::info!("里程碑:游戏报告首页素材加载成功");
        }
        // 接手预取:同一个绝对 URL、无请求头的 GET(引擎请求主 SWF 正是如此)。
        if request.method() == NavigationMethod::Get
            && request.body().is_none()
            && request.headers().is_empty()
            && !self.prefetched.borrow().is_empty()
            && let Ok(abs) = self.inner.borrow().resolve_url(request.url())
        {
            let mut slots = self.prefetched.borrow_mut();
            // 60 秒还没被接手的预取丢掉(连同它占着的连接):引擎没按预期请求(网址形式不同、
            // 带了请求头等),留着只会一直占资源。
            slots.retain(|(key, _, _, at)| {
                let keep = at.elapsed() < Duration::from_secs(60);
                if !keep {
                    tracing::info!("预取 60 秒未被接手,丢弃: {key}");
                }
                keep
            });
            if let Some(index) = slots
                .iter()
                .position(|(key, ignore_query, _, _)| *key == prefetch_key(&abs, *ignore_query))
            {
                let (key, _, state, _) = slots.remove(index);
                tracing::info!("接手预取的请求: {key}(引擎请求 {abs})");
                let url = abs.to_string();
                return match state {
                    Prefetch::Ready(result) => Box::pin(async move { result }),
                    Prefetch::Pending(receiver) => Box::pin(async move {
                        receiver.await.unwrap_or_else(|_| {
                            Err(ErrorResponse {
                                url,
                                error: Error::FetchError("预取任务被丢弃".into()),
                            })
                        })
                    }),
                };
            }
        }
        self.fetch_inner(request)
    }

    // ── 其余方法全部透传给内层(借用仅在调用期间)──
    fn navigate_to_url(
        &self,
        url: &str,
        target: &str,
        vars_method: Option<(NavigationMethod, IndexMap<String, String>)>,
    ) {
        self.inner
            .borrow()
            .navigate_to_url(url, target, vars_method)
    }

    fn resolve_url(&self, url: &str) -> Result<Url, ParseError> {
        self.inner.borrow().resolve_url(url)
    }

    fn spawn_future(&mut self, future: OwnedFuture<(), Error>) {
        self.inner.borrow_mut().spawn_future(future)
    }

    fn pre_process_url(&self, url: Url) -> Url {
        self.inner.borrow().pre_process_url(url)
    }

    fn connect_socket(
        &mut self,
        host: String,
        port: u16,
        timeout: Duration,
        handle: SocketHandle,
        receiver: Receiver<Vec<u8>>,
        sender: Sender<SocketAction>,
    ) {
        self.inner
            .borrow_mut()
            .connect_socket(host, port, timeout, handle, receiver, sender)
    }
}

impl<N: NavigatorBackend> CachingNavigator<N> {
    fn fetch_inner(
        &self,
        request: Request,
    ) -> OwnedFuture<Box<dyn SuccessResponse>, ErrorResponse> {
        // 非 GET(POST 等)不幂等:既不缓存也不重试,原样透传(重试 POST 会重复提交)。
        if request.method() != NavigationMethod::Get {
            return self.inner.borrow().fetch(request);
        }

        let url = request.url().to_string();
        // 子 SWF 请求 = 即将有大加载(新场景/小游戏:库位图+cacheAsBitmap+movie_library 一起来,
        // 全是清池碰不到的项)→ 通知守卫先催收腾余量。(按后缀判,相对/绝对 URL 都成立。)
        if url
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .ends_with(".swf")
        {
            PENDING_BIG_LOAD_TRIM.store(true, Ordering::Relaxed);
        }

        // ★★ 缓存失效根治(2026-07-26)★★
        // 老代码直接拿 `request.url()` 判 host,但**这里收到的是 SWF 里原样写的相对路径**
        // (`resource/login/LoginHome.swf`、`module/external/logo/randomMC.swf` …):
        // `Player::fetch` 把 Request 原样交给 navigator 不做解析,真正的 `resolve_url` 发生在
        // **内层** ExternalNavigatorBackend::fetch —— 也就是本缓存层的【下游】。
        // 相对路径不含 host → 老的 host 判定全 false → **几百个资源一个都没进缓存**,只有壳层
        // 直接喂绝对 URL 的 root Client.swf 漏了进去(实测本机缓存目录当时只有 1 个文件)。
        // 而 CACHE_HITS/MISSES 只在 cache_path.is_some() 时计数 → 统计恒为 0,完美掩盖了根因。
        // 修法:先借内层的 resolve_url 解析成绝对 URL(它内部还会走 pre_process_url,与真实抓取
        // URL 一致),再判定 + 用绝对 URL 做 hash key(不同服天然不撞 key)。
        // 借用只在这一行同步调用期间持有,不跨 await。
        let abs = self.inner.borrow().resolve_url(&url).ok();
        let cache_path = abs
            .as_ref()
            .filter(|u| is_cacheable(u, request.body().is_some()))
            .map(|u| self.cache_path(u.as_str()));

        // 只对可缓存的资源 SWF 在后台解压(root Client.swf 不可缓存,保持原样)。
        let is_swf = url
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .ends_with(".swf");
        // 命中时也返回【绝对】URL,与未命中路径的 final_url 一致。这个 URL 会成为子影片的
        // loaderInfo.url;以前缓存几乎不生效所以没暴露,现在命中返回相对路径会让两条路径行为不一。
        let resp_url = abs
            .as_ref()
            .map(|u| u.to_string())
            .unwrap_or_else(|| url.clone());
        let inner = self.inner.clone();
        let headers = request.headers().clone();
        // 只对冲发往当前游戏服务器的请求(静态资源、配置、root SWF);登录等其它主机的 GET 保持
        // 原来的"失败后顺序重试",不并发重发。
        let on_game_host = abs
            .as_ref()
            .is_some_and(|u| u.host_str() == Some(server::selected().host));
        // 对冲只用于静态资源、带版本串的文本、主 SWF 与版本清单;发往游戏主机的其它 GET(可能
        // 有副作用)只保留原有的"失败后顺序重试"。
        let hedge = on_game_host
            && request.body().is_none()
            && (cache_path.is_some()
                || abs.as_ref().is_some_and(|u| {
                    u.path().eq_ignore_ascii_case("/client.swf") || is_version_gate(u)
                }));
        let is_root = on_game_host
            && abs
                .as_ref()
                .is_some_and(|u| u.path().eq_ignore_ascii_case("/client.swf"));
        Box::pin(async move {
            // ① 缓存命中:读盘 +(SWF)解压都在后台线程完成,主线程只拿到可直接解析的字节。
            let path_for_revalidate = cache_path.clone();
            if let Some(path) = cache_path.clone() {
                let hit = workers::offload(move || {
                    let raw = std::fs::read(&path).ok()?;
                    let disk_len = raw.len();
                    Some((disk_len, if is_swf { cws_to_fws(raw) } else { raw }))
                })
                .await
                .flatten();
                if let Some((disk_len, bytes)) = hit {
                    CACHE_HITS.fetch_add(1, Ordering::Relaxed);
                    CACHE_HIT_BYTES.fetch_add(disk_len as u64, Ordering::Relaxed);
                    tracing::debug!("缓存命中: {resp_url}");
                    if let Some(abs) = abs.as_ref().filter(|u| is_versioned_text(u)) {
                        revalidate(
                            &inner,
                            abs.to_string(),
                            path_for_revalidate.clone(),
                            bytes.clone(),
                        );
                    }
                    return Ok(
                        Box::new(CachedResponse::new(resp_url, bytes)) as Box<dyn SuccessResponse>
                    );
                }
                CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
            }

            // ② 未命中:走网络。GET 幂等:失败重试至多 RETRIES 次;发往游戏服务器的请求停滞时并发对冲
            //    (见 STALL / HEDGE_GAP)。要读 body 的(可缓存资源、root SWF)分块读取,每拿到响应头或一块
            //    数据就记一次"进展";其余的拿到响应头就算成功,原样交回引擎(XML/文本的字符集信息在原
            //    响应对象上,不能换成我们自己的包装)。
            enum Fetched {
                Response(Box<dyn SuccessResponse>),
                Body {
                    final_url: String,
                    bytes: Vec<u8>,
                    encoding: Option<&'static Encoding>,
                    expected: Option<u64>,
                },
            }
            let want_body = cache_path.is_some() || is_root;
            let started = std::time::Instant::now();
            let last_progress = Rc::new(std::cell::Cell::new(started));
            let make_attempt = || {
                // 每次尝试重建一个 GET 请求(原请求已被消费)
                let mut req = Request::get(url.clone());
                req.set_headers(headers.clone());
                // 借用只在同步的 fetch() 调用期间持有,拿到 future 后立刻释放,绝不跨 await
                let fut = inner.borrow().fetch(req);
                let url = url.clone();
                let progress = last_progress.clone();
                let attempt_started = std::time::Instant::now();
                Box::pin(async move {
                    let result = fut.await;
                    // 首字节(响应头)时间,用于调对冲阈值(MOLE_NET_LOG=1 时打印)。
                    if net_log_enabled() {
                        tracing::info!(
                            "[网络] 响应头 {:>5}ms {} {url}",
                            attempt_started.elapsed().as_millis(),
                            if result.is_ok() { "成功" } else { "失败" }
                        );
                    }
                    let mut resp = result?;
                    progress.set(std::time::Instant::now());
                    if !want_body || resp.status() != 200 {
                        return Ok(Fetched::Response(resp)); // 非 200 不缓存,原样返回
                    }
                    let final_url = resp.url().to_string();
                    let encoding = resp.text_encoding();
                    let expected = resp.expected_length().ok().flatten();
                    let mut bytes = Vec::with_capacity(
                        resp.expected_length()
                            .ok()
                            .flatten()
                            .unwrap_or(0)
                            .min(64 << 20) as usize,
                    );
                    loop {
                        match resp.next_chunk().await {
                            Ok(Some(chunk)) => {
                                progress.set(std::time::Instant::now());
                                bytes.extend_from_slice(&chunk);
                            }
                            Ok(None) => break,
                            Err(error) => return Err(ErrorResponse { url, error }),
                        }
                    }
                    Ok(Fetched::Body {
                        final_url,
                        bytes,
                        encoding,
                        expected,
                    })
                })
                    as std::pin::Pin<
                        Box<dyn std::future::Future<Output = Result<Fetched, ErrorResponse>>>,
                    >
            };

            use futures::future::{Either, select};
            use futures::stream::{FuturesUnordered, StreamExt};
            let mut in_flight = FuturesUnordered::new();
            in_flight.push(make_attempt());
            let mut last_launch = started;
            let mut hedges = 0usize;
            let mut failures = 0u32;
            let mut hard_stalls = 0u32;
            // 收到过确定性失败(4xx)就不再对冲:同一个资源再发也是 4xx。
            let mut no_more_hedge = false;
            let mut last_err: Option<ErrorResponse> = None;
            // 有别的尝试在途时收到的确定性失败,及收到的时刻:给在途的宽限 STALL,期间没有任何进展就报它。
            let mut deterministic_err: Option<(ErrorResponse, std::time::Instant)> = None;
            let timeout_err = |url: &str| ErrorResponse {
                url: url.to_string(),
                error: Error::FetchError("请求长时间没有任何进展".into()),
            };
            let fetched = loop {
                // 下一次需要醒来的时刻:硬停滞上限、确定性失败的宽限期、重试用尽后的停滞判定、
                // 下一次对冲(距最近一次发起满 HEDGE_GAP[hedges] 且已停滞 STALL)取最早。
                let progress = last_progress.get();
                let mut wake = progress + HARD_STALL;
                if let Some((_, at)) = &deterministic_err {
                    wake = wake.min(*at + STALL);
                }
                if failures > RETRIES {
                    wake = wake.min(progress + STALL);
                }
                if hedge
                    && !no_more_hedge
                    && let Some(gap) = HEDGE_GAP.get(hedges)
                {
                    wake = wake.min((last_launch + *gap).max(progress + STALL));
                }
                match select(in_flight.next(), async_io::Timer::at(wake)).await {
                    Either::Left((Some(Ok(fetched)), _)) => break fetched,
                    Either::Left((Some(Err(err)), _)) => {
                        failures += 1;
                        let deterministic = !is_worth_retry(&err);
                        if !in_flight.is_empty() {
                            // 还有别的尝试在途(可能正在正常传输),先不判死。
                            if deterministic {
                                no_more_hedge = true;
                                if deterministic_err.is_none() {
                                    deterministic_err = Some((err, std::time::Instant::now()));
                                }
                            } else {
                                last_err = Some(err);
                            }
                            continue;
                        }
                        // 4xx 是确定性失败(资源真不存在),重试只是把一次 404 放大成 3 次请求。
                        if deterministic || failures > RETRIES {
                            return Err(err);
                        }
                        tracing::debug!("拉取失败,重试 {failures}/{RETRIES}: {url}");
                        in_flight.push(make_attempt());
                        last_launch = std::time::Instant::now();
                    }
                    Either::Left((None, _)) => {
                        // 不会发生(失败分支保证要么返回、要么留一个在途);以防万一按失败处理。
                        return Err(last_err.take().unwrap_or_else(|| timeout_err(&url)));
                    }
                    Either::Right(_) => {
                        // 用"现在"而不是计时器的到期时刻:主线程卡顿/切后台/预取迟接手后,
                        // 过期的几档不会被当成"刚到点"而一次连发。
                        let now = std::time::Instant::now();
                        let progress = last_progress.get();
                        let stalled = now.saturating_duration_since(progress);
                        // ① 确定性失败的宽限期到了,且之后在途的尝试没有任何进展 → 报这个 4xx。
                        if let Some((_, at)) = &deterministic_err
                            && now >= *at + STALL
                            && progress <= *at
                        {
                            return Err(deterministic_err.take().expect("上面刚检查过").0);
                        }
                        // ② 硬停滞上限:全部放弃;第二次就报错。
                        if stalled >= HARD_STALL {
                            hard_stalls += 1;
                            tracing::warn!(
                                "请求 {:.0} 秒没有任何进展,放弃在途的 {} 个尝试(第 {hard_stalls} 次): {url}",
                                stalled.as_secs_f32(),
                                in_flight.len()
                            );
                            in_flight = FuturesUnordered::new();
                            if hard_stalls >= 2 {
                                return Err(last_err.take().unwrap_or_else(|| timeout_err(&url)));
                            }
                            in_flight.push(make_attempt());
                            last_launch = now;
                            last_progress.set(now);
                            hedges = 0;
                            continue;
                        }
                        // ③ 重试已用尽,在途的又停滞了 → 不再干等。
                        if failures > RETRIES && stalled >= STALL {
                            return Err(last_err.take().unwrap_or_else(|| timeout_err(&url)));
                        }
                        // ④ 对冲。
                        if hedge
                            && !no_more_hedge
                            && let Some(gap) = HEDGE_GAP.get(hedges)
                            && now >= last_launch + *gap
                            && stalled >= STALL
                        {
                            hedges += 1;
                            tracing::info!(
                                "请求停滞 {:.1} 秒(距发起 {:.1} 秒),并发再发一次(第 {} 个副本): {url}",
                                stalled.as_secs_f32(),
                                started.elapsed().as_secs_f32(),
                                hedges
                            );
                            HEDGED_REQUESTS.fetch_add(1, Ordering::Relaxed);
                            in_flight.push(make_attempt());
                            last_launch = now;
                        }
                    }
                }
            };
            drop(in_flight); // 取消其余在途的尝试

            match fetched {
                Fetched::Response(resp) => Ok(resp),
                Fetched::Body {
                    final_url,
                    bytes,
                    encoding,
                    expected,
                } => {
                    // 文本文件:响应头带了字符集,或内容不像完整的正常文件(错误页、截断),就不写缓存。
                    let is_text = abs.as_ref().is_some_and(is_versioned_text);
                    let cache_path = cache_path.filter(|_| {
                        !is_text
                            || (encoding.is_none()
                                && text_looks_complete(&final_url, &bytes, expected))
                    });
                    let Some(path) = cache_path else {
                        // root SWF / 不缓存的文本:body 已读完,原样(连同字符集)交给引擎。
                        let mut resp = CachedResponse::new(final_url, bytes);
                        resp.text_encoding = encoding;
                        return Ok(Box::new(resp) as Box<dyn SuccessResponse>);
                    };
                    // 写盘(存原始压缩数据,不占用户存储)+ 解压都在后台线程。
                    let prepared = workers::offload(move || {
                        write_cache_atomic(&path, &bytes);
                        let n = bytes.len();
                        (n, if is_swf { cws_to_fws(bytes) } else { bytes })
                    })
                    .await;
                    let Some((n, bytes)) = prepared else {
                        return Err(ErrorResponse {
                            url,
                            error: Error::FetchError("后台线程处理资源失败".into()),
                        });
                    };
                    CACHE_WRITE_BYTES.fetch_add(n as u64, Ordering::Relaxed);
                    tracing::debug!("缓存写入: {final_url} ({n} 字节)");
                    Ok(Box::new(CachedResponse::new(final_url, bytes)) as Box<dyn SuccessResponse>)
                }
            }
        })
    }
}

/// 从本地缓存字节合成的 `SuccessResponse`(模拟一次成功的 HTTP 200)。
/// 同时支持 `body`(整取)与 `next_chunk`(流式,一次给完)——两条加载路径都兼容。
struct CachedResponse {
    url: String,
    bytes: Vec<u8>,
    chunk_done: bool,
    /// 原响应头里的字符集(从网络读完正文再包装时原样带上;磁盘缓存命中时为 None)。
    text_encoding: Option<&'static Encoding>,
}

impl CachedResponse {
    fn new(url: String, bytes: Vec<u8>) -> Self {
        Self {
            url,
            bytes,
            chunk_done: false,
            text_encoding: None,
        }
    }
}

impl SuccessResponse for CachedResponse {
    fn url(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.url)
    }

    fn set_url(&mut self, url: String) {
        self.url = url;
    }

    fn body(self: Box<Self>) -> OwnedFuture<Vec<u8>, Error> {
        let bytes = self.bytes;
        Box::pin(async move { Ok(bytes) })
    }

    fn text_encoding(&self) -> Option<&'static Encoding> {
        self.text_encoding
    }

    fn status(&self) -> u16 {
        200
    }

    fn redirected(&self) -> bool {
        false
    }

    fn next_chunk(&mut self) -> OwnedFuture<Option<Vec<u8>>, Error> {
        if self.chunk_done {
            Box::pin(async { Ok(None) })
        } else {
            self.chunk_done = true;
            let bytes = std::mem::take(&mut self.bytes);
            Box::pin(async move { Ok(Some(bytes)) })
        }
    }

    fn expected_length(&self) -> Result<Option<u64>, Error> {
        Ok(Some(self.bytes.len() as u64))
    }
}

/// 资源缓存根目录:各端缓存目录下 `MoleRuffle/http`(iOS=沙盒 `Library/Caches/MoleRuffle/http`,
/// 可被系统按需清理,正合缓存语义)。
pub fn cache_root() -> PathBuf {
    dirs::cache_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MoleRuffle")
        .join("http")
}

/// 当前服务器的资源缓存目录(`<root>/<服务器id>`)。
///
/// 按服分目录而不是共用一个目录:虽然 key 是完整绝对 URL 的 hash、两服天然不撞,但分目录让
/// "只清某个服的缓存"成为可能,也避免统计/容量预算互相干扰。
pub fn cache_dir() -> PathBuf {
    cache_root().join(server::selected().id)
}

/// 缓存容量预算(字节)。超出后由 [`trim_cache_in_background`] 按最近访问时间从老到新删。
/// 默认 1.5GB;`MOLE_CACHE_BUDGET_MB` 可覆盖,设 0 = 不限(旧行为)。
fn cache_budget_bytes() -> u64 {
    const DEFAULT_MB: u64 = 1536;
    std::env::var("MOLE_CACHE_BUDGET_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MB)
        * 1024
        * 1024
}

/// 后台修剪缓存目录:清理 `.tmp` 孤儿 + 超预算时按 mtime 从老到新删,直到降到预算的 80%。
///
/// 为什么需要:老实现**零上限、零 LRU、零 TTL、零清理入口**,只增不减。一局游戏几百个资源 SWF,
/// 长期玩会持续占用玩家磁盘(iOS 上玩家能在「储存空间」里看到这个数字,且自己删不掉)。
/// 另外 `write_cache_atomic` 若在 write 与 rename 之间被 jetsam/断电打断,会留下永不清理的
/// `.tmp` 孤儿,这里一并收掉。
///
/// 跑在独立线程:遍历+stat 是阻塞 IO,绝不能放在事件循环线程上(那会卡帧)。
/// 启动时调一次即可 —— 缓存增长是"每局几百个文件"的量级,不需要实时监控。
pub fn trim_cache_in_background() {
    let dir = cache_dir();
    let budget = cache_budget_bytes();
    std::thread::Builder::new()
        .name("mole-cache-trim".into())
        .spawn(move || {
            let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
            let mut total: u64 = 0;
            let mut orphans = 0usize;
            // 两层结构:<dir>/<hash前2位>/<hash>.swfcache
            let Ok(buckets) = std::fs::read_dir(&dir) else {
                return;
            };
            for bucket in buckets.flatten() {
                let Ok(files) = std::fs::read_dir(bucket.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    let path = f.path();
                    let Ok(md) = f.metadata() else { continue };
                    if !md.is_file() {
                        continue;
                    }
                    // .tmpN 孤儿:上次写盘被中断的残留(名字带进程内序号,见 write_cache_atomic),直接删
                    if path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e.starts_with("tmp"))
                    {
                        let _ = std::fs::remove_file(&path);
                        orphans += 1;
                        continue;
                    }
                    let mtime = md.modified().unwrap_or(std::time::UNIX_EPOCH);
                    total += md.len();
                    entries.push((path, md.len(), mtime));
                }
            }
            if orphans > 0 {
                tracing::info!("缓存清理: 删除 {orphans} 个 .tmp 孤儿");
            }
            if budget == 0 || total <= budget {
                tracing::info!(
                    "缓存 {} 个文件 / {}MB(预算 {}MB,无需修剪)",
                    entries.len(),
                    total / (1024 * 1024),
                    budget / (1024 * 1024)
                );
                return;
            }
            // 超预算:按 mtime 从老到新删到预算的 80%(留出余量,避免每次启动都紧贴阈值狂删)
            let target = budget / 10 * 8;
            entries.sort_by_key(|(_, _, mtime)| *mtime);
            let mut freed = 0u64;
            let mut removed = 0usize;
            for (path, size, _) in &entries {
                if total - freed <= target {
                    break;
                }
                if std::fs::remove_file(path).is_ok() {
                    freed += size;
                    removed += 1;
                }
            }
            tracing::info!(
                "缓存修剪: {}MB → {}MB(删 {removed} 个最旧文件,预算 {}MB)",
                total / (1024 * 1024),
                (total - freed) / (1024 * 1024),
                budget / (1024 * 1024)
            );
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::{cws_to_fws, is_home_loaded_ping};
    use std::io::Write;

    #[test]
    fn home_loaded_ping() {
        let ping = |item: &str| {
            format!(
                "http://newmisc.taomee.com/misc.js?gameid=1&stid=%E5%9F%BA%E6%9C%AC%E6%95%B0%E6%8D%AE&sstid=%E9%A6%96%E9%A1%B5%E7%B4%A0%E6%9D%90%E5%8A%A0%E8%BD%BD&item={item}&itemlen=36"
            )
        };
        assert!(is_home_loaded_ping(&ping("%E5%8A%A0%E8%BD%BD%E6%88%90%E5%8A%9F")));
        assert!(!is_home_loaded_ping(&ping("%E5%BC%80%E5%A7%8B%E5%8A%A0%E8%BD%BD")));
        assert!(!is_home_loaded_ping("http://mole.61.com/Client.swf"));
        assert!(!is_home_loaded_ping(
            "http://x/a.swf?sstid=%E9%A6%96%E9%A1%B5%E7%B4%A0%E6%9D%90%E5%8A%A0%E8%BD%BD&item=%E5%8A%A0%E8%BD%BD%E6%88%90%E5%8A%9F"
        ));
    }

    /// 造一个最小但合法的 SWF 正文:舞台矩形 + 帧率 + 帧数 + End 标签 + 若干填充。
    fn fake_swf_body() -> Vec<u8> {
        let mut body = vec![
            0x78, 0x00, 0x05, 0x5F, 0x00, 0x00, 0x0F, 0xA0, 0x00, // RECT(Nbits=15)
            0x00, 0x18, // 帧率 24.0(fixed8.8)
            0x01, 0x00, // 1 帧
        ];
        body.extend((0..5000u32).map(|i| (i * 31 % 251) as u8)); // 可压缩但不平凡的填充
        body.extend([0x00, 0x00]); // End
        body
    }

    fn make_cws(body: &[u8], version: u8) -> Vec<u8> {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(body).unwrap();
        let comp = z.finish().unwrap();
        let mut out = b"CWS".to_vec();
        out.push(version);
        out.extend(((body.len() + 8) as u32).to_le_bytes());
        out.extend(comp);
        out
    }

    #[test]
    fn 文本内容校验挡住错误页与截断() {
        use super::text_looks_complete as ok;
        assert!(ok(
            "a/Server.xml?i1",
            b"\xEF\xBB\xBF <?xml version=\"1.0\"?><a/>",
            None
        ));
        assert!(ok("a/x.xml", b"<taomee/>", Some(9)));
        assert!(!ok("a/x.xml", b"<taomee/>", Some(100)), "长度不符(截断)");
        assert!(!ok("a/x.xml", b"", None), "空正文");
        assert!(!ok("a/x.xml", b"Service Unavailable", None), "错误页");
        assert!(ok("a/x.txt", b"1452075536", None));
    }

    #[test]
    fn 带版本串的配置文本可缓存而版本闸不可() {
        use super::is_versioned_text;
        let host = crate::server::selected().host;
        let u = |s: &str| url::Url::parse(&format!("http://{host}/{s}")).unwrap();
        assert!(is_versioned_text(&u("config/Server.xml?i6ooed60")));
        assert!(is_versioned_text(&u(
            "resource/xml/logo/loadingWord.xml?i447fa34"
        )));
        assert!(is_versioned_text(&u("resource/xml/ext.xml?i816xnyw")));
        // 版本闸:纯数字随机串,必须每次取最新
        assert!(!is_versioned_text(&u("version/zzz_config.txt?5831196")));
        assert!(!is_versioned_text(&u("config/Server.xml")));
        assert!(!is_versioned_text(&u("config/Server.xml?v=1")));
        assert!(!is_versioned_text(&u("config/Server.xml?I6OOED60")));
        assert!(!is_versioned_text(&u("a/b.php?i6ooed60")));
    }

    #[test]
    fn 解压结果与引擎自己解出来的逐字节一致() {
        let cws = make_cws(&fake_swf_body(), 10);
        let fws = cws_to_fws(cws.clone());
        assert_eq!(&fws[0..3], b"FWS");
        let a = ruffle_core::swf::decompress_swf(&cws[..]).expect("引擎解 CWS");
        let b = ruffle_core::swf::decompress_swf(&fws[..]).expect("引擎解 FWS");
        assert_eq!(a.data, b.data, "正文必须完全一致");
        assert_eq!(a.header.version(), b.header.version());
        assert_eq!(a.header.uncompressed_len(), b.header.uncompressed_len());
        assert_eq!(a.header.num_frames(), b.header.num_frames());
    }

    #[test]
    fn 非压缩或非swf原样返回() {
        let fws = [b"FWS".as_slice(), &[10, 20, 0, 0, 0], &fake_swf_body()].concat();
        assert_eq!(cws_to_fws(fws.clone()), fws);
        let jpg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
        assert_eq!(cws_to_fws(jpg.clone()), jpg);
        assert_eq!(cws_to_fws(Vec::new()), Vec::<u8>::new());
    }

    #[test]
    fn 损坏的压缩流原样交回引擎() {
        let mut cws = make_cws(&fake_swf_body(), 10);
        let n = cws.len();
        cws[n / 2] ^= 0xFF; // 中间翻坏一个字节
        cws.truncate(n - 10);
        assert_eq!(cws_to_fws(cws.clone()), cws);
    }

    #[test]
    fn 本机缓存里的真实摩尔资源也一致() {
        // 有缓存就验,没有就跳过(CI 上没有)。
        let dir = super::cache_root().join("official");
        let Ok(buckets) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut checked = 0;
        for b in buckets.flatten() {
            let Ok(files) = std::fs::read_dir(b.path()) else {
                continue;
            };
            for f in files.flatten() {
                let Ok(raw) = std::fs::read(f.path()) else {
                    continue;
                };
                if raw.len() < 8 || &raw[0..3] != b"CWS" {
                    continue;
                }
                let a = ruffle_core::swf::decompress_swf(&raw[..]).expect("引擎解真实 CWS");
                let fws = cws_to_fws(raw);
                let b = ruffle_core::swf::decompress_swf(&fws[..]).expect("引擎解转换后的 FWS");
                assert_eq!(a.data, b.data, "{:?}", f.path());
                checked += 1;
            }
        }
        eprintln!("真实 SWF 校验 {checked} 个");
    }
}

/// 对冲/重试逻辑的时序单测:用假的下层导航器按剧本返回(真实计时,单个用例 3~10 秒)。
#[cfg(test)]
mod hedge_tests {
    use super::*;
    use std::time::Instant;

    /// 一次尝试的剧本。
    #[derive(Clone)]
    enum Script {
        /// 等 `header` 后回 200,正文分 `chunks` 块、每块间隔 `gap`。
        Ok {
            header: Duration,
            chunks: usize,
            gap: Duration,
        },
        /// 等 `delay` 后失败;`status` 为 0 表示网络错误,否则是 HTTP 状态码。
        Fail { delay: Duration, status: u16 },
        /// 永远不回。
        Hang,
    }

    struct MockNav {
        /// 第 i 次 fetch 用第 i 个剧本(超出则重复最后一个)。
        scripts: Vec<Script>,
        calls: Rc<RefCell<Vec<Instant>>>,
        /// spawn_future 交来的任务放到这个本地执行器上(没有就丢弃)。
        spawner: Option<futures::executor::LocalSpawner>,
    }

    impl NavigatorBackend for MockNav {
        fn navigate_to_url(
            &self,
            _url: &str,
            _target: &str,
            _vars_method: Option<(NavigationMethod, IndexMap<String, String>)>,
        ) {
        }

        fn fetch(&self, request: Request) -> OwnedFuture<Box<dyn SuccessResponse>, ErrorResponse> {
            let mut calls = self.calls.borrow_mut();
            let script = self.scripts[calls.len().min(self.scripts.len() - 1)].clone();
            calls.push(Instant::now());
            let url = request.url().to_string();
            Box::pin(async move {
                match script {
                    Script::Ok {
                        header,
                        chunks,
                        gap,
                    } => {
                        async_io::Timer::after(header).await;
                        Ok(Box::new(MockResp {
                            url,
                            chunks,
                            gap,
                            sent: 0,
                        }) as Box<dyn SuccessResponse>)
                    }
                    Script::Fail { delay, status } => {
                        async_io::Timer::after(delay).await;
                        let error = if status == 0 {
                            Error::FetchError("连接断开".into())
                        } else {
                            Error::HttpNotOk(url.clone(), status, false, 0)
                        };
                        Err(ErrorResponse { url, error })
                    }
                    Script::Hang => futures::future::pending().await,
                }
            })
        }

        fn resolve_url(&self, url: &str) -> Result<Url, ParseError> {
            Url::parse(url)
        }

        fn spawn_future(&mut self, future: OwnedFuture<(), Error>) {
            use futures::task::LocalSpawnExt;
            if let Some(spawner) = &self.spawner {
                let _ = spawner.spawn_local(async move {
                    let _ = future.await;
                });
            }
        }

        fn pre_process_url(&self, url: Url) -> Url {
            url
        }

        fn connect_socket(
            &mut self,
            _host: String,
            _port: u16,
            _timeout: Duration,
            _handle: SocketHandle,
            _receiver: Receiver<Vec<u8>>,
            _sender: Sender<SocketAction>,
        ) {
        }
    }

    struct MockResp {
        url: String,
        chunks: usize,
        gap: Duration,
        sent: usize,
    }

    impl SuccessResponse for MockResp {
        fn url(&self) -> Cow<'_, str> {
            Cow::Borrowed(&self.url)
        }
        fn set_url(&mut self, url: String) {
            self.url = url;
        }
        fn body(self: Box<Self>) -> OwnedFuture<Vec<u8>, Error> {
            let n = self.chunks * 1024;
            Box::pin(async move { Ok(vec![b'x'; n]) })
        }
        fn text_encoding(&self) -> Option<&'static Encoding> {
            None
        }
        fn status(&self) -> u16 {
            200
        }
        fn redirected(&self) -> bool {
            false
        }
        fn next_chunk(&mut self) -> OwnedFuture<Option<Vec<u8>>, Error> {
            if self.sent >= self.chunks {
                return Box::pin(async { Ok(None) });
            }
            self.sent += 1;
            let gap = self.gap;
            Box::pin(async move {
                async_io::Timer::after(gap).await;
                Ok(Some(vec![b'x'; 1024]))
            })
        }
        fn expected_length(&self) -> Result<Option<u64>, Error> {
            Ok(Some((self.chunks * 1024) as u64))
        }
    }

    /// 跑一次 fetch,返回 (是否成功, 正文长度, 下层被调用的相对时刻(秒), 总耗时(秒))。
    fn run(scripts: Vec<Script>, path: &str) -> (bool, usize, Vec<f32>, f32) {
        let dir = std::env::temp_dir().join(format!(
            "mole-hedge-test-{}-{}",
            std::process::id(),
            path.replace('/', "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let calls = Rc::new(RefCell::new(Vec::new()));
        let nav = CachingNavigator::new(
            MockNav {
                scripts,
                calls: calls.clone(),
                spawner: None,
            },
            dir.clone(),
        );
        let url = format!("http://{}/{path}", server::selected().host);
        let start = Instant::now();
        let result = futures::executor::block_on(async {
            match nav.fetch(Request::get(url)).await {
                Ok(resp) => resp.body().await.ok().map(|b| b.len()),
                Err(_) => None,
            }
        });
        let elapsed = start.elapsed().as_secs_f32();
        let at = calls
            .borrow()
            .iter()
            .map(|t| t.duration_since(start).as_secs_f32())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        (result.is_some(), result.unwrap_or(0), at, elapsed)
    }

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn 慢但持续传输的大文件不对冲() {
        // 响应头 0.3 秒到,之后每 0.4 秒一块、共 15 块(总计约 6.3 秒),从未停滞 2.5 秒。
        let (ok, len, calls, _) = run(
            vec![Script::Ok {
                header: MS(300),
                chunks: 15,
                gap: MS(400),
            }],
            "slow/big.swf",
        );
        assert!(ok);
        assert_eq!(len, 15 * 1024);
        assert_eq!(calls.len(), 1, "不该对冲:{calls:?}");
    }

    #[test]
    fn 卡住不回时约二点五秒对冲且副本成功() {
        let (ok, _, calls, elapsed) = run(
            vec![
                Script::Hang,
                Script::Ok {
                    header: MS(100),
                    chunks: 2,
                    gap: MS(10),
                },
            ],
            "stall/a.swf",
        );
        assert!(ok);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!((2.4..3.2).contains(&calls[1]), "对冲时刻 {calls:?}");
        assert!(elapsed < 3.5, "总耗时 {elapsed}");
    }

    #[test]
    fn 传输中途卡住也会对冲() {
        // 第一个:头很快到,发两块后卡死(第三块间隔 60 秒);副本正常。
        let (ok, _, calls, elapsed) = run(
            vec![
                Script::Ok {
                    header: MS(100),
                    chunks: 3,
                    gap: Duration::from_secs(60),
                },
                Script::Ok {
                    header: MS(100),
                    chunks: 1,
                    gap: MS(10),
                },
            ],
            "stall/b.swf",
        );
        assert!(ok);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(elapsed < 4.0, "总耗时 {elapsed}");
    }

    #[test]
    fn 副本回四百零四不会杀掉仍在传输的原请求() {
        // 原请求卡住 3 秒没头(触发对冲),然后正常传完;副本立刻回 404。
        let (ok, len, calls, _) = run(
            vec![
                Script::Ok {
                    header: MS(3000),
                    chunks: 2,
                    gap: MS(10),
                },
                Script::Fail {
                    delay: MS(50),
                    status: 404,
                },
            ],
            "replica404/a.swf",
        );
        assert!(ok, "原请求应当成功:{calls:?}");
        assert_eq!(len, 2 * 1024);
        assert_eq!(calls.len(), 2, "收到 4xx 后不应再对冲:{calls:?}");
    }

    #[test]
    fn 快速失败后立即重试且对冲计时从重试起算() {
        // 第一次 0.1 秒就断;重试的那次卡住;它应在重试后约 2.5 秒被对冲(而不是从首发起算的 6 秒档)。
        let (ok, _, calls, _) = run(
            vec![
                Script::Fail {
                    delay: MS(100),
                    status: 0,
                },
                Script::Hang,
                Script::Ok {
                    header: MS(50),
                    chunks: 1,
                    gap: MS(10),
                },
            ],
            "retry/a.swf",
        );
        assert!(ok);
        assert_eq!(calls.len(), 3, "{calls:?}");
        let since_retry = calls[2] - calls[1];
        assert!((2.4..3.2).contains(&since_retry), "{calls:?}");
    }

    #[test]
    fn 四百零四只请求一次() {
        let (ok, _, calls, _) = run(
            vec![Script::Fail {
                delay: MS(50),
                status: 404,
            }],
            "missing/a.swf",
        );
        assert!(!ok);
        assert_eq!(calls.len(), 1, "{calls:?}");
    }

    #[test]
    fn 预取的请求被同一网址的引擎请求接手且只发一次() {
        let dir = std::env::temp_dir().join(format!("mole-prefetch-test-{}", std::process::id()));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut pool = futures::executor::LocalPool::new();
        let nav = CachingNavigator::new(
            MockNav {
                scripts: vec![Script::Ok {
                    header: MS(300),
                    chunks: 2,
                    gap: MS(10),
                }],
                calls: calls.clone(),
                spawner: Some(pool.spawner()),
            },
            dir.clone(),
        );
        let url = format!("http://{}/Client.swf", server::selected().host);
        nav.prefetch(&url);
        assert_eq!(calls.borrow().len(), 1, "预取应立即发出请求");
        // 别的网址不会接手预取
        let other = format!("http://{}/other.xml", server::selected().host);
        let _ = pool.run_until(nav.fetch(Request::get(other)));
        assert_eq!(nav.prefetched.borrow().len(), 1, "别的网址不应消费预取");
        // 模拟"渲染器初始化"耗时 200ms,之后引擎请求同一网址
        std::thread::sleep(MS(200));
        let takeover = Instant::now();
        let len = pool.run_until(async {
            nav.fetch(Request::get(url))
                .await
                .ok()?
                .body()
                .await
                .ok()
                .map(|b| b.len())
        });
        assert_eq!(len, Some(2 * 1024));
        // 1 次预取 + 1 次 other.xml,主 SWF 没有被再发一次
        assert_eq!(calls.borrow().len(), 2, "{:?}", calls.borrow());
        // 预取的响应头(0.3 秒)早在前面的等待期间就到了:接手后只剩读正文,很快完成。
        assert!(takeover.elapsed() < MS(150), "{:?}", takeover.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 版本清单按路径预取且只被同路径接手() {
        let dir = std::env::temp_dir().join(format!("mole-prefetch-q-{}", std::process::id()));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut pool = futures::executor::LocalPool::new();
        let nav = CachingNavigator::new(
            MockNav {
                scripts: vec![Script::Ok {
                    header: MS(100),
                    chunks: 1,
                    gap: MS(5),
                }],
                calls: calls.clone(),
                spawner: Some(pool.spawner()),
            },
            dir.clone(),
        );
        let host = server::selected().host;
        nav.prefetch_ignoring_query(&format!("http://{host}/version/zzz_config.txt"));
        // 同路径、带随机串的引擎请求接手预取
        let ok = pool
            .run_until(nav.fetch(Request::get(format!(
                "http://{host}/version/zzz_config.txt?7898149"
            ))))
            .is_ok();
        assert!(ok);
        assert_eq!(calls.borrow().len(), 1, "应接手预取而不是重新请求");
        assert!(nav.prefetched.borrow().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 带超时地跑一次 fetch:超时返回 None(用来抓"永远不返回")。
    fn run_with_timeout(
        scripts: Vec<Script>,
        path: &str,
        limit: Duration,
    ) -> (Option<bool>, Vec<f32>) {
        let dir = std::env::temp_dir().join(format!(
            "mole-hang-test-{}-{}",
            std::process::id(),
            path.replace('/', "_")
        ));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let nav = CachingNavigator::new(
            MockNav {
                scripts,
                calls: calls.clone(),
                spawner: None,
            },
            dir.clone(),
        );
        let url = format!("http://{}/{path}", server::selected().host);
        let start = Instant::now();
        let result = futures::executor::block_on(async {
            use futures::future::{Either, select};
            let fetch = Box::pin(async { nav.fetch(Request::get(url)).await.is_ok() });
            match select(fetch, async_io::Timer::after(limit)).await {
                Either::Left((ok, _)) => Some(ok),
                Either::Right(_) => None,
            }
        });
        let at = calls
            .borrow()
            .iter()
            .map(|t| t.duration_since(start).as_secs_f32())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        (result, at)
    }

    #[test]
    fn 原请求死连接且副本都失败时会报错而不是永远等() {
        let (done, calls) = run_with_timeout(
            vec![
                Script::Hang,
                Script::Fail {
                    delay: MS(50),
                    status: 0,
                },
            ],
            "deadconn/Client.swf",
            Duration::from_secs(25),
        );
        assert_eq!(done, Some(false), "应当在合理时间内报错:{calls:?}");
    }

    #[test]
    fn 副本回四百零四而原请求卡死时报四百零四() {
        let (done, calls) = run_with_timeout(
            vec![
                Script::Hang,
                Script::Fail {
                    delay: MS(50),
                    status: 404,
                },
            ],
            "deadconn404/a.swf",
            Duration::from_secs(15),
        );
        assert_eq!(done, Some(false), "应当报 404:{calls:?}");
    }

    #[test]
    fn 全部尝试都卡死时有硬上限() {
        let (done, calls) =
            run_with_timeout(vec![Script::Hang], "allhang/a.swf", Duration::from_secs(45));
        assert_eq!(done, Some(false), "应当在硬上限后报错:{calls:?}");
    }

    #[test]
    fn 接手迟到时只补发一个副本() {
        let dir = std::env::temp_dir().join(format!("mole-burst-{}", std::process::id()));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut pool = futures::executor::LocalPool::new();
        let nav = CachingNavigator::new(
            MockNav {
                scripts: vec![
                    Script::Hang,
                    Script::Ok {
                        header: MS(300),
                        chunks: 1,
                        gap: MS(5),
                    },
                ],
                calls: calls.clone(),
                spawner: Some(pool.spawner()),
            },
            dir.clone(),
        );
        let url = format!("http://{}/Client.swf", server::selected().host);
        nav.prefetch(&url);
        // 主线程忙了 7 秒才接手(对冲时间表里的前两档都已过期)
        std::thread::sleep(Duration::from_secs(7));
        let ok = pool.run_until(async { nav.fetch(Request::get(url)).await.is_ok() });
        assert!(ok);
        assert_eq!(
            calls.borrow().len(),
            2,
            "接手时应只补发一个:{:?}",
            calls.borrow()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 预取在被接手前就会对冲() {
        let dir = std::env::temp_dir().join(format!("mole-prefetch-bg-{}", std::process::id()));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut pool = futures::executor::LocalPool::new();
        let nav = CachingNavigator::new(
            MockNav {
                scripts: vec![
                    Script::Hang,
                    Script::Ok {
                        header: MS(100),
                        chunks: 1,
                        gap: MS(5),
                    },
                ],
                calls: calls.clone(),
                spawner: Some(pool.spawner()),
            },
            dir.clone(),
        );
        let host = server::selected().host;
        nav.prefetch_ignoring_query(&format!("http://{host}/version/zzz_config.txt?1111111"));
        // 事件循环照常运转 3.5 秒(引擎还没来请求它):这期间预取应已自行对冲并拿到结果
        pool.run_until(async_io::Timer::after(MS(3500)));
        assert_eq!(
            calls.borrow().len(),
            2,
            "接手前应已对冲:{:?}",
            calls.borrow()
        );
        let takeover = Instant::now();
        let ok = pool
            .run_until(nav.fetch(Request::get(format!(
                "http://{host}/version/zzz_config.txt?2222222"
            ))))
            .is_ok();
        assert!(ok);
        assert!(takeover.elapsed() < MS(100), "接手时结果应已就绪");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 一直失败在重试用尽后报错() {
        let (ok, _, calls, _) = run(
            vec![Script::Fail {
                delay: MS(50),
                status: 503,
            }],
            "flaky/a.swf",
        );
        assert!(!ok);
        assert_eq!(calls.len() as u32, RETRIES + 1, "{calls:?}");
    }
}
