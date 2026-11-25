use super::{Workspace, WorkspaceError};
use std::io::Read;
use std::os::unix::fs::symlink;

fn root() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let physical = dir.path().canonicalize().unwrap();
    (dir, physical)
}

#[test]
fn relative_access_rejects_parent_absolute_links_and_special_files() {
    let (_root, path) = root();
    std::fs::write(path.join("real"), "inside").unwrap();
    symlink("real", path.join("link")).unwrap();
    symlink("/etc", path.join("escape")).unwrap();
    let workspace = Workspace::open(&path).unwrap();
    for value in [
        "/etc/passwd",
        "../outside",
        "real/../real",
        "",
        "x\0y",
        "x\\y",
    ] {
        assert!(matches!(
            workspace.file(value),
            Err(WorkspaceError::InvalidPath)
        ));
    }
    for value in ["link", "escape/passwd", "."] {
        assert!(matches!(
            workspace.file(value),
            Err(WorkspaceError::Forbidden)
        ));
    }
    let mut text = String::new();
    workspace
        .file("./real")
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "inside");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(path.join("fifo"))
            .status()
            .unwrap()
            .success()
    );
    assert!(matches!(
        workspace.file("fifo"),
        Err(WorkspaceError::Forbidden)
    ));
}

#[test]
fn root_replacement_and_open_child_rename_keep_the_pinned_resource() {
    let (_root, path) = root();
    let selected = path.join("selected");
    std::fs::create_dir(&selected).unwrap();
    std::fs::create_dir(selected.join("child")).unwrap();
    std::fs::write(selected.join("child/data"), "authorized").unwrap();
    let workspace = Workspace::open(&selected).unwrap();
    let child = workspace.directory("child").unwrap();
    std::fs::rename(&selected, path.join("moved")).unwrap();
    symlink("/etc", &selected).unwrap();
    assert!(Workspace::open(&selected).is_err());
    let mut text = String::new();
    workspace
        .child_file(&child, "data")
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "authorized");
    assert!(workspace.file("passwd").is_err());
}

#[test]
fn replacing_an_unopened_component_with_a_link_is_rejected() {
    let (_root, path) = root();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "outside").unwrap();
    std::fs::create_dir(path.join("child")).unwrap();
    let workspace = Workspace::open(&path).unwrap();
    let parent = workspace.directory(".").unwrap();
    std::fs::rename(path.join("child"), path.join("old")).unwrap();
    symlink(outside.path(), path.join("child")).unwrap();
    assert!(matches!(
        workspace.child_directory(&parent, "child"),
        Err(WorkspaceError::Forbidden)
    ));
    assert!(matches!(
        workspace.file("child/secret"),
        Err(WorkspaceError::Forbidden)
    ));
}

#[test]
fn concurrent_file_replacement_after_stat_cannot_redirect_the_open() {
    let (_root, path) = root();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(path.join("file"), "inside").unwrap();
    std::fs::write(outside.path().join("secret"), "outside").unwrap();
    let workspace = Workspace::open(&path).unwrap();
    let parent = workspace.directory(".").unwrap();
    let stat = rustix::fs::statat(&parent, "file", rustix::fs::AtFlags::SYMLINK_NOFOLLOW).unwrap();
    assert_eq!(
        rustix::fs::FileType::from_raw_mode(stat.st_mode),
        rustix::fs::FileType::RegularFile
    );
    let (changed, done) = std::sync::mpsc::channel();
    let mutator = std::thread::spawn(move || {
        std::fs::remove_file(path.join("file")).unwrap();
        symlink(outside.path().join("secret"), path.join("file")).unwrap();
        changed.send(()).unwrap();
        // Keep both fixture resources alive until the tested open finishes.
        outside
    });
    done.recv().unwrap();
    assert!(matches!(
        workspace.child_file(&parent, "file"),
        Err(WorkspaceError::Forbidden)
    ));
    drop(mutator.join().unwrap());
}

#[test]
fn a_real_different_device_mount_is_rejected_when_present() {
    use std::os::unix::fs::MetadataExt;
    let root_device = std::fs::metadata("/").unwrap().dev();
    let dev_device = std::fs::metadata("/dev").unwrap().dev();
    if root_device == dev_device {
        return;
    }
    let workspace = Workspace::open(std::path::Path::new("/")).unwrap();
    assert!(matches!(
        workspace.directory("dev"),
        Err(WorkspaceError::Forbidden)
    ));
}

#[test]
fn complete_workspace_paths_is_sorted_bounded_and_enforces_safety_boundaries() {
    let (_root, path) = root();
    std::fs::create_dir(path.join("src")).unwrap();
    std::fs::create_dir(path.join("docs")).unwrap();
    std::fs::create_dir(path.join(".git")).unwrap();
    std::fs::create_dir(path.join(".HG")).unwrap();
    std::fs::write(path.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
    std::fs::write(path.join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(path.join("README.md"), "# readme\n").unwrap();
    std::fs::write(path.join("src/lib.rs"), "").unwrap();
    std::fs::write(path.join("src/main.rs"), "").unwrap();
    std::fs::create_dir(path.join("src/sub")).unwrap();
    std::fs::write(path.join("src/sub/deep.rs"), "").unwrap();

    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    symlink(outside.path(), path.join("escape_dir")).unwrap();
    symlink(outside.path().join("secret.txt"), path.join("src/link.rs")).unwrap();

    // Empty prefix lists top-level files and dirs, sorted lexicographically, excluding symlinks and VCS.
    let top = super::complete_workspace_paths(&path, "", 64);
    assert_eq!(
        top,
        vec![
            super::WorkspacePathCandidate {
                relative_path: "Cargo.toml".into(),
                is_dir: false,
            },
            super::WorkspacePathCandidate {
                relative_path: "README.md".into(),
                is_dir: false,
            },
            super::WorkspacePathCandidate {
                relative_path: "docs".into(),
                is_dir: true,
            },
            super::WorkspacePathCandidate {
                relative_path: "src".into(),
                is_dir: true,
            },
        ]
    );

    // Subdirectory completion strips `./` and skips symlinks.
    let src = super::complete_workspace_paths(&path, "./src/", 64);
    assert_eq!(
        src,
        vec![
            super::WorkspacePathCandidate {
                relative_path: "src/lib.rs".into(),
                is_dir: false,
            },
            super::WorkspacePathCandidate {
                relative_path: "src/main.rs".into(),
                is_dir: false,
            },
            super::WorkspacePathCandidate {
                relative_path: "src/sub".into(),
                is_dir: true,
            },
        ]
    );

    // Prefix filtering + limit bound.
    let limited = super::complete_workspace_paths(&path, "", 2);
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].relative_path, "Cargo.toml");
    assert_eq!(limited[1].relative_path, "README.md");

    // Unsafe and VCS prefixes always return empty.
    for bad in [
        "/etc",
        "../",
        "src/../Cargo",
        "..",
        "src\\lib",
        "a\0b",
        ".git/",
        ".GIT/",
        ".HG/",
        "escape_dir/",
    ] {
        assert!(
            super::complete_workspace_paths(&path, bad, 64).is_empty(),
            "unsafe prefix {bad:?} must return no candidates"
        );
    }
}
