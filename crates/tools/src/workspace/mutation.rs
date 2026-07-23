//! 工作区 edit/patch 变更操作。

use super::*;

#[cfg(all(test, unix))]
thread_local! {
    static FORCE_TEMPORARY_CLEANUP_IDENTITY_COLLISION: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

impl WorkspaceTools {
    /// Force only the identity field to collide in the next Unix cleanup proof.
    #[cfg(all(test, unix))]
    pub(crate) fn force_next_temporary_cleanup_identity_collision_for_test(&self) {
        FORCE_TEMPORARY_CLEANUP_IDENTITY_COLLISION.with(|force| force.set(true));
    }

    fn cleanup_temporary_file_failure(
        &self,
        parent: &CapabilityDir,
        name: &OsStr,
        _relative: &str,
        temporary_file: CapabilityFile,
        _expected_identity: &str,
        failure: WorkspaceToolError,
    ) -> WorkspaceToolError {
        #[cfg(unix)]
        {
            use std::io::Seek as _;

            let relative = _relative;
            let mut temporary_file = temporary_file;
            if temporary_file.rewind().is_err() {
                return WorkspaceToolError::RollbackFailed(format!(
                    "{relative}: temporary file ownership could not be established; entry preserved"
                ));
            }
            let expected_revision = match self
                .read_file_bytes_with_revision(relative, &mut temporary_file)
            {
                Ok((_, revision)) => revision,
                Err(_) => {
                    return WorkspaceToolError::RollbackFailed(format!(
                        "{relative}: temporary file ownership could not be established; entry preserved"
                    ));
                }
            };
            match self.remove_owned_file_revision_unix(parent, name, relative, &expected_revision) {
                Ok(()) => failure,
                Err(error) => error,
            }
        }
        #[cfg(not(unix))]
        {
            drop(temporary_file);
            cleanup_owned_file(parent, name, _expected_identity, failure)
        }
    }

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
        let diff_digest = workspace_diff_digest(&prepared);
        let revision = self.advance_workspace_revision()?;
        let mut output = ToolOutput::success(json!({
            "changed_files": changed_files,
            "rolled_back": false,
        }));
        output.metadata[WORKSPACE_CHANGE_SUMMARY_METADATA] = json!(WorkspaceChangeSummary::new(
            prepared
                .iter()
                .map(|mutation| mutation.relative.clone())
                .collect(),
            diff_digest,
        ));
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
        let target_was_present = initial_target.is_some();
        #[cfg(unix)]
        let original_revision = initial_target.as_ref().map(|state| state.revision.clone());
        #[cfg(not(unix))]
        let original_identity = initial_target
            .as_ref()
            .map(|state| state.revision.object_identity.clone());
        let original_permissions = initial_target
            .as_ref()
            .map(|state| state.permissions.clone());
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
            return Err(self
                .cleanup_temporary_file_failure(
                    parent.dir(),
                    &temporary_name,
                    &path.display,
                    temporary_file,
                    &temporary_identity,
                    io_error(error),
                )
                .into());
        }
        temporary_identity = match file_object_identity_key(&temporary_file) {
            Ok(identity) => identity,
            Err(error) => {
                return Err(self
                    .cleanup_temporary_file_failure(
                        parent.dir(),
                        &temporary_name,
                        &path.display,
                        temporary_file,
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
                    return Err(self
                        .cleanup_temporary_file_failure(
                            parent.dir(),
                            &temporary_name,
                            &path.display,
                            temporary_file,
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
                return Err(self
                    .cleanup_temporary_file_failure(
                        parent.dir(),
                        &temporary_name,
                        &path.display,
                        temporary_file,
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
                    return Err(self
                        .cleanup_temporary_file_failure(
                            parent.dir(),
                            &temporary_name,
                            &path.display,
                            temporary_file,
                            &temporary_identity,
                            map_capability_error(error, &path.display),
                        )
                        .into());
                }
            };
        if source_revision != temporary_revision {
            drop(source_file);
            if temporary_cleanup_identity_matches(
                &temporary_identity,
                &source_revision.object_identity,
            ) {
                return Err(self
                    .cleanup_temporary_file_failure(
                        parent.dir(),
                        &temporary_name,
                        &path.display,
                        temporary_file,
                        &temporary_identity,
                        WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                    )
                    .into());
            }
            drop(temporary_file);
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
                return Err(self
                    .cleanup_temporary_file_failure(
                        parent.dir(),
                        &temporary_name,
                        &path.display,
                        temporary_file,
                        &temporary_identity,
                        map_capability_error(error, &path.display),
                    )
                    .into());
            }
        };
        if current_target.as_ref().map(|state| &state.revision) != expected_revision {
            drop(source_file);
            return Err(self
                .cleanup_temporary_file_failure(
                    parent.dir(),
                    &temporary_name,
                    &path.display,
                    temporary_file,
                    &temporary_identity,
                    WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                )
                .into());
        }
        #[cfg(unix)]
        let mut original_file = if let Some(original_revision) = original_revision.as_ref() {
            let mut file = match open_file_from_parent(parent.dir(), &parent.name) {
                Ok(file) => file,
                Err(error) => {
                    drop(source_file);
                    return Err(self
                        .cleanup_temporary_file_failure(
                            parent.dir(),
                            &temporary_name,
                            &path.display,
                            temporary_file,
                            &temporary_identity,
                            map_capability_error(error, &path.display),
                        )
                        .into());
                }
            };
            let observed = match self.read_file_bytes_with_revision(&path.display, &mut file) {
                Ok((_, revision)) => revision,
                Err(error) => {
                    drop(source_file);
                    return Err(self
                        .cleanup_temporary_file_failure(
                            parent.dir(),
                            &temporary_name,
                            &path.display,
                            temporary_file,
                            &temporary_identity,
                            map_capability_error(error, &path.display),
                        )
                        .into());
                }
            };
            if observed != *original_revision {
                drop(source_file);
                return Err(self
                    .cleanup_temporary_file_failure(
                        parent.dir(),
                        &temporary_name,
                        &path.display,
                        temporary_file,
                        &temporary_identity,
                        WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                    )
                    .into());
            }
            Some(file)
        } else {
            None
        };
        if let Err(error) = publish_temporary(
            parent.dir(),
            &temporary_name,
            &parent.name,
            target_was_present,
        ) {
            drop(source_file);
            let publish_error = match error {
                CapabilityAccessError::Missing | CapabilityAccessError::ConcurrentMutation => {
                    WorkspaceToolError::ConcurrentMutation(path.display.clone())
                }
                CapabilityAccessError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists => {
                    WorkspaceToolError::ConcurrentMutation(path.display.clone())
                }
                CapabilityAccessError::Unsupported => WorkspaceToolError::PathIdentityUnsupported(
                    "workspace atomic publish primitive is unavailable".to_string(),
                ),
                other => map_capability_error(other, &path.display),
            };
            return Err(self
                .cleanup_temporary_file_failure(
                    parent.dir(),
                    &temporary_name,
                    &path.display,
                    temporary_file,
                    &temporary_identity,
                    publish_error,
                )
                .into());
        }
        #[cfg(unix)]
        let published_revision = {
            use std::io::Seek as _;

            if source_file.rewind().is_err() {
                drop(source_file);
                drop(temporary_file);
                if target_was_present {
                    let restore = self.restore_exchanged_entries(
                        parent.dir(),
                        &parent.name,
                        &temporary_name,
                        &path.display,
                    );
                    return Err(restore
                        .err()
                        .unwrap_or_else(|| {
                            WorkspaceToolError::RollbackFailed(format!(
                                "{}: published entry ownership could not be established; entry preserved",
                                path.display
                            ))
                        })
                        .into());
                }
                return Err(WorkspaceToolError::RollbackFailed(format!(
                    "{}: published entry ownership could not be established",
                    path.display
                ))
                .into());
            }
            match self.read_file_bytes_with_revision(&path.display, &mut source_file) {
                Ok((_, revision)) => revision,
                Err(error) => {
                    drop(source_file);
                    drop(temporary_file);
                    if target_was_present {
                        let restore = self.restore_exchanged_entries(
                            parent.dir(),
                            &parent.name,
                            &temporary_name,
                            &path.display,
                        );
                        return Err(restore
                            .err()
                            .unwrap_or_else(|| {
                                WorkspaceToolError::RollbackFailed(format!(
                                    "{}: published entry ownership could not be established; entry preserved",
                                    path.display
                                ))
                            })
                            .into());
                    }
                    return Err(map_capability_error(error, &path.display).into());
                }
            }
        };
        drop(source_file);
        drop(temporary_file);
        #[cfg(unix)]
        {
            let published_state = self.atomic_target_state(
                parent.dir(),
                &parent.actual_relative,
                &parent.name,
                &path.display,
            );
            if !matches!(
                published_state,
                Ok(Some(ref state)) if state.revision == published_revision
            ) {
                if target_was_present {
                    return Err(self
                        .restore_failed_exchange(
                            parent.dir(),
                            &parent.name,
                            &temporary_name,
                            &published_revision,
                            &path.display,
                            WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                        )
                        .into());
                }
                return Err(WorkspaceToolError::RollbackFailed(format!(
                    "{}: published entry ownership changed",
                    path.display
                ))
                .into());
            }
        }
        let after_rename_result = after_rename();
        #[cfg(unix)]
        let backup_revision = if target_was_present {
            use std::io::Seek as _;

            let mut original_file = original_file
                .take()
                .expect("present Unix target retains its pinned original handle");
            if original_file.rewind().is_err() {
                return Err(self
                    .restore_failed_exchange(
                        parent.dir(),
                        &parent.name,
                        &temporary_name,
                        &published_revision,
                        &path.display,
                        WorkspaceToolError::RollbackFailed(format!(
                            "{}: exchanged backup ownership could not be established",
                            path.display
                        )),
                    )
                    .into());
            }
            let pinned_backup_revision =
                match self.read_file_bytes_with_revision(&path.display, &mut original_file) {
                    Ok((_, revision)) => revision,
                    Err(error) => {
                        return Err(self
                            .restore_failed_exchange(
                                parent.dir(),
                                &parent.name,
                                &temporary_name,
                                &published_revision,
                                &path.display,
                                map_capability_error(error, &path.display),
                            )
                            .into());
                    }
                };
            let backup_state = self.atomic_target_state(
                parent.dir(),
                &parent.actual_relative,
                &temporary_name,
                &path.display,
            );
            match backup_state {
                Ok(Some(state)) if state.revision == pinned_backup_revision => Some(state.revision),
                outcome => {
                    let backup_failure = match outcome {
                        Ok(_) => WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                        Err(error) => map_capability_error(error, &path.display),
                    };
                    return Err(self
                        .restore_failed_exchange(
                            parent.dir(),
                            &parent.name,
                            &temporary_name,
                            &published_revision,
                            &path.display,
                            backup_failure,
                        )
                        .into());
                }
            }
        } else {
            None
        };
        #[cfg(unix)]
        {
            let published_after_hook = self.atomic_target_state(
                parent.dir(),
                &parent.actual_relative,
                &parent.name,
                &path.display,
            );
            let published_is_owned = matches!(
                published_after_hook,
                Ok(Some(ref state))
                    if state.revision == published_revision
            );
            if !published_is_owned {
                if target_was_present {
                    return Err(self
                        .restore_failed_exchange(
                            parent.dir(),
                            &parent.name,
                            &temporary_name,
                            &published_revision,
                            &path.display,
                            WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                        )
                        .into());
                }
                match mutation_rename_noreplace(parent.dir(), &parent.name, &temporary_name) {
                    Ok(()) => {
                        return Err(
                            WorkspaceToolError::ConcurrentMutation(path.display.clone()).into()
                        );
                    }
                    Err(error) => {
                        return Err(map_capability_error(error, &path.display).into());
                    }
                }
            }
        }
        if let Err(error) = after_rename_result {
            #[cfg(unix)]
            {
                if target_was_present {
                    return Err(self
                        .restore_failed_exchange(
                            parent.dir(),
                            &parent.name,
                            &temporary_name,
                            &published_revision,
                            &path.display,
                            error,
                        )
                        .into());
                }
                return match self.atomic_target_state(
                    parent.dir(),
                    &parent.actual_relative,
                    &parent.name,
                    &path.display,
                ) {
                    Ok(Some(state)) if state.revision == published_revision => {
                        match self.remove_created_file_unix(path, &state.revision) {
                            Ok(()) => Err(error.into()),
                            Err(cleanup_error) => Err(cleanup_error.into()),
                        }
                    }
                    Ok(_) => {
                        Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()).into())
                    }
                    Err(state_error) => {
                        Err(map_capability_error(state_error, &path.display).into())
                    }
                };
            }
            #[cfg(not(unix))]
            return match self.atomic_target_state(
                parent.dir(),
                &parent.actual_relative,
                &parent.name,
                &path.display,
            ) {
                Ok(Some(state))
                    if state.revision.object_identity == temporary_identity
                        && state.revision.content_digest == temporary_revision.content_digest =>
                {
                    let rollback_error =
                        if let Some(original_identity) = original_identity.as_deref() {
                            cleanup_owned_file(
                                parent.dir(),
                                &temporary_name,
                                original_identity,
                                error.clone(),
                            )
                        } else {
                            error.clone()
                        };
                    Err(AtomicWriteFailure::published(
                        rollback_error,
                        state.revision,
                    ))
                }
                Ok(_) => Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()).into()),
                Err(state_error) => Err(map_capability_error(state_error, &path.display).into()),
            };
        }
        let published_state_result = self.atomic_target_state(
            parent.dir(),
            &parent.actual_relative,
            &parent.name,
            &path.display,
        );
        let published_state = match published_state_result {
            Ok(Some(state))
                if {
                    #[cfg(unix)]
                    {
                        state.revision == published_revision
                    }
                    #[cfg(not(unix))]
                    {
                        state.revision.object_identity == temporary_identity
                            && state.revision.content_digest == temporary_revision.content_digest
                    }
                } =>
            {
                state
            }
            outcome => {
                #[cfg(unix)]
                if target_was_present {
                    return Err(self
                        .restore_failed_exchange(
                            parent.dir(),
                            &parent.name,
                            &temporary_name,
                            &published_revision,
                            &path.display,
                            WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                        )
                        .into());
                }
                #[cfg(not(unix))]
                if let Ok(Some(ref state)) = outcome
                    && state.revision.object_identity == temporary_identity
                {
                    return Err(AtomicWriteFailure::published(
                        WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                        state.revision.clone(),
                    ));
                }
                return match outcome {
                    Ok(_) => {
                        Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()).into())
                    }
                    Err(error) => Err(map_capability_error(error, &path.display).into()),
                };
            }
        };
        #[cfg(unix)]
        if let Some(backup_revision) = backup_revision.as_ref()
            && let Err(cleanup_error) = self.remove_owned_file_revision_unix(
                parent.dir(),
                &temporary_name,
                &path.display,
                backup_revision,
            )
        {
            return Err(AtomicWriteFailure::published(
                cleanup_error,
                published_state.revision,
            ));
        }
        #[cfg(not(unix))]
        if let Some(original_identity) = original_identity.as_deref() {
            let cleanup_error = cleanup_owned_file(
                parent.dir(),
                &temporary_name,
                original_identity,
                WorkspaceToolError::ConcurrentMutation(path.display.clone()),
            );
            if cleanup_error != WorkspaceToolError::ConcurrentMutation(path.display.clone()) {
                return Err(AtomicWriteFailure::published(
                    cleanup_error,
                    published_state.revision,
                ));
            }
        }
        Ok(published_state.revision)
    }

    /// Reconcile an exchange whose old entry was not the revision observed
    /// during preflight.  The published entry is first detached into a
    /// quarantine with NOREPLACE; the unexpected old entry is then restored
    /// with another NOREPLACE move.  At no point is a concurrently created
    /// destination overwritten.
    #[cfg(unix)]
    fn restore_failed_exchange(
        &self,
        parent: &CapabilityDir,
        target: &OsStr,
        backup: &OsStr,
        published_revision: &WorkspaceContentRevision,
        relative: &str,
        failure: WorkspaceToolError,
    ) -> WorkspaceToolError {
        use std::io::Seek as _;

        let (mut published_file, target_was_missing) = match open_file_from_parent(parent, target) {
            Ok(mut file) => (
                match self.read_file_bytes_with_revision(relative, &mut file) {
                    Ok((_, revision)) if revision == *published_revision => Some(file),
                    Ok(_) | Err(_) => None,
                },
                false,
            ),
            Err(CapabilityAccessError::Missing) => (None, true),
            Err(_) => (None, false),
        };
        if let Err(error) = self.restore_exchanged_entries(parent, target, backup, relative) {
            return error;
        }
        if target_was_missing {
            return failure;
        }
        let Some(mut published_file) = published_file.take() else {
            return WorkspaceToolError::RollbackFailed(format!(
                "{relative}: published entry ownership changed; replacement preserved"
            ));
        };
        if published_file.rewind().is_err() {
            return WorkspaceToolError::RollbackFailed(format!(
                "{relative}: restored publication ownership could not be verified"
            ));
        }
        let pinned_revision =
            match self.read_file_bytes_with_revision(relative, &mut published_file) {
                Ok((_, revision)) => revision,
                Err(_) => {
                    return WorkspaceToolError::RollbackFailed(format!(
                        "{relative}: restored publication ownership could not be verified"
                    ));
                }
            };
        match self.remove_owned_file_revision_unix(parent, backup, relative, &pinned_revision) {
            Ok(()) => failure,
            Err(error) => error,
        }
    }

    #[cfg(unix)]
    fn restore_exchanged_entries(
        &self,
        parent: &CapabilityDir,
        target: &OsStr,
        backup: &OsStr,
        relative: &str,
    ) -> Result<(), WorkspaceToolError> {
        let quarantine = loop {
            let quarantine = mutation_quarantine_name(parent, "published")
                .map_err(|error| map_capability_error(error, relative))?;
            match mutation_rename_noreplace(parent, target, &quarantine) {
                Ok(()) => break quarantine,
                Err(CapabilityAccessError::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(CapabilityAccessError::Missing) => {
                    return mutation_rename_noreplace(parent, backup, target)
                        .map_err(|error| map_capability_error(error, relative));
                }
                Err(CapabilityAccessError::Unsupported) => {
                    return Err(WorkspaceToolError::PathIdentityUnsupported(
                        relative.to_string(),
                    ));
                }
                Err(error) => return Err(map_capability_error(error, relative)),
            }
        };

        match mutation_rename_noreplace(parent, backup, target) {
            Ok(()) => {}
            Err(CapabilityAccessError::Io(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                return Err(WorkspaceToolError::RollbackFailed(format!(
                    "{relative}: concurrent target appeared during exchange restoration"
                )));
            }
            Err(CapabilityAccessError::Missing) => {
                let _ = mutation_rename_noreplace(parent, &quarantine, target);
                return Err(WorkspaceToolError::RollbackFailed(format!(
                    "{relative}: exchanged backup disappeared during restoration"
                )));
            }
            Err(CapabilityAccessError::Unsupported) => {
                return Err(WorkspaceToolError::PathIdentityUnsupported(
                    relative.to_string(),
                ));
            }
            Err(error) => return Err(map_capability_error(error, relative)),
        }
        match mutation_rename_noreplace(parent, &quarantine, backup) {
            Ok(()) => Ok(()),
            Err(CapabilityAccessError::Unsupported) => Err(
                WorkspaceToolError::PathIdentityUnsupported(relative.to_string()),
            ),
            Err(error) => Err(WorkspaceToolError::RollbackFailed(format!(
                "{relative}: exchanged source restoration failed: {}",
                map_capability_error(error, relative)
            ))),
        }
    }

    /// Remove a file only while a complete content revision and a pinned file
    /// handle both prove that the quarantined entry is still the owned object.
    #[cfg(unix)]
    fn remove_owned_file_revision_unix(
        &self,
        parent: &CapabilityDir,
        name: &OsStr,
        relative: &str,
        expected_revision: &WorkspaceContentRevision,
    ) -> Result<(), WorkspaceToolError> {
        use std::io::Seek as _;

        let mut pinned = match open_file_from_parent(parent, name) {
            Ok(file) => file,
            Err(CapabilityAccessError::Missing) => return Ok(()),
            Err(error) => return Err(map_capability_error(error, relative)),
        };
        let initial_revision = self
            .read_file_bytes_with_revision(relative, &mut pinned)
            .map_err(|error| map_capability_error(error, relative))?
            .1;
        if !same_cleanup_revision(expected_revision, &initial_revision) {
            return Err(WorkspaceToolError::RollbackFailed(format!(
                "{relative}: cleanup ownership changed; entry preserved"
            )));
        }
        let quarantine = loop {
            let quarantine = mutation_quarantine_name(parent, "owned-file")
                .map_err(|error| map_capability_error(error, relative))?;
            match mutation_rename_noreplace(parent, name, &quarantine) {
                Ok(()) => break quarantine,
                Err(CapabilityAccessError::Missing) => return Ok(()),
                Err(CapabilityAccessError::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(CapabilityAccessError::Unsupported) => {
                    return Err(WorkspaceToolError::PathIdentityUnsupported(
                        relative.to_string(),
                    ));
                }
                Err(error) => return Err(map_capability_error(error, relative)),
            }
        };
        if pinned.rewind().is_err() {
            return Err(WorkspaceToolError::RollbackFailed(format!(
                "{relative}: quarantined cleanup ownership could not be verified"
            )));
        }
        let pinned_revision = match self.read_file_bytes_with_revision(relative, &mut pinned) {
            Ok((_, revision)) => revision,
            Err(_) => {
                return Err(WorkspaceToolError::RollbackFailed(format!(
                    "{relative}: quarantined cleanup ownership could not be verified"
                )));
            }
        };
        let mut quarantined = match open_file_from_parent(parent, &quarantine) {
            Ok(file) => file,
            Err(_) => {
                return Err(WorkspaceToolError::RollbackFailed(format!(
                    "{relative}: quarantined cleanup entry could not be verified"
                )));
            }
        };
        let quarantined_revision =
            match self.read_file_bytes_with_revision(relative, &mut quarantined) {
                Ok((_, revision)) => revision,
                Err(_) => {
                    return Err(WorkspaceToolError::RollbackFailed(format!(
                        "{relative}: quarantined cleanup entry could not be verified"
                    )));
                }
            };
        if quarantined_revision != pinned_revision {
            return Err(WorkspaceToolError::RollbackFailed(format!(
                "{relative}: quarantined cleanup ownership changed; entry preserved"
            )));
        }
        match mutation_unlink_file(parent, &quarantine) {
            Ok(()) | Err(CapabilityAccessError::Missing) => Ok(()),
            Err(CapabilityAccessError::Unsupported) => Err(
                WorkspaceToolError::PathIdentityUnsupported(relative.to_string()),
            ),
            Err(error) => Err(WorkspaceToolError::RollbackFailed(format!(
                "{relative}: quarantined cleanup failed: {}",
                map_capability_error(error, relative)
            ))),
        }
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
        #[cfg(unix)]
        {
            self.remove_created_file_unix(path, expected_revision)
        }
        #[cfg(not(unix))]
        {
            self.remove_created_file_non_unix(path, expected_revision)
        }
    }

    #[cfg(unix)]
    fn remove_created_file_unix(
        &self,
        path: &CapabilityRelativePath,
        expected_revision: &WorkspaceContentRevision,
    ) -> Result<(), WorkspaceToolError> {
        let parent = match self.open_parent_directory(&path.relative, false) {
            Ok(parent) => parent,
            Err(CapabilityAccessError::Missing) => return Ok(()),
            Err(error) => return Err(map_capability_error(error, &path.display)),
        };
        self.remove_owned_file_revision_unix(
            parent.dir(),
            &parent.name,
            &path.display,
            expected_revision,
        )
    }

    #[cfg(not(unix))]
    fn remove_created_file_non_unix(
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
        #[cfg(unix)]
        {
            self.remove_created_directories_unix(created)
        }
        #[cfg(not(unix))]
        {
            self.remove_created_directories_non_unix(created)
        }
    }

    #[cfg(unix)]
    fn remove_created_directories_unix(
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
                let quarantine = loop {
                    let quarantine = mutation_quarantine_name(parent.dir(), "created-dir")
                        .map_err(|error| map_capability_error(error, &directory.path.display))?;
                    match mutation_rename_noreplace(parent.dir(), &parent.name, &quarantine) {
                        Ok(()) => break quarantine,
                        Err(CapabilityAccessError::Missing) => return Ok(()),
                        Err(CapabilityAccessError::Io(error))
                            if error.kind() == std::io::ErrorKind::AlreadyExists =>
                        {
                            continue;
                        }
                        Err(CapabilityAccessError::Unsupported) => {
                            return Err(WorkspaceToolError::PathIdentityUnsupported(
                                directory.path.display.clone(),
                            ));
                        }
                        Err(error) => {
                            return Err(map_capability_error(error, &directory.path.display));
                        }
                    }
                };
                drop(directory._guard.take());
                let opened = match open_directory_component(parent.dir(), &quarantine, false) {
                    Ok(opened) => opened,
                    Err(error) => {
                        let open_error = map_capability_error(error, &directory.path.display);
                        return match restore_quarantined_entry(
                            parent.dir(),
                            &quarantine,
                            &parent.name,
                            &directory.path.display,
                        ) {
                            Ok(()) => Err(open_error),
                            Err(restore_error) => Err(restore_error),
                        };
                    }
                };
                let identity = match directory_object_identity_key(&opened) {
                    Ok(identity) => identity,
                    Err(error) => {
                        drop(opened);
                        let identity_error = map_capability_error(error, &directory.path.display);
                        return match restore_quarantined_entry(
                            parent.dir(),
                            &quarantine,
                            &parent.name,
                            &directory.path.display,
                        ) {
                            Ok(()) => Err(identity_error),
                            Err(restore_error) => Err(restore_error),
                        };
                    }
                };
                drop(opened);
                if identity != directory.identity {
                    return match restore_quarantined_entry(
                        parent.dir(),
                        &quarantine,
                        &parent.name,
                        &directory.path.display,
                    ) {
                        Ok(()) => Err(WorkspaceToolError::ConcurrentMutation(
                            directory.path.display.clone(),
                        )),
                        Err(error) => Err(error),
                    };
                }
                match mutation_unlink_directory(parent.dir(), &quarantine) {
                    Ok(()) | Err(CapabilityAccessError::Missing) => Ok(()),
                    Err(error) => {
                        // A non-empty directory (or another cleanup error) is
                        // recoverable only if its original name is still free.
                        let cleanup_error = match error {
                            CapabilityAccessError::Unsupported => {
                                WorkspaceToolError::PathIdentityUnsupported(
                                    directory.path.display.clone(),
                                )
                            }
                            error => io_error(std::io::Error::other(format!(
                                "created directory quarantine cleanup failed: {}",
                                map_capability_error(error, &directory.path.display)
                            ))),
                        };
                        match restore_quarantined_entry(
                            parent.dir(),
                            &quarantine,
                            &parent.name,
                            &directory.path.display,
                        ) {
                            Ok(()) => Err(cleanup_error),
                            Err(restore_error) => Err(restore_error),
                        }
                    }
                }
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

    #[cfg(not(unix))]
    fn remove_created_directories_non_unix(
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

/// Hash the canonical bytes that were replaced by one atomic patch.
fn workspace_diff_digest(prepared: &[PreparedMutation]) -> String {
    let mut entries = prepared
        .iter()
        .map(|mutation| {
            (
                mutation.relative.as_str(),
                mutation.original.as_bytes(),
                mutation.updated.as_bytes(),
            )
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    let mut hasher = Sha256::new();
    for (path, original, updated) in entries {
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update((original.len() as u64).to_le_bytes());
        hasher.update(original);
        hasher.update((updated.len() as u64).to_le_bytes());
        hasher.update(updated);
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn publish_temporary(
    parent: &CapabilityDir,
    temporary_name: &OsStr,
    target_name: &OsStr,
    target_was_present: bool,
) -> Result<(), CapabilityAccessError> {
    #[cfg(unix)]
    {
        if target_was_present {
            mutation_rename_exchange(parent, temporary_name, target_name)
        } else {
            mutation_rename_noreplace(parent, temporary_name, target_name)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = target_was_present;
        parent
            .rename(temporary_name, parent, target_name)
            .map_err(classify_io_error)
    }
}

fn temporary_cleanup_identity_matches(expected: &str, observed: &str) -> bool {
    #[cfg(all(test, unix))]
    if FORCE_TEMPORARY_CLEANUP_IDENTITY_COLLISION.with(|force| force.get()) {
        return true;
    }
    expected == observed
}

#[cfg(unix)]
fn same_cleanup_revision(
    expected: &WorkspaceContentRevision,
    observed: &WorkspaceContentRevision,
) -> bool {
    #[cfg(test)]
    if FORCE_TEMPORARY_CLEANUP_IDENTITY_COLLISION.with(|force| force.replace(false)) {
        let mut observed = observed.clone();
        observed
            .object_identity
            .clone_from(&expected.object_identity);
        return expected == &observed;
    }
    expected == observed
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
