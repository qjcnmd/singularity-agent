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
            let (original, original_revision) = self.existing_text_or_empty(&target)?;
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
                original_revision,
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
                mutation.original_revision.as_ref(),
            ) {
                Ok(published_revision) => published.push(PublishedMutation {
                    prepared: mutation.clone(),
                    published_revision,
                }),
                Err(write_failure) => {
                    let AtomicWriteFailure {
                        error: write_error,
                        published_revision,
                    } = write_failure;
                    if let Some(published_revision) = published_revision {
                        published.push(PublishedMutation {
                            prepared: mutation.clone(),
                            published_revision: *published_revision,
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
        expected_revision: Option<&WorkspaceContentRevision>,
    ) -> Result<WorkspaceContentRevision, AtomicWriteFailure> {
        self.atomic_write_with_hook(path, content, expected_revision, |_| {})
    }

    pub(crate) fn atomic_write_with_hook(
        &self,
        path: &CapabilityRelativePath,
        content: &str,
        expected_revision: Option<&WorkspaceContentRevision>,
        before_rename: impl FnOnce(&OsStr),
    ) -> Result<WorkspaceContentRevision, AtomicWriteFailure> {
        self.atomic_write_with_hooks(path, content, expected_revision, before_rename, || Ok(()))
    }

    pub(crate) fn atomic_write_with_hooks(
        &self,
        path: &CapabilityRelativePath,
        content: &str,
        expected_revision: Option<&WorkspaceContentRevision>,
        before_rename: impl FnOnce(&OsStr),
        after_rename: impl FnOnce() -> Result<(), WorkspaceToolError>,
    ) -> Result<WorkspaceContentRevision, AtomicWriteFailure> {
        let parent = self
            .open_parent_directory(&path.relative, false)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let initial_target = self
            .atomic_target_state(
                parent.dir(),
                &parent.actual_relative,
                &parent.name,
                &path.display,
            )
            .map_err(|error| map_capability_error(error, &path.display))?;
        if initial_target.as_ref().map(|state| &state.revision) != expected_revision {
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
        temporary_identity = match file_object_identity_key(&temporary_file) {
            Ok(identity) => identity,
            Err(error) => {
                drop(temporary_file);
                return Err(cleanup_owned_file(
                    parent.dir(),
                    &temporary_name,
                    &temporary_identity,
                    map_capability_error(error, &path.display),
                )
                .into());
            }
        };
        let temporary_revision =
            match self.file_revision_metadata(&path.display, &temporary_file) {
                Ok(revision) => revision,
                Err(error) => {
                    drop(temporary_file);
                    return Err(cleanup_owned_file(
                        parent.dir(),
                        &temporary_name,
                        &temporary_identity,
                        map_capability_error(error, &path.display),
                    )
                    .into());
                }
            }
            .with_digest(format!("sha256:{:x}", Sha256::digest(content.as_bytes())));
        before_rename(&temporary_name);
        let mut source_file = match open_file_from_parent(parent.dir(), &temporary_name) {
            Ok(file) => file,
            Err(error) => {
                drop(temporary_file);
                return Err(cleanup_owned_file(
                    parent.dir(),
                    &temporary_name,
                    &temporary_identity,
                    map_capability_error(error, &path.display),
                )
                .into());
            }
        };
        let source_revision =
            match self.read_file_bytes_with_revision(&path.display, &mut source_file) {
                Ok((_, revision)) => revision,
                Err(error) => {
                    drop(source_file);
                    drop(temporary_file);
                    return Err(cleanup_owned_file(
                        parent.dir(),
                        &temporary_name,
                        &temporary_identity,
                        map_capability_error(error, &path.display),
                    )
                    .into());
                }
            };
        if source_revision != temporary_revision {
            drop(source_file);
            drop(temporary_file);
            if source_revision.object_identity == temporary_identity {
                return Err(cleanup_owned_file(
                    parent.dir(),
                    &temporary_name,
                    &temporary_identity,
                    WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                )
                .into());
            }
            return Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()).into());
        }
        let current_target = match self.atomic_target_state(
            parent.dir(),
            &parent.actual_relative,
            &parent.name,
            &path.display,
        ) {
            Ok(state) => state,
            Err(error) => {
                drop(source_file);
                drop(temporary_file);
                return Err(cleanup_owned_file(
                    parent.dir(),
                    &temporary_name,
                    &temporary_identity,
                    map_capability_error(error, &path.display),
                )
                .into());
            }
        };
        if current_target.as_ref().map(|state| &state.revision) != expected_revision {
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
            return match self.atomic_target_state(
                parent.dir(),
                &parent.actual_relative,
                &parent.name,
                &path.display,
            ) {
                Ok(Some(state)) if state.revision.object_identity == temporary_identity => {
                    Err(AtomicWriteFailure::published(error, state.revision))
                }
                Ok(_) => Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()).into()),
                Err(state_error) => Err(map_capability_error(state_error, &path.display).into()),
            };
        }
        let published_state = self
            .atomic_target_state(
                parent.dir(),
                &parent.actual_relative,
                &parent.name,
                &path.display,
            )
            .map_err(|error| map_capability_error(error, &path.display))?
            .ok_or_else(|| WorkspaceToolError::ConcurrentMutation(path.display.clone()))?;
        if published_state.revision.object_identity != temporary_identity {
            return Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()).into());
        }
        if published_state.revision.content_digest != temporary_revision.content_digest {
            return Err(AtomicWriteFailure::published(
                WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                published_state.revision,
            ));
        }
        Ok(published_state.revision)
    }

    fn atomic_target_state(
        &self,
        parent: &CapabilityDir,
        parent_relative: &str,
        name: &OsStr,
        revision_relative: &str,
    ) -> Result<Option<AtomicTargetState>, CapabilityAccessError> {
        match parent.symlink_metadata(name) {
            Ok(metadata) => {
                if metadata_is_symlink_or_reparse(&metadata) {
                    return Err(CapabilityAccessError::Unsafe);
                }
                if !metadata.is_file() {
                    return Err(CapabilityAccessError::NotRegularFile);
                }
                let mut file = open_file_from_parent(parent, name)?;
                let actual = self
                    .actual_relative_for_file(&file, &join_relative_path(parent_relative, name))?;
                if is_protected_path(&actual) {
                    return Err(CapabilityAccessError::Protected(actual));
                }
                let permissions = file
                    .metadata()
                    .map_err(CapabilityAccessError::Io)?
                    .permissions();
                let (_, revision) =
                    self.read_file_bytes_with_revision(revision_relative, &mut file)?;
                Ok(Some(AtomicTargetState {
                    revision,
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
            let result = if mutation.prepared.original_revision.is_some() {
                self.atomic_write(
                    &mutation.prepared.path,
                    &mutation.prepared.original,
                    Some(&mutation.published_revision),
                )
                .map(|_| ())
                .map_err(|failure| failure.error)
            } else {
                self.remove_created_file(&mutation.prepared.path, &mutation.published_revision)
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
        expected_revision: &WorkspaceContentRevision,
    ) -> Result<(), WorkspaceToolError> {
        let parent = match self.open_parent_directory(&path.relative, false) {
            Ok(parent) => parent,
            Err(CapabilityAccessError::Missing) => return Ok(()),
            Err(error) => return Err(map_capability_error(error, &path.display)),
        };
        let current_identity = self
            .atomic_target_state(
                parent.dir(),
                &parent.actual_relative,
                &parent.name,
                &path.display,
            )
            .map_err(|error| map_capability_error(error, &path.display))?;
        match current_identity {
            None => Ok(()),
            Some(state) if state.revision == *expected_revision => parent
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
    pub(crate) original_revision: Option<WorkspaceContentRevision>,
}

pub(crate) struct PublishedMutation {
    pub(crate) prepared: PreparedMutation,
    pub(crate) published_revision: WorkspaceContentRevision,
}

#[derive(Debug)]
pub(crate) struct AtomicWriteFailure {
    pub(crate) error: WorkspaceToolError,
    pub(crate) published_revision: Option<Box<WorkspaceContentRevision>>,
}

impl AtomicWriteFailure {
    fn published(error: WorkspaceToolError, published_revision: WorkspaceContentRevision) -> Self {
        Self {
            error,
            published_revision: Some(Box::new(published_revision)),
        }
    }
}

impl From<WorkspaceToolError> for AtomicWriteFailure {
    fn from(error: WorkspaceToolError) -> Self {
        Self {
            error,
            published_revision: None,
        }
    }
}

struct AtomicTargetState {
    revision: WorkspaceContentRevision,
    permissions: CapabilityPermissions,
}

pub(crate) struct CreatedDirectory {
    path: CapabilityRelativePath,
    identity: String,
    _guard: Option<CapabilityDir>,
}
