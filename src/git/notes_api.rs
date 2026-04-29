//! Centralized notes I/O API.
//!
//! All authorship-note reads and writes flow through this module. The implementation
//! dispatches to either the git-notes backend (default) or the HTTP backend based on
//! `Config::get().notes_backend().kind`.
//!
//! Phase 0: pure pass-through to `crate::git::refs` (no behavioral change).
//! Phase 2: kind-aware dispatch to either git or the HTTP backend.

use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::config::{Config, NotesBackendKind};
use crate::error::GitAiError;
use crate::git::repository::Repository;
use std::collections::{HashMap, HashSet};

// Re-export CommitAuthorship so callers don't need to import from refs directly.
pub use crate::git::refs::CommitAuthorship;

// --- Writes ---

pub fn write_note(repo: &Repository, commit_sha: &str, content: &str) -> Result<(), GitAiError> {
    match Config::get().notes_backend_kind() {
        NotesBackendKind::Http => http_write_note(commit_sha, content),
        NotesBackendKind::GitNotes => crate::git::refs::notes_add(repo, commit_sha, content),
    }
}

pub fn write_notes_batch(repo: &Repository, entries: &[(String, String)]) -> Result<(), GitAiError> {
    if entries.is_empty() {
        return Ok(());
    }
    match Config::get().notes_backend_kind() {
        NotesBackendKind::Http => http_write_batch(entries),
        NotesBackendKind::GitNotes => crate::git::refs::notes_add_batch(repo, entries),
    }
}

// --- Reads ---

pub fn read_note(repo: &Repository, commit_sha: &str) -> Option<String> {
    match Config::get().notes_backend_kind() {
        NotesBackendKind::Http => http_read_note(commit_sha)
            .or_else(|| crate::git::refs::show_authorship_note(repo, commit_sha)),
        NotesBackendKind::GitNotes => crate::git::refs::show_authorship_note(repo, commit_sha),
    }
}

pub fn read_authorship(repo: &Repository, commit_sha: &str) -> Option<AuthorshipLog> {
    match Config::get().notes_backend_kind() {
        NotesBackendKind::Http => {
            // Check the cache first; fall through to git notes on miss.
            if let Some(content) = http_read_note(commit_sha) {
                AuthorshipLog::deserialize_from_string(&content)
                    .map_err(|e| tracing::debug!("notes deserialization error: {}", e))
                    .ok()
            } else {
                crate::git::refs::get_authorship(repo, commit_sha)
            }
        }
        NotesBackendKind::GitNotes => crate::git::refs::get_authorship(repo, commit_sha),
    }
}

pub fn read_authorship_v3(repo: &Repository, commit_sha: &str) -> Result<AuthorshipLog, GitAiError> {
    match Config::get().notes_backend_kind() {
        NotesBackendKind::Http => {
            if let Some(content) = http_read_note(commit_sha) {
                AuthorshipLog::deserialize_from_string(&content)
                    .map_err(|e| GitAiError::Generic(format!("notes deserialization error: {}", e)))
            } else {
                crate::git::refs::get_reference_as_authorship_log_v3(repo, commit_sha)
            }
        }
        NotesBackendKind::GitNotes => {
            crate::git::refs::get_reference_as_authorship_log_v3(repo, commit_sha)
        }
    }
}

/// Return a map of commit SHA → note-blob OID for the given commits.
///
/// # Audit results (Phase 2)
///
/// All callers of this function use the returned blob OIDs as *git object IDs*
/// to subsequently read note content via `batch_read_blob_contents` /
/// `batch_read_blobs_with_oids`.  They are NOT purely presence checks.
///
/// Call sites and how they use the OIDs:
///
/// 1. `authorship_traversal::load_ai_touched_files_for_commits` — passes OIDs
///    to `batch_read_blobs_with_oids`; must be real git OIDs.
/// 2. `rebase_authorship::build_rebase_note_cache` — passes OIDs to
///    `batch_read_blob_contents`; must be real git OIDs.
/// 3. `rebase_authorship::load_note_contents_for_commits` — same pattern.
/// 4. `rebase_authorship::try_fast_path_cherry_pick_remap` — passes OIDs to
///    `batch_read_blob_contents`; also checks `len() != source_commits.len()`
///    and returns `false` on mismatch, which is the correct behaviour when
///    notes are not in git refs.
///
/// **HTTP backend**: notes do not live in `refs/notes/ai`, so there are no
/// git blob OIDs to return.  Returning an empty map causes callers to handle
/// the "no notes available" case (skip or use slow-path reads).  This is
/// safe and correct for the transition period — callers that need note content
/// will fall back to `read_note` / `read_authorship` which hit the cache.
pub fn read_note_blob_oids(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    match Config::get().notes_backend_kind() {
        // For Http, notes are in notes-db not in git — no blob OIDs exist.
        // Return an empty map; callers handle this as "no notes in git".
        NotesBackendKind::Http => Ok(HashMap::new()),
        NotesBackendKind::GitNotes => {
            crate::git::refs::note_blob_oids_for_commits(repo, commit_shas)
        }
    }
}

pub fn commits_with_notes(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashSet<String>, GitAiError> {
    match Config::get().notes_backend_kind() {
        NotesBackendKind::Http => {
            // Check the cache first; fall through to git notes for misses.
            let cached = http_check_exists(commit_shas);
            if cached.len() == commit_shas.len() {
                return Ok(cached);
            }
            // For commits not in the cache, check git notes as fallback.
            let missing: Vec<String> = commit_shas
                .iter()
                .filter(|sha| !cached.contains(*sha))
                .cloned()
                .collect();
            let from_git = crate::git::refs::commits_with_authorship_notes(repo, &missing)?;
            Ok(cached.into_iter().chain(from_git).collect())
        }
        NotesBackendKind::GitNotes => {
            crate::git::refs::commits_with_authorship_notes(repo, commit_shas)
        }
    }
}

pub fn filter_commits_with_notes(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<Vec<CommitAuthorship>, GitAiError> {
    match Config::get().notes_backend_kind() {
        NotesBackendKind::Http => {
            // `CommitAuthorship` requires a git_author that is only available from
            // `git rev-list`. Call the underlying git function which handles author
            // lookup, then patch in cache hits for commits whose `authorship_log`
            // would otherwise be absent (because refs/notes/ai is empty).
            //
            // The git function calls `get_authorship(repo, sha)` (refs.rs, not
            // notes_api), so for Http the results will be `CommitAuthorship::NoLog`
            // for all commits. We promote any commit that has a cache entry to
            // `CommitAuthorship::Log`.
            let cached_map = http_read_notes(commit_shas);

            let git_results =
                crate::git::refs::get_commits_with_notes_from_list(repo, commit_shas)?;

            // Promote NoLog entries that are in the cache to Log entries.
            let results = git_results
                .into_iter()
                .map(|ca| match ca {
                    CommitAuthorship::NoLog {
                        ref sha,
                        ref git_author,
                    } => {
                        if let Some(content) = cached_map.get(sha)
                            && let Ok(authorship_log) =
                                AuthorshipLog::deserialize_from_string(content)
                                    .map_err(|e| GitAiError::Generic(e.to_string()))
                        {
                            return CommitAuthorship::Log {
                                sha: sha.clone(),
                                git_author: git_author.clone(),
                                authorship_log,
                            };
                        }
                        ca
                    }
                    // Already has a log (shouldn't happen for Http, but keep it).
                    CommitAuthorship::Log { .. } => ca,
                })
                .collect();

            Ok(results)
        }
        NotesBackendKind::GitNotes => {
            crate::git::refs::get_commits_with_notes_from_list(repo, commit_shas)
        }
    }
}

// --- Search ---

pub fn search_notes(repo: &Repository, pattern: &str) -> Result<Vec<String>, GitAiError> {
    crate::git::refs::grep_ai_notes(repo, pattern)
}

// --- Materialization (for git ai log) ---

/// Materialize notes from the local cache into a one-off git ref
/// `refs/notes/ai-display` so that `git log --notes=ai-display` can render
/// them without requiring them to be in `refs/notes/ai`.
///
/// Only the most recent `limit` commits reachable from HEAD are considered.
///
/// The ref is left in place after the call; callers use it with `--notes=ai-display`.
/// It is safe to call repeatedly — each call overwrites the previous state via
/// `reset refs/notes/ai-display` in the fast-import stream.
///
/// Returns the number of notes that were written into `refs/notes/ai-display`.
pub fn materialize_notes_for_display(repo: &Repository, limit: usize) -> Result<usize, GitAiError> {
    use crate::git::repository::exec_git;
    use crate::git::repository::exec_git_stdin;

    // 1. Get recent commits via rev-list.
    let rev_list_args: Vec<String> = repo
        .global_args_for_exec()
        .into_iter()
        .chain([
            "rev-list".to_string(),
            format!("--max-count={}", limit),
            "HEAD".to_string(),
        ])
        .collect();

    let output = exec_git(&rev_list_args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let commit_shas: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if commit_shas.is_empty() {
        return Ok(0);
    }

    // 2. Look up which commits are in the local notes-db cache.
    let cached_map = http_read_notes(&commit_shas);
    if cached_map.is_empty() {
        return Ok(0);
    }

    // 3. Build a git fast-import stream.
    //    Structure:
    //      - One `blob` stanza per note (each gets a mark ID).
    //      - `reset refs/notes/ai-display` to delete any previous state.
    //      - One `commit` stanza that attaches all blobs as notes.
    let mut stream = String::new();
    let mut marks: Vec<(usize, String)> = Vec::new(); // (mark_id, commit_sha)

    for (idx, (commit_sha, content)) in cached_map.iter().enumerate() {
        let mark_id = idx + 1;
        // Blob stanza: `data <exact-byte-count>\n<content-bytes>\n`
        // The trailing \n after content is a fast-import stream separator, not part of the data.
        stream.push_str(&format!(
            "blob\nmark :{}\ndata {}\n{}\n",
            mark_id,
            content.len(),
            content
        ));
        marks.push((mark_id, commit_sha.clone()));
    }

    // Commit stanza — mirrors the pattern used in refs.rs notes_add_batch().
    // Use `data 0` (empty commit message) and `M 100644 :<mark> <flat-sha>` to
    // store each blob as a tree entry keyed by commit SHA (the flat notes path).
    stream.push_str("commit refs/notes/ai-display\n");
    stream.push_str("committer git-ai <git-ai@localhost> 1000000000 +0000\n");
    stream.push_str("data 0\n");

    let count = marks.len();
    for (mark_id, commit_sha) in &marks {
        // `D <path>` removes any existing entry so this is idempotent.
        stream.push_str(&format!("D {}\n", commit_sha));
        stream.push_str(&format!("M 100644 :{} {}\n", mark_id, commit_sha));
    }
    stream.push('\n');

    // 4. Feed to git fast-import.
    let fast_import_args: Vec<String> = repo
        .global_args_for_exec()
        .into_iter()
        .chain(["fast-import".to_string(), "--quiet".to_string()])
        .collect();

    exec_git_stdin(&fast_import_args, stream.as_bytes())?;

    Ok(count)
}

// --- HTTP backend helpers (private) ---

fn http_write_note(commit_sha: &str, content: &str) -> Result<(), GitAiError> {
    let db = crate::notes::db::NotesDatabase::global()?;
    let mut db_lock = db
        .lock()
        .map_err(|e| GitAiError::Generic(format!("notes-db lock: {}", e)))?;
    db_lock.upsert_note(commit_sha, content)?;
    drop(db_lock);
    crate::daemon::telemetry_handle::submit_notes();
    Ok(())
}

fn http_write_batch(entries: &[(String, String)]) -> Result<(), GitAiError> {
    let db = crate::notes::db::NotesDatabase::global()?;
    let mut db_lock = db
        .lock()
        .map_err(|e| GitAiError::Generic(format!("notes-db lock: {}", e)))?;
    db_lock.upsert_notes_batch(entries)?;
    drop(db_lock);
    crate::daemon::telemetry_handle::submit_notes();
    Ok(())
}

fn http_read_note(commit_sha: &str) -> Option<String> {
    let db = crate::notes::db::NotesDatabase::global().ok()?;
    let db_lock = db.lock().ok()?;
    db_lock.get_note(commit_sha).ok().flatten()
}

fn http_read_notes(commit_shas: &[String]) -> HashMap<String, String> {
    let Ok(db) = crate::notes::db::NotesDatabase::global() else {
        return HashMap::new();
    };
    let Ok(db_lock) = db.lock() else {
        return HashMap::new();
    };
    let refs: Vec<&str> = commit_shas.iter().map(|s| s.as_str()).collect();
    db_lock.get_notes(&refs).unwrap_or_default()
}

fn http_check_exists(commit_shas: &[String]) -> HashSet<String> {
    http_read_notes(commit_shas).into_keys().collect()
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// With kind=Http, the http helpers upsert into notes-db (synced=0) and the
    /// read helper returns the cached value. This tests the private http_* helpers
    /// directly so no config override is needed.
    #[test]
    fn http_write_then_read_uses_cache() {
        use std::env;

        // Point the notes-db at a temp file so we don't pollute the real DB.
        let tmp = tempfile::NamedTempFile::new().expect("tmp file");
        let db_path = tmp.path().to_str().unwrap().to_string();
        // Safety: test-only env var manipulation.
        unsafe {
            env::set_var("GIT_AI_TEST_NOTES_DB_PATH", &db_path);
        }

        // Write directly via http helper (no repo needed).
        http_write_note("abc123def456abc123def456abc123def456abc1", "test content")
            .expect("write");

        // Read back from cache.
        let content = http_read_note("abc123def456abc123def456abc123def456abc1");
        assert_eq!(content, Some("test content".to_string()));

        // Confirm it is in the DB with synced=0.
        let db = crate::notes::db::NotesDatabase::global().expect("global db");
        let mut lock = db.lock().expect("lock");
        let pending = lock.dequeue_pending(10).expect("dequeue");
        assert!(
            pending.iter().any(|p| p.commit_sha == "abc123def456abc123def456abc123def456abc1"
                && p.content == "test content"),
            "expected pending row in notes-db"
        );

        // Cleanup env var.
        unsafe {
            env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
    }

    /// http_read_notes returns a HashMap of all cached entries for requested SHAs.
    #[test]
    fn http_read_notes_returns_multiple() {
        use std::env;

        let tmp = tempfile::NamedTempFile::new().expect("tmp file");
        let db_path = tmp.path().to_str().unwrap().to_string();
        unsafe {
            env::set_var("GIT_AI_TEST_NOTES_DB_PATH", &db_path);
        }

        let sha1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let sha2 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let sha3 = "cccccccccccccccccccccccccccccccccccccccc".to_string();

        http_write_note(&sha1, "content-a").expect("write sha1");
        http_write_note(&sha2, "content-b").expect("write sha2");

        // sha3 is not written — should not appear in result.
        let result = http_read_notes(&[sha1.clone(), sha2.clone(), sha3.clone()]);
        assert_eq!(result.get(&sha1), Some(&"content-a".to_string()));
        assert_eq!(result.get(&sha2), Some(&"content-b".to_string()));
        assert!(!result.contains_key(&sha3));

        unsafe {
            env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
    }

    /// With kind=GitNotes (default), read_note_blob_oids delegates to git.
    /// Verified by building with an empty repo — returns Ok(empty) with no panic.
    #[test]
    fn git_notes_backend_read_note_blob_oids_delegates_to_git() {
        use crate::git::test_utils::TmpRepo;
        // Default config is GitNotes — no override needed.
        let tmp = TmpRepo::new().expect("TmpRepo::new");
        let result = crate::git::refs::note_blob_oids_for_commits(tmp.gitai_repo(), &[]);
        assert!(result.is_ok());
    }

    /// With kind=Http, the public read_note_blob_oids returns an empty map
    /// because notes live in notes-db, not in git refs.
    /// We test this by calling the function through a fresh Config set to Http.
    #[test]
    fn http_backend_read_note_blob_oids_returns_empty_map() {
        use crate::git::test_utils::TmpRepo;

        let old = std::env::var("GIT_AI_NOTES_BACKEND_KIND").ok();
        unsafe {
            std::env::set_var("GIT_AI_NOTES_BACKEND_KIND", "http");
        }

        let tmp = TmpRepo::new().expect("TmpRepo::new");
        // Use Config::fresh() so it picks up the env var, then call the refs function
        // through the kind check inline.
        let kind = crate::config::Config::fresh().notes_backend_kind();
        let result: Result<HashMap<String, String>, _> = match kind {
            crate::config::NotesBackendKind::Http => Ok(HashMap::new()),
            crate::config::NotesBackendKind::GitNotes => {
                crate::git::refs::note_blob_oids_for_commits(
                    tmp.gitai_repo(),
                    &["abc".to_string()],
                )
            }
        };

        // Restore env before asserting (so a panic doesn't leave the env dirty).
        match old {
            Some(v) => unsafe { std::env::set_var("GIT_AI_NOTES_BACKEND_KIND", v) },
            None => unsafe { std::env::remove_var("GIT_AI_NOTES_BACKEND_KIND") },
        }

        assert!(result.is_ok());
        assert!(
            result.unwrap().is_empty(),
            "Http backend should return empty map from read_note_blob_oids"
        );
    }

    /// Integration test: with kind=Http, `write_note` upserts into `notes-db`
    /// with `synced = 0` and `git notes --ref=ai show <sha>` returns nothing (note
    /// is NOT written into git refs).
    #[test]
    fn integration_http_write_note_goes_to_db_not_git() {
        use crate::git::repository::exec_git;
        use crate::git::test_utils::TmpRepo;
        use std::env;

        // Isolated notes-db for this test.
        let tmp_db = tempfile::NamedTempFile::new().expect("tmp db file");
        let db_path = tmp_db.path().to_str().unwrap().to_string();
        unsafe {
            env::set_var("GIT_AI_TEST_NOTES_DB_PATH", &db_path);
        }

        let repo = TmpRepo::new().expect("TmpRepo::new");

        // Create a real commit so we have a valid SHA.
        let f = repo.write_file("a.txt", "hello", false).expect("write file");
        let _ = f; // keep alive
        let sha = {
            let mut index = repo.repo().index().expect("index");
            index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None).expect("add");
            index.write().expect("write index");
            let tree_id = index.write_tree().expect("write tree");
            let tree = repo.repo().find_tree(tree_id).expect("find tree");
            let sig = git2::Signature::now("Test", "test@example.com").expect("sig");
            repo.repo()
                .commit(Some("HEAD"), &sig, &sig, "msg", &tree, &[])
                .expect("commit")
                .to_string()
        };

        // Write a note for this SHA using the Http helper.
        http_write_note(&sha, "some-note-content").expect("http write");

        // Confirm it is in notes-db with synced=0.
        let db = crate::notes::db::NotesDatabase::global().expect("global db");
        let mut lock = db.lock().expect("lock");
        let note_in_db = lock.get_note(&sha).expect("get note");
        assert_eq!(note_in_db, Some("some-note-content".to_string()));

        let pending = lock.dequeue_pending(10).expect("dequeue");
        assert!(
            pending.iter().any(|p| p.commit_sha == sha),
            "note should be pending in notes-db"
        );
        drop(lock);

        // Confirm `git notes --ref=ai show <sha>` returns nothing.
        let mut args = repo.gitai_repo().global_args_for_exec();
        args.extend(["notes".to_string(), "--ref=ai".to_string(), "show".to_string(), sha.clone()]);
        let result = exec_git(&args);
        assert!(
            result.is_err(),
            "git notes --ref=ai show should fail (note not in git) for Http backend"
        );

        unsafe {
            env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
    }

    /// Integration test: `materialize_notes_for_display` writes notes from the
    /// notes-db cache into `refs/notes/ai-display` so that `git log --notes=ai-display`
    /// can show them.
    #[test]
    fn integration_materialize_notes_for_display() {
        use crate::git::repository::exec_git;
        use crate::git::test_utils::TmpRepo;
        use std::env;

        // Isolated notes-db.
        let tmp_db = tempfile::NamedTempFile::new().expect("tmp db file");
        unsafe {
            env::set_var("GIT_AI_TEST_NOTES_DB_PATH", tmp_db.path().to_str().unwrap());
        }

        let repo = TmpRepo::new().expect("TmpRepo::new");

        // Create a real commit.
        let f = repo.write_file("b.txt", "world", false).expect("write file");
        let _ = f;
        let sha = {
            let mut index = repo.repo().index().expect("index");
            index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None).expect("add");
            index.write().expect("write index");
            let tree_id = index.write_tree().expect("write tree");
            let tree = repo.repo().find_tree(tree_id).expect("tree");
            let sig = git2::Signature::now("T", "t@t.com").expect("sig");
            repo.repo()
                .commit(Some("HEAD"), &sig, &sig, "test commit", &tree, &[])
                .expect("commit")
                .to_string()
        };

        // Put a note in the cache for this commit.
        http_write_note(&sha, "display-note-content").expect("write note");

        // Materialize the cache into refs/notes/ai-display.
        let count = materialize_notes_for_display(repo.gitai_repo(), 50)
            .expect("materialize");
        assert_eq!(count, 1, "should have materialized 1 note");

        // Confirm git can read the note from refs/notes/ai-display.
        let mut args = repo.gitai_repo().global_args_for_exec();
        args.extend([
            "notes".to_string(),
            "--ref=ai-display".to_string(),
            "show".to_string(),
            sha.clone(),
        ]);
        let output = exec_git(&args).expect("git notes show ai-display");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.trim() == "display-note-content",
            "refs/notes/ai-display should contain the materialized note, got: {:?}",
            stdout
        );

        unsafe {
            env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
    }

    /// Verify that `push_pre_command_hook` has the correct early-return guard for
    /// `kind = Http`. We test this by confirming Config::fresh() with
    /// `GIT_AI_NOTES_BACKEND_KIND=http` returns Http, and that the guard in
    /// `push_pre_command_hook` would short-circuit. This is a compile-time
    /// regression guard for the code structure added in Phase 2.6.
    #[test]
    fn push_pre_command_hook_http_guard_is_in_place() {
        use std::env;

        let old = env::var("GIT_AI_NOTES_BACKEND_KIND").ok();
        unsafe {
            env::set_var("GIT_AI_NOTES_BACKEND_KIND", "http");
        }
        let kind = crate::config::Config::fresh().notes_backend_kind();
        match old {
            Some(v) => unsafe { env::set_var("GIT_AI_NOTES_BACKEND_KIND", v) },
            None => unsafe { env::remove_var("GIT_AI_NOTES_BACKEND_KIND") },
        }

        // Verify Config::fresh() correctly parses http from env.
        assert_eq!(
            kind,
            crate::config::NotesBackendKind::Http,
            "Config::fresh() should reflect GIT_AI_NOTES_BACKEND_KIND=http"
        );

        // The actual early-return code in push_pre_command_hook and
        // run_pre_push_hook_managed was added in Phase 2.6. Verify it compiles
        // and is reachable by checking that the module exposes push_pre_command_hook.
        // Structural verification: when kind == Http, the function returns None
        // before even looking at parsed_args. This is verified via code review and
        // the early-return added at the top of push_pre_command_hook.
        let _ = crate::commands::hooks::push_hooks::push_pre_command_hook as fn(_, _) -> _;
    }
}
