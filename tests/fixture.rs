mod common;

use common::{bundle, commit_all, fixture_repo, git, head};

const FIXTURE_FILES: [&str; 7] = [
    "README.md",
    "docs/pricing.md",
    "package-lock.json",
    "scripts/seed.py",
    "src/api/handlers.go",
    "src/billing/plans.rs",
    "web/src/settings.tsx",
];

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn has_bundle_header(bytes: &[u8]) -> bool {
    bytes.starts_with(b"# v2 git bundle") || bytes.starts_with(b"# v3 git bundle")
}

#[test]
fn fixture_repo_is_one_commit_on_main_with_the_sample_files() {
    let repo = fixture_repo();
    let files = git(&repo, &["ls-files"]);
    assert_eq!(files.lines().collect::<Vec<_>>(), FIXTURE_FILES);
    assert_eq!(git(&repo, &["branch", "--show-current"]).trim(), "main");
    assert_eq!(git(&repo, &["log", "--format=%s"]).trim(), "init");
    assert!(git(&repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn fixture_repos_are_independent_copies() {
    let a = fixture_repo();
    let b = fixture_repo();
    std::fs::write(a.path().join("README.md"), "changed\n").unwrap();
    commit_all(&a, "edit");
    assert_ne!(head(&a), head(&b));
    let readme = std::fs::read_to_string(b.path().join("README.md")).unwrap();
    assert!(readme.starts_with("# Sample app"));
}

#[test]
fn head_is_a_full_sha_that_moves_with_commits() {
    let repo = fixture_repo();
    let first = head(&repo);
    assert!(is_sha(&first), "{first}");
    std::fs::write(repo.path().join("NOTES.md"), "notes\n").unwrap();
    commit_all(&repo, "add notes");
    let second = head(&repo);
    assert!(is_sha(&second), "{second}");
    assert_ne!(first, second);
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]).trim(),
        "add notes"
    );
}

#[test]
fn commit_all_commits_new_modified_and_deleted_files() {
    let repo = fixture_repo();
    std::fs::write(repo.path().join("new.txt"), "new\n").unwrap();
    std::fs::write(repo.path().join("README.md"), "edited\n").unwrap();
    std::fs::remove_file(repo.path().join("package-lock.json")).unwrap();
    commit_all(&repo, "mixed");
    let changes = git(&repo, &["show", "--name-status", "--format=", "HEAD"]);
    let mut lines: Vec<&str> = changes.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        ["A\tnew.txt", "D\tpackage-lock.json", "M\tREADME.md"]
    );
    assert!(git(&repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn bundle_of_main_is_a_git_bundle_that_clones() {
    let repo = fixture_repo();
    let bytes = bundle(&repo, &["main"]);
    assert!(
        has_bundle_header(&bytes),
        "{:?}",
        &bytes[..bytes.len().min(20)]
    );

    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("full.bundle");
    std::fs::write(&file, &bytes).unwrap();
    let clone = work.path().join("clone");
    git(
        work.path(),
        &[
            "clone",
            "-q",
            "-b",
            "main",
            file.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert_eq!(head(&clone), head(&repo));
    assert_eq!(
        git(&clone, &["ls-files"]).lines().count(),
        FIXTURE_FILES.len()
    );
}

#[test]
fn bundle_of_an_incremental_range_applies_on_top_of_the_first() {
    let repo = fixture_repo();
    let first = head(&repo);
    let full = bundle(&repo, &["main"]);

    std::fs::write(repo.path().join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
    commit_all(&repo, "add extra");
    let second = head(&repo);
    let range = format!("{first}..main");
    let incremental = bundle(&repo, &[range.as_str()]);
    assert!(has_bundle_header(&incremental));
    // An incremental bundle lists the commit it builds on as a prerequisite.
    let header = String::from_utf8_lossy(&incremental[..incremental.len().min(512)]).into_owned();
    assert!(header.contains(&format!("\n-{first}")), "{header}");

    let work = tempfile::tempdir().unwrap();
    let full_file = work.path().join("full.bundle");
    std::fs::write(&full_file, &full).unwrap();
    let clone = work.path().join("clone");
    git(
        work.path(),
        &[
            "clone",
            "-q",
            "-b",
            "main",
            full_file.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert_eq!(head(&clone), first);

    let inc_file = work.path().join("inc.bundle");
    std::fs::write(&inc_file, &incremental).unwrap();
    // The incremental bundle requires the first commit, which the clone has.
    git(&clone, &["bundle", "verify", inc_file.to_str().unwrap()]);
    git(
        &clone,
        &[
            "fetch",
            "-q",
            inc_file.to_str().unwrap(),
            "main:refs/heads/incoming",
        ],
    );
    assert_eq!(git(&clone, &["rev-parse", "incoming"]).trim(), second);
}

#[test]
fn pricing_doc_contains_neither_plan_nor_limit() {
    // The ranking tests rely on it: "plan limits" must match the code, not the pricing doc.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sample-repo/docs/pricing.md");
    let text = std::fs::read_to_string(path).unwrap().to_lowercase();
    assert!(!text.contains("plan"), "pricing.md mentions plan");
    assert!(!text.contains("limit"), "pricing.md mentions limit");
    assert!(text.contains("## free") && text.contains("## pro") && text.contains("## team"));
}
