//! Shadow-git checkpoints (`runner::checkpoint`) against real temporary git repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;

use jsonschema::{Retrieve, Uri};
use protocol::turn_event::{Envelope, Event};
use runner::checkpoint::{self, Target};
use serde_json::{Value, json};
use tempfile::TempDir;

const THREAD: &str = "th_1";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 git output")
}

/// A repository with one commit: `.gitignore` (ignores `*.log`) and tracked `a.txt`.
fn repo_with_commit() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    write(dir.path(), ".gitignore", "*.log\n");
    write(dir.path(), "a.txt", "one\n");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, body).expect("write");
}

/// Everything a checkpoint must leave alone: status, HEAD, index bytes and the stash list.
#[derive(Debug, PartialEq, Eq)]
struct UserState {
    status: String,
    head: String,
    index: Vec<u8>,
    stash: String,
}

fn user_state(dir: &Path) -> UserState {
    // `rev-parse --verify` fails in a repository without commits; its empty output is the state.
    let head = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .output()
        .expect("run git");
    UserState {
        status: git(dir, &["status", "--porcelain"]),
        head: String::from_utf8(head.stdout).expect("utf-8"),
        index: fs::read(dir.join(".git").join("index")).unwrap_or_default(),
        stash: git(dir, &["stash", "list"]),
    }
}

fn read(dir: &Path, rel: &str) -> String {
    fs::read_to_string(dir.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

#[test]
fn take_commits_a_chain_without_touching_user_state() {
    let dir = repo_with_commit();
    let root = dir.path();
    write(root, "a.txt", "two\n");
    write(root, "b.txt", "untracked\n");
    write(root, "ignored.log", "not snapshotted\n");

    let before = user_state(root);
    let first = checkpoint::take(root, THREAD, Some("first")).expect("take 1");
    assert_eq!(user_state(root), before, "take 1 changed user state");
    assert_eq!(first.n, 1);
    assert_eq!(first.git_ref, "refs/sm/checkpoints/th_1/1");
    assert_eq!(
        git(root, &["rev-parse", "refs/sm/checkpoints/th_1/1"]).trim(),
        first.commit
    );
    // .gitignore, a.txt and b.txt; ignored.log is excluded.
    assert_eq!(first.files_changed, 3);
    let listed = git(root, &["ls-tree", "-r", "--name-only", &first.commit]);
    assert!(
        listed.contains("b.txt") && !listed.contains("ignored.log"),
        "{listed}"
    );

    write(root, "c.txt", "new\n");
    let before = user_state(root);
    let second = checkpoint::take(root, THREAD, None).expect("take 2");
    assert_eq!(user_state(root), before, "take 2 changed user state");
    assert_eq!(second.n, 2);
    assert_eq!(second.git_ref, "refs/sm/checkpoints/th_1/2");
    assert_eq!(
        git(root, &["rev-parse", "refs/sm/checkpoints/th_1/2^"]).trim(),
        first.commit,
        "checkpoint 2 is not parented on checkpoint 1"
    );
    // Only c.txt is new since checkpoint 1 (a.txt already read "two" there).
    assert_eq!(second.files_changed, 1);
    assert_eq!(checkpoint::list(root, THREAD).expect("list").len(), 2);
}

#[test]
fn restore_brings_back_the_snapshot_and_keeps_ignored_files() {
    let dir = repo_with_commit();
    let root = dir.path();
    write(root, "tracked.txt", "tracked\n");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-q", "-m", "add tracked"]);
    write(root, "b.txt", "untracked\n");
    write(root, "ignored.log", "ignored before\n");
    checkpoint::take(root, THREAD, None).expect("take");

    write(root, "a.txt", "changed\n");
    fs::remove_file(root.join("tracked.txt")).expect("delete tracked");
    fs::remove_file(root.join("b.txt")).expect("delete untracked");
    write(root, "new/dir/created.txt", "after\n");
    write(root, "ignored.log", "ignored after\n");

    // Raw bytes, read without running git: `git status` may refresh the index on its own.
    let index_before = fs::read(root.join(".git").join("index")).expect("read index");
    let head_before = user_state(root).head;
    checkpoint::restore(root, THREAD, 1).expect("restore");

    assert_eq!(read(root, "a.txt"), "one\n");
    assert_eq!(read(root, "tracked.txt"), "tracked\n");
    assert_eq!(read(root, "b.txt"), "untracked\n");
    assert!(
        !root.join("new/dir/created.txt").exists(),
        "created file survived"
    );
    assert!(!root.join("new").exists(), "empty directory survived");
    assert_eq!(
        read(root, "ignored.log"),
        "ignored after\n",
        "ignored file was touched"
    );
    assert_eq!(
        fs::read(root.join(".git").join("index")).expect("read index"),
        index_before,
        "restore touched the user's index"
    );
    assert_eq!(user_state(root).head, head_before);
}

#[test]
fn diff_between_checkpoints_and_against_the_working_tree() {
    let dir = repo_with_commit();
    let root = dir.path();
    checkpoint::take(root, THREAD, None).expect("take 1");
    write(root, "a.txt", "two\n");
    checkpoint::take(root, THREAD, None).expect("take 2");

    let between = checkpoint::diff(root, THREAD, 1, Target::Checkpoint(2)).expect("diff 1..2");
    assert_eq!(between.files, vec!["a.txt".to_owned()]);
    assert!(between.unified.contains("-one\n"), "{}", between.unified);
    assert!(between.unified.contains("+two\n"), "{}", between.unified);

    write(root, "new.txt", "fresh\n");
    let against_tree = checkpoint::diff(root, THREAD, 2, Target::WorkingTree).expect("diff 2..wt");
    assert_eq!(against_tree.files, vec!["new.txt".to_owned()]);
    assert!(
        against_tree.unified.contains("+++ b/new.txt"),
        "{}",
        against_tree.unified
    );
    assert!(
        against_tree.unified.contains("+fresh\n"),
        "{}",
        against_tree.unified
    );
}

#[test]
fn works_in_a_repository_without_commits() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    write(root, "a.txt", "first\n");

    let before = user_state(root);
    let snapshot = checkpoint::take(root, THREAD, None).expect("take in unborn repo");
    assert_eq!(user_state(root), before, "take changed user state");
    assert_eq!(snapshot.files_changed, 1);

    write(root, "a.txt", "second\n");
    checkpoint::restore(root, THREAD, 1).expect("restore in unborn repo");
    assert_eq!(read(root, "a.txt"), "first\n");
    assert_eq!(user_state(root).head, "", "HEAD was created");
}

#[test]
fn thread_ids_encode_to_valid_reversible_ref_components() {
    let tricky = [
        "th_1", "a.b", "run-2", ".", "..", ".hidden", "a..b", "a.", "a.lock", "a.lock.b", "-x",
        "a:b", ":", "::", "a:b.c", "th:1", "x.:.y", "1:2:3", "0", "_",
    ];
    for id in tricky {
        let encoded = checkpoint::encode_thread(id);
        let refname = format!("refs/sm/checkpoints/{encoded}/1");
        git(&std::env::temp_dir(), &["check-ref-format", &refname]);
        assert_eq!(
            checkpoint::decode_thread(&encoded).expect("decode"),
            id,
            "round trip of {id:?} via {encoded:?}"
        );
    }
    // Ids that are already valid components keep their name, so existing refs still resolve.
    for id in ["th_1", "a.b", "run-2", "_", "0"] {
        assert_eq!(checkpoint::encode_thread(id), id);
    }
}

#[test]
fn every_short_id_round_trips_and_encodings_do_not_collide() {
    // Every id of 1 to 3 characters over an alphabet that covers all the special cases.
    let alphabet = ['a', '.', ':', '-', '_', '0'];
    let mut ids = std::collections::BTreeSet::new();
    for a in alphabet {
        ids.insert(a.to_string());
        for b in alphabet {
            ids.insert(format!("{a}{b}"));
            for c in alphabet {
                ids.insert(format!("{a}{b}{c}"));
            }
        }
    }
    assert_eq!(ids.len(), 6 + 36 + 216);
    let mut encodings = std::collections::BTreeSet::new();
    for id in &ids {
        let encoded = checkpoint::encode_thread(id);
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-+%.".contains(c)),
            "{encoded:?} has a character git may reject"
        );
        assert_eq!(checkpoint::decode_thread(&encoded).expect("decode"), *id);
        assert!(
            encodings.insert(encoded),
            "two ids encode to the same ref name"
        );
    }
}

#[test]
fn take_list_and_restore_accept_a_colon_in_the_thread_id() {
    let dir = repo_with_commit();
    let root = dir.path();
    let thread = "th:1";
    write(root, "a.txt", "two\n");
    let snapshot = checkpoint::take(root, thread, None).expect("take");
    assert_eq!(snapshot.git_ref, "refs/sm/checkpoints/th+1/1");
    assert_eq!(
        git(root, &["rev-parse", "refs/sm/checkpoints/th+1/1"]).trim(),
        snapshot.commit
    );
    assert_eq!(checkpoint::list(root, thread).expect("list"), vec![1]);

    write(root, "a.txt", "changed\n");
    checkpoint::restore(root, thread, 1).expect("restore");
    assert_eq!(read(root, "a.txt"), "two\n");
    let diff = checkpoint::diff(root, thread, 1, Target::WorkingTree).expect("diff");
    assert!(diff.files.is_empty(), "{:?}", diff.files);
}

#[test]
fn concurrent_takes_on_one_thread_claim_distinct_numbers() {
    let dir = repo_with_commit();
    let root = dir.path().to_path_buf();
    write(&root, "a.txt", "two\n");

    const RACERS: usize = 4;
    let barrier = Arc::new(Barrier::new(RACERS));
    let handles: Vec<_> = (0..RACERS)
        .map(|_| {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                checkpoint::take(&root, THREAD, None).map(|snapshot| snapshot.n)
            })
        })
        .collect();
    let mut numbers: Vec<u64> = handles
        .into_iter()
        .map(|handle| handle.join().expect("join").expect("take"))
        .collect();
    numbers.sort_unstable();
    assert_eq!(
        numbers,
        vec![1, 2, 3, 4],
        "takes claimed overlapping numbers"
    );

    let mut listed = checkpoint::list(&root, THREAD).expect("list");
    listed.sort_unstable();
    assert_eq!(listed, numbers);
}

#[test]
fn refuses_outside_a_git_repository() {
    let dir = TempDir::new().expect("tempdir");
    assert!(checkpoint::take(dir.path(), THREAD, None).is_err());
    assert!(checkpoint::restore(dir.path(), THREAD, 1).is_err());
    assert!(checkpoint::diff(dir.path(), THREAD, 1, Target::WorkingTree).is_err());
}

#[test]
fn checkpoint_event_validates_against_the_turn_event_schema() {
    let dir = repo_with_commit();
    let snapshot = checkpoint::take(dir.path(), THREAD, Some("turn 1")).expect("take");
    let event = checkpoint::checkpoint_event(&snapshot, "cp_1");
    let envelope = Envelope::new(
        THREAD,
        Some("tk_1".to_owned()),
        Some("run_1".to_owned()),
        "2026-10-28T00:00:00.000Z",
        Event::Checkpoint(event),
    );
    let value = serde_json::to_value(&envelope).expect("serialize");

    assert_eq!(value["event"]["type"], json!("Checkpoint"));
    assert_eq!(value["event"]["kind"], json!("shadow_git"));
    assert_eq!(
        value["event"]["git_ref"],
        json!("refs/sm/checkpoints/th_1/1")
    );
    // .gitignore and a.txt.
    assert_eq!(value["event"]["files_changed"], json!(2));
    assert_eq!(value["event"]["label"], json!("turn 1"));

    let validator = envelope_validator();
    let errors: Vec<String> = validator
        .iter_errors(&value)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "{value}\nviolates the schema: {errors:?}"
    );
    let back: Envelope = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(serde_json::to_value(&back).expect("serialize"), value);
}

// ---- schema validation (same approach as crates/protocol/tests/golden_acp_turn.rs) -------

const SCHEMA_BASE: &str = "https://sprite-machine.dev/schemas/turn-event/v1/";

struct SchemaFiles {
    dir: PathBuf,
}

impl Retrieve for SchemaFiles {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let relative = uri
            .as_str()
            .strip_prefix(SCHEMA_BASE)
            .ok_or_else(|| format!("unexpected schema reference {uri}"))?;
        let text = fs::read_to_string(self.dir.join(relative))?;
        Ok(serde_json::from_str(&text)?)
    }
}

fn envelope_validator() -> jsonschema::Validator {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol/schemas/turn-event");
    let root: Value = serde_json::from_str(
        &fs::read_to_string(dir.join("envelope.schema.json")).expect("read envelope schema"),
    )
    .expect("parse envelope schema");
    jsonschema::options()
        .with_retriever(SchemaFiles { dir })
        .build(&root)
        .expect("envelope schema compiles")
}
