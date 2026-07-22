//! All git access lives here. Every function takes the open `Repository` (or a
//! `&mut` one for stash ops) and returns *owned* data — never a live git2 handle,
//! because those borrow the repo and can't be stored across frames.

use std::error::Error;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use git2::{
    BranchType, DiffFormat, DiffOptions, Oid, Repository, Signature, Status, StatusOptions,
};

/// Shorthand: any git error becomes a boxed error the UI turns into a popup.
pub type Res<T> = Result<T, Box<dyn Error>>;

/// The commit/stash signature, with a friendly hint when the identity is unset
/// (git2, like `git commit`, refuses without both user.name and user.email).
fn signature(repo: &Repository) -> Res<Signature<'static>> {
    repo.signature().map_err(|_| {
        "git identity not set — run:\n  git config --global user.name  \"Your Name\"\n  git config --global user.email \"you@example.com\""
            .into()
    })
}

// ---------------------------------------------------------------------------
// Owned display structs
// ---------------------------------------------------------------------------

/// A single commit, reduced to just the fields we want to show.
pub struct CommitInfo {
    pub short_id: String,
    pub summary: String,
    pub author: String,
    /// Pre-rendered ASCII graph prefix (the `● │` rails) for this row.
    pub graph: String,
    /// Full oid, kept so we can diff the commit on demand.
    pub oid: Oid,
    /// Refs pointing at this commit (e.g. "main", "origin/main"), so you can
    /// see where local branches sit relative to their remotes in the log.
    pub refs: Vec<RefLabel>,
}

/// A ref badge shown next to a commit.
#[derive(Clone)]
pub struct RefLabel {
    pub name: String,
    pub remote: bool,
}

/// The current branch's standing relative to its upstream (origin), shown in
/// the log header so local-vs-origin is always visible.
pub struct HeadInfo {
    pub branch: String,
    /// The upstream this branch tracks, e.g. "origin/main".
    pub upstream: Option<String>,
    /// Commits we have that the upstream doesn't (local-only work).
    pub ahead: usize,
    /// Commits the upstream has that we don't.
    pub behind: usize,
}

/// Which side of the index a change sits on.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Section {
    /// In the index — will be included in the next commit.
    Staged,
    /// Modified in the working tree but not staged.
    Unstaged,
    /// Not tracked by git at all.
    Untracked,
}

/// One row in the working-tree panel. A file that is both staged and further
/// modified produces two entries (one Staged, one Unstaged) — exactly what
/// `git status` shows — so it's never ambiguous what will be committed.
pub struct StatusEntry {
    pub path: String,
    pub section: Section,
    /// Deleted in the working tree — staging means removing it from the index.
    pub deleted: bool,
    /// Two-letter short code shown to the user (`A`, `M`, `??`, `D`, ...).
    pub code: String,
}

impl StatusEntry {
    pub fn staged(&self) -> bool {
        self.section == Section::Staged
    }
}

// ---------------------------------------------------------------------------
// Loaders
// ---------------------------------------------------------------------------

/// Walk history into memory. Returns an empty list on an unborn HEAD.
pub fn load_commits(repo: &Repository) -> Res<Vec<CommitInfo>> {
    let mut revwalk = repo.revwalk()?;
    // An empty repo has no HEAD to push; that's not an error, just no commits.
    if revwalk.push_head().is_err() {
        return Ok(Vec::new());
    }
    revwalk.set_sorting(git2::Sort::TIME)?; // newest first

    // First pass: display fields plus the parent links the graph needs.
    let mut display = Vec::new();
    let mut topology: Vec<(Oid, Vec<Oid>)> = Vec::new();

    for oid in revwalk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        display.push((
            oid.to_string()[..7].to_string(),
            commit
                .summary()
                .ok()
                .flatten()
                .unwrap_or("<no summary>")
                .to_string(),
            commit.author().name().unwrap_or("<unknown>").to_string(),
            oid,
        ));
        topology.push((oid, commit.parent_ids().collect()));
    }

    let ref_map = ref_labels(repo);
    let graphs = build_graph(&topology);
    Ok(display
        .into_iter()
        .zip(graphs)
        .map(|((short_id, summary, author, oid), graph)| CommitInfo {
            short_id,
            summary,
            author,
            graph,
            oid,
            refs: ref_map.get(&oid).cloned().unwrap_or_default(),
        })
        .collect())
}

/// Map each commit oid to the branch/remote refs that point at it.
fn ref_labels(repo: &Repository) -> std::collections::HashMap<Oid, Vec<RefLabel>> {
    let mut map: std::collections::HashMap<Oid, Vec<RefLabel>> = std::collections::HashMap::new();
    let Ok(refs) = repo.references() else {
        return map;
    };
    for r in refs.flatten() {
        let is_branch = r.is_branch();
        let is_remote = r.is_remote();
        if !is_branch && !is_remote {
            continue;
        }
        // Resolve symbolic refs (e.g. origin/HEAD) to a concrete oid.
        let Some(oid) = r.resolve().ok().and_then(|rr| rr.target()) else {
            continue;
        };
        let Ok(name) = r.shorthand().map(str::to_string).map_err(|_| ()) else {
            continue;
        };
        // Skip the noisy origin/HEAD pointer.
        if name.ends_with("HEAD") {
            continue;
        }
        map.entry(oid).or_default().push(RefLabel {
            name,
            remote: is_remote,
        });
    }
    map
}

/// Current branch + ahead/behind vs its upstream. `None` on an empty repo.
pub fn head_info(repo: &Repository) -> Option<HeadInfo> {
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return Some(HeadInfo {
            branch: "(detached HEAD)".to_string(),
            upstream: None,
            ahead: 0,
            behind: 0,
        });
    }
    let name = head.shorthand().ok()?.to_string();
    let branch = repo.find_branch(&name, BranchType::Local).ok()?;
    let local_oid = branch.get().target();
    let (upstream, ahead, behind) = match branch.upstream() {
        Ok(up) => {
            let up_name = up.name().ok().flatten().map(str::to_string);
            let (a, b) = match (local_oid, up.get().target()) {
                (Some(l), Some(u)) => repo.graph_ahead_behind(l, u).unwrap_or((0, 0)),
                _ => (0, 0),
            };
            (up_name, a, b)
        }
        Err(_) => (None, 0, 0),
    };
    Some(HeadInfo {
        branch: name,
        upstream,
        ahead,
        behind,
    })
}

/// Build the working-tree list, grouped Staged → Unstaged → Untracked. A file
/// with both index and working-tree changes appears once in each relevant group.
pub fn load_status(repo: &Repository) -> Res<Vec<StatusEntry>> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);

    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();

    for entry in repo.statuses(Some(&mut opts))?.iter() {
        let s = entry.status();
        let path = entry.path().unwrap_or("<non-utf8>").to_string();

        // Index side → staged.
        if let Some(code) = index_code(s) {
            staged.push(StatusEntry {
                path: path.clone(),
                section: Section::Staged,
                deleted: false,
                code,
            });
        }
        // Working-tree side → unstaged or untracked.
        if s.contains(Status::WT_NEW) {
            untracked.push(StatusEntry {
                path,
                section: Section::Untracked,
                deleted: false,
                code: "??".to_string(),
            });
        } else if let Some(code) = worktree_code(s) {
            let deleted = s.contains(Status::WT_DELETED);
            unstaged.push(StatusEntry {
                path,
                section: Section::Unstaged,
                deleted,
                code,
            });
        }
    }

    staged.extend(unstaged);
    staged.extend(untracked);
    Ok(staged)
}

/// One-letter code for the index (staged) side, or `None` if unchanged there.
fn index_code(s: Status) -> Option<String> {
    let c = if s.contains(Status::INDEX_NEW) {
        'A'
    } else if s.contains(Status::INDEX_MODIFIED) {
        'M'
    } else if s.contains(Status::INDEX_DELETED) {
        'D'
    } else if s.contains(Status::INDEX_RENAMED) {
        'R'
    } else if s.contains(Status::INDEX_TYPECHANGE) {
        'T'
    } else {
        return None;
    };
    Some(c.to_string())
}

/// One-letter code for the working-tree side, or `None` if unchanged there.
fn worktree_code(s: Status) -> Option<String> {
    let c = if s.contains(Status::WT_MODIFIED) {
        'M'
    } else if s.contains(Status::WT_DELETED) {
        'D'
    } else if s.contains(Status::WT_RENAMED) {
        'R'
    } else if s.contains(Status::WT_TYPECHANGE) {
        'T'
    } else {
        return None;
    };
    Some(c.to_string())
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

/// The repo's index, soft-reloaded from disk. The CLI ops (`git apply
/// --cached`, `git add -N`, pull) rewrite the index file behind libgit2's
/// back, and git2 caches the in-memory index per Repository — without the
/// reload, a later stage/commit would work from (and write back) stale state.
fn fresh_index(repo: &Repository) -> Res<git2::Index> {
    let mut index = repo.index()?;
    index.read(false)?;
    Ok(index)
}

/// Stage one path. Deletions are staged as removals; everything else as an add.
pub fn stage(repo: &Repository, path: &str, deleted: bool) -> Res<()> {
    let mut index = fresh_index(repo)?;
    if deleted {
        index.remove_path(Path::new(path))?;
    } else {
        index.add_path(Path::new(path))?;
    }
    index.write()?;
    Ok(())
}

/// Unstage one path by resetting it to HEAD. Falls back to removing it from the
/// index when HEAD is unborn (the very first commit hasn't been made yet).
pub fn unstage(repo: &Repository, path: &str) -> Res<()> {
    // Refresh the shared index object first — reset_default writes it back,
    // and doing that from a stale snapshot would clobber CLI-made changes.
    let mut index = fresh_index(repo)?;
    match repo.head() {
        Ok(head) => {
            let obj = head.peel(git2::ObjectType::Commit)?;
            repo.reset_default(Some(&obj), [path])?;
        }
        Err(_) => {
            index.remove_path(Path::new(path))?;
            index.write()?;
        }
    }
    Ok(())
}

/// Create a commit from the current index. Handles the unborn-HEAD (first commit) case.
pub fn commit(repo: &Repository, message: &str) -> Res<Oid> {
    let sig = signature(repo)?;
    let mut index = fresh_index(repo)?;
    let tree_oid = index.write_tree()?;
    let tree = repo.find_tree(tree_oid)?;

    // Parent is the current HEAD commit, unless HEAD is unborn (no commits yet).
    let parent = match repo.head() {
        Ok(head) => Some(head.peel_to_commit()?),
        Err(_) => None,
    };
    let parents: Vec<&git2::Commit> = parent.iter().collect();

    let oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)?;
    Ok(oid)
}

// ---------------------------------------------------------------------------
// Diffs
// ---------------------------------------------------------------------------

/// What a rendered diff line is — drives both coloring and partial-patch building.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    /// `diff --git`, `index`, `---`, `+++`, mode lines.
    File,
    /// `@@ -a,b +c,d @@ …`
    Hunk,
    Add,
    Del,
    Ctx,
    /// `\ No newline at end of file`, binary notices, placeholder text.
    Meta,
    /// Commit-message lines in the header of a commit diff.
    Msg,
}

/// One display line of a diff. `text` is what the UI shows; `raw` is the exact
/// patch bytes (marker excluded for Add/Del/Ctx, original line ending kept) so
/// a partial patch can be rebuilt byte-for-byte from highlighted lines.
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
    raw: String,
}

impl DiffLine {
    /// A display-only line that can never contribute to a patch.
    pub fn info(text: impl Into<String>) -> DiffLine {
        DiffLine {
            kind: LineKind::Meta,
            text: text.into(),
            raw: String::new(),
        }
    }
}

/// Diff a commit against its first parent (or against the empty tree for a
/// root commit), preceded by a `git show`-style header: oid, author, date,
/// and the full (multi-line) commit message.
pub fn diff_commit(repo: &Repository, oid: Oid) -> Res<Vec<DiffLine>> {
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;
    let parent_tree = match commit.parent(0) {
        Ok(parent) => Some(parent.tree()?),
        Err(_) => None,
    };
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
    let patch = collect_diff(&diff)?;

    let author = commit.author();
    let mut out = vec![
        DiffLine::info(format!("commit {oid}")),
        DiffLine::info(format!(
            "Author: {} <{}>",
            author.name().unwrap_or("<unknown>"),
            author.email().unwrap_or("")
        )),
        DiffLine::info(format!("Date:   {}", format_time(&author.when()))),
        DiffLine::info(""),
    ];
    let message = commit.message().unwrap_or("").trim_end();
    for l in message.lines() {
        out.push(DiffLine {
            kind: LineKind::Msg,
            text: format!("    {l}"),
            raw: String::new(),
        });
    }
    if !message.is_empty() {
        out.push(DiffLine::info(""));
    }
    out.extend(patch);
    Ok(out)
}

/// Format a git timestamp as `YYYY-MM-DD HH:MM ±HHMM` in the author's own
/// timezone (like `git show`, minus the weekday).
fn format_time(t: &git2::Time) -> String {
    let local = t.seconds() + i64::from(t.offset_minutes()) * 60;
    let days = local.div_euclid(86_400);
    let sod = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let off = t.offset_minutes();
    let sign = if off < 0 { '-' } else { '+' };
    let off = off.abs();
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} {sign}{:02}{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        off / 60,
        off % 60
    )
}

/// Days-since-epoch → (year, month, day). Howard Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Diff a single working-tree file: staged (index vs HEAD) or unstaged
/// (workdir vs index). Untracked files render as an all-added new-file diff.
pub fn diff_file(repo: &Repository, path: &str, staged: bool, untracked: bool) -> Res<Vec<DiffLine>> {
    let mut opts = DiffOptions::new();
    opts.pathspec(path);
    // Pass the index explicitly, soft-reloaded — partial staging modifies it
    // via the git CLI, and the cached in-memory copy would show a stale diff.
    let index = fresh_index(repo)?;
    let diff = if staged {
        let head_tree = match repo.head() {
            Ok(head) => Some(head.peel_to_tree()?),
            Err(_) => None,
        };
        repo.diff_tree_to_index(head_tree.as_ref(), Some(&index), Some(&mut opts))?
    } else {
        if untracked {
            opts.include_untracked(true)
                .recurse_untracked_dirs(true)
                .show_untracked_content(true);
        }
        repo.diff_index_to_workdir(Some(&index), Some(&mut opts))?
    };
    collect_diff(&diff)
}

/// Flatten a git2 `Diff` into structured display lines.
fn collect_diff(diff: &git2::Diff) -> Res<Vec<DiffLine>> {
    let mut out: Vec<DiffLine> = Vec::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        let raw = String::from_utf8_lossy(line.content()).into_owned();
        match line.origin() {
            // The file header arrives as one multi-line chunk.
            'F' => {
                for piece in raw.split_inclusive('\n') {
                    out.push(DiffLine {
                        kind: LineKind::File,
                        text: trim_eol(piece),
                        raw: piece.to_string(),
                    });
                }
            }
            'H' => out.push(DiffLine { kind: LineKind::Hunk, text: trim_eol(&raw), raw }),
            '+' => out.push(DiffLine { kind: LineKind::Add, text: trim_eol(&raw), raw }),
            '-' => out.push(DiffLine { kind: LineKind::Del, text: trim_eol(&raw), raw }),
            ' ' => out.push(DiffLine { kind: LineKind::Ctx, text: trim_eol(&raw), raw }),
            // '=' '<' '>' are the "\ No newline at end of file" markers; 'B' is
            // a binary notice. Raw bytes stay verbatim so patches stay exact.
            origin => {
                let text = if origin == 'B' {
                    trim_eol(&raw)
                } else {
                    "\\ No newline at end of file".to_string()
                };
                out.push(DiffLine { kind: LineKind::Meta, text, raw });
            }
        }
        true
    })?;
    if out.is_empty() {
        out.push(DiffLine::info("(no changes)"));
    }
    Ok(out)
}

fn trim_eol(s: &str) -> String {
    s.trim_end_matches('\n').trim_end_matches('\r').to_string()
}

// ---------------------------------------------------------------------------
// Partial (line / hunk) staging
// ---------------------------------------------------------------------------

/// Total number of +/− lines in a diff.
pub fn change_count(lines: &[DiffLine]) -> usize {
    lines
        .iter()
        .filter(|l| matches!(l.kind, LineKind::Add | LineKind::Del))
        .count()
}

/// "new" / "deleted" when the diff creates or deletes the file outright — those
/// can only be staged/unstaged whole, because a partial patch would need
/// context lines against /dev/null.
pub fn whole_file_kind(lines: &[DiffLine]) -> Option<&'static str> {
    lines
        .iter()
        .filter(|l| l.kind == LineKind::File)
        .find_map(|l| {
            if l.text.starts_with("new file mode") {
                Some("new")
            } else if l.text.starts_with("deleted file mode") {
                Some("deleted")
            } else {
                None
            }
        })
}

/// The display-line span (header..last body line) of the hunk containing
/// `cursor`, or None when the cursor sits outside any hunk.
pub fn hunk_at(lines: &[DiffLine], cursor: usize) -> Option<(usize, usize)> {
    if cursor >= lines.len() {
        return None;
    }
    let mut start = cursor;
    loop {
        match lines[start].kind {
            LineKind::Hunk => break,
            LineKind::File => return None,
            _ if start == 0 => return None,
            _ => start -= 1,
        }
    }
    let mut end = cursor;
    while end + 1 < lines.len() && !matches!(lines[end + 1].kind, LineKind::Hunk | LineKind::File) {
        end += 1;
    }
    Some((start, end))
}

/// Build a unified diff containing only the +/− lines inside `sel` (inclusive
/// display-line indices into a *single-file* diff).
///
/// `for_unstage` flips how unselected changes are neutralized:
///   - staging (apply forward onto the index): unselected `+` lines don't exist
///     in the index yet → dropped; unselected `-` lines are still in the index
///     → kept as context.
///   - unstaging (apply --reverse onto the index): unselected `+` lines are in
///     the index → context; unselected `-` lines aren't → dropped.
///
/// Hunk counts are recomputed; the start position of the side git anchors on
/// (old for forward, new for reverse) is unchanged by construction, so the
/// original values are kept. Returns the patch and how many change lines it
/// kept, or None when the selection holds no changes.
pub fn partial_patch(lines: &[DiffLine], sel: (usize, usize), for_unstage: bool) -> Option<(String, usize)> {
    let (lo, hi) = (sel.0.min(sel.1), sel.0.max(sel.1));
    let selected = |i: usize| i >= lo && i <= hi;

    let mut patch = String::new();
    let mut kept_total = 0usize;

    // File header block: everything before the first hunk.
    let mut i = 0;
    while i < lines.len() && lines[i].kind != LineKind::Hunk {
        if lines[i].kind == LineKind::File {
            patch.push_str(&lines[i].raw);
        }
        i += 1;
    }

    while i < lines.len() {
        let header = &lines[i];
        i += 1;

        let mut body = String::new();
        let (mut old_count, mut new_count) = (0u64, 0u64);
        let mut kept = 0usize;
        // Whether the previous body line made it into the patch — a following
        // "\ No newline" marker shares its fate.
        let mut last_kept = true;

        while i < lines.len() && !matches!(lines[i].kind, LineKind::Hunk | LineKind::File) {
            let l = &lines[i];
            match l.kind {
                LineKind::Ctx => {
                    body.push(' ');
                    body.push_str(&l.raw);
                    old_count += 1;
                    new_count += 1;
                    last_kept = true;
                }
                LineKind::Add if selected(i) => {
                    body.push('+');
                    body.push_str(&l.raw);
                    new_count += 1;
                    kept += 1;
                    last_kept = true;
                }
                LineKind::Add if for_unstage => {
                    body.push(' ');
                    body.push_str(&l.raw);
                    old_count += 1;
                    new_count += 1;
                    last_kept = true;
                }
                LineKind::Add => last_kept = false,
                LineKind::Del if selected(i) => {
                    body.push('-');
                    body.push_str(&l.raw);
                    old_count += 1;
                    kept += 1;
                    last_kept = true;
                }
                LineKind::Del if for_unstage => last_kept = false,
                LineKind::Del => {
                    body.push(' ');
                    body.push_str(&l.raw);
                    old_count += 1;
                    new_count += 1;
                    last_kept = true;
                }
                LineKind::Meta if last_kept => body.push_str(&l.raw),
                _ => {}
            }
            i += 1;
        }

        if kept > 0 {
            let (old_start, new_start, ctx) = parse_hunk_header(&header.text)?;
            patch.push_str(&format!(
                "@@ -{old_start},{old_count} +{new_start},{new_count} @@{ctx}\n"
            ));
            patch.push_str(&body);
            kept_total += kept;
        }
    }

    (kept_total > 0).then_some((patch, kept_total))
}

/// Pull the start numbers and trailing context out of `@@ -a[,b] +c[,d] @@ ctx`.
fn parse_hunk_header(text: &str) -> Option<(u64, u64, String)> {
    let after = text.strip_prefix("@@ ")?;
    let end = after.find(" @@")?;
    let (nums, tail) = after.split_at(end);
    let ctx = tail[3..].to_string(); // whatever followed " @@", leading space kept

    let (mut old_start, mut new_start) = (0u64, 0u64);
    for tok in nums.split_whitespace() {
        let (sign, rest) = tok.split_at(1);
        let start: u64 = rest.split(',').next()?.parse().ok()?;
        match sign {
            "-" => old_start = start,
            "+" => new_start = start,
            _ => return None,
        }
    }
    Some((old_start, new_start, ctx))
}

// ---------------------------------------------------------------------------
// Git CLI ops — shelling out reuses the user's credentials/config, and `git
// apply` is the battle-tested way to move selected lines in and out of the index.
// ---------------------------------------------------------------------------

fn workdir(repo: &Repository) -> Res<&Path> {
    repo.workdir()
        .ok_or_else(|| "bare repository — no working tree".into())
}

pub fn push(repo: &Repository) -> Res<String> {
    run_git(repo, &["push"])
}

pub fn pull(repo: &Repository) -> Res<String> {
    run_git(repo, &["pull"])
}

pub fn fetch(repo: &Repository) -> Res<String> {
    run_git(repo, &["fetch", "--all"])
}

/// `git add --intent-to-add` — gives an untracked file an index entry so a
/// partial patch can then be applied against it.
pub fn intent_to_add(repo: &Repository, path: &str) -> Res<()> {
    let out = Command::new("git")
        .current_dir(workdir(repo)?)
        .args(["add", "--intent-to-add", "--", path])
        .output()?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned().into());
    }
    Ok(())
}

/// Apply a constructed patch to the index only. `reverse = true` unstages.
pub fn apply_cached(repo: &Repository, patch: &str, reverse: bool) -> Res<()> {
    let mut cmd = Command::new("git");
    cmd.current_dir(workdir(repo)?)
        .args(["apply", "--cached", "--whitespace=nowarn"]);
    if reverse {
        cmd.arg("--reverse");
    }
    cmd.arg("-");
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(patch.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(format!(
            "git apply failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

fn run_git(repo: &Repository, args: &[&str]) -> Res<String> {
    let output = Command::new("git")
        .current_dir(workdir(repo)?)
        .args(args)
        .output()?;
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if text.trim().is_empty() {
        text = if output.status.success() {
            format!("git {} — done", args.join(" "))
        } else {
            format!("git {} failed (exit {:?})", args.join(" "), output.status.code())
        };
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// Graph layout (moved verbatim from the original main.rs)
// ---------------------------------------------------------------------------

/// Turn a newest-first list of (commit, parents) into one graph-prefix string
/// per commit.
fn build_graph(commits: &[(Oid, Vec<Oid>)]) -> Vec<String> {
    // `lanes[i]` = the commit OID that column `i` is currently waiting to draw,
    // or `None` if that column is free.
    let mut lanes: Vec<Option<Oid>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());

    for (oid, parents) in commits {
        // 1. Find this commit's lane, or open a new one (a branch tip).
        let my_lane = match lanes.iter().position(|l| *l == Some(*oid)) {
            Some(i) => i,
            None => allocate_lane(&mut lanes, *oid),
        };

        // 2. Other lanes waiting for this same commit converge here — close them.
        for (j, lane) in lanes.iter_mut().enumerate() {
            if j != my_lane && *lane == Some(*oid) {
                *lane = None;
            }
        }

        // 3. Draw the row: node in my lane, a rail for every other active lane.
        let mut row = String::new();
        for (j, lane) in lanes.iter().enumerate() {
            match (j == my_lane, lane.is_some()) {
                (true, _) => row.push('●'),
                (false, true) => row.push('│'),
                (false, false) => row.push(' '),
            }
            row.push(' ');
        }
        rows.push(row);

        // 4. Route parents into lanes for the rows below.
        match parents.split_first() {
            Some((first, rest)) => {
                lanes[my_lane] = Some(*first); // first parent continues my lane
                for p in rest {
                    // Extra parents = a merge. Give each its own lane unless it
                    // already has one (two branches sharing an ancestor).
                    if !lanes.contains(&Some(*p)) {
                        allocate_lane(&mut lanes, *p);
                    }
                }
            }
            None => lanes[my_lane] = None, // root commit: lane ends here
        }
    }

    rows
}

/// Find a free lane (or append one) and reserve it for `oid`. Returns its index.
fn allocate_lane(lanes: &mut Vec<Option<Oid>>, oid: Oid) -> usize {
    match lanes.iter().position(|l| l.is_none()) {
        Some(i) => {
            lanes[i] = Some(oid);
            i
        }
        None => {
            lanes.push(Some(oid));
            lanes.len() - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A throwaway repo in a unique temp dir with a local identity configured.
    fn temp_repo() -> (Repository, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "git-viz-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let repo = Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test User").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        (repo, dir)
    }

    fn write(dir: &PathBuf, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    /// The blob content currently staged for `path`.
    fn index_text(repo: &Repository, path: &str) -> String {
        let index = fresh_index(repo).unwrap();
        let entry = index.get_path(Path::new(path), 0).expect("index entry");
        let blob = repo.find_blob(entry.id).unwrap();
        String::from_utf8_lossy(blob.content()).into_owned()
    }

    /// Display index of the first line matching kind + text.
    fn find_line(lines: &[DiffLine], kind: LineKind, text: &str) -> usize {
        lines
            .iter()
            .position(|l| l.kind == kind && l.text == text)
            .unwrap_or_else(|| panic!("no {kind:?} line {text:?}"))
    }

    #[test]
    fn stage_commit_diff_unstage_flow() {
        let (repo, dir) = temp_repo();

        // Empty repo: no commits.
        assert!(load_commits(&repo).unwrap().is_empty());

        // Untracked file shows as "??".
        write(&dir, "a.txt", "hello\n");
        let status = load_status(&repo).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].code, "??");
        assert!(!status[0].staged());

        // Stage it → becomes staged; then commit (first commit, unborn HEAD).
        stage(&repo, "a.txt", false).unwrap();
        assert!(load_status(&repo).unwrap()[0].staged());
        let first = commit(&repo, "initial").unwrap();
        assert_eq!(load_commits(&repo).unwrap().len(), 1);
        assert!(load_status(&repo).unwrap().is_empty());

        // head_info reports the current branch (no upstream in a bare temp repo).
        let head = head_info(&repo).expect("head");
        assert!(!head.branch.is_empty());
        assert_eq!((head.ahead, head.behind), (0, 0));

        // Second commit, then diff it against its parent.
        write(&dir, "b.txt", "world\n");
        stage(&repo, "b.txt", false).unwrap();
        let second = commit(&repo, "add b").unwrap();
        assert_ne!(first, second);
        let diff = diff_commit(&repo, second).unwrap();
        assert!(diff.iter().any(|l| l.kind == LineKind::File && l.text.contains("b.txt")));
        assert!(diff.iter().any(|l| l.kind == LineKind::Add && l.text.contains("world")));

        // Staging then unstaging round-trips between the Staged and Unstaged groups.
        write(&dir, "a.txt", "changed\n");
        stage(&repo, "a.txt", false).unwrap();
        assert!(load_status(&repo).unwrap()[0].staged());
        unstage(&repo, "a.txt").unwrap();
        assert!(!load_status(&repo).unwrap()[0].staged());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_stage_and_unstage_selected_lines() {
        let (repo, dir) = temp_repo();

        // Base commit: five lines.
        write(&dir, "f.txt", "a\nb\nc\nd\ne\n");
        stage(&repo, "f.txt", false).unwrap();
        commit(&repo, "base").unwrap();

        // Working tree: b→X, d deleted, f appended.
        write(&dir, "f.txt", "a\nX\nc\ne\nf\n");
        let lines = diff_file(&repo, "f.txt", false, false).unwrap();

        // Stage only the b→X edit (the adjacent -b / +X pair).
        let del_b = find_line(&lines, LineKind::Del, "b");
        let add_x = find_line(&lines, LineKind::Add, "X");
        let (patch, kept) = partial_patch(&lines, (del_b, add_x), false).expect("patch");
        assert_eq!(kept, 2);
        apply_cached(&repo, &patch, false).unwrap();
        assert_eq!(index_text(&repo, "f.txt"), "a\nX\nc\nd\ne\n");

        // Stage only the deletion of d, from a fresh diff.
        let lines = diff_file(&repo, "f.txt", false, false).unwrap();
        let del_d = find_line(&lines, LineKind::Del, "d");
        let (patch, kept) = partial_patch(&lines, (del_d, del_d), false).expect("patch");
        assert_eq!(kept, 1);
        apply_cached(&repo, &patch, false).unwrap();
        assert_eq!(index_text(&repo, "f.txt"), "a\nX\nc\ne\n");

        // Stage the rest via hunk-at-cursor: everything left is "+f".
        let lines = diff_file(&repo, "f.txt", false, false).unwrap();
        let add_f = find_line(&lines, LineKind::Add, "f");
        let hunk = hunk_at(&lines, add_f).expect("hunk");
        let (patch, _) = partial_patch(&lines, hunk, false).expect("patch");
        apply_cached(&repo, &patch, false).unwrap();
        assert_eq!(index_text(&repo, "f.txt"), "a\nX\nc\ne\nf\n");

        // Now unstage just the +X line from the staged diff. The b-deletion
        // stays staged, so the index ends up with neither b nor X.
        let staged = diff_file(&repo, "f.txt", true, false).unwrap();
        let add_x = find_line(&staged, LineKind::Add, "X");
        let (patch, kept) = partial_patch(&staged, (add_x, add_x), true).expect("patch");
        assert_eq!(kept, 1);
        apply_cached(&repo, &patch, true).unwrap();
        assert_eq!(index_text(&repo, "f.txt"), "a\nc\ne\nf\n");

        // A selection with no +/- lines yields no patch.
        let lines = diff_file(&repo, "f.txt", false, false).unwrap();
        let ctx = lines
            .iter()
            .position(|l| l.kind == LineKind::Ctx)
            .expect("context line");
        assert!(partial_patch(&lines, (ctx, ctx), false).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_stage_untracked_via_intent_to_add() {
        let (repo, dir) = temp_repo();

        // Need one commit so HEAD exists (not strictly required, but realistic).
        write(&dir, "base.txt", "base\n");
        stage(&repo, "base.txt", false).unwrap();
        commit(&repo, "base").unwrap();

        write(&dir, "u.txt", "one\ntwo\nthree\n");
        let lines = diff_file(&repo, "u.txt", false, true).unwrap();
        assert_eq!(whole_file_kind(&lines), Some("new"));
        assert_eq!(change_count(&lines), 3);

        // Stage only the first line of the untracked file.
        let one = find_line(&lines, LineKind::Add, "one");
        let (patch, kept) = partial_patch(&lines, (one, one), false).expect("patch");
        assert_eq!(kept, 1);
        intent_to_add(&repo, "u.txt").unwrap();
        apply_cached(&repo, &patch, false).unwrap();
        assert_eq!(index_text(&repo, "u.txt"), "one\n");

        // The worktree still has all three lines; the rest shows as unstaged.
        let rest = diff_file(&repo, "u.txt", false, false).unwrap();
        assert_eq!(change_count(&rest), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hunk_at_finds_enclosing_hunk_only() {
        let (repo, dir) = temp_repo();
        write(&dir, "f.txt", "a\nb\n");
        stage(&repo, "f.txt", false).unwrap();
        commit(&repo, "base").unwrap();
        write(&dir, "f.txt", "a\nB\n");

        let lines = diff_file(&repo, "f.txt", false, false).unwrap();
        let hdr = lines
            .iter()
            .position(|l| l.kind == LineKind::Hunk)
            .expect("hunk header");

        // File-header lines are outside any hunk.
        assert!(hunk_at(&lines, 0).is_none());
        // The header itself and every body line map to the same span.
        let span = hunk_at(&lines, hdr).expect("span");
        assert_eq!(span.0, hdr);
        assert_eq!(span.1, lines.len() - 1);
        assert_eq!(hunk_at(&lines, lines.len() - 1), Some(span));

        let _ = fs::remove_dir_all(&dir);
    }
}
