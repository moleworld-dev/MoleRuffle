fn main() {
    // iOS 主线程默认栈仅 1MB。摩尔庄园部分场景(深层嵌套显示列表 / 递归 AS 脚本)递归很深,
    // 会撑爆主线程栈 → 撞栈保护页 EXC_BAD_ACCESS/SIGSEGV(真机崩溃报告:三函数逐层递归、
    // "Could not determine thread index for stack guard region")。winit 的事件循环(及其上跑的
    // Ruffle tick/render)在 iOS 上固定跑主线程,无法换大栈线程,故通过链接器把 LC_MAIN.stacksize
    // 调到 64MB(0x4000000,16KB 页对齐),给深层递归足够空间。仅对 iOS 目标二进制生效。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        println!("cargo:rustc-link-arg=-Wl,-stack_size,0x4000000");
    }
}
