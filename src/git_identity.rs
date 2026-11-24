//! A `git` command with a fixed identity. Test fixtures that create commits
//! must not depend on the runner's ambient git config (Linux CI has none).

pub(crate) fn command() -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.env("GIT_AUTHOR_NAME", "Flywheel Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Flywheel Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    cmd
}
