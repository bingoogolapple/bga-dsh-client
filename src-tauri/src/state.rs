//! 全局状态访问辅助：锁中毒（poisoning）安全封装 + 常用状态读取糖。
//!
//! # 为什么需要这个模块
//!
//! 应用内有 4~6 个后台线程共享 `AppState` 里的各个 `Mutex`：
//! `auto_boot` / `start_heartbeat` / `start_log_tailer` / `startup_probe` /
//! `spawn_exit_watcher` / Sentry 遥测线程等。
//!
//! 任一线程在**持锁期间 panic**，Rust 会把该 `Mutex` 标记为「已中毒（poisoned）」。
//! 此后所有 `mutex.lock().unwrap()` 都会立刻 panic——即使状态本身完全完好。
//! 对一个常驻托盘的桌面壳来说，这意味着：某个后台线程出一次错 → 用户点任何菜单
//! 都直接崩溃。这是不可接受的。
//!
//! 本模块提供统一的 `lock()`：中毒时**恢复内部数据**继续运行（数据本身仍然一致，
//! 因为 Rust 的 `Mutex` 在 panic 时不会留下"半写"状态——panic 只能发生在
//! `PoisonError` 上，而临界区内的赋值要么完成要么没开始）。
//!
//! 另提供 `settings()` 等读取糖，收敛散落各处的
//! `app.state::<AppState>().settings.lock().unwrap().clone()`。

use std::sync::{Mutex, MutexGuard};

use tauri::{AppHandle, Manager};

use crate::settings::Settings;
use crate::AppState;

/// 加锁，永不 panic：锁中毒时恢复内部数据并返回 guard。
///
/// 中毒只说明「曾有线程在持锁时 panic」，不代表数据损坏——Rust 的 `Mutex`
/// 保证 panic 不会让被保护的数据处于半更新状态（要么写入前 panic，要么写完整）。
/// 对常驻桌面的应用，崩溃远比带着一个可能陈旧的状态继续运行更糟。
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 读取当前设置的快照（克隆一份，避免在持锁期间做耗时操作）。
pub fn settings(app: &AppHandle) -> Settings {
    lock(&app.state::<AppState>().settings).clone()
}

/// 在闭包内修改设置；闭包返回 `true` 时落盘。
///
/// 收敛 `dsh_set_active_version` / `dsh_set_registry` / `save_settings` 里重复的
/// 「取锁 → 改字段 → 取 config_path → 保存 → 写回」样板。
/// `config_path` 缺失时返回错误（不 panic）。
pub fn update_settings<F>(app: &AppHandle, f: F) -> Result<(), String>
where
    F: FnOnce(&mut Settings),
{
    let state = app.state::<AppState>();
    // Keep the settings lock across the read-modify-write transaction.  Cloning
    // and releasing it before saving lets two concurrent commands overwrite one
    // another (for example registry and active-version changes).
    let mut current = lock(&state.settings);
    f(&mut current);
    let path = lock(&state.config_path)
        .clone()
        .ok_or_else(|| crate::i18n::tr(crate::i18n::current(app), "set.config_dir_missing", &[]))?;
    current.save(&path)?;
    Ok(())
}

/// 读取配置文件路径（缺失返回 None）。
pub fn config_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    lock(&app.state::<AppState>().config_path).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 中毒的锁仍可取到数据（不 panic）——本模块存在的核心理由。
    #[test]
    fn lock_recovers_from_poisoned_mutex() {
        let m = std::sync::Arc::new(Mutex::new(7u32));

        // 在持锁期间 panic → 该 Mutex 被标记为中毒。
        let m2 = m.clone();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = m2.lock().unwrap();
            panic!("poison it");
        }));
        assert!(res.is_err(), "前置条件：线程应确实 panic 了");

        // 前置校验：标准库的 lock() 现在返回 Err（锁确实中毒了）。
        // 若此断言失败说明 Rust 行为有变，需重新审视本模块的设计前提。
        assert!(
            m.lock().is_err(),
            "前置条件：该锁应已中毒（标准库行为变化时需重新审视本模块）"
        );

        // 关键：我们的 lock() 不 panic，且能读到完好数据。
        assert_eq!(*lock(&m), 7);

        // 中毒恢复后仍可正常写入。
        *lock(&m) = 9;
        assert_eq!(*lock(&m), 9);
    }

    /// 正常（未中毒）锁行为不变。
    #[test]
    fn lock_works_on_healthy_mutex() {
        let m = Mutex::new(String::from("ok"));
        assert_eq!(*lock(&m), "ok");
        *lock(&m) = "changed".into();
        assert_eq!(*lock(&m), "changed");
    }
}
