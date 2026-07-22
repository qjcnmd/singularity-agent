//! tools crate 内部失败路径和注册契约测试。

use super::*;

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn test_workspace(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "singularity-tools-cancellation-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test workspace");
        path
    }

    fn remove_workspace(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    fn cancel_after(checks: &AtomicUsize, threshold: usize) -> impl Fn() -> bool + '_ {
        move || checks.fetch_add(1, Ordering::SeqCst).saturating_add(1) >= threshold
    }

    #[test]
    fn cancellable_read_stops_after_a_file_chunk_boundary() {
        let workspace = test_workspace("read-boundary");
        let content = "x".repeat(FILE_READ_CHUNK_SIZE.saturating_mul(3));
        std::fs::write(workspace.join("lines.txt"), content).expect("write lines");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let checks = AtomicUsize::new(0);

        let result = tools.read_with_cancellation_check(
            ReadToolInput {
                path: "lines.txt".to_string(),
                max_chars: None,
                line_start: None,
                line_end: None,
            },
            &cancel_after(&checks, 9),
        );

        assert!(matches!(result, Err(WorkspaceToolError::Cancelled)));
        assert!(checks.load(Ordering::SeqCst) >= 9);
        remove_workspace(&workspace);
    }

    #[test]
    fn cancellable_recursive_list_stops_at_an_entry_boundary() {
        let workspace = test_workspace("list-boundary");
        for directory_index in 0..4 {
            let directory = workspace.join(format!("dir-{directory_index}"));
            std::fs::create_dir_all(directory.join("nested")).expect("create nested directory");
            for file_index in 0..4 {
                std::fs::write(
                    directory.join(format!("file-{file_index}.txt")),
                    "content\n",
                )
                .expect("write nested file");
            }
            std::fs::write(directory.join("nested").join("deep.txt"), "deep\n")
                .expect("write deep file");
        }
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let checks = AtomicUsize::new(0);

        let result = tools.list_with_cancellation_check(
            ListToolInput {
                path: None,
                max_entries: Some(1_000),
                recursive: true,
                max_depth: Some(8),
            },
            &cancel_after(&checks, 45),
        );

        assert!(matches!(result, Err(WorkspaceToolError::Cancelled)));
        assert!(checks.load(Ordering::SeqCst) >= 45);
        remove_workspace(&workspace);
    }

    #[test]
    fn cancellable_recursive_grep_stops_at_a_file_boundary() {
        let workspace = test_workspace("grep-boundary");
        for directory_index in 0..4 {
            let directory = workspace.join(format!("dir-{directory_index}"));
            std::fs::create_dir_all(&directory).expect("create directory");
            for file_index in 0..4 {
                std::fs::write(
                    directory.join(format!("file-{file_index}.txt")),
                    "no match\nno match\n",
                )
                .expect("write grep file");
            }
        }
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let checks = AtomicUsize::new(0);

        let result = tools.grep_with_cancellation_check(
            GrepToolInput {
                path: None,
                pattern: "needle".to_string(),
                max_matches: Some(1_000),
                case_sensitive: true,
            },
            &cancel_after(&checks, 45),
        );

        assert!(matches!(result, Err(WorkspaceToolError::Cancelled)));
        assert!(checks.load(Ordering::SeqCst) >= 45);
        remove_workspace(&workspace);
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;

    fn test_workspace(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "singularity-tools-mutation-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test workspace");
        path
    }

    fn remove_workspace(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn atomic_write_rejects_target_replacement_and_cleans_its_temp_file() {
        let workspace = test_workspace("target-replacement");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_revision) = tools.existing_text_or_empty(&path).expect("read original");

        let result =
            tools.atomic_write_with_hook(&path, "after", original_revision.as_ref(), |_| {
                std::fs::remove_file(&target).expect("remove original");
                std::fs::write(&target, "after!").expect("write concurrent target");
            });

        assert!(matches!(
            result,
            Err(AtomicWriteFailure {
                error: WorkspaceToolError::ConcurrentMutation(_),
                published_revision: None,
            })
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "after!");
        assert!(
            std::fs::read_dir(&workspace)
                .expect("read workspace")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .contains("singularity-tmp"))
        );
        remove_workspace(&workspace);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_never_overwrites_a_concurrently_created_target() {
        use std::os::unix::fs::MetadataExt as _;

        let workspace = test_workspace("concurrent-create-noreplace");
        let target = workspace.join("target.txt");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let mut external_identity = None;

        let result = tools.atomic_write_with_hook(&path, "owned", None, |_| {
            std::fs::write(&target, "external").expect("create concurrent target");
            let metadata = std::fs::metadata(&target).expect("concurrent target metadata");
            external_identity = Some((metadata.dev(), metadata.ino()));
        });

        assert!(matches!(
            result,
            Err(AtomicWriteFailure {
                error: WorkspaceToolError::ConcurrentMutation(_),
                published_revision: None,
            })
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "external");
        let metadata = std::fs::metadata(&target).expect("final target metadata");
        assert_eq!(external_identity, Some((metadata.dev(), metadata.ino())));
        assert!(
            std::fs::read_dir(&workspace)
                .expect("read workspace")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .contains("singularity-tmp"))
        );
        remove_workspace(&workspace);
    }

    #[test]
    fn atomic_write_does_not_delete_a_replaced_temp_source() {
        let workspace = test_workspace("temp-replacement");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_revision) = tools.existing_text_or_empty(&path).expect("read original");
        let mut replacement_path = None;

        let result = tools.atomic_write_with_hook(
            &path,
            "after",
            original_revision.as_ref(),
            |temporary_name| {
                let temporary_path = workspace.join(temporary_name);
                std::fs::remove_file(&temporary_path).expect("remove owned temp");
                std::fs::write(&temporary_path, "concurrent temp").expect("write replacement temp");
                replacement_path = Some(temporary_path);
            },
        );

        assert!(matches!(
            result,
            Err(AtomicWriteFailure {
                error: WorkspaceToolError::ConcurrentMutation(_),
                published_revision: None,
            })
        ));
        let replacement_path = replacement_path.expect("replacement path");
        assert_eq!(
            std::fs::read_to_string(&replacement_path).unwrap(),
            "concurrent temp"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        remove_workspace(&workspace);
    }

    #[test]
    fn rollback_restores_only_published_mutations() {
        let workspace = test_workspace("published-rollback");
        std::fs::write(workspace.join("existing.txt"), "before").expect("write existing target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let existing_path = CapabilityRelativePath::parse("existing.txt").expect("existing path");
        let created_path = CapabilityRelativePath::parse("created.txt").expect("created path");
        let (original, original_revision) = tools
            .existing_text_or_empty(&existing_path)
            .expect("read existing");
        let existing_published = tools
            .atomic_write(&existing_path, "after", original_revision.as_ref())
            .expect("publish existing");
        let created_published = tools
            .atomic_write(&created_path, "created", None)
            .expect("publish created");
        let published = vec![
            PublishedMutation {
                prepared: PreparedMutation {
                    path: existing_path,
                    relative: "existing.txt".to_string(),
                    original,
                    updated: "after".to_string(),
                    original_revision,
                },
                published_revision: existing_published,
            },
            PublishedMutation {
                prepared: PreparedMutation {
                    path: created_path,
                    relative: "created.txt".to_string(),
                    original: String::new(),
                    updated: "created".to_string(),
                    original_revision: None,
                },
                published_revision: created_published,
            },
        ];

        tools
            .rollback_published(&published)
            .expect("rollback published mutations");

        assert_eq!(
            std::fs::read_to_string(workspace.join("existing.txt")).unwrap(),
            "before"
        );
        assert!(!workspace.join("created.txt").exists());
        remove_workspace(&workspace);
    }

    #[test]
    fn post_publish_failure_includes_current_mutation_in_safe_rollback() {
        let workspace = test_workspace("post-publish-rollback");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        #[cfg(unix)]
        let original_identity = {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = std::fs::metadata(&target).expect("original metadata");
            (metadata.dev(), metadata.ino())
        };
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_original, original_revision) =
            tools.existing_text_or_empty(&path).expect("read original");

        let failure = tools
            .atomic_write_with_hooks(
                &path,
                "published",
                original_revision.as_ref(),
                |_| {},
                || {
                    Err(WorkspaceToolError::ConcurrentMutation(
                        "target.txt".to_string(),
                    ))
                },
            )
            .expect_err("post-publish verification fails");
        #[cfg(unix)]
        assert!(failure.published_revision.is_none());
        #[cfg(not(unix))]
        {
            let published_revision = *failure
                .published_revision
                .expect("failure retains published identity");
            let published = vec![PublishedMutation {
                prepared: PreparedMutation {
                    path,
                    relative: "target.txt".to_string(),
                    original: _original,
                    updated: "published".to_string(),
                    original_revision,
                },
                published_revision,
            }];
            tools
                .rollback_published(&published)
                .expect("rollback current published mutation");
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = std::fs::metadata(&target).expect("restored metadata");
            assert_eq!((metadata.dev(), metadata.ino()), original_identity);
        }
        remove_workspace(&workspace);
    }

    #[test]
    fn atomic_write_rejects_same_object_temp_content_tampering() {
        let workspace = test_workspace("temp-content-tampering");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_revision) = tools.existing_text_or_empty(&path).expect("read original");

        let result = tools.atomic_write_with_hook(
            &path,
            "after",
            original_revision.as_ref(),
            |temporary_name| {
                std::fs::write(workspace.join(temporary_name), "other")
                    .expect("tamper with owned temp content");
            },
        );

        assert!(matches!(
            result,
            Err(AtomicWriteFailure {
                error: WorkspaceToolError::ConcurrentMutation(_),
                published_revision: None,
            })
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        assert!(
            std::fs::read_dir(&workspace)
                .expect("read workspace")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .contains("singularity-tmp"))
        );
        remove_workspace(&workspace);
    }

    #[test]
    fn atomic_write_cleans_temp_when_target_revision_cannot_be_read() {
        let workspace = test_workspace("target-revision-error-cleanup");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_revision) = tools.existing_text_or_empty(&path).expect("read original");

        let failure = tools
            .atomic_write_with_hook(&path, "after", original_revision.as_ref(), |_| {
                std::fs::remove_file(&target).expect("remove target");
                std::fs::create_dir(&target).expect("replace target with directory");
            })
            .expect_err("an unreadable target revision must fail closed");

        assert!(failure.published_revision.is_none());
        assert!(target.is_dir());
        assert!(
            std::fs::read_dir(&workspace)
                .expect("read workspace")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .contains("singularity-tmp"))
        );
        remove_workspace(&workspace);
    }

    #[test]
    fn post_publish_replacement_is_not_claimed_or_overwritten_by_rollback() {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;

        let workspace = test_workspace("post-publish-external-replacement");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_revision) = tools.existing_text_or_empty(&path).expect("read original");
        #[cfg(unix)]
        let mut external_identity = None;

        let failure = tools
            .atomic_write_with_hooks(
                &path,
                "published",
                original_revision.as_ref(),
                |_| {},
                || {
                    std::fs::remove_file(&target).expect("remove published target");
                    std::fs::write(&target, "external").expect("write external target");
                    #[cfg(unix)]
                    {
                        let metadata = std::fs::metadata(&target).expect("external metadata");
                        external_identity = Some((metadata.dev(), metadata.ino()));
                    }
                    Err(WorkspaceToolError::ReadFailed(
                        "injected failure".to_string(),
                    ))
                },
            )
            .expect_err("external replacement must fail closed");

        #[cfg(not(unix))]
        {
            assert!(matches!(
                failure,
                AtomicWriteFailure {
                    error: WorkspaceToolError::ConcurrentMutation(_),
                    published_revision: None,
                }
            ));
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "external");
        }
        #[cfg(unix)]
        {
            assert!(failure.published_revision.is_none());
            assert!(matches!(
                failure.error,
                WorkspaceToolError::RollbackFailed(_)
            ));
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
            let external_entries = std::fs::read_dir(&workspace)
                .expect("read workspace")
                .filter_map(|entry| {
                    let entry = entry.expect("entry");
                    (entry.path() != target
                        && matches!(
                            std::fs::read_to_string(entry.path()),
                            Ok(content) if content == "external"
                        ))
                    .then_some(entry.path())
                })
                .collect::<Vec<_>>();
            assert_eq!(external_entries.len(), 1);
            let metadata = std::fs::metadata(&external_entries[0]).expect("preserved metadata");
            assert_eq!(external_identity, Some((metadata.dev(), metadata.ino())));
        }
        remove_workspace(&workspace);
    }

    #[cfg(unix)]
    #[test]
    fn post_exchange_backup_inspection_failure_restores_the_concurrent_object() {
        use std::cell::RefCell;
        use std::os::unix::fs::MetadataExt as _;

        let workspace = test_workspace("post-exchange-backup-inspection");
        let target = workspace.join("target.txt");
        let saved_original = workspace.join("saved-original.txt");
        std::fs::write(&target, "before").expect("write target");
        let original_metadata = std::fs::metadata(&target).expect("original metadata");
        let original_identity = (original_metadata.dev(), original_metadata.ino());
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_revision) = tools.existing_text_or_empty(&path).expect("read original");
        let exchanged_backup = RefCell::new(None::<PathBuf>);
        let concurrent_identity = RefCell::new(None);

        let failure = tools
            .atomic_write_with_hooks(
                &path,
                "published",
                original_revision.as_ref(),
                |temporary_name| {
                    exchanged_backup.replace(Some(workspace.join(temporary_name)));
                },
                || {
                    let backup = exchanged_backup
                        .borrow()
                        .clone()
                        .expect("record exchanged backup path");
                    std::fs::rename(&backup, &saved_original).expect("preserve original object");
                    std::fs::create_dir(&backup).expect("replace backup with directory");
                    let metadata = std::fs::metadata(&backup).expect("concurrent metadata");
                    concurrent_identity.replace(Some((metadata.dev(), metadata.ino())));
                    Ok(())
                },
            )
            .expect_err("backup inspection failure must restore the exchange");

        assert!(failure.published_revision.is_none());
        assert!(matches!(failure.error, WorkspaceToolError::ReadFailed(_)));
        assert!(target.is_dir());
        let target_metadata = std::fs::metadata(&target).expect("restored concurrent metadata");
        assert_eq!(
            *concurrent_identity.borrow(),
            Some((target_metadata.dev(), target_metadata.ino()))
        );
        assert_eq!(
            std::fs::read_to_string(&saved_original).expect("read preserved original"),
            "before"
        );
        let saved_metadata = std::fs::metadata(&saved_original).expect("saved original metadata");
        assert_eq!(
            (saved_metadata.dev(), saved_metadata.ino()),
            original_identity
        );
        assert!(
            std::fs::read_dir(&workspace)
                .expect("read workspace")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .contains("singularity"))
        );
        remove_workspace(&workspace);
    }

    #[test]
    fn post_publish_disappearance_is_not_claimed_for_rollback() {
        let workspace = test_workspace("post-publish-disappearance");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_revision) = tools.existing_text_or_empty(&path).expect("read original");

        let failure = tools
            .atomic_write_with_hooks(
                &path,
                "published",
                original_revision.as_ref(),
                |_| {},
                || {
                    std::fs::remove_file(&target).expect("remove published target");
                    Ok(())
                },
            )
            .expect_err("a missing published target must fail closed");

        assert!(matches!(
            failure,
            AtomicWriteFailure {
                error: WorkspaceToolError::ConcurrentMutation(_),
                published_revision: None,
            }
        ));
        #[cfg(not(unix))]
        assert!(!target.exists());
        #[cfg(unix)]
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        remove_workspace(&workspace);
    }

    #[test]
    fn post_publish_same_object_content_tampering_is_rolled_back() {
        let workspace = test_workspace("post-publish-content-tampering");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_original, original_revision) =
            tools.existing_text_or_empty(&path).expect("read original");

        let failure = tools
            .atomic_write_with_hooks(
                &path,
                "published",
                original_revision.as_ref(),
                |_| {},
                || {
                    std::fs::write(&target, "tampered").expect("tamper published target");
                    Ok(())
                },
            )
            .expect_err("tampered published content must fail closed");
        assert!(matches!(
            failure.error,
            WorkspaceToolError::ConcurrentMutation(_)
        ));
        #[cfg(unix)]
        assert!(failure.published_revision.is_none());
        #[cfg(not(unix))]
        {
            let published_revision = *failure
                .published_revision
                .expect("owned published object remains safe to roll back");
            tools
                .rollback_published(&[PublishedMutation {
                    prepared: PreparedMutation {
                        path,
                        relative: "target.txt".to_string(),
                        original: _original,
                        updated: "published".to_string(),
                        original_revision,
                    },
                    published_revision,
                }])
                .expect("roll back owned tampered publication");
        }

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        remove_workspace(&workspace);
    }

    #[test]
    fn rollback_preserves_a_concurrently_replaced_published_target() {
        let workspace = test_workspace("rollback-concurrent");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (original, original_revision) =
            tools.existing_text_or_empty(&path).expect("read original");
        let published_revision = tools
            .atomic_write(&path, "published", original_revision.as_ref())
            .expect("publish mutation");
        std::fs::remove_file(&target).expect("remove published target");
        std::fs::write(&target, "concurrent").expect("write concurrent target");
        let published = vec![PublishedMutation {
            prepared: PreparedMutation {
                path,
                relative: "target.txt".to_string(),
                original,
                updated: "published".to_string(),
                original_revision,
            },
            published_revision,
        }];

        assert!(matches!(
            tools.rollback_published(&published),
            Err(WorkspaceToolError::RollbackFailed(_))
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "concurrent");
        remove_workspace(&workspace);
    }

    #[test]
    fn failed_batch_cleanup_removes_only_its_nested_directories() {
        let workspace = test_workspace("directory-cleanup");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("new/nested/file.txt").expect("relative path");
        let mut created = Vec::new();

        tools
            .ensure_parent_directories(&path, &mut created)
            .expect("create parents");
        assert!(workspace.join("new/nested").is_dir());
        tools
            .remove_created_directories(&mut created)
            .expect("remove created parents");

        assert!(!workspace.join("new").exists());
        remove_workspace(&workspace);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_patch_preserves_existing_unix_file_mode() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let workspace = test_workspace("unix-mode");
        let target = workspace.join("script.sh");
        std::fs::write(&target, "#!/bin/sh\necho before\n").expect("write script");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o751))
            .expect("set executable mode");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");

        tools
            .patch(
                WorkspacePatch {
                    changes: vec![WorkspacePatchChange {
                        path: "script.sh".to_string(),
                        expected: Some("before".to_string()),
                        replacement: "after".to_string(),
                    }],
                },
                &ToolBrokerDecision::Allow,
            )
            .expect("patch script");

        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o751);
        remove_workspace(&workspace);
    }

    #[test]
    fn atomic_write_rejects_same_length_replacement_with_restored_mtime() {
        let workspace = test_workspace("same-length-restored-mtime");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let original_modified = std::fs::metadata(&target)
            .expect("target metadata")
            .modified()
            .expect("target modified time");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_revision) = tools.existing_text_or_empty(&path).expect("read original");

        std::fs::remove_file(&target).expect("remove original target");
        std::fs::write(&target, "after!").expect("recreate target content");
        let replacement = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .expect("open replacement");
        replacement
            .set_times(std::fs::FileTimes::new().set_modified(original_modified))
            .expect("restore replacement mtime");

        let result = tools.atomic_write(&path, "updated", original_revision.as_ref());

        assert!(matches!(
            result,
            Err(AtomicWriteFailure {
                error: WorkspaceToolError::ConcurrentMutation(_),
                published_revision: None,
            })
        ));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "after!",
            "a same-length replacement with restored mtime must not be overwritten"
        );
        remove_workspace(&workspace);
    }

    #[test]
    fn atomic_write_rejects_same_content_recreated_object_with_restored_mtime() {
        let workspace = test_workspace("same-content-recreated-object");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let original_modified = std::fs::metadata(&target)
            .expect("target metadata")
            .modified()
            .expect("target modified time");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_revision) = tools.existing_text_or_empty(&path).expect("read original");

        std::fs::remove_file(&target).expect("remove original target");
        std::fs::write(&target, "before").expect("recreate target content");
        let replacement = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .expect("open replacement");
        replacement
            .set_times(std::fs::FileTimes::new().set_modified(original_modified))
            .expect("restore replacement mtime");

        let result = tools.atomic_write(&path, "updated", original_revision.as_ref());

        assert!(matches!(
            result,
            Err(AtomicWriteFailure {
                error: WorkspaceToolError::ConcurrentMutation(_),
                published_revision: None,
            })
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        remove_workspace(&workspace);
    }

    #[test]
    fn content_revision_rejects_mutation_after_pre_read_metadata() {
        let workspace = test_workspace("mutation-during-read");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let result = tools.read_file_with_revision_after_metadata_hook_for_test(&path, || {
            std::fs::write(&target, "changed").expect("mutate target after pre-read metadata");
        });

        assert!(matches!(
            result,
            Err(WorkspaceToolError::ConcurrentMutation(_))
        ));
        remove_workspace(&workspace);
    }
}

#[cfg(test)]
mod registry_contract_tests {
    use super::*;

    fn accept_input(_: &Value) -> Result<(), ToolInputValidationError> {
        Ok(())
    }

    #[test]
    fn agent_control_binding_rejects_inconsistent_authorization() {
        let mut entry = ToolEntry::model(
            ToolSpec::new(
                "update_plan",
                "Update the plan",
                json!({"type": "object"}),
                ToolExecutionMode::Exclusive,
                accept_input,
            ),
            1,
            ToolCapability::PlanManagement,
            ToolAuthorization::AgentControl,
            ToolExecutor::AgentControl(AgentControlToolExecutor::UpdatePlan),
        )
        .expect("valid agent control entry");
        entry.authorization = ToolAuthorization::WorkspaceRead;

        let name = entry.id.as_str().to_string();
        let mut registry = ToolRegistry::default();
        registry.tools.insert(name.clone(), entry);
        let broker = ToolBroker::new(registry);

        let result = broker.bind_authorization(
            &name,
            json!({"steps": []}),
            None,
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        );

        assert!(matches!(result, Err(WorkspaceToolError::InvalidInput(_))));

        let envelope = ToolCallRequest::new(&name, &name, "{}");
        let mut executor_called = false;
        let result = broker.execute(&envelope, ToolBrokerDecision::Allow, |_, _| {
            executor_called = true;
            ToolOutput::success(json!({"unexpected": true}))
        });
        assert!(!executor_called);
        assert_eq!(
            result.error_code.as_deref(),
            Some(TOOL_CONTRACT_INVALID_ERROR)
        );
    }
}
