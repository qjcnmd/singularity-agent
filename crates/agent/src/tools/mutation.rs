//! 同路径文件修改互斥；覆盖读入、匹配和原子替换，不限制模型的读取方式。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

/// 文件修改互斥的路径键；Windows 上统一分隔符与大小写。
pub(crate) fn path_key(cwd: &Path, path: &str) -> String {
    let joined = cwd.join(path);
    let absolute = std::path::absolute(&joined).unwrap_or(joined);
    let text = absolute.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

#[allow(clippy::expect_used)]
pub(crate) fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("tool batch lock poisoned (fail-stop)")
}

/// 本进程的同路径修改互斥。弱引用只保留正在执行或等待的锁，
/// 锁覆盖文件读取、匹配和原子替换；外部进程及 bash 不受此锁约束。
pub(crate) fn mutation_lock(key: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut locks = lock_unpoisoned(LOCKS.get_or_init(Mutex::default));
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key.to_string(), Arc::downgrade(&lock));
    lock
}
