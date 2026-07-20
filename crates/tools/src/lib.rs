#![forbid(unsafe_code)]

//! tool 模式、tool 代理器决策、工作区操作和公开 tool 结果投影。
//!
//! tool 代理器会在执行边界再次校验面向模型的输入；`WorkspaceTools` 则在任何文件系统副作用前
//! 强制执行工作区和受保护路径规则。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt as StdMetadataExt, OpenOptionsExt as StdOpenOptionsExt};
#[cfg(windows)]
use std::path::Prefix;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirEntryExt as _, DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
use cap_std::fs::{
    Dir as CapabilityDir, File as CapabilityFile, Metadata as CapabilityMetadata,
    OpenOptions as CapabilityOpenOptions, Permissions as CapabilityPermissions,
};
#[cfg(windows)]
use cap_std::fs::{MetadataExt as _, OpenOptionsExt as _};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub use singularity_core::is_protected_path;
use singularity_core::{CancellationToken, contains_sensitive_text};
pub use singularity_policy::{
    CommandScopeDigest, PermissionOperation, PermissionResource, ToolId, WorkspaceRelativePath,
};
pub use singularity_sandbox::{
    CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest, CommandResult,
    CommandScriptRequest, CommandSemanticStatus, DEFAULT_COMMAND_TIMEOUT_SECONDS, SandboxBackend,
    SandboxBackendEnforcement, SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode,
    WorkspaceMutation,
};

mod broker;
mod command;
mod registry;
#[cfg(test)]
mod tests;
mod workspace;

pub use broker::*;
pub use command::*;
pub use registry::*;
pub use workspace::*;

pub(crate) use broker::contains_artifact_reference;
pub(crate) use command::{
    CommandModelInput, bounded_text, command_tool_output, io_error, is_binary, next_command_id,
    normalize_path, redact_public_text, validate_tool_name,
};
#[cfg(unix)]
pub(crate) use registry::ERROR_TOO_MANY_SYMLINKS;
pub(crate) use registry::{
    APPROXIMATE_ASCII_CHARS_PER_TOKEN, ARTIFACT_REFERENCE_OMITTED, BINARY_CONTENT_PREVIEW,
    COMMAND_ID_COUNTER, DEFAULT_GREP_MAX_MATCHES, DEFAULT_LIST_MAX_DEPTH, DEFAULT_LIST_MAX_ENTRIES,
    DEFAULT_READ_MAX_CHARS, DEFAULT_RESULT_PREVIEW_MAX_CHARS, DUPLICATE_PATCH_TARGET,
    FILE_READ_CHUNK_SIZE, INVALID_TOOL_ARGUMENTS_ERROR, MAX_COMMAND_SCRIPT_CHARS,
    MAX_COMMAND_TIMEOUT_SECONDS, MAX_GREP_MAX_MATCHES, MAX_LIST_MAX_DEPTH, MAX_LIST_MAX_ENTRIES,
    MAX_READ_MAX_CHARS, MAX_TRUNCATED_SUMMARY_STRING_CHARS, MUTATION_TEMP_COUNTER,
    MUTATION_TEMP_FILE_ATTEMPTS, PROMPT_INJECTION_MARKERS, REDACTED_TOOL_OUTPUT,
    TOOL_APPROVAL_REQUIRED_ERROR, TOOL_CONTRACT_INVALID_ERROR, TOOL_DENIED_ERROR,
    TOOL_SANDBOX_UNAVAILABLE_ERROR, TRUNCATED_OUTPUT_OMITTED, TRUNCATED_RAW_OUTPUT_KEYS,
    UNKNOWN_TOOL_ERROR, WORKSPACE_MUTATION_NOT_APPROVED, WORKSPACE_OBSERVATION_METADATA,
};
#[cfg(windows)]
pub(crate) use registry::{
    ERROR_STOPPED_ON_SYMLINK, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
#[cfg(test)]
pub(crate) use workspace::{
    AtomicWriteFailure, CapabilityRelativePath, PreparedMutation, PublishedMutation,
};
