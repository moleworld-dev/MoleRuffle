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

/// 失败重试次数(总尝试 = RETRIES + 1)。只对幂等 GET 生效。抖动网络下瞬时超时重试一次往往就成。
const RETRIES: u32 = 2;

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

/// 哪些请求可缓存:GET + 无 body + 静态资源主机 + 无 query/非动态脚本。
/// 登录/会话(account.61.com、带 ? 的动态、.php 等)一律不缓存,避免拿到陈旧的动态响应。
fn is_cacheable(req: &Request) -> bool {
    if req.method() != NavigationMethod::Get || req.body().is_some() {
        return false;
    }
    let url = req.url();
    url.contains("mole.61.com")
        && !url.contains('?')
        && !url.contains(".php")
        && !url.contains(".jsp")
}

fn write_cache_atomic(path: &PathBuf, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 写临时文件再 rename,保证不会出现半截的损坏缓存
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

impl<N: NavigatorBackend> NavigatorBackend for CachingNavigator<N> {
    fn fetch(&self, request: Request) -> OwnedFuture<Box<dyn SuccessResponse>, ErrorResponse> {
        // 非 GET(POST 等)不幂等:既不缓存也不重试,原样透传(重试 POST 会重复提交)。
        if request.method() != NavigationMethod::Get {
            return self.inner.borrow().fetch(request);
        }

        let url = request.url().to_string();
        let cacheable = is_cacheable(&request);
        let cache_path = cacheable.then(|| self.cache_path(&url));

        // 缓存命中:直接读盘返回(免网络,秒开)
        if let Some(path) = &cache_path {
            if let Ok(bytes) = std::fs::read(path) {
                tracing::debug!("缓存命中: {url}");
                let u = url.clone();
                return Box::pin(async move {
                    Ok(Box::new(CachedResponse::new(u, bytes)) as Box<dyn SuccessResponse>)
                });
            }
        }

        // 未命中:走网络。GET 幂等,失败/超时重试 RETRIES 次;成功且可缓存(200)则存盘。
        let inner = self.inner.clone();
        let headers = request.headers().clone();
        Box::pin(async move {
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
                                write_cache_atomic(path, &bytes);
                                tracing::debug!("缓存写入: {url} ({} 字节)", bytes.len());
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
                        if attempt < RETRIES {
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

/// 资源缓存目录:各端缓存目录下 MoleRuffle/http(iOS=沙盒 Library/Caches/MoleRuffle/http)。
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MoleRuffle")
        .join("http")
}
