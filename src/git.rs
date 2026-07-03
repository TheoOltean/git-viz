//! All git access lives here. Every function takes the open `Repository` (or a
//! `&mut` one for stash ops) and returns *owned* data — never a live git2 handle,
//! because those borrow the repo and can't be stored across frames.

use std::error::Error;
use std::path::Path;
use std::process::Command;

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

/// Stage one path. Deletions are staged as removals; everything else as an add.
pub fn stage(repo: &Repository, path: &str, deleted: bool) -> Res<()> {
    let mut index = repo.index()?;
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
    match repo.head() {
        Ok(head) => {
            let obj = head.peel(git2::ObjectType::Commit)?;
            repo.reset_default(Some(&obj), [path])?;
        }
        Err(_) => {
            let mut index = repo.index()?;
            index.remove_path(Path::new(path))?;
            index.write()?;
        }
    }
    Ok(())
}

/// Create a commit from the current index. Handles the unborn-HEAD (first commit) case.
pub fn commit(repo: &Repository, message: &str) -> Res<Oid> {
    let sig = signature(repo)?;
    let mut index = repo.index()?;
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

/// Diff a commit against its first parent (or against the empty tree for a root
/// commit). Returns the patch as one string per line.
pub fn diff_commit(repo: &Repository, oid: Oid) -> Res<Vec<String>> {
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;
    let parent_tree = match commit.parent(0) {
        Ok(parent) => Some(parent.tree()?),
        Err(_) => None,
    };
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
    render_diff(&diff)
}

/// Diff a single working-tree file: staged (index vs HEAD) or unstaged (workdir vs index).
pub fn diff_file(repo: &Repository, path: &str, staged: bool) -> Res<Vec<String>> {
    let mut opts = DiffOptions::new();
    opts.pathspec(path);
    let diff = if staged {
        let head_tree = match repo.head() {
            Ok(head) => Some(head.peel_to_tree()?),
            Err(_) => None,
        };
        repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts))?
    } else {
        repo.diff_index_to_workdir(None, Some(&mut opts))?
    };
    render_diff(&diff)
}

/// Flatten a git2 `Diff` into printable lines, keeping the +/- origin markers.
fn render_diff(diff: &git2::Diff) -> Res<Vec<String>> {
    let mut buf = String::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        // Context/add/remove lines carry their marker in `origin`; headers don't.
        if matches!(line.origin(), '+' | '-' | ' ') {
            buf.push(line.origin());
        }
        buf.push_str(&String::from_utf8_lossy(line.content()));
        true
    })?;
    let lines: Vec<String> = buf.lines().map(|l| l.to_string()).collect();
    if lines.is_empty() {
        Ok(vec!["(no changes)".to_string()])
    } else {
        Ok(lines)
    }
}

// ---------------------------------------------------------------------------
// Remote ops — shell out to the git CLI so we reuse the user's credentials/config.
// ---------------------------------------------------------------------------

pub fn push() -> Res<String> {
    run_git(&["push"])
}

pub fn pull() -> Res<String> {
    run_git(&["pull"])
}

pub fn fetch() -> Res<String> {
    run_git(&["fetch", "--all"])
}

fn run_git(args: &[&str]) -> Res<String> {
    let output = Command::new("git").args(args).output()?;
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
        assert!(diff.iter().any(|l| l.contains("b.txt")));
        assert!(diff.iter().any(|l| l.starts_with('+') && l.contains("world")));

        // Staging then unstaging round-trips between the Staged and Unstaged groups.
        write(&dir, "a.txt", "changed\n");
        stage(&repo, "a.txt", false).unwrap();
        assert!(load_status(&repo).unwrap()[0].staged());
        unstage(&repo, "a.txt").unwrap();
        assert!(!load_status(&repo).unwrap()[0].staged());

        let _ = fs::remove_dir_all(&dir);
    }
}
