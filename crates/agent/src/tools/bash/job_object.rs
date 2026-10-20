//! 与 MCP 共用 process-wrap 管理 Windows 进程树。

use process_wrap::tokio::{ChildWrapper, CommandWrap, CreationFlags, JobObject, KillOnDrop};
use std::{io, path::Path, process::Stdio, time::Duration};
use tokio::process::Command;

pub(crate) struct ManagedChild {
    pub(super) child: Box<dyn ChildWrapper>,
}

impl ManagedChild {
    /// 主进程退出也要回收后代；包装层的 wait 等待整个作业退出。
    pub(super) async fn reclaim(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        if let Err(error) = self.child.start_kill() {
            failures.push(format!("failed to terminate the command process tree: {error}"));
        }
        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => failures.push(format!("failed to wait for the command process tree: {error}")),
            Err(_) => failures.push("the command process tree did not exit within 5000 ms".into()),
        }
        failures
    }
}

pub(crate) fn spawn_in_job(shell: &str, script: &str, cwd: &Path) -> io::Result<ManagedChild> {
    let mut command = Command::new(shell);
    command
        .args(["-c", script])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut wrapped = CommandWrap::from(command);
    wrapped
        .wrap(CreationFlags(windows::Win32::System::Threading::CREATE_NO_WINDOW))
        .wrap(JobObject)
        .wrap(KillOnDrop);
    Ok(ManagedChild { child: wrapped.spawn()? })
}
