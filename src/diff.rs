use anyhow::{Result, bail};
use git_surgeon::diff::DiffHunk;
use std::collections::HashSet;
use std::process::Command;

pub use git_surgeon::hunk_id::assign_ids;

/// Preamble lines that indicate unsupported metadata operations.
/// Note: "new file mode" and "deleted file mode" are supported (they work fine
/// with the --- /dev/null or +++ /dev/null headers we already capture).
const UNSUPPORTED_PREAMBLE_PREFIXES: &[&str] = &[
    "rename from ",
    "rename to ",
    "copy from ",
    "copy to ",
    "old mode ",
    "new mode ",
    "similarity index ",
    "dissimilarity index ",
];

/// Parse a unified diff into hunks.
///
/// Replaces `git_surgeon::diff::parse_diff`, which treats any line starting
/// with "--- "/"+++ " as a file header — even inside a hunk body, where such
/// lines are content (deleting a `-- comment` line renders as `--- comment`).
/// That swallowed the line as a bogus header, corrupting rebuilt patches.
/// This version tracks the line counts from each @@ header, so lines within
/// a hunk body are always taken as content.
pub fn parse_diff(input: &str) -> Vec<DiffHunk> {
    #[derive(Default)]
    struct Builder {
        old_file: String,
        new_file: String,
        file_header: String,
        header: Option<String>,
        lines: Vec<String>,
        unsupported: Option<String>,
    }

    impl Builder {
        fn flush(&mut self, hunks: &mut Vec<DiffHunk>) {
            if let Some(header) = self.header.take() {
                hunks.push(DiffHunk {
                    file: display_file(&self.old_file, &self.new_file),
                    old_file: self.old_file.clone(),
                    new_file: self.new_file.clone(),
                    file_header: self.file_header.clone(),
                    header,
                    lines: std::mem::take(&mut self.lines),
                    unsupported_metadata: self.unsupported.clone(),
                });
            }
        }
    }

    let mut hunks = Vec::new();
    let mut cur = Builder::default();
    // Body lines of the current @@ hunk still unaccounted for, per side.
    let mut old_left = 0usize;
    let mut new_left = 0usize;

    for line in input.lines() {
        // Inside a hunk body every line is content, even metadata lookalikes.
        if cur.header.is_some() && (old_left > 0 || new_left > 0) {
            match line.as_bytes().first() {
                Some(b'+') => new_left = new_left.saturating_sub(1),
                Some(b'-') => old_left = old_left.saturating_sub(1),
                Some(b'\\') => {} // "\ No newline at end of file"
                _ => {
                    // Context lines advance both sides.
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
            }
            cur.lines.push(line.to_string());
            continue;
        }

        if line.starts_with("diff --git") {
            cur.flush(&mut hunks);
            cur = Builder::default();
        } else if cur.unsupported.is_none()
            && let Some(prefix) = UNSUPPORTED_PREAMBLE_PREFIXES
                .iter()
                .find(|p| line.starts_with(*p))
        {
            cur.unsupported = Some(prefix.trim().to_string());
        }

        if line.starts_with("--- ") {
            cur.file_header = line.to_string();
            cur.old_file = strip_diff_prefix(line).to_string();
        } else if line.starts_with("+++ ") {
            cur.file_header.push('\n');
            cur.file_header.push_str(line);
            cur.new_file = strip_diff_prefix(line).to_string();
        } else if line.starts_with("@@ ") {
            cur.flush(&mut hunks);
            (old_left, new_left) = parse_header_counts(line);
            cur.header = Some(line.to_string());
        } else if cur.header.is_some() {
            // Counts exhausted; keep trailers like "\ No newline at end of file".
            cur.lines.push(line.to_string());
        }
    }
    cur.flush(&mut hunks);

    hunks
}

/// Parse (old_count, new_count) from "@@ -start[,count] +start[,count] @@...".
/// A missing count means 1; a malformed header yields (0, 0), which falls
/// back to prefix-based body parsing.
fn parse_header_counts(header: &str) -> (usize, usize) {
    fn count(range: &str) -> Option<usize> {
        match range.split_once(',') {
            Some((_, c)) => c.parse().ok(),
            None => Some(1),
        }
    }
    let parsed = (|| {
        let ranges = header.strip_prefix("@@ -")?.split(" @@").next()?;
        let (old, new) = ranges.split_once(" +")?;
        Some((count(old)?, count(new)?))
    })();
    parsed.unwrap_or((0, 0))
}

/// Extract a file path from a `--- a/...` or `+++ b/...` line.
fn strip_diff_prefix(line: &str) -> &str {
    line.strip_prefix("--- a/")
        .or_else(|| line.strip_prefix("+++ b/"))
        .or_else(|| line.strip_prefix("--- /"))
        .or_else(|| line.strip_prefix("+++ /"))
        .or_else(|| line.strip_prefix("+++ a/"))
        .or_else(|| line.strip_prefix("--- "))
        .or_else(|| line.strip_prefix("+++ "))
        .unwrap_or(line)
}

/// Choose the display path for a hunk. Prefer new-side, fall back to old-side
/// for deletions where new is /dev/null.
fn display_file(old: &str, new: &str) -> String {
    if new == "dev/null" || new.is_empty() {
        old.to_string()
    } else {
        new.to_string()
    }
}

/// True if a diff-side path is the /dev/null marker (file added or deleted).
/// `parse_diff` strips the leading slash, so match both spellings.
pub fn is_dev_null(path: &str) -> bool {
    path == "/dev/null" || path == "dev/null"
}

/// Get the jj workspace root directory.
pub fn get_repo_root() -> Result<std::path::PathBuf> {
    let output = Command::new("jj")
        .args(["root", "--no-pager"])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj root failed: {stderr}");
    }
    let root = String::from_utf8(output.stdout)?.trim().to_string();
    Ok(std::path::PathBuf::from(root))
}

/// Get per-line annotation (change IDs) for a file at a given revision.
/// File path must be repo-root-relative.
/// Returns one change ID per line of the file.
pub fn get_jj_annotations(revision: &str, file: &str, repo_root: &std::path::Path) -> Result<Vec<String>> {
    let output = Command::new("jj")
        .args([
            "file",
            "annotate",
            "--no-pager",
            "-r",
            revision,
            "-T",
            "commit.change_id().shortest(8) ++ \"\\n\"",
            file,
        ])
        .current_dir(repo_root)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj file annotate failed: {stderr}");
    }
    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout.lines().map(|l| l.to_string()).collect())
}

/// Get mutable ancestors and their descriptions in a single jj call.
/// Returns (set of change IDs, map of change ID → first line of description).
pub fn get_mutable_ancestors_with_descriptions(
    source_rev: &str,
) -> Result<(HashSet<String>, std::collections::HashMap<String, String>)> {
    use std::collections::HashMap;
    let revset = format!("immutable_heads()..({source_rev}-)");
    let output = Command::new("jj")
        .args([
            "log",
            "--no-pager",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            r#"change_id.shortest(8) ++ "\t" ++ description.first_line() ++ "\n""#,
        ])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj log failed: {stderr}");
    }
    let stdout = String::from_utf8(output.stdout)?;
    let mut ids = HashSet::new();
    let mut descs = HashMap::new();
    for line in stdout.lines() {
        if let Some((id, desc)) = line.split_once('\t') {
            ids.insert(id.to_string());
            descs.insert(id.to_string(), desc.to_string());
        }
    }
    Ok((ids, descs))
}

/// Get mutable ancestors that touched a specific file, ordered most-recent-first.
/// File path must be repo-root-relative.
pub fn get_ancestors_touching_file(source_rev: &str, file: &str, repo_root: &std::path::Path) -> Result<Vec<String>> {
    let revset = format!("(immutable_heads()..({source_rev}-)) & files(\"{file}\")");
    let output = Command::new("jj")
        .args([
            "log",
            "--no-pager",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            "change_id.shortest(8) ++ \"\\n\"",
        ])
        .current_dir(repo_root)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj log failed: {stderr}");
    }
    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout.lines().map(|l| l.to_string()).collect())
}

/// Get the current jj operation ID.
pub fn get_current_op_id() -> Result<String> {
    let output = Command::new("jj")
        .args([
            "op", "log", "--no-pager", "--no-graph", "--limit", "1",
            "-T", "self.id().short(16)",
        ])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj op log failed: {stderr}");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Run `jj diff --git` for the given revision and return the raw output.
pub fn get_jj_diff(revision: &Option<String>, debug: bool) -> Result<String> {
    let mut cmd = Command::new("jj");
    cmd.args(["diff", "--git", "--no-pager"]);
    if let Some(rev) = revision {
        cmd.args(["-r", rev]);
    }
    if debug {
        eprintln!("debug: running jj diff --git --no-pager{}", revision.as_ref().map(|r| format!(" -r {r}")).unwrap_or_default());
    }
    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if debug {
            eprintln!("debug: jj diff failed with stderr: {stderr}");
        }
        bail!("jj diff failed: {stderr}");
    }
    if debug {
        eprintln!("debug: jj diff returned {} bytes", output.stdout.len());
    }
    Ok(preserve_crlf(&String::from_utf8(output.stdout)?))
}

/// Run `jj diff --git --from FROM --to TO` and return the raw output.
pub fn get_jj_diff_from_to(from: &str, to: &str, debug: bool) -> Result<String> {
    if debug {
        eprintln!("debug: running jj diff --git --no-pager --from {from} --to {to}");
    }
    let output = Command::new("jj")
        .args(["diff", "--git", "--no-pager", "--from", from, "--to", to])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if debug {
            eprintln!("debug: jj diff failed with stderr: {stderr}");
        }
        bail!("jj diff failed: {stderr}");
    }
    if debug {
        eprintln!("debug: jj diff returned {} bytes", output.stdout.len());
    }
    Ok(preserve_crlf(&String::from_utf8(output.stdout)?))
}

/// Protect the '\r' of CRLF content lines from `parse_diff`.
///
/// Content lines from CRLF files end with "\r\n" in the diff — the '\r' is
/// part of the content. `parse_diff` splits with `str::lines()`, which strips
/// "\r\n" as a unit, so patches rebuilt from the parsed hunks would silently
/// lose the '\r' and fail to apply. Double the '\r' so one survives.
fn preserve_crlf(raw: &str) -> String {
    raw.replace("\r\n", "\r\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserve_crlf_keeps_cr_through_parse() {
        let raw = "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n line1\r\n-line2\r\n+line2 changed\r\n";
        let hunks = parse_diff(&preserve_crlf(raw));
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0].lines,
            vec![" line1\r", "-line2\r", "+line2 changed\r"]
        );
    }

    #[test]
    fn preserve_crlf_leaves_lf_content_alone() {
        let raw = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-old\n+new\n";
        assert_eq!(preserve_crlf(raw), raw);
    }

    #[test]
    fn parse_diff_keeps_dash_dash_deletions_as_content() {
        // Deleting a `-- comment` line renders as `--- comment`, which must
        // parse as hunk content, not as an old-file header.
        let raw = "\
diff --git a/f.lua b/f.lua
--- a/f.lua
+++ b/f.lua
@@ -1,3 +1,3 @@
 -- header stays
--- old comment
+-- new comment
";
        let hunks = parse_diff(raw);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file, "f.lua");
        assert_eq!(hunks[0].file_header, "--- a/f.lua\n+++ b/f.lua");
        assert_eq!(
            hunks[0].lines,
            vec![" -- header stays", "--- old comment", "+-- new comment"]
        );
    }

    #[test]
    fn parse_diff_body_lookalikes_do_not_leak_into_next_hunk() {
        // A body containing `--- x`, `+++ x`, and a context `@@ x` line must
        // not disturb the following hunk's file header or boundaries.
        let raw = "\
diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -1,2 +1,2 @@
--- minus minus line
+++ plus plus line
 @@ context at-signs
@@ -10,1 +10,1 @@
-second hunk old
+second hunk new
";
        let hunks = parse_diff(raw);
        assert_eq!(hunks.len(), 2);
        assert_eq!(
            hunks[0].lines,
            vec![
                "--- minus minus line",
                "+++ plus plus line",
                " @@ context at-signs"
            ]
        );
        assert_eq!(hunks[1].file_header, "--- a/f.txt\n+++ b/f.txt");
        assert_eq!(hunks[1].header, "@@ -10,1 +10,1 @@");
        assert_eq!(hunks[1].lines, vec!["-second hunk old", "+second hunk new"]);
    }

    #[test]
    fn parse_diff_multiple_files() {
        let raw = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,1 +1,1 @@
-a old
+a new
diff --git a/b.txt b/b.txt
--- a/b.txt
+++ b/b.txt
@@ -1,1 +1,2 @@
 b
+b more
";
        let hunks = parse_diff(raw);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].file, "a.txt");
        assert_eq!(hunks[1].file, "b.txt");
        assert_eq!(hunks[1].lines, vec![" b", "+b more"]);
    }

    #[test]
    fn parse_diff_new_and_deleted_files() {
        let raw = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,1 @@
+hello
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
\\ No newline at end of file
";
        let hunks = parse_diff(raw);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_file, "dev/null");
        assert_eq!(hunks[0].file, "new.txt");
        assert_eq!(hunks[1].new_file, "dev/null");
        assert_eq!(hunks[1].file, "gone.txt");
        assert_eq!(hunks[1].lines, vec!["-bye", "\\ No newline at end of file"]);
    }

    #[test]
    fn parse_diff_flags_unsupported_metadata() {
        let raw = "\
diff --git a/old.txt b/renamed.txt
similarity index 90%
rename from old.txt
rename to renamed.txt
--- a/old.txt
+++ b/renamed.txt
@@ -1,1 +1,1 @@
-x
+y
";
        let hunks = parse_diff(raw);
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0].unsupported_metadata.as_deref(),
            Some("similarity index")
        );
    }
}
