use fs::{FakeFs, Fs, RemoveOptions};
use gpui::{BackgroundExecutor, TestAppContext};
use serde_json::json;
use std::path::{Path, PathBuf};
use util::path;

#[gpui::test]
async fn test_fake_worktree_lifecycle(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", json!({".git": {}, "file.txt": "content"}))
        .await;
    let repo = fs
        .open_repo(Path::new("/project/.git"), None)
        .expect("should open fake repo");

    // Initially only the main worktree exists
    let worktrees = repo.worktrees().await.unwrap();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(worktrees[0].path, PathBuf::from("/project"));

    fs.create_dir("/my-worktrees".as_ref()).await.unwrap();
    let worktrees_dir = Path::new("/my-worktrees");

    // Create a worktree
    let worktree_1_dir = worktrees_dir.join("feature-branch");
    repo.create_worktree(
        git::repository::CreateWorktreeTarget::NewBranch {
            branch_name: "feature-branch".to_string(),
            base_sha: Some("abc123".to_string()),
        },
        worktree_1_dir.clone(),
    )
    .await
    .unwrap();

    // List worktrees — should have main + one created
    let worktrees = repo.worktrees().await.unwrap();
    assert_eq!(worktrees.len(), 2);
    assert_eq!(worktrees[0].path, PathBuf::from("/project"));
    assert_eq!(worktrees[1].path, worktree_1_dir);
    assert_eq!(
        worktrees[1].ref_name,
        Some("refs/heads/feature-branch".into())
    );
    assert_eq!(worktrees[1].sha.as_ref(), "abc123");

    // Directory should exist in FakeFs after create
    assert!(fs.is_dir(&worktrees_dir.join("feature-branch")).await);

    // Create a second worktree (without explicit commit)
    let worktree_2_dir = worktrees_dir.join("bugfix-branch");
    repo.create_worktree(
        git::repository::CreateWorktreeTarget::NewBranch {
            branch_name: "bugfix-branch".to_string(),
            base_sha: None,
        },
        worktree_2_dir.clone(),
    )
    .await
    .unwrap();

    let worktrees = repo.worktrees().await.unwrap();
    assert_eq!(worktrees.len(), 3);
    assert!(fs.is_dir(&worktree_2_dir).await);

    // Rename the first worktree
    repo.rename_worktree(worktree_1_dir, worktrees_dir.join("renamed-branch"))
        .await
        .unwrap();

    let worktrees = repo.worktrees().await.unwrap();
    assert_eq!(worktrees.len(), 3);
    assert!(
        worktrees
            .iter()
            .any(|w| w.path == worktrees_dir.join("renamed-branch")),
    );
    assert!(
        worktrees
            .iter()
            .all(|w| w.path != worktrees_dir.join("feature-branch")),
    );

    // Directory should be moved in FakeFs after rename
    assert!(!fs.is_dir(&worktrees_dir.join("feature-branch")).await);
    assert!(fs.is_dir(&worktrees_dir.join("renamed-branch")).await);

    // Rename a nonexistent worktree should fail
    let result = repo
        .rename_worktree(PathBuf::from("/nonexistent"), PathBuf::from("/somewhere"))
        .await;
    assert!(result.is_err());

    // Remove a worktree
    repo.remove_worktree(worktrees_dir.join("renamed-branch"), false)
        .await
        .unwrap();

    let worktrees = repo.worktrees().await.unwrap();
    assert_eq!(worktrees.len(), 2);
    assert_eq!(worktrees[0].path, PathBuf::from("/project"));
    assert_eq!(worktrees[1].path, worktree_2_dir);

    // Directory should be removed from FakeFs after remove
    assert!(!fs.is_dir(&worktrees_dir.join("renamed-branch")).await);

    // Remove a nonexistent worktree should fail
    let result = repo
        .remove_worktree(PathBuf::from("/nonexistent"), false)
        .await;
    assert!(result.is_err());

    // Remove the last worktree
    repo.remove_worktree(worktree_2_dir.clone(), false)
        .await
        .unwrap();

    let worktrees = repo.worktrees().await.unwrap();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(worktrees[0].path, PathBuf::from("/project"));
    assert!(!fs.is_dir(&worktree_2_dir).await);
}

#[gpui::test]
async fn test_fake_remove_stale_worktree_matches_git_validation(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", json!({".git": {}, "file.txt": "content"}))
        .await;
    let repo = fs
        .open_repo(Path::new("/project/.git"), None)
        .expect("should open fake repo");
    let worktree_path = PathBuf::from("/worktrees/stale");
    repo.create_worktree(
        git::repository::CreateWorktreeTarget::NewBranch {
            branch_name: "stale".to_string(),
            base_sha: Some("abc123".to_string()),
        },
        worktree_path.clone(),
    )
    .await
    .unwrap();

    fs.remove_file(&worktree_path.join(".git"), RemoveOptions::default())
        .await
        .unwrap();

    for force in [false, true] {
        let error = repo
            .remove_worktree(worktree_path.clone(), force)
            .await
            .expect_err("existing checkout without .git must fail validation");
        assert!(
            error.to_string().contains(&format!(
                "cannot remove working tree: '{}' does not exist",
                worktree_path.join(".git").display()
            )),
            "unexpected error: {error:#}"
        );
    }
    assert!(fs.is_dir(&worktree_path).await);
    assert_eq!(repo.worktrees().await.unwrap().len(), 2);

    fs.remove_dir(
        &worktree_path,
        RemoveOptions {
            recursive: true,
            ignore_if_not_exists: false,
        },
    )
    .await
    .unwrap();
    repo.remove_worktree(worktree_path.clone(), true)
        .await
        .unwrap();
    assert_eq!(repo.worktrees().await.unwrap().len(), 1);

    repo.remove_worktree(PathBuf::from("/project"), true)
        .await
        .expect_err("main worktree must not be removable");
    assert!(fs.is_dir(Path::new("/project")).await);
}

#[gpui::test]
async fn test_fake_remove_can_fail_after_removing_registration(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", json!({".git": {}, "file.txt": "content"}))
        .await;
    let repo = fs
        .open_repo(Path::new("/project/.git"), None)
        .expect("should open fake repo");
    let worktree_path = PathBuf::from("/worktrees/undeletable");
    repo.create_worktree(
        git::repository::CreateWorktreeTarget::NewBranch {
            branch_name: "undeletable".to_string(),
            base_sha: Some("abc123".to_string()),
        },
        worktree_path.clone(),
    )
    .await
    .unwrap();
    fs.set_remove_dir_error(&worktree_path, "permission denied".to_string());

    let error = repo
        .remove_worktree(worktree_path.clone(), true)
        .await
        .expect_err("checkout deletion error should still be reported");
    assert!(error.to_string().contains("permission denied"));
    assert!(fs.is_dir(&worktree_path).await);
    assert!(
        repo.worktrees()
            .await
            .unwrap()
            .iter()
            .all(|worktree| worktree.path != worktree_path)
    );
}

#[gpui::test]
async fn test_fake_remove_dirty_and_locked_worktrees_is_atomic(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", json!({".git": {}, "file.txt": "content"}))
        .await;
    let repo = fs
        .open_repo(Path::new("/project/.git"), None)
        .expect("should open fake repo");
    let dirty_path = PathBuf::from("/worktrees/dirty");
    let locked_path = PathBuf::from("/worktrees/locked");
    for (path, branch_name) in [(&dirty_path, "dirty"), (&locked_path, "locked")] {
        repo.create_worktree(
            git::repository::CreateWorktreeTarget::NewBranch {
                branch_name: branch_name.to_string(),
                base_sha: Some("abc123".to_string()),
            },
            path.clone(),
        )
        .await
        .unwrap();
    }
    fs.with_git_state(Path::new("/project/.git"), false, |state| {
        state
            .worktrees_requiring_force_delete
            .insert(dirty_path.clone());
        state.locked_worktrees.insert(locked_path.clone());
    })
    .unwrap();

    assert!(
        repo.remove_worktree(dirty_path.clone(), false)
            .await
            .is_err()
    );
    assert!(fs.is_dir(&dirty_path).await);
    repo.remove_worktree(dirty_path.clone(), true)
        .await
        .unwrap();
    assert!(!fs.is_dir(&dirty_path).await);

    for force in [false, true] {
        assert!(
            repo.remove_worktree(locked_path.clone(), force)
                .await
                .is_err()
        );
        assert!(fs.is_dir(&locked_path).await);
        assert!(fs.is_file(&locked_path.join(".git")).await);
    }
    assert!(
        repo.worktrees()
            .await
            .unwrap()
            .iter()
            .any(|worktree| worktree.path == locked_path)
    );
}

#[gpui::test]
async fn test_fake_rename_worktree_validation_is_atomic(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", json!({".git": {}, "file.txt": "content"}))
        .await;
    let repo = fs
        .open_repo(Path::new("/project/.git"), None)
        .expect("should open fake repo");
    let source = PathBuf::from("/worktrees/source");
    let destination = PathBuf::from("/worktrees/destination");
    repo.create_worktree(
        git::repository::CreateWorktreeTarget::NewBranch {
            branch_name: "source".to_string(),
            base_sha: Some("abc123".to_string()),
        },
        source.clone(),
    )
    .await
    .unwrap();
    fs.create_dir(&destination).await.unwrap();

    assert!(
        repo.rename_worktree(source.clone(), destination.clone())
            .await
            .is_err()
    );
    assert!(fs.is_dir(&source).await);
    assert!(fs.is_file(&source.join(".git")).await);

    fs.remove_dir(&destination, RemoveOptions::default())
        .await
        .unwrap();

    fs.insert_file(&destination, b"keep".to_vec()).await;
    repo.rename_worktree(source.clone(), destination.clone())
        .await
        .expect_err("an existing file must be rejected");
    assert_eq!(fs.load(&destination).await.unwrap(), "keep");
    assert!(fs.is_file(&source.join(".git")).await);
    fs.remove_file(&destination, RemoveOptions::default())
        .await
        .unwrap();

    fs.insert_tree(&destination, json!({"keep.txt": "keep"}))
        .await;
    repo.rename_worktree(source.clone(), destination.clone())
        .await
        .expect_err("an existing nonempty directory must be rejected");
    assert_eq!(
        fs.load(&destination.join("keep.txt")).await.unwrap(),
        "keep"
    );
    assert!(fs.is_file(&source.join(".git")).await);
    fs.remove_dir(
        &destination,
        RemoveOptions {
            recursive: true,
            ignore_if_not_exists: false,
        },
    )
    .await
    .unwrap();

    repo.rename_worktree(source.clone(), source.clone())
        .await
        .expect_err("renaming a worktree to itself must be rejected");
    assert!(fs.is_file(&source.join(".git")).await);

    let dot_git_content = fs.load(&source.join(".git")).await.unwrap();
    fs.insert_file(&source.join(".git"), b"not a gitdir".to_vec())
        .await;
    repo.rename_worktree(source.clone(), destination.clone())
        .await
        .expect_err("a corrupt .git file must be rejected");
    assert!(fs.is_dir(&source).await);
    assert!(!fs.is_dir(&destination).await);
    fs.insert_file(&source.join(".git"), dot_git_content.into_bytes())
        .await;

    let worktree_entry_dir = PathBuf::from(
        fs.load(&source.join(".git"))
            .await
            .unwrap()
            .strip_prefix("gitdir:")
            .unwrap()
            .trim(),
    );
    let registered_dot_git = fs.load(&worktree_entry_dir.join("gitdir")).await.unwrap();
    fs.insert_file(
        worktree_entry_dir.join("gitdir"),
        b"/worktrees/different/.git".to_vec(),
    )
    .await;
    repo.rename_worktree(source.clone(), destination.clone())
        .await
        .expect_err("mismatched admin metadata must be rejected");
    assert!(fs.is_file(&source.join(".git")).await);
    assert!(!fs.is_dir(&destination).await);
    fs.insert_file(
        worktree_entry_dir.join("gitdir"),
        registered_dot_git.into_bytes(),
    )
    .await;

    fs.remove_dir(
        Path::new("/project/.git/worktrees/source"),
        RemoveOptions {
            recursive: true,
            ignore_if_not_exists: false,
        },
    )
    .await
    .unwrap();

    assert!(
        repo.rename_worktree(source.clone(), destination.clone())
            .await
            .is_err()
    );
    assert!(fs.is_dir(&source).await);
    assert!(!fs.is_dir(&destination).await);
}

#[gpui::test]
async fn test_fake_rename_worktree_matches_git_path_and_lock_rules(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", json!({".git": {}, "file.txt": "content"}))
        .await;
    let repo = fs
        .open_repo(Path::new("/project/.git"), None)
        .expect("should open fake repo");
    let source = PathBuf::from("/worktrees/- dirty wørktree 名");
    repo.create_worktree(
        git::repository::CreateWorktreeTarget::NewBranch {
            branch_name: "rename-path-rules".to_string(),
            base_sha: Some("abc123".to_string()),
        },
        source.clone(),
    )
    .await
    .unwrap();
    fs.insert_file(source.join("untracked.txt"), b"dirty".to_vec())
        .await;

    let missing_parent_destination = PathBuf::from("/missing/parent/destination");
    repo.rename_worktree(source.clone(), missing_parent_destination.clone())
        .await
        .expect_err("git does not create destination parents");
    assert!(fs.is_file(&source.join(".git")).await);
    assert!(!fs.is_dir(&missing_parent_destination).await);

    fs.with_git_state(Path::new("/project/.git"), false, |state| {
        state.locked_worktrees.insert(source.clone());
    })
    .unwrap();
    let destination = PathBuf::from("/worktrees/- renamed wørktree 名");
    repo.rename_worktree(source.clone(), destination.clone())
        .await
        .expect_err("locked worktrees cannot be moved");
    assert!(fs.is_file(&source.join(".git")).await);
    assert!(!fs.is_dir(&destination).await);

    fs.with_git_state(Path::new("/project/.git"), false, |state| {
        state.locked_worktrees.remove(&source);
        state
            .worktrees_requiring_force_delete
            .insert(source.clone());
    })
    .unwrap();
    repo.rename_worktree(source.clone(), destination.clone())
        .await
        .unwrap();
    assert!(!fs.is_dir(&source).await);
    assert!(fs.is_file(&destination.join(".git")).await);
    assert_eq!(
        fs.load(&destination.join("untracked.txt")).await.unwrap(),
        "dirty"
    );
    repo.remove_worktree(destination.clone(), false)
        .await
        .expect_err("a renamed dirty worktree must still require force");
    assert!(fs.is_file(&destination.join(".git")).await);
    repo.remove_worktree(destination.clone(), true)
        .await
        .unwrap();
    assert!(!fs.is_dir(&destination).await);

    repo.rename_worktree(PathBuf::from("/project"), PathBuf::from("/main-moved"))
        .await
        .expect_err("the main worktree cannot be moved");
    assert!(fs.is_dir(Path::new("/project/.git")).await);
    assert!(!fs.is_dir(Path::new("/main-moved")).await);
}

#[gpui::test]
async fn test_checkpoints(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor);
    fs.insert_tree(
        path!("/"),
        json!({
            "bar": {
                "baz": "qux"
            },
            "foo": {
                ".git": {},
                "a": "lorem",
                "b": "ipsum",
            },
        }),
    )
    .await;
    fs.with_git_state(Path::new("/foo/.git"), true, |_git| {})
        .unwrap();
    let repository = fs
        .open_repo(Path::new("/foo/.git"), Some("git".as_ref()))
        .unwrap();

    let checkpoint_1 = repository.checkpoint().await.unwrap();
    fs.write(Path::new("/foo/b"), b"IPSUM").await.unwrap();
    fs.write(Path::new("/foo/c"), b"dolor").await.unwrap();
    let checkpoint_2 = repository.checkpoint().await.unwrap();
    let checkpoint_3 = repository.checkpoint().await.unwrap();

    assert!(
        repository
            .compare_checkpoints(checkpoint_2.clone(), checkpoint_3.clone())
            .await
            .unwrap()
    );
    assert!(
        !repository
            .compare_checkpoints(checkpoint_1.clone(), checkpoint_2.clone())
            .await
            .unwrap()
    );

    repository
        .restore_checkpoint(checkpoint_1.clone())
        .await
        .unwrap();
    assert_eq!(
        fs.files_with_contents(Path::new("")),
        [
            (Path::new(path!("/bar/baz")).into(), b"qux".into()),
            (Path::new(path!("/foo/a")).into(), b"lorem".into()),
            (Path::new(path!("/foo/b")).into(), b"ipsum".into())
        ]
    );

    // diff_checkpoints: identical checkpoints produce empty diff
    let diff = repository
        .diff_checkpoints(checkpoint_2.clone(), checkpoint_3.clone())
        .await
        .unwrap();
    assert!(
        diff.is_empty(),
        "identical checkpoints should produce empty diff"
    );

    // diff_checkpoints: different checkpoints produce non-empty diff
    let diff = repository
        .diff_checkpoints(checkpoint_1.clone(), checkpoint_2.clone())
        .await
        .unwrap();
    assert!(diff.contains("b"), "diff should mention changed file 'b'");
    assert!(diff.contains("c"), "diff should mention added file 'c'");
}
