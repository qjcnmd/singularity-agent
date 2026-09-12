//! 同路径文件修改互斥；覆盖读入、匹配和原子替换，不限制模型的读取方式。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

#[allow(clippy::expect_used)]
pub(crate) fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("tool batch lock poisoned (fail-stop)")
}

/// 本进程的同路径修改互斥。弱引用只保留正在执行或等待的锁，
/// 锁覆盖文件读取、匹配和原子替换；外部进程及 bash 不受此锁约束。
/// 父目录必须已存在；解析其别名后按目录项加锁，不跟随会被原子替换的末级符号链接。
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
    let mut locks = lock_unpoisoned(LOCKS.get_or_init(Mutex::default));
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    Ok(lock)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn new_file_and_parent_traversal_share_the_existing_lock() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        let file = root.path().join("new.txt");
        let lock = mutation_lock(&file).unwrap();
        let _guard = lock.lock().unwrap();
        let alias = mutation_lock(&root.path().join("sub/../new.txt")).unwrap();
        assert!(matches!(
            alias.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        std::fs::write(&file, "created").unwrap();
        let after_creation = mutation_lock(&file).unwrap();
        assert!(matches!(
            after_creation.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
    }
}
