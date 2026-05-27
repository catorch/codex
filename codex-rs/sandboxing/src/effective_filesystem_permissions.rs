use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxKind;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::ReadDenyMatcher;
use codex_protocol::permissions::is_protected_metadata_name;
use codex_protocol::protocol::WritableRoot;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::fmt;
use std::path::Path;

/// Inputs needed to bind any remaining symbolic filesystem permissions.
pub struct FilesystemPermissionsContext<'a> {
    pub permission_profile_cwd: &'a AbsolutePathBuf,
}

/// The outer filesystem access mode represented by effective permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemPermissionsMode {
    Restricted,
    Unrestricted,
    External,
}

/// A deny-read glob retained in effective filesystem enforcement inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDenyGlob {
    pattern: String,
}

impl ValidatedDenyGlob {
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// Effective filesystem enforcement facts derived from a permission profile.
///
/// This internal representation centralizes effective roots, writable carveouts,
/// protected metadata, and read-deny matching before platform-specific lowering.
pub struct EffectiveFilesystemPermissions {
    pub mode: FilesystemPermissionsMode,
    pub readable_roots: Vec<AbsolutePathBuf>,
    pub writable_roots: Vec<WritableRoot>,
    pub unreadable_roots: Vec<AbsolutePathBuf>,
    pub unreadable_globs: Vec<ValidatedDenyGlob>,
    pub include_platform_defaults: bool,
    pub glob_scan_max_depth: Option<usize>,
    file_system_policy: FileSystemSandboxPolicy,
    missing_protected_metadata_carveouts: Vec<AbsolutePathBuf>,
    permission_profile_cwd: AbsolutePathBuf,
    read_deny_matcher: Option<ReadDenyMatcher>,
}

impl fmt::Debug for EffectiveFilesystemPermissions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectiveFilesystemPermissions")
            .field("mode", &self.mode)
            .field("readable_roots", &self.readable_roots)
            .field("writable_roots", &self.writable_roots)
            .field("unreadable_roots", &self.unreadable_roots)
            .field("unreadable_globs", &self.unreadable_globs)
            .field("include_platform_defaults", &self.include_platform_defaults)
            .field("glob_scan_max_depth", &self.glob_scan_max_depth)
            .finish_non_exhaustive()
    }
}

impl EffectiveFilesystemPermissions {
    /// Derives effective filesystem enforcement facts for platform consumers.
    ///
    /// Callers must pass an effective `PermissionProfile` after runtime grants and
    /// configured workspace roots have been applied. Any symbolic workspace entry
    /// that remains is bound to `permission_profile_cwd`.
    pub fn from_profile(
        permission_profile: &PermissionProfile,
        context: FilesystemPermissionsContext<'_>,
    ) -> Result<Self, FilesystemPermissionsError> {
        let source_file_system_policy = permission_profile.file_system_sandbox_policy();
        let missing_protected_metadata_carveouts = source_file_system_policy
            .entries
            .iter()
            .filter(|entry| entry.access == FileSystemAccessMode::Read)
            .filter_map(|entry| {
                let FileSystemPath::Special {
                    value:
                        FileSystemSpecialPath::ProjectRoots {
                            subpath: Some(subpath),
                        },
                } = &entry.path
                else {
                    return None;
                };
                if !is_protected_metadata_name(subpath.as_os_str()) {
                    return None;
                }
                let path = AbsolutePathBuf::resolve_path_against_base(
                    subpath,
                    context.permission_profile_cwd.as_path(),
                );
                (!path.as_path().exists()).then_some(path)
            })
            .collect();
        let file_system_policy = source_file_system_policy
            .materialize_project_roots_with_cwd(context.permission_profile_cwd.as_path());
        // Direct enforcement queries have historically failed closed for malformed
        // deny patterns. Platform lowering that expands concrete targets can still
        // validate the patterns before acting on the filesystem.
        let read_deny_matcher = ReadDenyMatcher::new(
            &file_system_policy,
            context.permission_profile_cwd.as_path(),
        );
        let mode = match file_system_policy.kind {
            FileSystemSandboxKind::Restricted => FilesystemPermissionsMode::Restricted,
            FileSystemSandboxKind::Unrestricted => FilesystemPermissionsMode::Unrestricted,
            FileSystemSandboxKind::ExternalSandbox => FilesystemPermissionsMode::External,
        };
        let readable_roots = file_system_policy
            .get_readable_roots_with_cwd(context.permission_profile_cwd.as_path());
        let writable_roots = file_system_policy
            .get_writable_roots_with_cwd(context.permission_profile_cwd.as_path());
        let unreadable_roots = file_system_policy
            .get_unreadable_roots_with_cwd(context.permission_profile_cwd.as_path());
        let unreadable_globs = file_system_policy
            .get_unreadable_globs_with_cwd(context.permission_profile_cwd.as_path())
            .into_iter()
            .map(|pattern| ValidatedDenyGlob { pattern })
            .collect();
        let include_platform_defaults = file_system_policy.include_platform_defaults();
        let glob_scan_max_depth = file_system_policy.glob_scan_max_depth;

        Ok(Self {
            mode,
            readable_roots,
            writable_roots,
            unreadable_roots,
            unreadable_globs,
            include_platform_defaults,
            glob_scan_max_depth,
            file_system_policy,
            missing_protected_metadata_carveouts,
            permission_profile_cwd: context.permission_profile_cwd.clone(),
            read_deny_matcher,
        })
    }

    /// Returns whether a read is permitted after applying explicit read denies.
    pub fn can_read(&self, path: &Path) -> bool {
        self.file_system_policy
            .can_read_path_with_cwd(path, self.permission_profile_cwd.as_path())
            && !self.is_read_denied(path)
    }

    /// Returns whether a write is permitted, including protected metadata rules.
    pub fn can_write(&self, path: &Path) -> bool {
        self.file_system_policy
            .can_write_path_with_cwd(path, self.permission_profile_cwd.as_path())
    }

    /// Returns whether `path` is matched by an explicit deny-read entry.
    pub fn is_read_denied(&self, path: &Path) -> bool {
        self.read_deny_matcher
            .as_ref()
            .is_some_and(|matcher| matcher.is_read_denied(path))
    }

    pub fn has_full_disk_read_access(&self) -> bool {
        self.file_system_policy.has_full_disk_read_access()
    }

    pub fn has_full_disk_write_access(&self) -> bool {
        self.file_system_policy.has_full_disk_write_access()
    }

    /// Returns whether `path` is a missing automatic metadata carveout that a
    /// platform lowerer must enforce without materializing it as a readable path.
    pub fn is_missing_protected_metadata_carveout(&self, path: &Path) -> bool {
        self.missing_protected_metadata_carveouts
            .iter()
            .any(|carveout| carveout.as_path() == path)
    }
}

/// An error deriving filesystem enforcement facts from a permission profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesystemPermissionsError {
    InvalidDenyGlob(String),
}

impl fmt::Display for FilesystemPermissionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDenyGlob(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for FilesystemPermissionsError {}

#[cfg(test)]
#[path = "effective_filesystem_permissions_tests.rs"]
mod tests;
