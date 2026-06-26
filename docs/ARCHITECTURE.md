# MoleRuffle 架构

## 设计原则:一个共享 Rust 核心 + 各平台薄壳

```
                ┌─────────────────────────────────────────┐
                │            moleruffle-core               │
                │  摩尔庄园配置 / Player 装配 / spoof 守卫   │
                │  MoleNavigatorInterface(socket 放行)     │
                │  中文字体回退                              │
                └───────────────┬─────────────────────────┘
                                │ 调用
   ┌───────────────┬───────────┼────────────┬───────────────┐
   ▼               ▼           ▼            ▼               ▼
 desktop         desktop     desktop      android          ios
(Windows)       (macOS)     (Linux)     (NDK/JNI)     (wgpu/Metal)
 winit+wgpu     winit+wgpu  winit+wgpu   原生 Activity   UIView+CAMetalLayer
```

所有平台共享同一份 `ruffle_core`(AVM1/AVM2 纯 Rust **AOT 解释器,运行时不生成机器码 → iOS 无需 JIT**),
只替换平台后端:

| 维度 | 共享 | 平台差异 |
|---|---|---|
| AVM / 游戏逻辑 | `ruffle_core` | 无 |
| 渲染 | `ruffle_render_wgpu` | wgpu 自动选:Metal(mac/iOS)/ DX12·Vulkan(Win)/ Vulkan·GLES(Linux/Android) |
| Socket | `ExternalNavigatorBackend`(`tokio::net::TcpStream`,**无平台分叉**) | 五端同;`SocketMode::Allow` |
| HTTP | reqwest + rustls(**自带 TLS 栈,不走系统 NSURLSession/OkHttp**) | 故 iOS ATS / Android cleartext 策略管不到它 |
| 音频 | `ruffle_core` 解码 | cpal(Win/mac/Linux)/ AAudio(Android)/ coreaudio(iOS) |
| 窗口/输入 | `Player::handle_event` | winit(桌面)/ 触摸→鼠标映射(移动) |

## 摩尔庄园的三个命门(均在 core 固化)

1. **域名守卫**:`Client.swf` 检测到自己不在官网会 `navigateToURL("http://mole.61.com")` 弹走。
   → `PlayerBuilder::with_spoofed_url(GAME_SWF_URL)` + `with_page_url(...)`。
2. **裸 TCP 服务器连接**:游戏用 `flash.net.Socket` 连 `123.206.131.236:1865` / `:3200`。
   浏览器/WASM 必须走 WebSocket 代理(websockify);**原生五端直接裸 TCP**。
   → `ExternalNavigatorBackend` + `SocketMode::Allow` + `MoleNavigatorInterface::confirm_socket → true`。
3. **相对资源回源**:`version/`、`resource/`、`config/`、`dll/` 都相对加载。
   → `base_url = http://mole.61.com/`。

## 引擎版本钉死

Ruffle 未发布 crates.io,且 master 会 break(API 不稳定)。
所有 ruffle 依赖钉在已实测能跑通 mole.61.com 的 commit:

```
rev = "304a3c9dcaf42ed6d4b1c8bbd05f10d21f407c2e"
```

升级引擎 = 改一处 rev + 重测全玩法 + 提交新 `Cargo.lock`。

## 已知风险 / 待办

- **移动端性能**:AVM2 无 JIT,中端 ARM 手机帧率/发热未实测 —— Android Phase 2 的定级关口。
- **全玩法兼容**:主世界已通,但 Flex `spark/TLF` 富文本控件、`socketData→AMF` 反序列化是 Ruffle 已知短板,需逐子模块(钓鱼/卡丁车/小屋)点亮。
- **中文字体**:当前依赖系统字体回退;移动端应内置一份中文 TTF 注册为 device font 才稳。
- **存储**:当前用内存 storage(SharedObject 不持久化),后续换磁盘 storage 以保留本地存档。
- **键盘/IME**:聊天输入需补;移动端需软键盘。
