//! 同路径文件修改互斥；覆盖读入、匹配和原子替换，不限制模型的读取方式。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

/// 取得本模块的 mutation 锁。中毒即 fail-stop：不恢复、不静默继续。
#[allow(clippy::expect_used)]
pub(crate) fn acquire_mutation_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("mutation lock poisoned (fail-stop)")
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
    let mut locks = acquire_mutation_lock(LOCKS.get_or_init(Mutex::default));
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    Ok(lock)
}

/// 测试注入点：让指定路径的 mutation 锁中毒，模拟「持有该锁时 panic」。返回的
/// 强引用由调用方持有，锁因此不会被下一次查询当作已释放而重建。
#[cfg(test)]
#[allow(clippy::expect_used)] // 测试夹具构造失败即测试环境损坏，直接 panic 是正确语义
pub(crate) fn poison_lock(path: &Path) -> Arc<Mutex<()>> {
    let lock = mutation_lock(path).expect("mutation lock");
    let poisoning = Arc::clone(&lock);
    std::thread::spawn(move || {
        let _guard = poisoning.lock().expect("mutation lock");
        panic!("poison the mutation lock");
    })
    .join()
    .expect_err("the poisoning thread must panic");
    lock
}

/// 按实际前后内容生成统一的变更展示；edit 与 write 的 diff 反馈共用这一份实现。
pub(super) fn unified_diff(path: &str, before: &str, after: &str) -> String {
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(4)
        .header(path, path)
        .to_string()
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
