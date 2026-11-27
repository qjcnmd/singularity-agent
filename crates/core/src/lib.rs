pub mod identity;
mod project_instructions;
pub mod skills;
mod tool;
mod user_home;
mod workspace;

pub use project_instructions::{InstructionFile, load_agent_instructions};
pub use tool::{ToolDefinition, estimate_tool_tokens};
pub use user_home::resolve_home;
pub use workspace::{canonicalize_workspace, same_saved_path, saved_path_key, workspace_key};

pub(crate) const PROJECT_ROOT_MARKER: &str = ".git";

pub fn estimate_tokens_of(text: &str) -> u64 {
    let quarters: u64 = text.chars().map(|ch| if ch.is_ascii() { 1 } else { 4 }).sum();
    quarters.div_ceil(4)
}

pub fn duration_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

const ISO_FORMAT: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

pub fn now_iso() -> String {
    time::OffsetDateTime::now_utc().format(ISO_FORMAT).expect("utc timestamp always formats")
}

pub fn iso_minus_millis(timestamp: &str, millis: u64) -> Option<String> {
    let parsed = time::PrimitiveDateTime::parse(timestamp, ISO_FORMAT).ok()?;
    let shifted = parsed.checked_sub(time::Duration::milliseconds(i64::try_from(millis).ok()?))?;
    shifted.format(ISO_FORMAT).ok()
}

pub fn display_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("//?/UNC/")) {
        format!("//{}", &text[8..])
    } else if let Some(rest) = text.strip_prefix("//?/") {
        rest.to_owned()
    } else {
        text
    }
}

pub fn write_json_atomic<T: serde::Serialize + ?Sized>(
    path: &std::path::Path,
    value: &T,
) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    write_bytes_atomic(path, &bytes)
}

pub fn write_bytes_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let temporary = parent.join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| -> std::io::Result<()> {
        let mut handle = std::fs::File::create_new(&temporary)?;
        handle.write_all(bytes)?;
        handle.sync_all()?;
        Ok(())
    })()
    .and_then(|()| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
