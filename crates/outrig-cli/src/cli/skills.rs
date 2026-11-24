//! The CLI's skill sources, shared by interactive and host-program sessions.

use std::path::{Path, PathBuf};

use directories::BaseDirs;
use outrig::harness::DirectorySource;

/// Project skills first, then user skills unless the repository is the home directory.
pub(crate) fn sources(repo_root: &Path) -> Vec<(&'static str, DirectorySource)> {
    let home = BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    sources_at(repo_root, home)
}

fn sources_at(repo_root: &Path, home: Option<PathBuf>) -> Vec<(&'static str, DirectorySource)> {
    let mut sources = vec![("project", DirectorySource::within(repo_root, SKILLS_DIR))];
    if let Some(home) = user_skills_home(repo_root, home) {
        sources.push(("user", DirectorySource::new(home.join(SKILLS_DIR))));
    }
    sources
}

/// Where skills live, beneath a repository and beneath a home directory.
const SKILLS_DIR: &str = ".agents/skills";

/// The home directory whose `.agents/skills` holds the user's skills, served
/// after the repository's: none when there is no home, or when the home is the
/// repository root, whose `.agents/skills` the project's source already
/// serves.
fn user_skills_home(repo_root: &Path, home: Option<PathBuf>) -> Option<PathBuf> {
    let home = home?;
    let same = match (
        std::fs::canonicalize(&home),
        std::fs::canonicalize(repo_root),
    ) {
        (Ok(home), Ok(root)) => home == root,
        _ => home == repo_root,
    };
    (!same).then_some(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both CLI paths get the same names, order and files, without changing process HOME.
    #[test]
    fn sources_are_project_then_user_and_omit_a_duplicate_home() {
        use outrig::harness::SkillSource;

        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        for (root, name) in [(home.path(), "user-only"), (repo.as_path(), "project-only")] {
            std::fs::create_dir_all(root.join(SKILLS_DIR).join(name)).unwrap();
        }
        let sources = sources_at(&repo, Some(home.path().to_path_buf()));
        assert_eq!(
            sources.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            ["project", "user"]
        );
        assert_eq!(sources[0].1.skills().unwrap(), ["project-only"]);
        assert_eq!(sources[1].1.skills().unwrap(), ["user-only"]);
        assert_eq!(sources_at(&repo, Some(repo.clone())).len(), 1);
        assert_eq!(sources_at(&repo, None).len(), 1);
    }

    /// The user's skills are served after the project's, from the home directory, unless the
    /// repository is the home directory -- reached through a link or not -- whose
    /// `.agents/skills` the project's source already serves.
    #[test]
    fn the_users_skills_are_served_unless_home_is_the_repository() {
        let home = tempfile::tempdir().expect("a home");
        let repo = home.path().join("repo");
        std::fs::create_dir(&repo).expect("a repository");
        let at_home = Some(home.path().to_path_buf());
        assert_eq!(user_skills_home(&repo, at_home.clone()), at_home);
        assert_eq!(user_skills_home(home.path(), at_home.clone()), None);
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).expect("a link to the repository");
        assert_eq!(user_skills_home(&link, Some(repo.clone())), None);
        assert_eq!(user_skills_home(&repo, None), None);
    }
}
