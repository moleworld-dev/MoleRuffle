# vendor/wgpu-hal(MoleRuffle 本地补丁)

官方 wgpu-hal 27.0.4 源码 + 下面的改动(源码里搜 `MoleRuffle:`):

- `src/metal/device.rs`:着色器编译(`new_library_with_source`)与渲染管线创建
  (`new_render_pipeline_state`)不再在 `shared.device` 互斥锁内进行——锁内只 `clone()` 出
  `metal::Device`(引用计数 +1),编译在锁外。上游在整个编译期间持锁,后台线程预编管线时,
  主线程的 `new_buffer` / `new_texture` / `new_sampler` 都会被挡住(冷缓存时一条管线 20~35ms)。
  MTLDevice 按苹果文档线程安全。

升级 wgpu 时:重新拷贝对应版本的 wgpu-hal 源码,把上述两处搬过去,再跑 `desktop/fastcb-regress.sh`。
