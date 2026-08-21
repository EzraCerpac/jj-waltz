use jj_waltz::links;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[test]
fn apply_links_supports_large_rule_sets_and_nested_parents() {
    let env = LinkTestEnv::new();
    let mut config = String::new();

    for idx in 0..120 {
        let source = format!("mounts/group-{idx}/dataset");
        let target = format!("../repo/targets/{idx}");
        fs::create_dir_all(env.workspace_root.join(format!("targets/{idx}")))
            .expect("create target");
        config.push_str("[[link]]\n");
        config.push_str(&format!("source = \"{source}\"\n"));
        config.push_str(&format!("target = \"{target}\"\n"));
        config.push_str("required = true\n");
    }

    fs::write(env.workspace_root.join(".jwlinks.toml"), config).expect("write links config");
    let target_workspace = env.tempdir.path().join("repo.stress");
    fs::create_dir_all(&target_workspace).expect("create target workspace");

    let report =
        links::apply_workspace_links(&env.workspace_root, &target_workspace).expect("apply links");
    assert_eq!(report.linked, 120);
    assert_eq!(report.satisfied, 0);
    assert_eq!(report.skipped_missing_target, 0);

    for idx in [0, 47, 119] {
        let link = target_workspace.join(format!("mounts/group-{idx}/dataset"));
        let metadata = fs::symlink_metadata(&link).expect("link metadata");
        assert!(
            metadata.file_type().is_symlink(),
            "expected symlink for {idx}"
        );
    }
}

#[test]
fn apply_links_respects_local_override_and_keeps_optional_missing_targets_non_fatal() {
    let env = LinkTestEnv::new();
    let existing_target = env.workspace_root.join("data/shared");
    fs::create_dir_all(&existing_target).expect("create existing target");
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"artifact\"",
            "target = \"../repo/data/missing\"",
            "required = true",
            "",
            "[[link]]",
            "source = \"optional-cache\"",
            "target = \"../repo/data/does-not-exist\"",
            "required = false",
            "",
        ]
        .join("\n"),
    )
    .expect("write base config");
    fs::write(
        env.workspace_root.join(".jwlinks.local.toml"),
        [
            "[[link]]",
            "source = \"artifact\"",
            "target = \"../repo/data/shared\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write local override");

    let target_workspace = env.tempdir.path().join("repo.local");
    fs::create_dir_all(&target_workspace).expect("create target workspace");
    let report =
        links::apply_workspace_links(&env.workspace_root, &target_workspace).expect("apply links");

    assert_eq!(report.linked, 1);
    assert_eq!(report.satisfied, 0);
    assert_eq!(report.skipped_missing_target, 1);
    assert_symlink(&target_workspace.join("artifact"));
}

#[cfg(unix)]
#[test]
fn apply_links_rejects_private_path_when_optional_target_is_missing() {
    let env = LinkTestEnv::new();
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"optional-cache\"",
            "target = \"../repo/data/does-not-exist\"",
            "required = false",
            "",
        ]
        .join("\n"),
    )
    .expect("write links config");

    let target_workspace = env.tempdir.path().join("repo.local");
    fs::create_dir_all(&target_workspace).expect("create target workspace");
    fs::create_dir_all(target_workspace.join("optional-cache")).expect("create private path");

    let error = links::apply_workspace_links(&env.workspace_root, &target_workspace)
        .expect_err("private optional path must be a conflict");
    assert!(error.to_string().contains("link conflict"));
    assert!(target_workspace.join("optional-cache").is_dir());
}

#[cfg(unix)]
#[test]
fn apply_links_keeps_expected_dangling_optional_link_as_skipped() {
    let env = LinkTestEnv::new();
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"optional-cache\"",
            "target = \"../repo/data/does-not-exist\"",
            "required = false",
            "",
        ]
        .join("\n"),
    )
    .expect("write links config");

    let target_workspace = env.tempdir.path().join("repo.local");
    fs::create_dir_all(&target_workspace).expect("create target workspace");
    std::os::unix::fs::symlink(
        "../repo/data/does-not-exist",
        target_workspace.join("optional-cache"),
    )
    .expect("create expected dangling link");

    let report =
        links::apply_workspace_links(&env.workspace_root, &target_workspace).expect("inspect link");
    assert_eq!(report.linked, 0);
    assert_eq!(report.satisfied, 0);
    assert_eq!(report.skipped_missing_target, 1);
    assert_symlink(&target_workspace.join("optional-cache"));
}

#[cfg(unix)]
#[test]
fn apply_links_rejects_expected_dangling_required_link() {
    let env = LinkTestEnv::new();
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"required-cache\"",
            "target = \"../repo/data/does-not-exist\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write links config");

    let target_workspace = env.tempdir.path().join("repo.local");
    fs::create_dir_all(&target_workspace).expect("create target workspace");
    std::os::unix::fs::symlink(
        "../repo/data/does-not-exist",
        target_workspace.join("required-cache"),
    )
    .expect("create expected dangling link");

    let error = links::apply_workspace_links(&env.workspace_root, &target_workspace)
        .expect_err("required dangling link must fail");
    assert!(
        error
            .to_string()
            .contains("required link target is missing")
    );
    assert_symlink(&target_workspace.join("required-cache"));
}

#[cfg(unix)]
#[test]
fn apply_links_rejects_wrong_dangling_link_even_when_target_is_optional() {
    let env = LinkTestEnv::new();
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"optional-cache\"",
            "target = \"../repo/data/does-not-exist\"",
            "required = false",
            "",
        ]
        .join("\n"),
    )
    .expect("write links config");

    let target_workspace = env.tempdir.path().join("repo.local");
    fs::create_dir_all(&target_workspace).expect("create target workspace");
    std::os::unix::fs::symlink("../repo/private", target_workspace.join("optional-cache"))
        .expect("create wrong dangling link");

    let error = links::apply_workspace_links(&env.workspace_root, &target_workspace)
        .expect_err("wrong dangling link must be a conflict");
    assert!(error.to_string().contains("link conflict"));
}

#[test]
fn apply_links_resolves_relative_targets_from_each_receiving_workspace() {
    let env = LinkTestEnv::new();
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"cache\"",
            "target = \"shared\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write links config");

    for name in ["repo.sibling", "nested/repo.receiver"] {
        let receiver = env.tempdir.path().join(name);
        fs::create_dir_all(receiver.join("shared")).expect("create receiver target");
        let report =
            links::apply_workspace_links(&env.workspace_root, &receiver).expect("apply links");
        assert_eq!(report.linked, 1);
        assert_symlink(&receiver.join("cache"));
        assert_eq!(
            receiver
                .join("cache")
                .canonicalize()
                .expect("resolve source"),
            receiver
                .join("shared")
                .canonicalize()
                .expect("resolve target")
        );
    }
}

#[test]
fn apply_links_accepts_an_ordinary_path_that_is_the_target() {
    let env = LinkTestEnv::new();
    let target = env.workspace_root.join("data/shared");
    fs::create_dir_all(&target).expect("create target");
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"data/shared\"",
            "target = \"data/shared\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write links config");

    let report = links::apply_workspace_links(&env.workspace_root, &env.workspace_root)
        .expect("same ordinary path is satisfied");
    assert_eq!(report.linked, 0);
    assert_eq!(report.satisfied, 1);
    assert_eq!(report.skipped_missing_target, 0);
    assert!(target.is_dir());
}

#[test]
fn apply_links_rejects_invalid_source_values_and_missing_required_targets() {
    let env = LinkTestEnv::new();
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"   \"",
            "target = \"../repo/data\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write empty source config");
    let err = links::apply_workspace_links(&env.workspace_root, &env.workspace_root)
        .expect_err("must fail");
    assert!(err.to_string().contains("link source cannot be empty"));

    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"/abs/source\"",
            "target = \"../repo/data\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write absolute source config");
    let err = links::apply_workspace_links(&env.workspace_root, &env.workspace_root)
        .expect_err("must fail");
    assert!(err.to_string().contains("link source must be relative"));

    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"cache\"",
            "target = \"../repo/data/missing-required\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write missing target config");
    let err = links::apply_workspace_links(&env.workspace_root, &env.workspace_root)
        .expect_err("must fail");
    assert!(err.to_string().contains("required link target is missing"));
}

#[test]
fn apply_links_rejects_parent_traversal_in_source() {
    let env = LinkTestEnv::new();
    let target = env.workspace_root.join("target");
    fs::create_dir_all(&target).expect("create target");
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"../escaped\"",
            "target = \"target\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write traversal config");

    let err = links::apply_workspace_links(&env.workspace_root, &env.workspace_root)
        .expect_err("must fail");
    assert!(err.to_string().contains("parent traversal"));
    assert!(!env.tempdir.path().join("escaped").exists());
}

#[test]
fn apply_links_preflights_every_rule_before_mutating() {
    let env = LinkTestEnv::new();
    let valid_target = env.workspace_root.join("targets/valid");
    fs::create_dir_all(&valid_target).expect("create valid target");
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"nested/valid\"",
            "target = \"targets/valid\"",
            "required = true",
            "",
            "[[link]]",
            "source = \"conflict\"",
            "target = \"targets/valid\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write links config");
    fs::create_dir_all(env.workspace_root.join("conflict")).expect("create conflict");

    let err = links::apply_workspace_links(&env.workspace_root, &env.workspace_root)
        .expect_err("must fail");
    assert!(err.to_string().contains("link conflict"));
    assert!(!env.workspace_root.join("nested").exists());
}

#[cfg(unix)]
#[test]
fn apply_links_rejects_source_paths_through_symlinked_parents() {
    let env = LinkTestEnv::new();
    let outside = env.tempdir.path().join("outside");
    let target = env.workspace_root.join("target");
    fs::create_dir_all(&outside).expect("create outside directory");
    fs::create_dir_all(&target).expect("create target");
    std::os::unix::fs::symlink(&outside, env.workspace_root.join("escaped-parent"))
        .expect("create escaping parent symlink");
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"escaped-parent/link\"",
            "target = \"target\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write escaping source config");

    let err = links::apply_workspace_links(&env.workspace_root, &env.workspace_root)
        .expect_err("must fail");
    assert!(err.to_string().contains("parent cannot be a symlink"));
    assert!(!outside.join("link").exists());
}

#[cfg(unix)]
#[test]
fn apply_links_rejects_missing_source_below_a_nested_symlinked_parent() {
    let env = LinkTestEnv::new();
    let outside = env.tempdir.path().join("outside");
    let target = env.workspace_root.join("target");
    fs::create_dir_all(&outside).expect("create outside directory");
    fs::create_dir_all(&target).expect("create target");
    std::os::unix::fs::symlink(&outside, env.workspace_root.join("escaped-parent"))
        .expect("create escaping parent symlink");
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"escaped-parent/missing/link\"",
            "target = \"target\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write escaping source config");

    let err = links::apply_workspace_links(&env.workspace_root, &env.workspace_root)
        .expect_err("must fail");
    assert!(err.to_string().contains("parent cannot be a symlink"));
    assert!(!outside.join("missing/link").exists());
}

#[test]
fn apply_links_rejects_missing_source_below_a_non_directory_parent() {
    let env = LinkTestEnv::new();
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"file-parent/missing\"",
            "target = \"target\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write link config");
    fs::write(env.workspace_root.join("file-parent"), "not a directory")
        .expect("create non-directory parent");
    fs::create_dir_all(env.workspace_root.join("target")).expect("create target");

    let err = links::apply_workspace_links(&env.workspace_root, &env.workspace_root)
        .expect_err("must fail");
    assert!(
        err.to_string().contains("source parent is not a directory"),
        "{err:#}"
    );
}

#[test]
fn apply_links_reports_unreadable_target_metadata() {
    let env = LinkTestEnv::new();
    fs::write(
        env.workspace_root.join(".jwlinks.toml"),
        [
            "[[link]]",
            "source = \"cache\"",
            "target = \"target-parent/missing\"",
            "required = true",
            "",
        ]
        .join("\n"),
    )
    .expect("write links config");

    let receiver = env.tempdir.path().join("receiver");
    fs::create_dir_all(&receiver).expect("create receiver");
    fs::write(receiver.join("target-parent"), "not a directory").expect("create target parent");

    let err = links::apply_workspace_links(&env.workspace_root, &receiver)
        .expect_err("target metadata must be reported as unreadable");
    assert!(err.to_string().contains("link is unreadable"), "{err:#}");
    assert!(!receiver.join("cache").exists());
}

fn assert_symlink(path: &Path) {
    let metadata = fs::symlink_metadata(path).expect("metadata");
    assert!(
        metadata.file_type().is_symlink(),
        "expected symlink: {path:?}"
    );
}

struct LinkTestEnv {
    tempdir: TempDir,
    workspace_root: PathBuf,
}

impl LinkTestEnv {
    fn new() -> Self {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let workspace_root = tempdir.path().join("repo");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        Self {
            tempdir,
            workspace_root,
        }
    }
}
