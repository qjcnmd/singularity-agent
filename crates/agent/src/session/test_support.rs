//! 隔离的 workspace 测试夹具（feature test-support）。
//!
//! 跨 crate 的调用链测试共用这一套夹具：每个夹具拥有独立的临时工作目录，
//! 绝不触碰真实工作区。夹具只提供构造与读写入口，不拥有任何产品行为。
#![allow(clippy::expect_used)] // 夹具构造失败即测试环境损坏，直接 panic 是正确语义

use std::path::Path;

/// 隔离的 workspace 夹具：一个临时工作目录，工具与项目入口测试的 cwd。
pub struct WorkspaceFixture {
    dir: tempfile::TempDir,
}

impl Default for WorkspaceFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceFixture {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("temp workspace"),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// 在 workspace 内写入一个文件（自动建父目录）。
    pub fn write_file(&self, relative: &str, content: &str) {
        let path = self.dir.path().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, content).expect("write file");
    }

    /// 读取 workspace 内文件的当前内容。
    pub fn read_file(&self, relative: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(relative)).expect("read file")
    }
}
