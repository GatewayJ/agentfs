use agentfs_engine::ContentService;
use agentfs_local::LocalBackend;
use agentfs_model::*;
use agentfs_platform::ContainerValidation;
use agentfs_ports::*;
use bytes::Bytes;
use std::sync::Arc;

#[tokio::test]
#[ignore = "requires Docker and a preloaded debian:bookworm-slim image"]
async fn container_validation_reads_private_files_and_enforces_limits() {
    let root = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalBackend::open(root.path()).unwrap());
    let content = ContentService::new(local.clone(), None, None);
    let workspace = WorkspaceId::new();
    let object = local
        .put(
            workspace,
            ObjectKind::File,
            Bytes::from_static(b"candidate content"),
        )
        .await
        .unwrap();
    let directory = Inode {
        id: InodeId::ROOT,
        kind: FileKind::Directory,
        mode: 0o700,
        uid: 1,
        gid: 1,
        size: 0,
        created_ns: 0,
        modified_ns: 0,
        content: None,
    };
    let file = Inode {
        id: InodeId(2),
        kind: FileKind::File,
        mode: 0o600,
        content: Some(object),
        size: 17,
        ..directory.clone()
    };
    let tree = FileTree::from([
        (WorkspacePath::root(), directory),
        ("/input".to_owned().try_into().unwrap(), file),
    ]);
    let runner = ContainerValidation::new("docker".into());
    for (command, passed, timeout) in [
        (
            "test \"$(id -u)\" = 65534 && test \"$(cat /workspace/input)\" = 'candidate content' && ! touch /workspace/write",
            true,
            30,
        ),
        ("printf 'too much output for this limit'", false, 30),
        ("sleep 30", false, 1),
    ] {
        let result = runner
            .run(ValidationRequest {
                candidate: RevisionId::new(),
                workspace,
                tree: tree.clone(),
                content: content.clone(),
                config: ValidationConfig {
                    image: "debian:bookworm-slim".into(),
                    program: "/bin/sh".into(),
                    arguments: vec!["-c".into(), command.into()],
                    timeout_seconds: timeout,
                    max_output_bytes: if command.starts_with("printf") {
                        8
                    } else {
                        4096
                    },
                },
            })
            .await
            .unwrap();
        assert_eq!(result.passed, passed, "{}", result.output);
        assert!(!result.skipped);
    }
}
