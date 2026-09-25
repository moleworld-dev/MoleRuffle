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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 缓存运行时统计(全局):命中数 / 未命中数 / 命中读盘字节 / 写盘字节。
/// 供各端(如桌面 perf 日志)读取,量化缓存实际加速,避免盲调。
pub static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
pub static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
pub static CACHE_HIT_BYTES: AtomicU64 = AtomicU64::new(0);
pub static CACHE_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);

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
}

impl<N> CachingNavigator<N> {
    /// `cache_dir` 用各端缓存目录(iOS=沙盒 Library/Caches,可被系统按需清理,正合缓存语义)。
    pub fn new(inner: N, cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        tracing::info!("资源缓存目录: {}", cache_dir.display());
        Self {
            inner: Rc::new(RefCell::new(inner)),
            cache_dir,
        }
    }

    /// URL → 缓存文件路径(对完整 URL 取稳定 hash,按前两位分桶,避免单目录文件过多)。
    fn cache_path(&self, url: &str) -> PathBuf {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut h);
        let hex = format!("{:016x}", h.finish());
        self.cache_dir.join(&hex[0..2]).join(format!("{hex}.swfcache"))
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
    STATIC_EXT.iter().any(|ext| path.ends_with(ext))
}

/// 确定性失败(4xx):重试毫无意义,只会把一次 404 放大成 3 次请求 + 3 倍延迟。
/// 5xx / 网络抖动 / 超时才值得重试(摩尔服务器抖起来确实靠重试救回)。
fn is_worth_retry(err: &ErrorResponse) -> bool {
    !matches!(err.error, Error::HttpNotOk(_, status, ..) if (400..500).contains(&status))
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

impl<N: NavigatorBackend> NavigatorBackend for CachingNavigator<N> {
    fn fetch(&self, request: Request) -> OwnedFuture<Box<dyn SuccessResponse>, ErrorResponse> {
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
        let resp_url = abs.as_ref().map(|u| u.to_string()).unwrap_or_else(|| url.clone());
        let inner = self.inner.clone();
        let headers = request.headers().clone();
        Box::pin(async move {
            // ① 缓存命中:读盘 +(SWF)解压都在后台线程完成,主线程只拿到可直接解析的字节。
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
                    return Ok(Box::new(CachedResponse::new(resp_url, bytes)) as Box<dyn SuccessResponse>);
                }
                CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
            }

            // ② 未命中:走网络。GET 幂等,失败/超时重试 RETRIES 次;成功且可缓存(200)则存盘。
            let mut attempt = 0u32;
            loop {
                // 每次尝试重建一个 GET 请求(原请求已被上一次 fetch 消费)
                let mut req = Request::get(url.clone());
                req.set_headers(headers.clone());
                // 借用只在同步的 fetch() 调用期间持有,拿到 future 后立刻释放,绝不跨 await
                let fut = inner.borrow().fetch(req);
                match fut.await {
                    Ok(resp) => {
                        let Some(path) = &cache_path else {
                            return Ok(resp); // 可重试但不缓存(如 account.61.com 的 GET)
                        };
                        if resp.status() != 200 {
                            return Ok(resp); // 非 200 不缓存,原样返回
                        }
                        let final_url = resp.url().to_string();
                        match resp.body().await {
                            Ok(bytes) => {
                                // 写盘(存原始压缩数据,不占用户存储)+ 解压都在后台线程。
                                let p = path.clone();
                                let prepared = workers::offload(move || {
                                    write_cache_atomic(&p, &bytes);
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
                                return Ok(Box::new(CachedResponse::new(final_url, bytes))
                                    as Box<dyn SuccessResponse>);
                            }
                            Err(error) => {
                                if attempt < RETRIES {
                                    attempt += 1;
                                    tracing::debug!("读 body 失败,重试 {attempt}/{RETRIES}: {url}");
                                    continue;
                                }
                                return Err(ErrorResponse { url, error });
                            }
                        }
                    }
                    Err(err) => {
                        // 4xx 是确定性失败(资源真不存在),重试只是把一次 404 放大成 3 次请求。
                        if attempt < RETRIES && is_worth_retry(&err) {
                            attempt += 1;
                            tracing::debug!("拉取失败,重试 {attempt}/{RETRIES}: {url}");
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
        })
    }

    // ── 其余方法全部透传给内层(借用仅在调用期间)──
    fn navigate_to_url(
        &self,
        url: &str,
        target: &str,
        vars_method: Option<(NavigationMethod, IndexMap<String, String>)>,
    ) {
        self.inner.borrow().navigate_to_url(url, target, vars_method)
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

/// 从本地缓存字节合成的 `SuccessResponse`(模拟一次成功的 HTTP 200)。
/// 同时支持 `body`(整取)与 `next_chunk`(流式,一次给完)——两条加载路径都兼容。
struct CachedResponse {
    url: String,
    bytes: Vec<u8>,
    chunk_done: bool,
}

impl CachedResponse {
    fn new(url: String, bytes: Vec<u8>) -> Self {
        Self { url, bytes, chunk_done: false }
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
        None
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
            let Ok(buckets) = std::fs::read_dir(&dir) else { return };
            for bucket in buckets.flatten() {
                let Ok(files) = std::fs::read_dir(bucket.path()) else { continue };
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
    use super::cws_to_fws;
    use std::io::Write;

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
        let Ok(buckets) = std::fs::read_dir(&dir) else { return };
        let mut checked = 0;
        for b in buckets.flatten() {
            let Ok(files) = std::fs::read_dir(b.path()) else { continue };
            for f in files.flatten() {
                let Ok(raw) = std::fs::read(f.path()) else { continue };
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
