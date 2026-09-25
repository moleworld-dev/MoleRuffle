//! 后台工作线程池:把主线程上"与 ActionScript 执行无关、又能并行"的活挪走。
//!
//! 背景:Ruffle 的脚本执行(gc-arena 单写者)和显示列表遍历天生只能在一个线程上跑,
//! 这部分没法多核化;能拿到多核收益的,是它外围那些纯数据活 —— 读盘、SWF 解压、写盘。
//! 这些活以前全在主线程(winit 事件循环)上做,切场景时一次几十个资源,直接吃掉帧时间。
//!
//! 线程数 = 核心数 - 1(给主线程留一个),上限 4:这些是 IO + 解压,再多也没收益,
//! 反而在移动端多费电。苹果平台把线程设为「用户发起」优先级(QOS_CLASS_USER_INITIATED),
//! 让系统在玩家等加载时把它们放到性能核上(iPhone Duo 的 A20 Pro 是 2 个性能核 + 4 个能效核)。
//!
//! 为什么不用 tokio 的 spawn_blocking:本 crate 是五端共享库,不想强制壳层进入 tokio 上下文;
//! std 线程 + async-channel(已是依赖)就够,而且能设 QoS。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;

use async_channel::{Receiver, Sender};

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 后台线程数:核心数 - 1,限定在 1~4。
pub fn worker_count() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    cores.saturating_sub(1).clamp(1, 4)
}

fn pool() -> &'static Sender<Job> {
    static POOL: OnceLock<Sender<Job>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = async_channel::unbounded::<Job>();
        let n = worker_count();
        for i in 0..n {
            let rx: Receiver<Job> = rx.clone();
            std::thread::Builder::new()
                .name(format!("mole-worker-{i}"))
                .spawn(move || {
                    set_qos_user_initiated();
                    while let Ok(job) = rx.recv_blocking() {
                        job();
                    }
                })
                .expect("创建后台工作线程失败");
        }
        tracing::info!("后台工作线程 {n} 个(读盘/解压/写盘)");
        tx
    })
}

/// 在后台线程执行 `f`,异步等待结果。
///
/// 结果经 channel 送回;接收方的唤醒会把等待中的任务重新调度回主线程执行器继续跑。
/// `f` 若 panic,工作线程不会死(catch_unwind 兜住),本函数返回 `None`,由调用方走降级路径。
pub async fn offload<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = async_channel::bounded::<Option<T>>(1);
    let job: Job = Box::new(move || {
        let r = catch_unwind(AssertUnwindSafe(f)).ok();
        let _ = tx.send_blocking(r);
    });
    pool().send_blocking(job).ok()?;
    rx.recv().await.ok().flatten()
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn set_qos_user_initiated() {
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    /// `<sys/qos.h>` QOS_CLASS_USER_INITIATED
    const QOS_CLASS_USER_INITIATED: u32 = 0x19;
    // SAFETY:只影响当前线程的调度类别,参数是系统定义的常量。
    unsafe {
        pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0);
    }
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn set_qos_user_initiated() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        // 测试里没有执行器,用最简单的忙等轮询即可(结果来自另一线程)。
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop(_: *const ()) {}
        fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut f = std::pin::pin!(f);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::yield_now();
        }
    }

    #[test]
    fn 在后台线程执行并取回结果() {
        let main = std::thread::current().id();
        let (v, tid) = block_on(offload(|| (21 * 2, std::thread::current().id()))).unwrap();
        assert_eq!(v, 42);
        assert_ne!(tid, main, "应在后台线程执行");
    }

    #[test]
    fn 任务panic不拖死线程池() {
        assert!(block_on(offload(|| -> i32 { panic!("故意的") })).is_none());
        // 之后照常可用
        assert_eq!(block_on(offload(|| 7)), Some(7));
    }

    #[test]
    fn 线程数在1到4之间() {
        let n = worker_count();
        assert!((1..=4).contains(&n));
    }
}
