//! 运行时内存观测(主要给 iOS):看 app 实际内存足迹 + 距 jetsam 内存墙还剩多少余量。
//! 桌面端无 jetsam 墙,两个函数都返回 None(调用方据此跳过)。
//!
//! - [`available_mb`]:`os_proc_available_memory()`——本进程被 jetsam 杀掉前还能再用多少字节。
//!   这是 Apple 官方"距内存墙余量"API,**会自动反映 increased-memory-limit entitlement 抬高后的新上限**,
//!   所以也用它来验证 entitlement 是否真生效(没生效 ≈ 默认 ~3.4GB,生效 ≈ ~9GB)。
//! - [`footprint_mb`]:`task_vm_info.phys_footprint`——与 Xcode/Jetsam 完全同口径的内存足迹
//!   (常驻 + 压缩 + IOKit),即 JetsamEvent 里那个被拿来跟上限比较的 rpages 折算值。

#[cfg(target_os = "ios")]
mod imp {
    // 三个符号都由 libSystem 导出,声明即用,无需额外链接指令。
    unsafe extern "C" {
        /// 本进程距 per-app 内存上限还剩多少字节(0 = 不可用/不支持)。
        fn os_proc_available_memory() -> usize;
        /// 当前 mach task 端口。C 里 `mach_task_self()` 是读这个全局的宏。
        static mach_task_self_: u32;
        fn task_info(task: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
    }

    const TASK_VM_INFO: u32 = 22;

    /// 镜像 `<mach/task_info.h>` 的 `task_vm_info`,只取到 `phys_footprint` 为止。
    /// `task_info` 按传入的 count 截断填充,所以短结构体也能安全拿到 phys_footprint(它在我们这段的末尾)。
    #[repr(C)]
    #[derive(Default)]
    struct TaskVmInfo {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
    }

    pub fn available_mb() -> Option<u64> {
        let b = unsafe { os_proc_available_memory() };
        (b != 0).then(|| b as u64 / (1024 * 1024))
    }

    pub fn footprint_mb() -> Option<u64> {
        let mut info = TaskVmInfo::default();
        let mut count = (core::mem::size_of::<TaskVmInfo>() / core::mem::size_of::<i32>()) as u32;
        let kr = unsafe {
            task_info(
                mach_task_self_,
                TASK_VM_INFO,
                &mut info as *mut TaskVmInfo as *mut i32,
                &mut count,
            )
        };
        (kr == 0).then_some(info.phys_footprint / (1024 * 1024))
    }
}

#[cfg(not(target_os = "ios"))]
mod imp {
    pub fn available_mb() -> Option<u64> {
        None
    }
    pub fn footprint_mb() -> Option<u64> {
        None
    }
}

pub use imp::{available_mb, footprint_mb};

/// 设备总物理内存(MB)。sysctl hw.memsize,macOS/iOS 通用、无需权限;其他平台返回 0。
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub fn total_ram_mb() -> u64 {
    use std::os::raw::{c_char, c_int, c_void};
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }
    let mut bytes: u64 = 0;
    let mut len = core::mem::size_of::<u64>();
    let name = b"hw.memsize\0".as_ptr() as *const c_char;
    let rc = unsafe {
        sysctlbyname(
            name,
            &mut bytes as *mut u64 as *mut c_void,
            &mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len != core::mem::size_of::<u64>() {
        return 0;
    }
    bytes / (1024 * 1024)
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
pub fn total_ram_mb() -> u64 {
    0
}
