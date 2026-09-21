//! 同一路径上文件修改的互斥；锁覆盖读入、匹配和原子替换，不限制模型的读取方式。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

/// 取得本模块的 mutation 锁。锁中毒就直接 panic 停止，不恢复也不静默继续。
#[allow(clippy::expect_used)]
pub(crate) fn acquire_mutation_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("mutation lock poisoned (fail-stop)")
}

/// 本进程内同一路径的修改互斥。用弱引用，只保留正在执行或等待中的锁；锁覆盖
/// 文件读取、匹配和原子替换；外部进程和 bash 不受这把锁约束。父目录必须已存在；
/// 先解析父目录的别名再按目录项加锁，末级符号链接会被原子替换，因此不跟随它。
pub(crate) fn mutation_lock(path: &Path) -> io::Result<Arc<Mutex<()>>> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file path has no file name"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let key = parent.canonicalize()?.join(name);
    let key = PathBuf::from(key.to_string_lossy().to_lowercase());
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut locks = acquire_mutation_lock(LOCKS.get_or_init(Mutex::default));
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    Ok(lock)
}

/// 按改动前后的实际内容生成统一的变更展示；edit 与 write 的 diff 反馈共用这一份实现。
pub(super) fn unified_diff(path: &str, before: &str, after: &str) -> String {
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(4)
        .header(path, path)
        .to_string()
}
