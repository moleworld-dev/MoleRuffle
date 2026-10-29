//! 私服(摩尔庄园 1 网页版那套服)的 Socket → WebSocket 中转。
//!
//! 私服的登录服、游戏服、小游戏服不对外开 TCP 端口,只在 `wss://moleworld.net/mole2016-*-ws` 后面挂了
//! websockify(WebSocket 二进制帧 ↔ TCP 字节流)。客户端拿到的地址是服务端下发的内网地址
//! (`127.0.0.1:7788` 等),这里按 [`ProxyRule`] 表把它们改接到对应的 WebSocket,语义与网页版
//! `SOCKET_PROXY`(MoleRosetta/deploy/browser/index.html)逐条对齐:
//!
//! - `connect_on_open = true`(登录服):握手完成才告诉游戏"已连接"。登录包从发出起计 5 秒超时,
//!   握手慢时先报已连接会让登录包排在握手后面被丢。
//! - `connect_on_open = false`(游戏服、小游戏服):立刻报"已连接",游戏先发的数据在通道里排队,
//!   握手完成后再发出。游戏服连接 6 秒内没收到 CONNECT 就改连写死的淘米老地址,所以不能等握手。
//!
//! 每条连接一个线程,用阻塞式 tungstenite:读设短超时,超时间隙把游戏要发的数据取出来写出去。
//! 连接数只有两三条,不值得为它接一个异步运行时。

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use async_channel::{Receiver, Sender, TryRecvError};
use ruffle_core::socket::{ConnectionState, SocketAction, SocketHandle};
use tungstenite::Message;

/// 一条中转规则:游戏连 `host:port` 时改连 `url`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyRule {
    pub host: &'static str,
    pub port: u16,
    pub url: &'static str,
    pub connect_on_open: bool,
}

/// 读超时:也是游戏要发的数据最多等多久才发出。
const POLL: Duration = Duration::from_millis(5);

/// 按规则表找中转目标。
pub fn find(rules: &'static [ProxyRule], host: &str, port: u16) -> Option<&'static ProxyRule> {
    rules.iter().find(|r| r.port == port && r.host == host)
}

/// 起一个线程把这条 Socket 接到 WebSocket。
pub fn spawn(
    rule: &'static ProxyRule,
    timeout: Duration,
    handle: SocketHandle,
    receiver: Receiver<Vec<u8>>,
    sender: Sender<SocketAction>,
) {
    let spawned = std::thread::Builder::new()
        .name("mole-ws".into())
        .spawn(move || run(rule, timeout, handle, receiver, sender.clone()));
    if let Err(e) = spawned {
        tracing::warn!("中转线程起不来: {e}");
    }
}

fn run(
    rule: &'static ProxyRule,
    timeout: Duration,
    handle: SocketHandle,
    receiver: Receiver<Vec<u8>>,
    sender: Sender<SocketAction>,
) {
    if !rule.connect_on_open {
        // 先报已连接;之后连不上就按"对方断开"处理(与网页版 Ruffle 一致)。
        if sender
            .send_blocking(SocketAction::Connect(handle, ConnectionState::Connected))
            .is_err()
        {
            return;
        }
    }

    let mut ws = match open(rule.url, timeout) {
        Ok(ws) => ws,
        Err((state, why)) => {
            tracing::warn!("中转 {}:{} → {} 失败: {why}", rule.host, rule.port, rule.url);
            let action = if rule.connect_on_open {
                SocketAction::Connect(handle, state)
            } else {
                SocketAction::Close(handle)
            };
            let _ = sender.send_blocking(action);
            return;
        }
    };
    tracing::info!("中转 {}:{} → {} 已接通", rule.host, rule.port, rule.url);

    if rule.connect_on_open
        && sender
            .send_blocking(SocketAction::Connect(handle, ConnectionState::Connected))
            .is_err()
    {
        let _ = ws.close(None);
        return;
    }

    loop {
        // 游戏要发的数据(每次调用 send 对应一条消息,与 websockify 的二进制帧一一对应)。
        loop {
            match receiver.try_recv() {
                Ok(data) => {
                    if let Err(e) = ws.send(Message::Binary(data.into())) {
                        tracing::warn!("中转写出失败: {e}");
                        let _ = sender.send_blocking(SocketAction::Close(handle));
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => {
                    // 游戏关了这条 Socket。
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return;
                }
            }
        }

        let data = match ws.read() {
            Ok(Message::Binary(b)) => b.to_vec(),
            Ok(Message::Text(t)) => t.as_bytes().to_vec(),
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(e) => {
                if !matches!(e, tungstenite::Error::ConnectionClosed) {
                    tracing::warn!("中转读入失败: {e}");
                }
                break;
            }
        };
        if !data.is_empty()
            && sender
                .send_blocking(SocketAction::Data(handle, data))
                .is_err()
        {
            return;
        }
    }
    let _ = sender.send_blocking(SocketAction::Close(handle));
}

type Ws = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

fn open(url: &str, timeout: Duration) -> Result<Ws, (ConnectionState, String)> {
    let failed = |e: &dyn std::fmt::Display| (ConnectionState::Failed, e.to_string());
    let parsed = url::Url::parse(url).map_err(|e| failed(&e))?;
    let host = parsed.host_str().ok_or_else(|| failed(&"网址没有主机名"))?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| failed(&e))?
        .next()
        .ok_or_else(|| failed(&"解析不到地址"))?;
    let tcp = TcpStream::connect_timeout(&addr, timeout).map_err(|e| {
        let state = if e.kind() == ErrorKind::TimedOut {
            ConnectionState::TimedOut
        } else {
            ConnectionState::Failed
        };
        (state, e.to_string())
    })?;
    let _ = tcp.set_nodelay(true);
    // 握手期间按连接超时阻塞;握手完再改成短超时轮询。
    tcp.set_read_timeout(Some(timeout)).map_err(|e| failed(&e))?;
    tcp.set_write_timeout(Some(timeout)).map_err(|e| failed(&e))?;
    let poll = tcp.try_clone().map_err(|e| failed(&e))?;
    let (ws, _) = tungstenite::client_tls(url, tcp).map_err(|e| failed(&e))?;
    poll.set_read_timeout(Some(POLL)).map_err(|e| failed(&e))?;
    Ok(ws)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &[ProxyRule] = &[
        ProxyRule { host: "127.0.0.1", port: 7788, url: "wss://a/login", connect_on_open: true },
        ProxyRule { host: "127.0.0.1", port: 1863, url: "wss://a/game", connect_on_open: false },
    ];

    #[test]
    fn 按主机和端口精确匹配() {
        assert_eq!(find(RULES, "127.0.0.1", 7788).unwrap().url, "wss://a/login");
        assert_eq!(find(RULES, "127.0.0.1", 1863).unwrap().url, "wss://a/game");
        assert!(find(RULES, "127.0.0.1", 1864).is_none());
        assert!(find(RULES, "127.0.0.2", 7788).is_none());
    }

    /// 联网:真连 2016 联机版的三个中转入口,登录服要等握手完才报已连接。
    /// `cargo test -p moleruffle-core --lib ws_proxy -- --ignored`
    #[test]
    #[ignore]
    fn 联网_三个中转入口都能握手() {
        let rules = crate::server::MOLE2016.socket_proxy;
        for (port, on_open) in [(7788, true), (1863, false), (3001, false)] {
            let rule = find(rules, "127.0.0.1", port).unwrap();
            assert_eq!(rule.connect_on_open, on_open);
            let ws = open(rule.url, Duration::from_secs(10));
            assert!(ws.is_ok(), "{} 握手失败: {:?}", rule.url, ws.err().map(|e| e.1));
        }
    }
}
