use std::path::{Path, PathBuf};

use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WorktreeError {
    #[error("branch name is not safe")]
    UnsafeBranch,
    #[error("repository root must be absolute and lexically normalized")]
    UnsafeRepositoryRoot,
    #[error("base commit must be a full hexadecimal object id")]
    InvalidBaseCommit,
    #[error("changed file inventory is not safe")]
    UnsafeChangedFiles,
}

pub const MAX_CHANGED_FILES: usize = 500;
pub const MAX_CHANGED_FILE_BYTES: usize = 4_096;
pub const MAX_CHANGED_FILES_BYTES: usize = 100_000;

pub fn validate_base_commit(base_commit: &str) -> Result<(), WorktreeError> {
    let valid_length = matches!(base_commit.len(), 40 | 64);
    if !valid_length || !base_commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WorktreeError::InvalidBaseCommit);
    }
    Ok(())
}

/// Validate and canonicalize a repository-relative changed-file inventory.
///
/// Canonical inventories are sorted bytewise and contain no duplicates.  The
/// operation is pure: it does not inspect or access the filesystem.
pub fn validate_changed_files(files: &[String]) -> Result<Vec<String>, WorktreeError> {
    if files.len() > MAX_CHANGED_FILES {
        return Err(WorktreeError::UnsafeChangedFiles);
    }
    let mut canonical = Vec::with_capacity(files.len());
    let mut total_bytes = 0usize;
    for file in files {
        if file.is_empty()
            || file.len() > MAX_CHANGED_FILE_BYTES
            || file.starts_with('/')
            || file.contains('\\')
            || file.chars().any(char::is_control)
        {
            return Err(WorktreeError::UnsafeChangedFiles);
        }
        let mut segments = file.split('/');
        if segments.any(|segment| segment.is_empty() || segment == "." || segment == "..") {
            return Err(WorktreeError::UnsafeChangedFiles);
        }
        total_bytes = total_bytes
            .checked_add(file.len())
            .and_then(|size| size.checked_add(1))
            .ok_or(WorktreeError::UnsafeChangedFiles)?;
        if total_bytes > MAX_CHANGED_FILES_BYTES {
            return Err(WorktreeError::UnsafeChangedFiles);
        }
        canonical.push(file.clone());
    }
    canonical.sort_unstable();
    canonical.dedup();
    Ok(canonical)
}

pub fn task_branch(task_id: Uuid, title: &str) -> String {
    let slug: String = title
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    let short_id = &task_id.simple().to_string()[..12];
    if slug.is_empty() {
        format!("agent/task-{short_id}")
    } else {
        format!("agent/{short_id}-{slug}")
    }
}

fn segment_is_only_dot(branch: &str) -> bool {
    branch.split('/').any(|segment| segment == ".")
}

pub fn validate_branch(branch: &str) -> Result<(), WorktreeError> {
    if branch.is_empty()
        || branch.starts_with('/')
        || branch.starts_with('-')
        || branch.starts_with("refs/")
        || branch.ends_with('/')
        || branch.ends_with('.')
        || branch.ends_with(".lock")
        || branch.contains("..")
        || branch.contains("@{")
        || branch.contains('@')
        || branch.contains("//")
        || segment_is_only_dot(branch)
        || branch.split('/').any(|segment| segment.starts_with('.'))
        || branch.chars().any(|character| {
            character.is_control()
                || matches!(character, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
    {
        return Err(WorktreeError::UnsafeBranch);
    }
    Ok(())
}

pub fn worktree_path(repository_root: &Path, task_id: Uuid) -> Result<PathBuf, WorktreeError> {
    let root = repository_root
        .to_str()
        .ok_or(WorktreeError::UnsafeRepositoryRoot)?;
    if !repository_root.is_absolute()
        || root.contains("//")
        || (root.len() > 1 && root.ends_with('/'))
        || root
            .split('/')
            .skip(1)
            .any(|segment| matches!(segment, "." | ".."))
    {
        return Err(WorktreeError::UnsafeRepositoryRoot);
    }
    Ok(repository_root
        .join("worktrees")
        .join(format!("task-{}", &task_id.simple().to_string()[..12])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_branches_are_valid_and_stable() {
        let id = Uuid::parse_str("018f47de-1d2a-7d39-8f10-000000000001").unwrap();
        let branch = task_branch(id, "Implement Library Search!");
        assert_eq!(branch, "agent/018f47de1d2a-implement-library-search");
        assert!(validate_branch(&branch).is_ok());
    }

    #[test]
    fn dangerous_ref_syntax_is_rejected() {
        for branch in [
            "../main",
            "main@{1}",
            "bad branch",
            "-option",
            "ends/",
            "ends.",
            "x.lock",
            "refs/heads/main",
            "foo/./bar",
        ] {
            assert!(validate_branch(branch).is_err(), "{branch}");
        }
    }

    #[test]
    fn validate_branch_rejects_trailing_dot() {
        for branch in ["foo.", "feature/end.", "agent/x."] {
            assert!(validate_branch(branch).is_err(), "{branch}");
        }
    }

    #[test]
    fn validate_branch_rejects_dotlock_suffix() {
        for branch in ["refs.lock", "feature/x.lock", "agent/task.lock"] {
            assert!(validate_branch(branch).is_err(), "{branch}");
        }
    }

    #[test]
    fn validate_branch_rejects_ref_namespace_injection() {
        for branch in ["refs/heads/main", "refs/tags/x", "refs/anything"] {
            assert!(validate_branch(branch).is_err(), "{branch}");
        }
    }

    #[test]
    fn validate_branch_rejects_segment_of_only_dots_or_dot() {
        for branch in ["foo/./bar", "foo/.../bar"] {
            assert!(validate_branch(branch).is_err(), "{branch}");
        }
    }

    #[test]
    fn validate_branch_accepts_normal_nested_slash_paths() {
        for branch in ["agent/abc-def", "feature/foo/bar", "x/y-z"] {
            assert!(validate_branch(branch).is_ok(), "{branch}");
        }
    }

    #[test]
    fn validate_base_commit_requires_full_hex_object_id() {
        assert!(validate_base_commit(&"a".repeat(40)).is_ok());
        assert!(validate_base_commit(&"b".repeat(64)).is_ok());
        let invalid = [
            String::new(),
            "abc".to_string(),
            "a".repeat(39),
            "a".repeat(41),
            "g".repeat(40),
        ];
        for value in invalid {
            assert!(validate_base_commit(&value).is_err(), "{value}");
        }
    }
    #[test]
    fn worktree_path_rejects_non_normal_roots() {
        let id = Uuid::now_v7();
        assert!(worktree_path(Path::new("/repo/./root"), id).is_err());
        assert!(worktree_path(Path::new("/repo/../root"), id).is_err());
        assert!(worktree_path(Path::new("/repo//root"), id).is_err());
        assert!(worktree_path(Path::new("repo"), id).is_err());
        assert!(worktree_path(Path::new("/repo/root/"), id).is_err());
        assert_eq!(
            worktree_path(Path::new("/repo/root"), id).unwrap(),
            Path::new("/repo/root/worktrees")
                .join(format!("task-{}", &id.simple().to_string()[..12]))
        );
    }

    #[test]
    fn changed_files_are_sorted_and_deduplicated() {
        let files = vec!["z.txt".into(), "a/b.txt".into(), "z.txt".into()];
        assert_eq!(
            validate_changed_files(&files).unwrap(),
            vec!["a/b.txt".to_string(), "z.txt".to_string()]
        );
        for value in [
            "".to_string(),
            "/absolute".to_string(),
            "a/../b".to_string(),
            "a//b".to_string(),
            "a\\b".to_string(),
            "a/\u{0}/b".to_string(),
        ] {
            assert!(validate_changed_files(&[value]).is_err());
        }
    }

    #[test]
    fn changed_file_count_boundary_is_enforced() {
        let at_limit = vec!["a".to_string(); MAX_CHANGED_FILES];
        assert!(validate_changed_files(&at_limit).is_ok());

        let over_limit = vec!["a".to_string(); MAX_CHANGED_FILES + 1];
        assert_eq!(
            validate_changed_files(&over_limit),
            Err(WorktreeError::UnsafeChangedFiles)
        );
    }

    #[test]
    fn changed_file_byte_boundary_is_enforced() {
        assert!(validate_changed_files(&["a".repeat(MAX_CHANGED_FILE_BYTES)]).is_ok());
        assert_eq!(
            validate_changed_files(&["a".repeat(MAX_CHANGED_FILE_BYTES + 1)]),
            Err(WorktreeError::UnsafeChangedFiles)
        );
    }

    #[test]
    fn changed_files_aggregate_byte_boundary_is_enforced() {
        let at_limit: Vec<String> = (0..25)
            .map(|index| format!("{}{:04}", "a".repeat(3_995), index))
            .collect();
        assert_eq!(
            at_limit.iter().map(|file| file.len() + 1).sum::<usize>(),
            MAX_CHANGED_FILES_BYTES
        );
        assert!(validate_changed_files(&at_limit).is_ok());

        let mut over_limit = at_limit;
        over_limit[0].push('b');
        assert_eq!(
            validate_changed_files(&over_limit),
            Err(WorktreeError::UnsafeChangedFiles)
        );
    }

    #[test]
    fn branch_hazards_include_at_and_repeated_slash_and_dot_components() {
        for branch in ["@", "foo@bar", "foo//bar", ".hidden/x", "foo/.bar"] {
            assert!(validate_branch(branch).is_err(), "{branch}");
        }
    }

    #[test]
    fn branch_validator_accepts_closing_bracket_like_sql() {
        assert!(validate_branch("agent/closing]bracket").is_ok());
    }
}
