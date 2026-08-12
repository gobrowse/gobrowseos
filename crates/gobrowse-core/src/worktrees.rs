use std::path::{Path, PathBuf};

use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WorktreeError {
    #[error("branch name is not safe")]
    UnsafeBranch,
    #[error("repository root must be absolute")]
    RelativeRepositoryRoot,
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
        || branch.starts_with('-')
        || branch.starts_with("refs/")
        || branch.ends_with('/')
        || branch.ends_with('.')
        || branch.ends_with(".lock")
        || branch.contains("..")
        || branch.contains("@{")
        || segment_is_only_dot(branch)
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
    if !repository_root.is_absolute() {
        return Err(WorktreeError::RelativeRepositoryRoot);
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
}
