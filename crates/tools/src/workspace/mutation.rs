//! 工作区 edit/patch 变更操作。

use super::*;

impl WorkspaceTools {
    /// 以单文件 patch 语义执行受保护的替换。
    pub fn edit(
        &self,
        input: EditToolInput,
        decision: &ToolBrokerDecision,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        input.validate()?;
        self.patch(
            WorkspacePatch {
                changes: vec![WorkspacePatchChange {
                    path: input.path,
                    expected: Some(input.expected),
                    replacement: input.replacement,
                }],
            },
            decision,
        )
    }

    /// 先整批预检再原子写入多个文件变更。
    pub fn patch(
        &self,
        patch: WorkspacePatch,
        decision: &ToolBrokerDecision,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        if !decision.is_allowed() {
            return Err(WorkspaceToolError::InvalidInput(
                WORKSPACE_MUTATION_NOT_APPROVED.to_string(),
            ));
        }
        patch.validate()?;
        let mut prepared = Vec::new();
        let mut targets = BTreeSet::new();
        for change in &patch.changes {
            let target = self.resolve_workspace_path(&change.path, false)?;
            let relative = target.display.clone();
            if !targets.insert(self.duplicate_target_key(&target)?) {
                return Err(WorkspaceToolError::InvalidInput(format!(
                    "{DUPLICATE_PATCH_TARGET}: {relative}"
                )));
            }
            let (original, original_identity) = self.existing_text_or_empty(&target)?;
            let updated = if let Some(expected) = &change.expected {
                if !original.contains(expected) {
                    return Err(WorkspaceToolError::ExpectedContentMissing(relative));
                }
                original.replacen(expected, &change.replacement, 1)
            } else {
                change.replacement.clone()
            };
            if updated == original {
                return Err(WorkspaceToolError::InvalidInput(format!(
                    "workspace mutation made no change: {relative}"
                )));
            }
            prepared.push(PreparedMutation {
                path: target,
                relative,
                original,
                updated,
                original_identity,
            });
        }
        let mut created_directories = Vec::new();
        for mutation in &prepared {
            if let Err(error) =
                self.ensure_parent_directories(&mutation.path, &mut created_directories)
            {
                return match self.remove_created_directories(&mut created_directories) {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(WorkspaceToolError::RollbackFailed(format!(
                        "directory preparation error: {error}; cleanup error: {cleanup_error}"
                    ))),
                };
            }
        }
        let mut published = Vec::new();
        for mutation in &prepared {
            match self.atomic_write(
                &mutation.path,
                &mutation.updated,
                mutation.original_identity.as_deref(),
            ) {
                Ok(published_identity) => published.push(PublishedMutation {
                    prepared: mutation.clone(),
                    published_identity,
                }),
                Err(write_failure) => {
                    let AtomicWriteFailure {
                        error: write_error,
                        published_identity,
                    } = write_failure;
                    if let Some(published_identity) = published_identity {
                        published.push(PublishedMutation {
                            prepared: mutation.clone(),
                            published_identity,
                        });
                    }
                    let file_rollback = self.rollback_published(&published);
                    let directory_rollback =
                        self.remove_created_directories(&mut created_directories);
                    if let Err(rollback_error) = file_rollback.and(directory_rollback) {
                        return Err(WorkspaceToolError::RollbackFailed(format!(
                            "write error: {write_error}; rollback error: {rollback_error}"
                        )));
                    }
                    return Err(write_error);
                }
            }
        }
        let changed_files = prepared
            .iter()
            .map(|mutation| mutation.relative.clone())
            .collect::<Vec<_>>();
        let revision = self.advance_workspace_revision()?;
        let mut output = ToolOutput::success(json!({
                "changed_files": changed_files,
            "rolled_back": false,
        }));
        Self::attach_workspace_observation(&mut output, &WorkspaceObservation::changed(revision))?;
        Ok(output)
    }
    pub(crate) fn ensure_parent_directories(
        &self,
        path: &CapabilityRelativePath,
        created: &mut Vec<CreatedDirectory>,
    ) -> Result<(), WorkspaceToolError> {
        let mut components = path.relative.components().collect::<Vec<_>>();
        components
            .pop()
            .ok_or_else(|| WorkspaceToolError::OutsideWorkspace(path.display.clone()))?;
        let root = self.workspace_capability.as_ref();
        let mut current = None;
        let mut requested_relative = String::new();
        for component in components {
            let name = normal_component(component)
                .map_err(|error| map_capability_error(error, &path.display))?;
            let parent = current
                .as_ref()
                .map_or(root, |directory: &CapabilityDir| directory);
            let was_created = match parent.symlink_metadata(name) {
                Ok(metadata) => {
                    if metadata_is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
                        return Err(WorkspaceToolError::OutsideWorkspace(path.display.clone()));
                    }
                    false
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match parent.create_dir(name) {
                        Ok(()) => true,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
                        Err(error) => return Err(io_error(error)),
                    }
                }
                Err(error) => return Err(io_error(error)),
            };
            let directory = open_directory_component(parent, name, false)
                .map_err(|error| map_capability_error(error, &path.display))?;
            requested_relative = join_relative_path(&requested_relative, name);
            if was_created {
                let relative = PathBuf::from(&requested_relative);
                let identity = directory_object_identity_key(&directory)
                    .map_err(|error| map_capability_error(error, &requested_relative))?;
                let guard = directory.try_clone().map_err(io_error)?;
                created.push(CreatedDirectory {
                    path: CapabilityRelativePath {
                        relative: relative.clone(),
                        display: requested_relative.clone(),
                        key: relative_path_key(&relative),
                    },
                    identity,
                    _guard: Some(guard),
                });
            }
            let actual_relative = self
                .actual_relative_for_directory(&directory, &requested_relative)
                .map_err(|error| map_capability_error(error, &path.display))?;
            if is_protected_path(&actual_relative) {
                return Err(WorkspaceToolError::ProtectedPath(actual_relative));
            }
            current = Some(directory);
        }
        Ok(())
    }

    pub(crate) fn atomic_write(
        &self,
        path: &CapabilityRelativePath,
        content: &str,
        expected_identity: Option<&str>,
    ) -> Result<String, AtomicWriteFailure> {
        self.atomic_write_with_hook(path, content, expected_identity, |_| {})
    }

    pub(crate) fn atomic_write_with_hook(
        &self,
        path: &CapabilityRelativePath,
        content: &str,
        expected_identity: Option<&str>,
        before_rename: impl FnOnce(&OsStr),
    ) -> Result<String, AtomicWriteFailure> {
        self.atomic_write_with_hooks(path, content, expected_identity, before_rename, || Ok(()))
    }

    pub(crate) fn atomic_write_with_hooks(
        &self,
        path: &CapabilityRelativePath,
        content: &str,
        expected_identity: Option<&str>,
        before_rename: impl FnOnce(&OsStr),
        after_rename: impl FnOnce() -> Result<(), WorkspaceToolError>,
    ) -> Result<String, AtomicWriteFailure> {
        let parent = self
            .open_parent_directory(&path.relative, false)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let initial_target = self
            .atomic_target_state(parent.dir(), &parent.actual_relative, &parent.name)
            .map_err(|error| map_capability_error(error, &path.display))?;
        if initial_target.as_ref().map(|state| state.identity.as_str()) != expected_identity {
            return Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()).into());
        }
        let original_permissions = initial_target.map(|state| state.permissions);
        let (temporary_name, mut temporary_file) = create_unique_temp_file(parent.dir())
            .map_err(|error| map_capability_error(error, &path.display))?;
        let mut temporary_identity = file_object_identity_key(&temporary_file)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let write_result = temporary_file.write_all(content.as_bytes()).and_then(|()| {
            if let Some(permissions) = original_permissions {
                temporary_file.set_permissions(permissions)?;
            }
            temporary_file.sync_all()
        });
        if let Err(error) = write_result {
            drop(temporary_file);
            return Err(cleanup_owned_file(
                parent.dir(),
                &temporary_name,
                &temporary_identity,
                io_error(error),
            )
            .into());
        }
        temporary_identity = file_object_identity_key(&temporary_file)
            .map_err(|error| map_capability_error(error, &path.display))?;
        before_rename(&temporary_name);
        let source_identity = open_file_from_parent(parent.dir(), &temporary_name)
            .and_then(|file| file_object_identity_key(&file))
            .map_err(|error| map_capability_error(error, &path.display))?;
        if source_identity != temporary_identity {
            drop(temporary_file);
            return Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()).into());
        }
        let current_identity = self
            .atomic_target_state(parent.dir(), &parent.actual_relative, &parent.name)
            .map_err(|error| map_capability_error(error, &path.display))?;
        if current_identity
            .as_ref()
            .map(|state| state.identity.as_str())
            != expected_identity
        {
            drop(temporary_file);
            return Err(cleanup_owned_file(
                parent.dir(),
                &temporary_name,
                &temporary_identity,
                WorkspaceToolError::ConcurrentMutation(path.display.clone()),
            )
            .into());
        }
        if let Err(error) = parent
            .dir()
            .rename(&temporary_name, parent.dir(), &parent.name)
        {
            drop(temporary_file);
            return Err(cleanup_owned_file(
                parent.dir(),
                &temporary_name,
                &temporary_identity,
                io_error(error),
            )
            .into());
        }
        drop(temporary_file);
        if let Err(error) = after_rename() {
            let published_identity = self
                .atomic_target_state(parent.dir(), &parent.actual_relative, &parent.name)
                .ok()
                .flatten()
                .map(|state| state.identity)
                .unwrap_or_else(|| temporary_identity.clone());
            return Err(AtomicWriteFailure::published(error, published_identity));
        }
        let published_state = self
            .atomic_target_state(parent.dir(), &parent.actual_relative, &parent.name)
            .map_err(|error| {
                AtomicWriteFailure::published(
                    map_capability_error(error, &path.display),
                    temporary_identity.clone(),
                )
            })?
            .ok_or_else(|| {
                AtomicWriteFailure::published(
                    WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                    temporary_identity.clone(),
                )
            })?;
        if published_state.object_identity != temporary_identity {
            return Err(AtomicWriteFailure::published(
                WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                temporary_identity,
            ));
        }
        Ok(published_state.identity)
    }

    fn atomic_target_state(
        &self,
        parent: &CapabilityDir,
        parent_relative: &str,
        name: &OsStr,
    ) -> Result<Option<AtomicTargetState>, CapabilityAccessError> {
        match parent.symlink_metadata(name) {
            Ok(metadata) => {
                if metadata_is_symlink_or_reparse(&metadata) {
                    return Err(CapabilityAccessError::Unsafe);
                }
                if !metadata.is_file() {
                    return Err(CapabilityAccessError::NotRegularFile);
                }
                let file = open_file_from_parent(parent, name)?;
                let actual = self
                    .actual_relative_for_file(&file, &join_relative_path(parent_relative, name))?;
                if is_protected_path(&actual) {
                    return Err(CapabilityAccessError::Protected(actual));
                }
                let identity = file_target_identity_key(&file)?;
                let object_identity = file_object_identity_key(&file)?;
                let permissions = file
                    .metadata()
                    .map_err(CapabilityAccessError::Io)?
                    .permissions();
                Ok(Some(AtomicTargetState {
                    identity,
                    object_identity,
                    permissions,
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(classify_io_error(error)),
        }
    }

    pub(crate) fn rollback_published(
        &self,
        published: &[PublishedMutation],
    ) -> Result<(), WorkspaceToolError> {
        let mut failures = Vec::new();
        for mutation in published.iter().rev() {
            let result = if mutation.prepared.original_identity.is_some() {
                self.atomic_write(
                    &mutation.prepared.path,
                    &mutation.prepared.original,
                    Some(&mutation.published_identity),
                )
                .map(|_| ())
                .map_err(|failure| failure.error)
            } else {
                self.remove_created_file(&mutation.prepared.path, &mutation.published_identity)
            };
            if let Err(error) = result {
                failures.push(format!("{}: {error}", mutation.prepared.relative));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(WorkspaceToolError::RollbackFailed(failures.join("; ")))
        }
    }

    fn remove_created_file(
        &self,
        path: &CapabilityRelativePath,
        expected_identity: &str,
    ) -> Result<(), WorkspaceToolError> {
        let parent = match self.open_parent_directory(&path.relative, false) {
            Ok(parent) => parent,
            Err(CapabilityAccessError::Missing) => return Ok(()),
            Err(error) => return Err(map_capability_error(error, &path.display)),
        };
        let current_identity = self
            .atomic_target_state(parent.dir(), &parent.actual_relative, &parent.name)
            .map_err(|error| map_capability_error(error, &path.display))?;
        match current_identity {
            None => Ok(()),
            Some(state) if state.identity == expected_identity => parent
                .dir()
                .remove_file_or_symlink(&parent.name)
                .map_err(io_error),
            Some(_) => Err(WorkspaceToolError::ConcurrentMutation(path.display.clone())),
        }
    }

    pub(crate) fn remove_created_directories(
        &self,
        created: &mut [CreatedDirectory],
    ) -> Result<(), WorkspaceToolError> {
        let mut failures = Vec::new();
        for directory in created.iter_mut().rev() {
            let result = (|| {
                let parent = match self.open_parent_directory(&directory.path.relative, false) {
                    Ok(parent) => parent,
                    Err(CapabilityAccessError::Missing) => return Ok(()),
                    Err(error) => {
                        return Err(map_capability_error(error, &directory.path.display));
                    }
                };
                let opened = match open_directory_component(parent.dir(), &parent.name, false) {
                    Ok(opened) => opened,
                    Err(CapabilityAccessError::Missing) => return Ok(()),
                    Err(error) => {
                        return Err(map_capability_error(error, &directory.path.display));
                    }
                };
                let identity = directory_object_identity_key(&opened)
                    .map_err(|error| map_capability_error(error, &directory.path.display))?;
                if identity != directory.identity {
                    return Err(WorkspaceToolError::ConcurrentMutation(
                        directory.path.display.clone(),
                    ));
                }
                drop(opened);
                drop(directory._guard.take());
                let reopened = open_directory_component(parent.dir(), &parent.name, false)
                    .map_err(|error| map_capability_error(error, &directory.path.display))?;
                let reopened_identity = directory_object_identity_key(&reopened)
                    .map_err(|error| map_capability_error(error, &directory.path.display))?;
                if reopened_identity != directory.identity {
                    return Err(WorkspaceToolError::ConcurrentMutation(
                        directory.path.display.clone(),
                    ));
                }
                drop(reopened);
                parent.dir().remove_dir(&parent.name).map_err(io_error)
            })();
            if let Err(error) = result {
                failures.push(format!("{}: {error}", directory.path.display));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(WorkspaceToolError::RollbackFailed(failures.join("; ")))
        }
    }
}

#[derive(Clone)]
pub(crate) struct PreparedMutation {
    pub(crate) path: CapabilityRelativePath,
    pub(crate) relative: String,
    pub(crate) original: String,
    pub(crate) updated: String,
    pub(crate) original_identity: Option<String>,
}

pub(crate) struct PublishedMutation {
    pub(crate) prepared: PreparedMutation,
    pub(crate) published_identity: String,
}

#[derive(Debug)]
pub(crate) struct AtomicWriteFailure {
    pub(crate) error: WorkspaceToolError,
    pub(crate) published_identity: Option<String>,
}

impl AtomicWriteFailure {
    fn published(error: WorkspaceToolError, published_identity: String) -> Self {
        Self {
            error,
            published_identity: Some(published_identity),
        }
    }
}

impl From<WorkspaceToolError> for AtomicWriteFailure {
    fn from(error: WorkspaceToolError) -> Self {
        Self {
            error,
            published_identity: None,
        }
    }
}

struct AtomicTargetState {
    identity: String,
    object_identity: String,
    permissions: CapabilityPermissions,
}

pub(crate) struct CreatedDirectory {
    path: CapabilityRelativePath,
    identity: String,
    _guard: Option<CapabilityDir>,
}
