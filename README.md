# MoleRuffle

专为 **摩尔庄园网页版**(`mole.61.com`)量身定制的跨平台原生客户端,基于 [Ruffle](https://github.com/ruffle-rs/ruffle) 引擎,用 Rust 编写。

目标:**Windows / macOS / Linux / Android / iOS** 五端,开箱即在线进入游戏,高性能、原生体验。

> 这是一个面向小规模分发的个人/同好项目。客户端只是“引擎 + 装配”;摩尔庄园的美术资源与服务器属于上海淘米,本仓库不分发任何游戏资源,运行时由客户端直接从官方地址加载。

## 为什么可行(已实测)

把 Ruffle 桌面引擎钉在 commit `304a3c9d`,直接喂 `http://mole.61.com/Client.swf`:

- ✅ 成功进入游戏世界(渲染主城、坐骑、其他在线玩家)
- ✅ **原生 TCP** 连真实服务器 `123.206.131.236:1865` / `:3200`,收发二进制游戏协议(登录 Session、进入地图、走路、防沉迷计时)
- ✅ 91 个资源 SWF 在线加载成功,仅 1 个非致命 AVM2 错误,9.5 分钟零崩

关键三点(全部固化进 `moleruffle-core`):

| 命门 | 解法 |
|---|---|
| `Client.swf` 脱离官网会 `navigateToURL` 弹回首页 | `with_spoofed_url` 伪装 SWF 自身 URL,破域名守卫 |
| 游戏靠裸 TCP 连服务器(浏览器/WASM 做不到) | 原生 `ExternalNavigatorBackend` + `SocketMode::Allow` |
| 相对资源路径要回源 | `base_url = http://mole.61.com/` |

> iOS/Android 同理用**原生**栈(非 WASM),裸 TCP 可直接用;详见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。

## 结构

```
MoleRuffle/
├── crates/moleruffle-core/   # 五端共享:摩尔庄园配置 + Player 装配 + spoof + socket 放行 + 中文字体
├── desktop/                  # Win/macOS/Linux 壳(winit + wgpu)  →  binary: moleruffle
├── android/                  # Android 壳(规划中,fork ruffle-android)
├── ios/                      # iOS 壳(规划中,参照 madsmtm/ruffle-ios + wgpu/Metal)
└── docs/ARCHITECTURE.md
```

## 构建 & 运行(桌面)

```bash
cargo run --release -p moleruffle-desktop
# 或
cargo build --release -p moleruffle-desktop && ./target/release/moleruffle
```

首次构建会从 GitHub 拉取并编译 Ruffle 引擎(数分钟)。运行后窗口直接加载摩尔庄园登录页,用淘米账号登录即可进入游戏。

## 路线图

- [x] **Phase 0** — 验证 Ruffle 能在线跑通摩尔庄园(macOS 桌面,已实测)
- [ ] **Phase 1** — macOS 薄壳(本仓库当前阶段)
- [ ] **Phase 2** — Android 在线可玩(真机帧率/触摸实测)
- [ ] **Phase 3** — Windows + Linux 平移
- [ ] **Phase 4** — iOS 自用打通(wgpu/Metal + 自签/内部分发)
- [ ] **Phase 5** — 全玩法点亮(钓鱼/卡丁车/小屋…)+ 中文字体内置 + 回归加固

## License

引擎与本壳代码遵循 `MIT OR Apache-2.0`(同 Ruffle)。游戏内容版权归上海淘米所有,本仓库不含任何游戏资源。
