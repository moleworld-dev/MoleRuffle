fn main() {
    // iOS 主线程默认栈仅 1MB。摩尔庄园部分场景(深层嵌套显示列表 / 递归 AS 脚本)递归很深,
    // 会撑爆主线程栈 → 撞栈保护页 EXC_BAD_ACCESS/SIGSEGV(真机崩溃报告:三函数逐层递归、
    // "Could not determine thread index for stack guard region")。winit 的事件循环(及其上跑的
    // Ruffle tick/render)在 iOS 上固定跑主线程,无法换大栈线程,故通过链接器把 LC_MAIN.stacksize
    // 调到 64MB(0x4000000,16KB 页对齐),给深层递归足够空间。仅对 iOS 目标二进制生效。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        // ★只对 bin(main executable)加★:-stack_size 是 LC_MAIN 的属性,cdylib(本 crate
        //   crate-type 含 cdylib,给安卓/其它壳用)不是 main executable,给它加会链接失败
        //   (ld: -stack_size option can only be used when linking a main executable)。
        //   iOS 装机注入的正是 release/moleruffle 这个 bin,故 bins-only 完全够用。
        println!("cargo:rustc-link-arg-bins=-Wl,-stack_size,0x4000000");

        // 注:iOS 27 上"装上一点就闪退"的真根因不在链接/dead_strip,而是 iOS 27 强制 UIScene
        // 生命周期、winit 0.30 未适配 —— 凡链接 SDK ≥ 27 的二进制启动即被系统终止。
        // 解法是用正式版 Xcode(SDK 26.x)构建,见 ios/build-device.sh、ios/make-testflight.sh。
        // (曾误判为 -dead_strip 剥掉了 winit 的 delegate 类,那个类在 winit 0.30 里并不存在。)
    }
}
