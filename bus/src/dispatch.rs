use std::fs;

pub(crate) fn read(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("Cannot read {path}. {e}"))
}

pub(crate) fn prefix_overlaps(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

pub(crate) fn check_board(path: &str, files: &str) -> Result<(), String> {
    let files: Vec<_> = globs(files).collect();
    if files.is_empty() {
        return Err("--files needs at least one glob".into());
    }
    let board = read(path)?;
    for (index, line) in board.trim_start_matches('\u{feff}').lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let cols: Vec<_> = line.split('\t').map(str::trim).collect();
        let [id, topic, owner, _, _, row_files, state] = cols.as_slice() else {
            return Err(format!(
                "Board line {} needs seven tab separated columns",
                index + 1
            ));
        };
        if matches!(*state, "running" | "leased") {
            for row_file in globs(row_files) {
                if let Some(file) = files.iter().find(|file| {
                    prefix_overlaps(file.trim_end_matches('*'), row_file.trim_end_matches('*'))
                }) {
                    return Err(format!(
                        "{topic} is held by {owner} on board row {id}. File overlap between {file} and {row_file}"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn globs(list: &str) -> impl Iterator<Item = &str> {
    list.split(',')
        .map(str::trim)
        .filter(|glob| !glob.is_empty())
}

fn name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '@' | '%' | '=' | '#')
}

fn names(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c| !name_char(c))
        .map(|s| s.trim_end_matches('.'))
        .filter(|s| !s.is_empty())
}

fn branches(text: &str) -> impl Iterator<Item = &str> {
    names(text).filter_map(|name| {
        let name = ["refs/heads/", "refs/remotes/origin/", "origin/"]
            .into_iter()
            .find_map(|prefix| name.strip_prefix(prefix))
            .unwrap_or(name);
        let (namespace, rest) = name.split_once('/')?;
        (matches!(namespace, "codex" | "gemini") && !rest.is_empty()).then_some(name)
    })
}

fn foreign_branch(line: &str, branch: &str) -> Result<(), String> {
    if let Some(other) = branches(line).find(|name| *name != branch) {
        return Err(format!(
            "Found branch {other}, expected {branch}. Line: {line}"
        ));
    }
    Ok(())
}

pub(crate) fn check_brief(text: &str, branch: &str, task: &str) -> Result<(), String> {
    let text = text.trim_start_matches('\u{feff}');
    for line in text.lines() {
        foreign_branch(line, branch)?;
    }
    let first = text.lines().next().unwrap_or("");
    let header = format!("TASK {task} BRANCH {branch}");
    if first.starts_with("TASK ") {
        if first
            .strip_prefix(&header)
            .is_some_and(|tail| tail.starts_with(char::is_whitespace) || tail.is_empty())
        {
            return Ok(());
        }
        return Err(format!("Expected {header}. Line: {first}"));
    }
    let mut found = false;
    for line in text.lines() {
        if let Some(note) = line.strip_prefix("BRANCH NOTE:") {
            if !names(note).any(|name| name == branch) {
                return Err(format!("Expected branch {branch}. Line: {line}"));
            }
            found = true;
        }
    }
    if found {
        Ok(())
    } else {
        Err(format!(
            "Expected {header} or a BRANCH NOTE naming {branch}"
        ))
    }
}

pub(crate) fn check_result(
    text: &str,
    branch: &str,
    task: &str,
    remote_path: Option<&str>,
) -> Result<(), String> {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or("");
    if !matches!(names(first).next(), Some("READY" | "STUCK"))
        || !(first.starts_with("READY") || first.starts_with("STUCK"))
    {
        return Err("First nonempty line must start with READY or STUCK".into());
    }

    let end = text.char_indices().nth(400).map_or(text.len(), |(i, _)| i);
    if !names(text).any(|name| {
        (name == branch || name == task)
            && name.as_bytes().as_ptr_range().end <= text.as_bytes()[..end].as_ptr_range().end
    }) {
        return Err(format!("First 400 characters must name {branch} or {task}"));
    }

    if let Some(pushed) = branches(first).next()
        && pushed != branch
    {
        return Err(format!(
            "Found result branch {pushed}, expected {branch}. Line: {first}"
        ));
    }
    for line in std::iter::once(first).chain(lines) {
        if names(line).any(|word| matches!(word.to_ascii_lowercase().as_str(), "push" | "pushed"))
            || line.to_ascii_lowercase().starts_with("branch:")
        {
            foreign_branch(line, branch)?;
        }
    }
    if let Some(path) = remote_path {
        check_remote(text, branch, &read(path)?)?;
    }
    Ok(())
}

fn is_sha(word: &str) -> bool {
    (7..=64).contains(&word.len()) && word.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn check_remote(text: &str, branch: &str, remote: &str) -> Result<(), String> {
    let mut commits = names(text).filter(|word| is_sha(word));
    let commit = commits.next().ok_or("Result does not name a commit")?;
    if commits.any(|other| !other.eq_ignore_ascii_case(commit)) {
        return Err("Result names more than one commit".into());
    }
    let expected_ref = format!("refs/heads/{branch}");
    let remote_sha = remote
        .trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find_map(|(sha, name)| (name.trim() == expected_ref).then_some(sha))
        .ok_or_else(|| format!("Remote output has no entry for {expected_ref}"))?;
    if matches!(remote_sha.len(), 40 | 64)
        && is_sha(remote_sha)
        && remote_sha
            .get(..commit.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(commit))
    {
        Ok(())
    } else {
        Err(format!(
            "Result commit {commit} does not match remote commit {remote_sha} for {branch}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new(name: &str, content: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("sommelier-{name}-{}", std::process::id()));
            fs::write(&path, content).unwrap();
            Self(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }

        fn write(&self, content: &str) {
            fs::write(&self.0, content).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn dispatch_checks_topic_prefixes_and_active_board_rows() {
        assert!(prefix_overlaps("issue/206", "issue/206/memory"));
        assert!(prefix_overlaps("issue/206/memory", "issue/206"));
        assert!(prefix_overlaps("issue/206", "issue/206"));
        assert!(!prefix_overlaps("issue/206", "issue/207"));

        let board = Fixture::new(
            "board",
            concat!(
                "\u{feff}id\ttopic\towner\tclass\tbranch\tfiles\tstate\r\n",
                "1\tissue/1\tworker1\twork\tcodex/one\tsrc/*\tdone\r\n",
                "2\tissue/2\tworker2\twork\tcodex/two\tdocs/*, src/api/*\trunning\r\n",
                "3\tissue/3\tworker3\twork\tcodex/three\tui/*\tleased\r\n",
            ),
        );
        assert!(check_board(board.path(), "src/main.rs").is_ok());
        assert!(
            check_board(board.path(), "src/api/lib.rs")
                .unwrap_err()
                .contains("worker2")
        );
        assert!(
            check_board(board.path(), "ui/main.js")
                .unwrap_err()
                .contains("worker3")
        );
        assert!(check_board(board.path(), "src/*").is_err());
        assert!(check_board(board.path(), ", ").is_err());
        board.write("broken row");
        assert!(check_board(board.path(), "src/*").is_err());
    }

    #[test]
    fn brief_checks_task_header_and_legacy_branch_notes() {
        let file = Fixture::new("brief", "");
        let check = |text| {
            file.write(text);
            check_brief(&read(file.path()).unwrap(), "codex/job", "job")
        };
        assert!(check("TASK job BRANCH codex/job\r\nDetails").is_ok());
        assert!(check("Details\nBRANCH NOTE: use `codex/job`.").is_ok());
        assert!(check("TASK job2 BRANCH codex/job\nBRANCH NOTE: codex/job").is_err());
        assert!(check("TASK job BRANCH codex/job/other").is_err());
        assert!(check("Details\nBRANCH NOTE: codex/job2").is_err());
        let error = check("TASK job BRANCH codex/job\nSee gemini/other.").unwrap_err();
        assert!(error.contains("See gemini/other."));
    }

    #[test]
    fn result_checks_status_identity_pushed_branch_and_saved_remote() {
        let file = Fixture::new("result", "");
        let remote = Fixture::new(
            "remote",
            concat!(
                "abcdef0123456789abcdef0123456789abcdef01\trefs/heads/codex/other\n",
                "0123456789abcdef0123456789abcdef01234567\trefs/heads/codex/job\n",
            ),
        );
        let check = |text: &str, remote_path| {
            file.write(text);
            check_result(&read(file.path()).unwrap(), "codex/job", "job", remote_path)
        };
        assert!(check("\n  READY codex/job 0123456\r\n", Some(remote.path())).is_ok());
        assert!(check("READY codex/job abcdef0", Some(remote.path())).is_err());
        assert!(check("READY codex/job", Some(remote.path())).is_err());
        assert!(check("STUCK job\nReference gemini/other for context.", None).is_ok());
        assert!(check("READY job\nPushed branch origin/gemini/other.", None).is_err());
        assert!(check("READY codex/other\nTask job", None).is_err());
        assert!(check("READYISH codex/job", None).is_err());
        assert!(check("READY job2", None).is_err());
        assert!(check(&format!("READY {} job", "\u{e9}".repeat(394)), None).is_err());
        assert!(check(&format!("READY {} job", "\u{e9}".repeat(390)), None).is_ok());
    }
}
