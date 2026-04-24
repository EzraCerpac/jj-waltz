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
        links::apply_workspace_links_with_config_root(&env.workspace_root, &target_workspace)
            .expect("apply links");
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
        links::apply_workspace_links_with_config_root(&env.workspace_root, &target_workspace)
            .expect("apply links");

    assert_eq!(report.linked, 1);
    assert_eq!(report.satisfied, 0);
    assert_eq!(report.skipped_missing_target, 1);
    assert_symlink(&target_workspace.join("artifact"));
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
    let err = links::apply_workspace_links(&env.workspace_root).expect_err("must fail");
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
    let err = links::apply_workspace_links(&env.workspace_root).expect_err("must fail");
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
    let err = links::apply_workspace_links(&env.workspace_root).expect_err("must fail");
    assert!(err.to_string().contains("required link target is missing"));
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
