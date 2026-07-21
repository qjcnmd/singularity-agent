//! 工作区文件读取、目录列举和文本搜索操作。

use super::*;

impl WorkspaceTools {
    /// 在工作区内读取有界文件内容。
    pub fn read(&self, input: ReadToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        self.read_cancellable(input, &CancellationToken::new())
    }

    /// 在工作区内读取有界文件内容，并在文件系统边界传播取消。
    pub fn read_cancellable(
        &self,
        input: ReadToolInput,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        self.read_with_cancellation_check(input, &|| cancellation.is_cancelled())
    }

    pub(crate) fn read_with_cancellation_check(
        &self,
        input: ReadToolInput,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        input.validate()?;
        check_cancelled(cancellation)?;
        let max_chars = input.max_chars.unwrap_or(DEFAULT_READ_MAX_CHARS);
        let line_start = input.line_start.unwrap_or(1);
        let line_end = input.line_end.unwrap_or(usize::MAX);
        let target = self.resolve_workspace_path(&input.path, false)?;
        check_cancelled(cancellation)?;
        let relative = target.display.clone();
        check_cancelled(cancellation)?;
        let file = self.open_file_at(&target)?;
        check_cancelled(cancellation)?;
        let mut reader = CancellableLineReader::new(file);
        let mut line = Vec::new();
        let mut preview = String::new();
        let mut preview_truncated = false;
        let mut actual_line_start = None;
        let mut actual_line_end = None;
        let mut total_lines = 0usize;
        let mut last_line_partial = false;

        loop {
            check_cancelled(cancellation)?;
            line.clear();
            let bytes_read = reader.read_until(b'\n', &mut line, cancellation)?;
            check_cancelled(cancellation)?;
            if bytes_read == 0 {
                break;
            }
            total_lines = total_lines.saturating_add(1);
            if is_binary(&line) {
                check_cancelled(cancellation)?;
                return Ok(ToolOutput::success(json!({
                    "path": relative,
                    "binary": true,
                    "preview": BINARY_CONTENT_PREVIEW,
                    "truncated": true,
                    "line_start": Value::Null,
                    "line_end": Value::Null,
                    "total_lines": total_lines,
                })));
            }
            let text = std::str::from_utf8(&line)
                .map(str::to_string)
                .map_err(|error| {
                    WorkspaceToolError::ReadFailed(format!(
                        "invalid utf-8 after binary check: {error}"
                    ))
                })?;
            if total_lines < line_start || total_lines > line_end {
                continue;
            }
            actual_line_start.get_or_insert(total_lines);
            let remaining = max_chars.saturating_sub(preview.chars().count());
            if remaining == 0 {
                preview_truncated = true;
                continue;
            }
            let (bounded, truncated) = bounded_text(&text, remaining);
            preview.push_str(&bounded);
            actual_line_end = Some(total_lines);
            if truncated {
                preview_truncated = true;
                last_line_partial = true;
            }
        }

        check_cancelled(cancellation)?;
        let next_line_start = actual_line_end.and_then(|line_end| {
            if last_line_partial {
                None
            } else if line_end < total_lines {
                line_end.checked_add(1)
            } else {
                None
            }
        });
        let mut output = json!({
            "path": relative,
            "binary": false,
            "preview": preview,
            "truncated": preview_truncated,
            "line_start": actual_line_start,
            "line_end": actual_line_end,
            "total_lines": total_lines,
            "partial_line": last_line_partial,
        });
        if let Some(next_line_start) = next_line_start {
            output["next_line_start"] = json!(next_line_start);
        }
        check_cancelled(cancellation)?;
        Ok(ToolOutput::success(output))
    }

    /// 列出工作区内的有界目录内容。
    pub fn list(&self, input: ListToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        self.list_cancellable(input, &CancellationToken::new())
    }

    /// 列出工作区内的有界目录内容，并在目录递归边界传播取消。
    pub fn list_cancellable(
        &self,
        input: ListToolInput,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        self.list_with_cancellation_check(input, &|| cancellation.is_cancelled())
    }

    pub(crate) fn list_with_cancellation_check(
        &self,
        input: ListToolInput,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        input.validate()?;
        check_cancelled(cancellation)?;
        let target = self.resolve_optional_workspace_path(input.path.as_deref(), false)?;
        check_cancelled(cancellation)?;
        let max_entries = input.max_entries.unwrap_or(DEFAULT_LIST_MAX_ENTRIES);
        let max_depth = input.max_depth.unwrap_or(DEFAULT_LIST_MAX_DEPTH);
        let mut state = ListState {
            entries: Vec::new(),
            redacted_entries: 0,
            truncated: false,
            collection_limit: max_entries.saturating_add(1),
            recursive: input.recursive,
            max_depth,
        };
        self.with_directory_at(&target, |directory| {
            self.collect_list_entries(directory, &target.display, 0, &mut state, cancellation)
        })?;
        check_cancelled(cancellation)?;
        state
            .entries
            .sort_by(|left, right| left.relative.cmp(&right.relative));
        let truncated_by_count = state.entries.len() > max_entries;
        state.entries.truncate(max_entries);
        check_cancelled(cancellation)?;
        Ok(ToolOutput::success(json!({
            "entries": state
                .entries
                .into_iter()
                .map(|entry| json!({
                    "path": entry.relative,
                    "kind": if entry.is_dir { "directory" } else { "file" },
                }))
                .collect::<Vec<_>>(),
            "redacted_entries": state.redacted_entries,
            "truncated": state.truncated || truncated_by_count,
        })))
    }

    /// 在工作区内执行有界文本搜索。
    pub fn grep(&self, input: GrepToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        self.grep_cancellable(input, &CancellationToken::new())
    }

    /// 在工作区内执行有界文本搜索，并在递归和文件读取边界传播取消。
    pub fn grep_cancellable(
        &self,
        input: GrepToolInput,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        self.grep_with_cancellation_check(input, &|| cancellation.is_cancelled())
    }

    pub(crate) fn grep_with_cancellation_check(
        &self,
        input: GrepToolInput,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        input.validate()?;
        check_cancelled(cancellation)?;
        let root =
            self.resolve_optional_workspace_path(input.path.as_deref(), input.path.is_none())?;
        check_cancelled(cancellation)?;
        let max_matches = input.max_matches.unwrap_or(DEFAULT_GREP_MAX_MATCHES);
        let mut matches = Vec::new();
        let collection_limit = max_matches.saturating_add(1);
        let metadata = self.metadata_at(&root)?;
        let truncated = if metadata.is_dir() {
            self.with_directory_at(&root, |directory| {
                self.grep_directory(
                    directory,
                    &root.display,
                    &input.pattern,
                    input.case_sensitive,
                    collection_limit,
                    &mut matches,
                    cancellation,
                )
            })?
        } else {
            let file = self.open_file_at(&root)?;
            self.grep_file(
                file,
                &root.display,
                &input.pattern,
                input.case_sensitive,
                collection_limit,
                &mut matches,
                cancellation,
            )?
        };
        check_cancelled(cancellation)?;
        matches.truncate(max_matches);
        check_cancelled(cancellation)?;
        Ok(ToolOutput::success(json!({
            "matches": matches,
            "truncated": truncated,
        })))
    }
    fn collect_list_entries(
        &self,
        directory: &CapabilityDir,
        prefix: &str,
        depth: usize,
        state: &mut ListState,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<(), WorkspaceToolError> {
        check_cancelled(cancellation)?;
        if state.entries.len() >= state.collection_limit {
            state.truncated = true;
            return Ok(());
        }
        let entries = self.sorted_directory_entries(directory, prefix, cancellation)?;
        for entry in entries {
            check_cancelled(cancellation)?;
            if is_protected_path(&entry.relative) {
                state.redacted_entries = state.redacted_entries.saturating_add(1);
                continue;
            }
            if entry.is_symlink_or_reparse {
                continue;
            }
            state.entries.push(entry.clone());
            if state.entries.len() >= state.collection_limit {
                state.truncated = true;
                check_cancelled(cancellation)?;
                return Ok(());
            }
            if state.recursive && entry.is_dir {
                match open_directory_component(directory, &entry.name, false) {
                    Ok(child) if depth < state.max_depth => {
                        self.collect_list_entries(
                            &child,
                            &entry.relative,
                            depth + 1,
                            state,
                            cancellation,
                        )?;
                    }
                    Ok(child) => {
                        self.mark_depth_boundary(&child, &entry.relative, state, cancellation)?;
                    }
                    Err(CapabilityAccessError::Unsafe | CapabilityAccessError::Missing) => {}
                    Err(error) => {
                        return Err(map_capability_error(error, &entry.relative));
                    }
                }
            }
        }
        check_cancelled(cancellation)?;
        Ok(())
    }

    fn mark_depth_boundary(
        &self,
        directory: &CapabilityDir,
        prefix: &str,
        state: &mut ListState,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<(), WorkspaceToolError> {
        check_cancelled(cancellation)?;
        let entries = self.sorted_directory_entries(directory, prefix, cancellation)?;
        for entry in entries {
            check_cancelled(cancellation)?;
            if is_protected_path(&entry.relative) {
                state.redacted_entries = state.redacted_entries.saturating_add(1);
            } else if !entry.is_symlink_or_reparse {
                state.truncated = true;
            }
        }
        check_cancelled(cancellation)?;
        Ok(())
    }

    fn sorted_directory_entries(
        &self,
        directory: &CapabilityDir,
        prefix: &str,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<Vec<DirectoryEntry>, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        let mut entries = Vec::new();
        let mut directory_entries = directory.entries().map_err(io_error)?;
        check_cancelled(cancellation)?;
        loop {
            check_cancelled(cancellation)?;
            let Some(entry) = directory_entries.next() else {
                break;
            };
            let entry = entry.map_err(io_error)?;
            let name = entry.file_name();
            let file_type = entry.file_type().map_err(io_error)?;
            #[cfg(windows)]
            let is_symlink_or_reparse = file_type.is_symlink()
                || metadata_is_symlink_or_reparse(&entry.full_metadata().map_err(io_error)?);
            #[cfg(not(windows))]
            let is_symlink_or_reparse = file_type.is_symlink();
            let relative = join_relative_path(prefix, &name);
            entries.push(DirectoryEntry {
                name,
                relative,
                is_dir: file_type.is_dir() && !is_symlink_or_reparse,
                is_symlink_or_reparse,
            });
        }
        check_cancelled(cancellation)?;
        entries.sort_by(|left, right| left.relative.cmp(&right.relative));
        check_cancelled(cancellation)?;
        Ok(entries)
    }

    #[allow(clippy::too_many_arguments)]
    fn grep_directory(
        &self,
        directory: &CapabilityDir,
        prefix: &str,
        pattern: &str,
        case_sensitive: bool,
        collection_limit: usize,
        matches: &mut Vec<Value>,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<bool, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        if matches.len() >= collection_limit {
            return Ok(true);
        }
        let entries = self.sorted_directory_entries(directory, prefix, cancellation)?;
        for entry in entries {
            check_cancelled(cancellation)?;
            if is_protected_path(&entry.relative) || entry.is_symlink_or_reparse {
                continue;
            }
            if entry.is_dir {
                let child = match open_directory_component(directory, &entry.name, false) {
                    Ok(child) => child,
                    Err(CapabilityAccessError::Unsafe | CapabilityAccessError::Missing) => {
                        continue;
                    }
                    Err(error) => {
                        return Err(map_capability_error(error, &entry.relative));
                    }
                };
                if self.grep_directory(
                    &child,
                    &entry.relative,
                    pattern,
                    case_sensitive,
                    collection_limit,
                    matches,
                    cancellation,
                )? {
                    return Ok(true);
                }
            } else {
                let file = match open_file_from_parent(directory, &entry.name) {
                    Ok(file) => file,
                    Err(CapabilityAccessError::Unsafe | CapabilityAccessError::Missing) => {
                        continue;
                    }
                    Err(CapabilityAccessError::NotRegularFile) => continue,
                    Err(error) => {
                        return Err(map_capability_error(error, &entry.relative));
                    }
                };
                if self.grep_file(
                    file,
                    &entry.relative,
                    pattern,
                    case_sensitive,
                    collection_limit,
                    matches,
                    cancellation,
                )? {
                    return Ok(true);
                }
            }
        }
        check_cancelled(cancellation)?;
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn grep_file(
        &self,
        file: CapabilityFile,
        relative: &str,
        pattern: &str,
        case_sensitive: bool,
        collection_limit: usize,
        matches: &mut Vec<Value>,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<bool, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        let mut reader = CancellableLineReader::new(file);
        let mut raw_line = Vec::new();
        let mut file_matches = Vec::new();
        let folded_pattern = (!case_sensitive).then(|| pattern.to_lowercase());
        let mut line_number = 0usize;
        loop {
            check_cancelled(cancellation)?;
            raw_line.clear();
            let bytes_read = reader.read_until(b'\n', &mut raw_line, cancellation)?;
            check_cancelled(cancellation)?;
            if bytes_read == 0 {
                break;
            }
            if is_binary(&raw_line) {
                check_cancelled(cancellation)?;
                return Ok(false);
            }
            let line = std::str::from_utf8(&raw_line)
                .map_err(|_error| WorkspaceToolError::BinaryPattern)?;
            line_number = line_number.saturating_add(1);
            let matches_pattern = folded_pattern.as_ref().map_or_else(
                || line.contains(pattern),
                |folded| line.to_lowercase().contains(folded),
            );
            if matches_pattern {
                check_cancelled(cancellation)?;
                let line = line.trim_end_matches(['\n', '\r']);
                let (preview, _) = bounded_text(line, DEFAULT_RESULT_PREVIEW_MAX_CHARS);
                file_matches.push(json!({
                    "path": relative,
                    "line": line_number,
                    "preview": preview,
                }));
                if matches.len().saturating_add(file_matches.len()) >= collection_limit {
                    matches.extend(
                        file_matches
                            .into_iter()
                            .take(collection_limit.saturating_sub(matches.len())),
                    );
                    check_cancelled(cancellation)?;
                    return Ok(true);
                }
            }
        }
        check_cancelled(cancellation)?;
        matches.extend(file_matches);
        Ok(false)
    }
}
struct CancellableLineReader<R> {
    reader: R,
    chunk: [u8; FILE_READ_CHUNK_SIZE],
    chunk_start: usize,
    chunk_end: usize,
}

impl<R: Read> CancellableLineReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            chunk: [0; FILE_READ_CHUNK_SIZE],
            chunk_start: 0,
            chunk_end: 0,
        }
    }

    fn read_until(
        &mut self,
        delimiter: u8,
        output: &mut Vec<u8>,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<usize, WorkspaceToolError> {
        output.clear();
        loop {
            check_cancelled(cancellation)?;
            if self.chunk_start == self.chunk_end {
                let bytes_read = self.reader.read(&mut self.chunk).map_err(io_error)?;
                check_cancelled(cancellation)?;
                self.chunk_start = 0;
                self.chunk_end = bytes_read;
                if bytes_read == 0 {
                    return Ok(output.len());
                }
            }

            let available = &self.chunk[self.chunk_start..self.chunk_end];
            if let Some(delimiter_index) = available.iter().position(|byte| *byte == delimiter) {
                let end = delimiter_index.saturating_add(1);
                output.extend_from_slice(&available[..end]);
                self.chunk_start = self.chunk_start.saturating_add(end);
                check_cancelled(cancellation)?;
                return Ok(output.len());
            }
            output.extend_from_slice(available);
            self.chunk_start = self.chunk_end;
            check_cancelled(cancellation)?;
        }
    }
}

#[derive(Debug, Clone)]
struct DirectoryEntry {
    name: OsString,
    pub(crate) relative: String,
    is_dir: bool,
    is_symlink_or_reparse: bool,
}

struct ListState {
    entries: Vec<DirectoryEntry>,
    redacted_entries: usize,
    truncated: bool,
    collection_limit: usize,
    recursive: bool,
    max_depth: usize,
}
