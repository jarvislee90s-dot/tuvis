use multi_agents_manager_lib::remote::attachments::write_dsh_attachment;

#[test]
fn instance_channels_are_shared_only_within_instance() {
    use multi_agents_manager_lib::services::dsh_control::actions::PeerChannel;
    let a = PeerChannel::for_instance("fixture-a".into());
    let same = PeerChannel::for_instance("fixture-a".into());
    let other = PeerChannel::for_instance("fixture-b".into());
    assert!(std::sync::Arc::ptr_eq(&a, &same));
    assert!(!std::sync::Arc::ptr_eq(&a, &other));
}

#[cfg(unix)]
#[test]
fn symlink_cannot_escape_attachment_scope() {
    use std::os::unix::fs::symlink;

    let cwd = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), cwd.path().join(".tuvis-attachments")).unwrap();
    assert!(write_dsh_attachment(
        cwd.path(),
        "session-12345678",
        "test.txt",
        b"secret",
        None,
        None
    )
    .is_err());
    assert!(!outside.path().join("session-12345678/test.txt").exists());
}

#[test]
fn concurrent_uploads_never_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|n| {
            let cwd = root.path().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                write_dsh_attachment(&cwd, "session-12345678", "file.txt", &[n], None, None)
                    .unwrap()
            })
        })
        .collect();
    let paths: std::collections::HashSet<_> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(paths.len(), 8);
    let contents: std::collections::HashSet<_> =
        paths.iter().map(|p| std::fs::read(p).unwrap()[0]).collect();
    assert_eq!(contents.len(), 8);
}

#[test]
fn attachment_session_id_cannot_traverse_outside_project() {
    let parent = tempfile::tempdir().unwrap();
    let cwd = parent.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    assert!(write_dsh_attachment(&cwd, "../../escape", "test.txt", b"secret", None, None).is_err());
    assert!(!parent.path().join("escape/test.txt").exists());
}
