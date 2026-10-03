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
        let hedge = on_game_host && request.body().is_none();
        let is_root = on_game_host
            && abs
                .as_ref()
                .is_some_and(|u| u.path().eq_ignore_ascii_case("/client.swf"));
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
                Body { final_url: String, bytes: Vec<u8> },
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
                Box::pin(async move {
                    let mut resp = fut.await?;
                    progress.set(std::time::Instant::now());
                    if !want_body || resp.status() != 200 {
                        return Ok(Fetched::Response(resp)); // 非 200 不缓存,原样返回
                    }
                    let final_url = resp.url().to_string();
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
                    Ok(Fetched::Body { final_url, bytes })
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
            // 收到过确定性失败(4xx)就不再对冲:同一个资源再发也是 4xx。
            let mut no_more_hedge = false;
            let mut last_err: Option<ErrorResponse> = None;
            let fetched = loop {
                // 下一次对冲的时刻:距最近一次发起满 HEDGE_GAP[hedges],且距最近一次进展满 STALL。
                // 不对冲(别的主机)、名额用完、或收到过 4xx 就只等在途的尝试。
                let hedge_timer = match HEDGE_GAP.get(hedges) {
                    Some(gap) if hedge && !no_more_hedge => {
                        let due = (last_launch + *gap).max(last_progress.get() + STALL);
                        Either::Left(async_io::Timer::at(due))
                    }
                    _ => Either::Right(futures::future::pending::<std::time::Instant>()),
                };
                match select(in_flight.next(), hedge_timer).await {
                    Either::Left((Some(Ok(fetched)), _)) => break fetched,
                    Either::Left((Some(Err(err)), _)) => {
                        failures += 1;
                        let deterministic = !is_worth_retry(&err);
                        no_more_hedge |= deterministic;
                        if !in_flight.is_empty() {
                            // 还有别的尝试在途(可能正在正常传输),等它们,不因为某个副本失败就判死。
                            last_err = Some(err);
                            continue;
                        }
                        // 4xx 是确定性失败(资源真不存在),重试只是把一次 404 放大成 3 次请求。
                        if failures > RETRIES || deterministic {
                            return Err(err);
                        }
                        tracing::debug!("拉取失败,重试 {failures}/{RETRIES}: {url}");
                        in_flight.push(make_attempt());
                        last_launch = std::time::Instant::now();
                    }
                    Either::Left((None, _)) => {
                        // 不会发生(失败分支保证要么返回、要么留一个在途);以防万一按失败处理。
                        return Err(last_err.take().unwrap_or_else(|| ErrorResponse {
                            url: url.clone(),
                            error: Error::FetchError("没有在途的请求".into()),
                        }));
                    }
                    Either::Right((now, _)) => {
                        // 计时器到点期间可能刚有进展,再确认一次确实停滞了。
                        if now < last_progress.get() + STALL {
                            continue;
                        }
                        hedges += 1;
                        tracing::info!(
                            "请求停滞 {:.1} 秒(距发起 {:.1} 秒),并发再发一次(第 {} 个副本): {url}",
                            last_progress.get().elapsed().as_secs_f32(),
                            started.elapsed().as_secs_f32(),
                            hedges
                        );
                        HEDGED_REQUESTS.fetch_add(1, Ordering::Relaxed);
                        in_flight.push(make_attempt());
                        last_launch = now;
                    }
                }
            };
            drop(in_flight); // 取消其余在途的尝试

            match fetched {
                Fetched::Response(resp) => Ok(resp),
                Fetched::Body { final_url, bytes } => {
                    let Some(path) = cache_path else {
                        // root SWF:不进磁盘缓存(版本闸),body 已读完,直接交给引擎。
                        return Ok(Box::new(CachedResponse::new(final_url, bytes))
                            as Box<dyn SuccessResponse>);
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

/// 从本地缓存字节合成的 `SuccessResponse`(模拟一次成功的 HTTP 200)。
/// 同时支持 `body`(整取)与 `next_chunk`(流式,一次给完)——两条加载路径都兼容。
struct CachedResponse {
    url: String,
    bytes: Vec<u8>,
    chunk_done: bool,
}

impl CachedResponse {
    fn new(url: String, bytes: Vec<u8>) -> Self {
        Self {
            url,
            bytes,
            chunk_done: false,
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

        fn spawn_future(&mut self, _future: OwnedFuture<(), Error>) {}

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
