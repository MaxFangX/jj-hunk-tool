use std::io::Write as _;
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use console::Style;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;

use git_surgeon::diff::DiffHunk;

const PATCH_ENV_VAR: &str = "JJ_HUNK_TOOL_PATCH";
const IN_PLACE_ENV_VAR: &str = "JJ_HUNK_TOOL_IN_PLACE";

/// How the `_jj-tool` handler transforms the `$right` directory.
#[derive(Clone, Copy)]
enum ApplyMode {
    /// Reset $right to $left, then apply the patch (split, squash, restore,
    /// absorb).
    Reset,
    /// Apply the patch to $right as-is, without resetting (diffedit). Leaves
    /// changes a text patch can't represent (binary files, renames, mode
    /// changes) untouched instead of silently deleting them.
    InPlace,
}

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Display a hunk with syntax highlighting, diff colors, and absolute line numbers.
/// Returns the formatted string (no ANSI codes if `color` is false).
fn format_hunk_display(hunk: &DiffHunk, color: bool) -> String {
    let mut out = String::new();

    // Parse old/new start lines from @@ header
    let (old_start, new_start) = parse_header_starts(&hunk.header).unwrap_or((1, 1));

    // Set up syntax highlighting
    let ss = &*SYNTAX_SET;
    let ts = &*THEME_SET;
    let syntax = ss
        .find_syntax_by_extension(
            Path::new(&hunk.file)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or(""),
        )
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let theme = &ts.themes["base16-ocean.dark"];
    let mut highlighter = HighlightLines::new(syntax, theme);

    let add_style = Style::new().green();
    let del_style = Style::new().red();
    let ctx_style = Style::new().dim();
    let lineno_style = Style::new().dim();

    let mut old_line = old_start;
    let mut new_line = new_start;

    // Compute max line number width for alignment
    let max_line = old_start
        .max(new_start)
        + hunk.lines.len();
    let width = max_line.to_string().len();

    for line in &hunk.lines {
        let (prefix, content, lineno) = if let Some(rest) = line.strip_prefix('+') {
            let ln = new_line;
            new_line += 1;
            ("+", rest, ln)
        } else if let Some(rest) = line.strip_prefix('-') {
            let ln = old_line;
            old_line += 1;
            ("-", rest, ln)
        } else if let Some(rest) = line.strip_prefix(' ') {
            let ln = old_line;
            old_line += 1;
            new_line += 1;
            (" ", rest, ln)
        } else {
            // Shouldn't happen, but handle gracefully
            let ln = old_line;
            old_line += 1;
            new_line += 1;
            (" ", line.as_str(), ln)
        };

        if color {
            // Syntax-highlight the content
            let content_with_nl = format!("{content}\n");
            let highlighted = if prefix != "-" {
                // Syntax highlight additions and context
                highlighter
                    .highlight_line(&content_with_nl, &ss)
                    .map(|ranges| {
                        let mut s = as_24_bit_terminal_escaped(&ranges, false);
                        // Strip trailing newline from highlighted output
                        if s.ends_with('\n') {
                            s.pop();
                        }
                        // Reset at end
                        s.push_str("\x1b[0m");
                        s
                    })
                    .unwrap_or_else(|_| content.to_string())
            } else {
                // For deleted lines, just use the raw content (red coloring applied to whole line)
                content.to_string()
            };

            let formatted_lineno = lineno_style.apply_to(format!("{lineno:>width$}"));
            let formatted_line = match prefix {
                "+" => format!(
                    "{formatted_lineno} {} {highlighted}",
                    add_style.apply_to("+")
                ),
                "-" => format!(
                    "{formatted_lineno} {} {}",
                    del_style.apply_to("-"),
                    del_style.apply_to(content)
                ),
                _ => format!(
                    "{formatted_lineno} {} {highlighted}",
                    ctx_style.apply_to(" ")
                ),
            };
            out.push_str(&formatted_line);
        } else {
            // Plain text: absolute line numbers, no color
            out.push_str(&format!("{lineno:>width$}:{prefix}{content}"));
        }
        out.push('\n');
    }

    out
}

/// Parse the old and new start lines from a @@ header.
/// "@@ -old_start,count +new_start,count @@..." → (old_start, new_start)
fn parse_header_starts(header: &str) -> Option<(usize, usize)> {
    let ranges = header.trim().strip_prefix("@@ -")?.split(" @@").next()?;
    let (old, new) = ranges.split_once(" +")?;
    let start = |range: &str| range.split(',').next()?.parse::<usize>().ok();
    Some((start(old)?, start(new)?))
}

/// A hunk spec: (hunk_id, hunk, line_ranges).
pub type HunkSpec<'a> = (&'a str, &'a DiffHunk, Vec<(usize, usize)>);

/// Build a combined patch from selected hunks with optional line ranges.
/// With `reverse`, the patch *undoes* the selected changes when applied
/// forward to the state that contains them.
pub fn build_combined_patch(specs: &[HunkSpec<'_>], reverse: bool) -> Result<String> {
    let mut combined = String::new();
    for (id, hunk, ranges) in specs {
        git_surgeon::diff::check_supported(hunk, id)?;
        let mut patched = if !ranges.is_empty() {
            git_surgeon::patch::slice_hunk_multi(hunk, ranges, reverse)?
        } else {
            (*hunk).clone()
        };
        if reverse {
            patched = reverse_hunk(&patched);
        }
        combined.push_str(&git_surgeon::patch::build_patch(&patched));
    }
    Ok(combined)
}

/// Flip a hunk so that applying it forward undoes the original change.
fn reverse_hunk(hunk: &DiffHunk) -> DiffHunk {
    let lines = hunk
        .lines
        .iter()
        .map(|line| {
            if let Some(rest) = line.strip_prefix('+') {
                format!("-{rest}")
            } else if let Some(rest) = line.strip_prefix('-') {
                format!("+{rest}")
            } else {
                line.clone()
            }
        })
        .collect();

    let old_side = if crate::diff::is_dev_null(&hunk.new_file) {
        "--- /dev/null".to_string()
    } else {
        format!("--- a/{}", hunk.new_file)
    };
    let new_side = if crate::diff::is_dev_null(&hunk.old_file) {
        "+++ /dev/null".to_string()
    } else {
        format!("+++ b/{}", hunk.old_file)
    };

    DiffHunk {
        file: hunk.file.clone(),
        old_file: hunk.new_file.clone(),
        new_file: hunk.old_file.clone(),
        file_header: format!("{old_side}\n{new_side}"),
        header: reverse_header(&hunk.header),
        lines,
        unsupported_metadata: hunk.unsupported_metadata.clone(),
    }
}

/// Swap the ranges in a @@ header: "@@ -a,b +c,d @@ ctx" → "@@ -c,d +a,b @@ ctx".
fn reverse_header(header: &str) -> String {
    let parts = header
        .strip_prefix("@@ -")
        .and_then(|rest| rest.split_once(" @@"))
        .and_then(|(ranges, tail)| {
            ranges
                .split_once(" +")
                .map(|(old, new)| (old, new, tail))
        });
    match parts {
        Some((old, new, tail)) => format!("@@ -{new} +{old} @@{tail}"),
        None => header.to_string(),
    }
}

/// Run a jj command with our tool configured via inline --config flags.
fn run_jj_with_tool(
    jj_args: &[&str],
    patch_content: &str,
    mode: ApplyMode,
    debug: bool,
) -> Result<()> {
    let exe = std::env::current_exe().context("finding own executable")?;

    let mut patch_file = tempfile::NamedTempFile::new().context("creating temp patch file")?;
    patch_file
        .write_all(patch_content.as_bytes())
        .context("writing patch")?;

    let exe_str = exe.display().to_string();
    let config_program = format!("merge-tools.jj-hunk-tool.program={exe_str:?}");
    let config_edit_args =
        r#"merge-tools.jj-hunk-tool.edit-args=["_jj-tool", "$left", "$right"]"#;

    let mut cmd = Command::new("jj");
    cmd.args(jj_args);
    cmd.args(["--config", r#"ui.editor="true""#]);
    cmd.args(["--config", &config_program]);
    cmd.args(["--config", config_edit_args]);
    cmd.args(["--tool", "jj-hunk-tool"]);
    cmd.env(PATCH_ENV_VAR, patch_file.path());
    if let ApplyMode::InPlace = mode {
        cmd.env(IN_PLACE_ENV_VAR, "1");
    }

    if debug {
        eprintln!("debug: running jj {}", jj_args.join(" "));
        eprintln!("debug: patch content ({} bytes):\n{patch_content}", patch_content.len());
        eprintln!("debug: patch file: {}", patch_file.path().display());
    }

    let output = cmd.output().context("running jj")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        print!("{stdout}");
    }

    if !output.status.success() {
        bail!("jj command failed");
    }

    Ok(())
}

/// Split selected hunks out of a revision using jj split --tool.
pub fn split_hunks(
    specs: &[HunkSpec<'_>],
    revision: Option<&str>,
    message: Option<&str>,
    parallel: bool,
    extra_args: &[&str],
    debug: bool,
) -> Result<()> {
    let patch_content = build_combined_patch(specs, false)?;
    if patch_content.is_empty() {
        bail!("no hunks selected");
    }

    let mut args: Vec<&str> = vec!["split"];
    if let Some(rev) = revision {
        args.extend_from_slice(&["-r", rev]);
    }
    let msg_storage;
    if let Some(msg) = message {
        msg_storage = msg.to_string();
        args.extend_from_slice(&["-m", &msg_storage]);
    }
    if parallel {
        args.push("--parallel");
    }
    args.extend_from_slice(extra_args);

    run_jj_with_tool(&args, &patch_content, ApplyMode::Reset, debug)?;
    Ok(())
}

/// Squash selected hunks from source into destination using jj squash --tool.
pub fn squash_hunks(specs: &[HunkSpec<'_>], extra_args: &[&str], debug: bool) -> Result<()> {
    let patch_content = build_combined_patch(specs, false)?;
    if patch_content.is_empty() {
        bail!("no hunks selected");
    }
    let mut args = vec!["squash"];
    args.extend_from_slice(extra_args);
    run_jj_with_tool(&args, &patch_content, ApplyMode::Reset, debug)
}

/// Rewrite a revision in-place, keeping only the selected hunks.
///
/// Rather than rebuilding the revision from its base — which would silently
/// delete changes a text patch can't represent (binary files, renames, mode
/// changes) — this removes the *unselected* hunks from the revision's current
/// state. Unselected hunks that can't be removed (e.g. carrying a mode
/// change) fail closed with an error.
pub fn diffedit_hunks(
    identified: &[(String, &DiffHunk)],
    selected: &[HunkSpec<'_>],
    jj_extra_args: &[&str],
    debug: bool,
) -> Result<()> {
    if selected.is_empty() {
        bail!("no hunks selected");
    }
    let removal = complement_specs(identified, selected);
    let patch_content =
        build_combined_patch(&removal, true).context("cannot remove unselected hunk")?;
    if patch_content.is_empty() {
        println!("All hunks kept; revision unchanged.");
        return Ok(());
    }
    let mut args = vec!["diffedit"];
    args.extend_from_slice(jj_extra_args);
    run_jj_with_tool(&args, &patch_content, ApplyMode::InPlace, debug)
}

/// Compute the complement of a hunk selection: specs covering every change in
/// `identified` not covered by `selected`.
fn complement_specs<'a>(
    identified: &'a [(String, &'a DiffHunk)],
    selected: &[HunkSpec<'a>],
) -> Vec<HunkSpec<'a>> {
    let mut result = Vec::new();
    for (id, hunk) in identified {
        // An empty range list selects the whole hunk.
        let selected_ranges: Vec<&Vec<(usize, usize)>> = selected
            .iter()
            .filter(|(sid, _, _)| sid == id)
            .map(|(_, _, ranges)| ranges)
            .collect();
        if selected_ranges.iter().any(|ranges| ranges.is_empty()) {
            continue; // whole hunk kept
        }
        if selected_ranges.is_empty() {
            result.push((id.as_str(), *hunk, Vec::new()));
            continue;
        }

        let kept: Vec<(usize, usize)> =
            selected_ranges.into_iter().flatten().copied().collect();
        let complement = invert_ranges(&kept, hunk.lines.len());
        let has_changes = hunk.lines.iter().enumerate().any(|(i, line)| {
            (line.starts_with('+') || line.starts_with('-'))
                && complement.iter().any(|&(s, e)| (s..=e).contains(&(i + 1)))
        });
        if has_changes {
            result.push((id.as_str(), *hunk, complement));
        }
    }
    result
}

/// Invert 1-based inclusive ranges over [1, len]. Ranges may be unsorted or
/// overlapping; the result is sorted and disjoint.
fn invert_ranges(ranges: &[(usize, usize)], len: usize) -> Vec<(usize, usize)> {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();
    let mut result = Vec::new();
    let mut next = 1;
    for (start, end) in sorted {
        if start > next {
            result.push((next, start - 1));
        }
        next = next.max(end + 1);
    }
    if next <= len {
        result.push((next, len));
    }
    result
}

/// Restore (undo) selected hunks. The caller provides the jj-specific args
/// (e.g. ["--changes-in", "@"] or ["--from", "x", "--into", "y"]).
///
/// jj hands the tool $left = the destination's current state; the reversed
/// patch applied on top of it undoes the selected hunks.
pub fn restore_hunks(specs: &[HunkSpec<'_>], jj_extra_args: &[&str], debug: bool) -> Result<()> {
    let patch_content = build_combined_patch(specs, true)?;
    if patch_content.is_empty() {
        bail!("no hunks selected");
    }
    let mut args = vec!["restore"];
    args.extend_from_slice(jj_extra_args);
    run_jj_with_tool(&args, &patch_content, ApplyMode::Reset, debug)
}

/// A hunk fingerprint for stable matching across re-computations.
/// Uses file path + non-context lines (strips context which can shift).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HunkFingerprint {
    pub file: String,
    pub change_lines: Vec<String>,
}

impl HunkFingerprint {
    pub fn from_hunk(hunk: &DiffHunk) -> Self {
        let change_lines = hunk
            .lines
            .iter()
            .filter(|l| l.starts_with('+') || l.starts_with('-'))
            .cloned()
            .collect();
        HunkFingerprint {
            file: hunk.file.clone(),
            change_lines,
        }
    }
}

/// Result of routing a single hunk.
#[derive(Debug)]
pub struct HunkRouting {
    pub hunk_id: String,
    pub file: String,
    pub additions: usize,
    pub deletions: usize,
    pub target: Option<String>,
    pub candidates: Vec<String>,
    pub reason: &'static str,
}

/// Absorb hunks into ancestor commits based on annotation overlap.
pub fn absorb_hunks(
    selected: &[(&str, &DiffHunk)],
    source: &str,
    dry_run: bool,
    interactive: bool,
    debug: bool,
) -> Result<()> {
    use crate::diff;

    // 1. Get mutable ancestors and repo root in parallel
    let (ancestors_result, repo_root_result) = std::thread::scope(|s| {
        let anc = s.spawn(|| diff::get_mutable_ancestors_with_descriptions(source));
        let root = s.spawn(|| diff::get_repo_root());
        (anc.join().unwrap(), root.join().unwrap())
    });
    let (ancestors, ancestor_descs) = ancestors_result?;
    let repo_root = repo_root_result?;

    if debug {
        eprintln!("debug: mutable ancestors: {:?}", ancestors);
    }

    if ancestors.is_empty() {
        println!("Nothing to absorb: no mutable ancestors.");
        return Ok(());
    }

    // 2. Pre-fetch annotations and file-ancestor data in parallel
    let parent_rev = format!("{source}-");

    let unique_files: Vec<String> = {
        let mut files = std::collections::HashSet::new();
        for (_, hunk) in selected {
            if !crate::diff::is_dev_null(&hunk.old_file) {
                files.insert(hunk.file.clone());
            }
        }
        files.into_iter().collect()
    };

    let (annotations_cache, file_ancestors_cache) = std::thread::scope(|s| {
        let ann_handles: Vec<_> = unique_files
            .iter()
            .map(|file| {
                let pr = &parent_rev;
                let root = &repo_root;
                if debug {
                    eprintln!("debug: annotating {file} at revision {pr}");
                }
                s.spawn(move || (file.clone(), diff::get_jj_annotations(pr, file, root)))
            })
            .collect();

        let fa_handles: Vec<_> = unique_files
            .iter()
            .map(|file| {
                let root = &repo_root;
                s.spawn(move || (file.clone(), diff::get_ancestors_touching_file(source, file, root)))
            })
            .collect();

        let annotations: std::collections::HashMap<String, Vec<String>> = ann_handles
            .into_iter()
            .filter_map(|h| {
                let (f, r) = h.join().ok()?;
                match r {
                    Ok(a) => {
                        if debug {
                            eprintln!("debug: annotation for {f}: {} lines", a.len());
                            for (i, change_id) in a.iter().enumerate() {
                                eprintln!("debug:   line {}: {change_id}", i + 1);
                            }
                        }
                        Some((f, a))
                    }
                    Err(e) => {
                        eprintln!("warning: annotation failed for {f}: {e}");
                        if debug {
                            eprintln!("debug: annotation error detail for {f}: {e:?}");
                        }
                        None
                    }
                }
            })
            .collect();

        let file_ancestors: std::collections::HashMap<String, Vec<String>> = fa_handles
            .into_iter()
            .filter_map(|h| {
                let (f, r) = h.join().ok()?;
                match r {
                    Ok(a) => {
                        if debug {
                            eprintln!("debug: file ancestors for {f}: {a:?}");
                        }
                        Some((f, a))
                    }
                    Err(e) => {
                        eprintln!("warning: file ancestor lookup failed for {f}: {e}");
                        if debug {
                            eprintln!("debug: file ancestor error detail for {f}: {e:?}");
                        }
                        None
                    }
                }
            })
            .collect();

        (annotations, file_ancestors)
    });

    // 3. Route each hunk
    let mut routings: Vec<(HunkRouting, HunkFingerprint)> = Vec::new();

    for (id, hunk) in selected {
        let additions = hunk.lines.iter().filter(|l| l.starts_with('+')).count();
        let deletions = hunk.lines.iter().filter(|l| l.starts_with('-')).count();
        let fingerprint = HunkFingerprint::from_hunk(hunk);

        // New files can't be annotated
        if crate::diff::is_dev_null(&hunk.old_file) {
            routings.push((
                HunkRouting {
                    hunk_id: id.to_string(),
                    file: hunk.file.clone(),
                    additions,
                    deletions,
                    target: None,
                    candidates: vec![],
                    reason: "new file",
                },
                fingerprint,
            ));
            continue;
        }

        // Get annotations for this file (from pre-fetched cache)
        let annotations = match annotations_cache.get(&hunk.file) {
            Some(ann) => ann,
            None => {
                if debug {
                    eprintln!(
                        "debug: hunk {id} ({file}): no annotations in cache \
                         (annotation command likely failed, see warning above)",
                        file = hunk.file,
                    );
                }
                routings.push((
                    HunkRouting {
                        hunk_id: id.to_string(),
                        file: hunk.file.clone(),
                        additions,
                        deletions,
                        target: None,
                        candidates: vec![],
                        reason: "annotation failed",
                    },
                    fingerprint,
                ));
                continue;
            }
        };

        // Collect mutable ancestor change IDs from the hunk's changed lines.
        let mut ancestor_hits: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        if let Some((old_start, _)) = parse_header_starts(&hunk.header) {
            if debug {
                eprintln!(
                    "debug: hunk {id} ({file}): old range starts at line {old_start}, \
                     annotation has {} lines",
                    annotations.len(),
                    file = hunk.file,
                );
            }
            let has_deletions = hunk.lines.iter().any(|l| l.starts_with('-'));
            if has_deletions {
                let mut old_line = old_start; // 1-based
                for line in &hunk.lines {
                    if line.starts_with('-') {
                        let ann_idx = old_line.saturating_sub(1);
                        if let Some(change_id) = annotations.get(ann_idx) {
                            let is_mutable = ancestors.contains(change_id);
                            if debug {
                                eprintln!(
                                    "debug: hunk {id}: deleted line {old_line} -> \
                                     change {change_id} (mutable: {is_mutable})"
                                );
                            }
                            if is_mutable {
                                *ancestor_hits.entry(change_id.clone()).or_insert(0) += 1;
                            }
                        } else if debug {
                            eprintln!(
                                "debug: hunk {id}: deleted line {old_line} -> \
                                 annotation index {ann_idx} out of bounds \
                                 (annotations len: {})",
                                annotations.len()
                            );
                        }
                        old_line += 1;
                    } else if line.starts_with('+') {
                        // Addition: doesn't consume an old line
                    } else {
                        // Context line: consumes an old line but don't count it
                        old_line += 1;
                    }
                }
            } else if debug {
                eprintln!("debug: hunk {id}: no deletions, skipping line-level annotation");
            }
        } else if debug {
            eprintln!(
                "debug: hunk {id} ({file}): failed to parse header: {:?}",
                hunk.header,
                file = hunk.file,
            );
        }

        if debug {
            eprintln!("debug: hunk {id}: ancestor_hits = {ancestor_hits:?}");
        }

        let (target, candidates, reason) = if ancestor_hits.len() == 1 {
            let target = ancestor_hits.into_keys().next().unwrap();
            (Some(target), vec![], "matched")
        } else if ancestor_hits.is_empty() {
            // Fallback: find the most recent mutable ancestor that touched this file
            if !crate::diff::is_dev_null(&hunk.old_file) {
                match file_ancestors_cache.get(&hunk.file) {
                    Some(file_ancestors) if !file_ancestors.is_empty() => {
                        let target = file_ancestors[0].clone();
                        (Some(target), vec![], "matched (file)")
                    }
                    _ => (None, vec![], "no overlapping ancestor hunk"),
                }
            } else {
                (None, vec![], "no overlapping ancestor hunk")
            }
        } else {
            let candidates: Vec<String> = ancestor_hits.into_keys().collect();
            (None, candidates, "ambiguous")
        };

        routings.push((
            HunkRouting {
                hunk_id: id.to_string(),
                file: hunk.file.clone(),
                additions,
                deletions,
                target,
                candidates,
                reason,
            },
            fingerprint,
        ));
    }

    // 3b. Interactive review: let user accept/skip/retarget each hunk
    if interactive {
        let ancestor_list: Vec<String> = ancestors.iter().cloned().collect();
        let mut quit = false;
        let mut skip_file: Option<String> = None;
        let mut absorb_file: Option<String> = None;

        // Read single chars: use console::Term for TTY, raw bytes for piped stdin
        let term = console::Term::stdout();
        let is_tty = term.is_term();

        let read_char = |term: &console::Term, is_tty: bool| -> Result<char> {
            if is_tty {
                Ok(term.read_char()?)
            } else {
                use std::io::Read;
                let mut buf = [0u8; 1];
                let n = std::io::stdin().lock().read(&mut buf)?;
                if n == 0 {
                    bail!("unexpected end of input");
                }
                Ok(buf[0] as char)
            }
        };

        for (routing, _fp) in routings.iter_mut() {
            if quit {
                routing.target = None;
                routing.reason = "skipped (quit)";
                continue;
            }

            if let Some(ref sf) = skip_file {
                if routing.file == *sf {
                    routing.target = None;
                    routing.reason = "skipped (file)";
                    continue;
                } else {
                    skip_file = None;
                }
            }

            if let Some(ref af) = absorb_file {
                if routing.file == *af {
                    // Auto-absorb: keep existing target (if any)
                    continue;
                } else {
                    absorb_file = None;
                }
            }

            // Find the original hunk to display its content
            let hunk_opt = selected
                .iter()
                .find(|(id, _)| *id == routing.hunk_id)
                .map(|(_, h)| *h);

            // Display hunk with syntax highlighting and absolute line numbers
            let header_style = if is_tty { Style::new().bold() } else { Style::new() };
            println!(
                "\n{} {} (+{} -{})",
                header_style.apply_to(&routing.hunk_id),
                header_style.apply_to(&routing.file),
                routing.additions,
                routing.deletions,
            );
            if let Some(hunk) = hunk_opt {
                print!("{}", format_hunk_display(hunk, is_tty));
            }

            // Show current target
            let target_desc = if let Some(ref t) = routing.target {
                let desc = ancestor_descs.get(t).cloned().unwrap_or_default();
                if desc.is_empty() {
                    format!("Target: {t}")
                } else {
                    format!("Target: {t} ({desc})")
                }
            } else if routing.reason == "ambiguous" {
                let descs: Vec<String> = routing
                    .candidates
                    .iter()
                    .map(|c| {
                        let desc = ancestor_descs.get(c).cloned().unwrap_or_default();
                        if desc.is_empty() {
                            c.clone()
                        } else {
                            format!("{c} ({desc})")
                        }
                    })
                    .collect();
                format!("Ambiguous: {}", descs.join(", "))
            } else {
                format!("Unmatched: {}", routing.reason)
            };
            println!("{target_desc}");

            // Prompt loop — single keypress, no Enter needed
            loop {
                print!("[a]bsorb / [A]bsorb file / [s]kip / [S]kip file / [t]arget / [q]uit: ");
                std::io::Write::flush(&mut std::io::stdout())?;
                let ch = match read_char(&term, is_tty) {
                    Ok(c) => c,
                    Err(_) => {
                        // EOF — treat as quit
                        quit = true;
                        routing.target = None;
                        routing.reason = "skipped (quit)";
                        break;
                    }
                };
                if is_tty {
                    println!(); // newline after the keypress echo
                }

                match ch {
                    'a' => {
                        if routing.target.is_none() {
                            println!("No target set. Use [t] to pick a target first.");
                            continue;
                        }
                        break;
                    }
                    'A' => {
                        if routing.target.is_none() {
                            println!("No target set. Use [t] to pick a target first.");
                            continue;
                        }
                        absorb_file = Some(routing.file.clone());
                        break;
                    }
                    's' => {
                        routing.target = None;
                        routing.reason = "skipped";
                        break;
                    }
                    'S' => {
                        skip_file = Some(routing.file.clone());
                        routing.target = None;
                        routing.reason = "skipped (file)";
                        break;
                    }
                    't' | 'T' => {
                        // Show numbered list of ancestors
                        println!("Select target:");
                        for (i, cid) in ancestor_list.iter().enumerate() {
                            let desc = ancestor_descs.get(cid).cloned().unwrap_or_default();
                            if desc.is_empty() {
                                println!("  {}: {cid}", i + 1);
                            } else {
                                println!("  {}: {cid} ({desc})", i + 1);
                            }
                        }
                        print!("Enter number: ");
                        std::io::Write::flush(&mut std::io::stdout())?;
                        // Target selection still uses line input for the number
                        let mut num_input = String::new();
                        std::io::stdin().read_line(&mut num_input)?;
                        if let Ok(n) = num_input.trim().parse::<usize>() {
                            if n >= 1 && n <= ancestor_list.len() {
                                routing.target = Some(ancestor_list[n - 1].clone());
                                routing.reason = "retargeted";
                                let desc = ancestor_descs.get(&ancestor_list[n - 1]).cloned().unwrap_or_default();
                                println!("→ Retargeted to {}{}", ancestor_list[n - 1],
                                    if desc.is_empty() { String::new() } else { format!(" ({desc})") });
                                break;
                            }
                        }
                        println!("Invalid selection.");
                        continue;
                    }
                    'q' | 'Q' => {
                        quit = true;
                        routing.target = None;
                        routing.reason = "skipped (quit)";
                        break;
                    }
                    '\n' | '\r' | ' ' => continue,
                    _ => {
                        println!("Unknown action. Use a/A/s/S/t/q.");
                        continue;
                    }
                }
            }
        }
    }

    // 4. Print routing plan
    let absorbed: Vec<&(HunkRouting, HunkFingerprint)> =
        routings.iter().filter(|(r, _)| r.target.is_some()).collect();
    let ambiguous: Vec<&(HunkRouting, HunkFingerprint)> = routings
        .iter()
        .filter(|(r, _)| r.reason == "ambiguous")
        .collect();
    let unmatched: Vec<&(HunkRouting, HunkFingerprint)> = routings
        .iter()
        .filter(|(r, _)| r.target.is_none() && r.reason != "ambiguous")
        .collect();

    if absorbed.is_empty() {
        println!("Nothing to absorb: no hunks matched any ancestor.");
        if !ambiguous.is_empty() {
            println!("\nAmbiguous (staying in {source}):");
            for (r, _) in &ambiguous {
                let descs: Vec<String> = r
                    .candidates
                    .iter()
                    .map(|c| {
                        let desc = ancestor_descs.get(c).cloned().unwrap_or_default();
                        if desc.is_empty() {
                            c.clone()
                        } else {
                            format!("{c} ({desc})")
                        }
                    })
                    .collect();
                println!(
                    "  {} ({} +{} -{}) — overlaps {}",
                    r.hunk_id,
                    r.file,
                    r.additions,
                    r.deletions,
                    descs.join(", ")
                );
            }
        }
        if !unmatched.is_empty() {
            println!("\nUnmatched (staying in {source}):");
            for (r, _) in &unmatched {
                println!(
                    "  {} ({} +{} -{}) — {}",
                    r.hunk_id, r.file, r.additions, r.deletions, r.reason
                );
            }
        }
        return Ok(());
    }

    let verb = if dry_run { "Would absorb" } else { "Absorbed" };
    println!("{verb} {} hunk(s):", absorbed.len());
    for (r, _) in &absorbed {
        let target = r.target.as_ref().unwrap();
        let desc = ancestor_descs.get(target).cloned().unwrap_or_default();
        let desc_part = if desc.is_empty() {
            String::new()
        } else {
            format!(" ({desc})")
        };
        println!(
            "  {} ({} +{} -{}) → {target}{desc_part}",
            r.hunk_id, r.file, r.additions, r.deletions
        );
    }
    if !ambiguous.is_empty() {
        println!("\nAmbiguous (staying in {source}):");
        for (r, _) in &ambiguous {
            let descs: Vec<String> = r
                .candidates
                .iter()
                .map(|c| {
                    let desc = ancestor_descs.get(c).cloned().unwrap_or_default();
                    if desc.is_empty() {
                        c.clone()
                    } else {
                        format!("{c} ({desc})")
                    }
                })
                .collect();
            println!(
                "  {} ({} +{} -{}) — overlaps {}",
                r.hunk_id,
                r.file,
                r.additions,
                r.deletions,
                descs.join(", ")
            );
        }
    }
    if !unmatched.is_empty() {
        println!("\nUnmatched (staying in {source}):");
        for (r, _) in &unmatched {
            println!(
                "  {} ({} +{} -{}) — {}",
                r.hunk_id, r.file, r.additions, r.deletions, r.reason
            );
        }
    }

    if dry_run {
        return Ok(());
    }

    // Record the current operation ID for undo hint
    let pre_op_id = diff::get_current_op_id()?;

    // 5. Execute: sequential squash per target, re-identifying by fingerprint
    // Group absorbed hunks by target
    let mut target_groups: std::collections::HashMap<String, Vec<HunkFingerprint>> =
        std::collections::HashMap::new();
    for (r, fp) in &routings {
        if let Some(ref target) = r.target {
            target_groups
                .entry(target.clone())
                .or_default()
                .push(fp.clone());
        }
    }

    for (target, fingerprints) in &target_groups {
        // Re-get current diff (it changes after each squash)
        let raw = diff::get_jj_diff(&Some(source.to_string()), debug)?;
        let hunks = crate::diff::parse_diff(&raw);
        let identified = crate::diff::assign_ids(&hunks);

        // Match current hunks to fingerprints
        let mut specs: Vec<HunkSpec<'_>> = Vec::new();
        for (hid, hunk) in &identified {
            let fp = HunkFingerprint::from_hunk(hunk);
            if fingerprints.contains(&fp) {
                specs.push((hid.as_str(), *hunk, vec![]));
            }
        }

        if specs.is_empty() {
            if debug {
                eprintln!("debug: no hunks matched fingerprints for target {target}, skipping");
            }
            continue;
        }

        let patch_content = build_combined_patch(&specs, false)?;
        if patch_content.is_empty() {
            continue;
        }

        if debug {
            eprintln!("debug: squashing {} hunks into {target}", specs.len());
        }

        let args: Vec<&str> = vec!["squash", "--from", source, "--into", target];
        run_jj_with_tool(&args, &patch_content, ApplyMode::Reset, debug)?;
    }

    println!("To undo, run: jj op restore {pre_op_id}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_header_starts_same() {
        assert_eq!(parse_header_starts("@@ -7,3 +7,4 @@"), Some((7, 7)));
    }

    #[test]
    fn parse_header_starts_different() {
        assert_eq!(parse_header_starts("@@ -10,5 +12,3 @@"), Some((10, 12)));
    }

    #[test]
    fn parse_header_starts_with_context_label() {
        assert_eq!(
            parse_header_starts("@@ -1,3 +1,3 @@ fn main()"),
            Some((1, 1))
        );
    }

    #[test]
    fn parse_header_starts_no_count() {
        assert_eq!(parse_header_starts("@@ -5 +5,2 @@"), Some((5, 5)));
    }

    #[test]
    fn parse_header_starts_invalid() {
        assert_eq!(parse_header_starts("not a header"), None);
    }

    #[test]
    fn format_hunk_display_absolute_lines() {
        let hunk = DiffHunk {
            file: "test.rs".into(),
            old_file: "test.rs".into(),
            new_file: "test.rs".into(),
            file_header: String::new(),
            header: "@@ -7,3 +7,3 @@".into(),
            lines: vec![
                " context".into(),
                "-old_line".into(),
                "+new_line".into(),
                " context2".into(),
            ],
            unsupported_metadata: None,
        };
        let output = format_hunk_display(&hunk, false);
        // Should contain absolute line numbers starting at 7
        assert!(output.contains(" 7:"), "should start at line 7: {output}");
        assert!(output.contains(" 8:"), "should have line 8: {output}");
        assert!(!output.contains(" 1:"), "should NOT have line 1: {output}");
    }

    fn test_hunk(lines: &[&str]) -> DiffHunk {
        DiffHunk {
            file: "a.txt".into(),
            old_file: "a.txt".into(),
            new_file: "a.txt".into(),
            file_header: "--- a/a.txt\n+++ b/a.txt".into(),
            header: "@@ -1,3 +1,3 @@".into(),
            lines: lines.iter().map(|l| l.to_string()).collect(),
            unsupported_metadata: None,
        }
    }

    #[test]
    fn reverse_hunk_swaps_lines_and_sides() {
        let hunk = test_hunk(&[" ctx", "-old", "+new"]);
        let reversed = reverse_hunk(&hunk);
        assert_eq!(reversed.lines, vec![" ctx", "+old", "-new"]);
        assert_eq!(reversed.file_header, "--- a/a.txt\n+++ b/a.txt");
    }

    #[test]
    fn reverse_hunk_creation_becomes_deletion() {
        let mut hunk = test_hunk(&["+hello"]);
        hunk.old_file = "dev/null".into();
        hunk.file_header = "--- /dev/null\n+++ b/a.txt".into();
        hunk.header = "@@ -0,0 +1,1 @@".into();

        let reversed = reverse_hunk(&hunk);
        assert_eq!(reversed.lines, vec!["-hello"]);
        assert_eq!(reversed.file_header, "--- a/a.txt\n+++ /dev/null");
        assert_eq!(reversed.header, "@@ -1,1 +0,0 @@");
    }

    #[test]
    fn reverse_header_swaps_ranges() {
        assert_eq!(reverse_header("@@ -1,3 +5,7 @@"), "@@ -5,7 +1,3 @@");
        assert_eq!(
            reverse_header("@@ -1,3 +1,4 @@ fn main()"),
            "@@ -1,4 +1,3 @@ fn main()"
        );
        assert_eq!(reverse_header("@@ -5 +5,2 @@"), "@@ -5,2 +5 @@");
    }

    #[test]
    fn invert_ranges_basic() {
        assert_eq!(invert_ranges(&[(3, 5)], 10), vec![(1, 2), (6, 10)]);
    }

    #[test]
    fn invert_ranges_full_coverage() {
        assert_eq!(invert_ranges(&[(1, 10)], 10), vec![]);
    }

    #[test]
    fn invert_ranges_empty_selection() {
        assert_eq!(invert_ranges(&[], 10), vec![(1, 10)]);
    }

    #[test]
    fn invert_ranges_unsorted_overlapping() {
        assert_eq!(
            invert_ranges(&[(6, 8), (2, 4), (3, 5)], 10),
            vec![(1, 1), (9, 10)]
        );
    }

    #[test]
    fn invert_ranges_at_bounds() {
        assert_eq!(invert_ranges(&[(1, 3), (8, 10)], 10), vec![(4, 7)]);
    }

    #[test]
    fn complement_specs_unselected_hunk_included_whole() {
        let h1 = test_hunk(&[" ctx", "-old", "+new"]);
        let h2 = test_hunk(&[" ctx", "+added"]);
        let identified = vec![("aaaaaaa".to_string(), &h1), ("bbbbbbb".to_string(), &h2)];
        let selected: Vec<HunkSpec<'_>> = vec![("aaaaaaa", &h1, vec![])];

        let complement = complement_specs(&identified, &selected);
        assert_eq!(complement.len(), 1);
        assert_eq!(complement[0].0, "bbbbbbb");
        assert!(complement[0].2.is_empty(), "whole hunk, no ranges");
    }

    #[test]
    fn complement_specs_partial_selection_inverts_ranges() {
        let h = test_hunk(&[" ctx", "+one", "+two", " ctx2"]);
        let identified = vec![("aaaaaaa".to_string(), &h)];
        let selected: Vec<HunkSpec<'_>> = vec![("aaaaaaa", &h, vec![(2, 2)])];

        let complement = complement_specs(&identified, &selected);
        assert_eq!(complement.len(), 1);
        assert_eq!(complement[0].2, vec![(1, 1), (3, 4)]);
    }

    #[test]
    fn complement_specs_complement_without_changes_skipped() {
        // Only line 2 is a change; selecting it leaves just context lines.
        let h = test_hunk(&[" ctx", "+added", " ctx2"]);
        let identified = vec![("aaaaaaa".to_string(), &h)];
        let selected: Vec<HunkSpec<'_>> = vec![("aaaaaaa", &h, vec![(2, 2)])];

        assert!(complement_specs(&identified, &selected).is_empty());
    }

    #[test]
    fn complement_specs_multiple_specs_same_hunk_merged() {
        let h = test_hunk(&["+one", "+two", "+three"]);
        let identified = vec![("aaaaaaa".to_string(), &h)];
        let selected: Vec<HunkSpec<'_>> =
            vec![("aaaaaaa", &h, vec![(1, 1)]), ("aaaaaaa", &h, vec![(3, 3)])];

        let complement = complement_specs(&identified, &selected);
        assert_eq!(complement.len(), 1);
        assert_eq!(complement[0].2, vec![(2, 2)]);
    }

    #[test]
    fn fingerprint_ignores_context() {
        let hunk1 = DiffHunk {
            file: "a.txt".into(),
            old_file: "a.txt".into(),
            new_file: "a.txt".into(),
            file_header: String::new(),
            header: String::new(),
            lines: vec![
                " context1".into(),
                "-old".into(),
                "+new".into(),
                " context2".into(),
            ],
            unsupported_metadata: None,
        };
        let hunk2 = DiffHunk {
            file: "a.txt".into(),
            old_file: "a.txt".into(),
            new_file: "a.txt".into(),
            file_header: String::new(),
            header: String::new(),
            lines: vec![
                " different_context".into(),
                "-old".into(),
                "+new".into(),
                " also_different".into(),
            ],
            unsupported_metadata: None,
        };
        assert_eq!(
            HunkFingerprint::from_hunk(&hunk1),
            HunkFingerprint::from_hunk(&hunk2),
        );
    }
}

/// JJ tool protocol handler.
///
/// JJ invokes: `jj-hunk-tool _jj-tool $left $right`
/// - `$left` = parent/base state directory (read-only)
/// - `$right` = current state directory (writable)
///
/// Algorithm:
/// 1. Read patch path from JJ_HUNK_TOOL_PATCH env var
/// 2. Reset $right to match $left (copy all files from left, remove extras),
///    unless JJ_HUNK_TOOL_IN_PLACE is set
/// 3. Apply the patch to $right
/// 4. Remove files the patch marked as deleted (patch only empties them)
pub fn jj_tool_apply(left: &str, right: &str) -> Result<()> {
    let patch_path = std::env::var(PATCH_ENV_VAR)
        .with_context(|| format!("{PATCH_ENV_VAR} environment variable not set"))?;

    let left_path = Path::new(left);
    let right_path = Path::new(right);

    // Step 1: Reset $right to $left state. In-place mode skips this so that
    // changes the patch can't represent (binary files, renames) survive.
    if std::env::var(IN_PLACE_ENV_VAR).is_err() {
        reset_dir_to(left_path, right_path)?;
    }

    // Step 2: Apply the pre-computed patch. Close stdin so patch fails
    // instead of prompting when it can't resolve a filename.
    let mut patch_cmd = Command::new("patch");
    patch_cmd.args(["-p1", "--silent"]);
    patch_cmd.arg("-i").arg(&patch_path);
    patch_cmd.current_dir(right_path);
    patch_cmd.stdin(std::process::Stdio::null());
    let status = patch_cmd.status().context("failed to run patch")?;

    if !status.success() {
        bail!("patch failed to apply (exit code: {:?})", status.code());
    }

    // Step 3: patch leaves a file emptied rather than deleted when a hunk's
    // target side is /dev/null; jj would then record an emptied file instead
    // of a deletion. Remove such files.
    let patch_text = std::fs::read_to_string(&patch_path)
        .with_context(|| format!("reading patch {patch_path}"))?;
    for hunk in git_surgeon::diff::parse_diff(&patch_text) {
        if !crate::diff::is_dev_null(&hunk.new_file) {
            continue;
        }
        let target = right_path.join(&hunk.old_file);
        if target.exists() && target.metadata()?.len() == 0 {
            std::fs::remove_file(&target)
                .with_context(|| format!("removing deleted file {}", target.display()))?;
        }
    }

    Ok(())
}

/// Reset `dst` directory to match `src` directory contents.
fn reset_dir_to(src: &Path, dst: &Path) -> Result<()> {
    remove_dir_contents(dst)?;
    copy_dir_recursive(src, dst)?;
    Ok(())
}

fn remove_dir_contents(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing dir {}", path.display()))?;
        } else {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing file {}", path.display()))?;
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("reading dir {}", src.display()))? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            std::fs::create_dir_all(&dst_path)
                .with_context(|| format!("creating dir {}", dst_path.display()))?;
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).with_context(|| {
                format!("copying {} to {}", src_path.display(), dst_path.display())
            })?;
        }
    }
    Ok(())
}
