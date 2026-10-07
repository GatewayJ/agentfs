use agentfs_model::*;
use agentfs_ports::*;
use agentfs_s3::{S3Backend, S3Config};
use bytes::Bytes;
use object_store::{
    aws::AmazonS3Builder,
    path::Path,
    signer::{Method, Signer},
};
use std::time::Duration;
use tokio::io::AsyncReadExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated S3 bucket endpoint and test credentials"]
async fn s3_conditional_refs_and_streaming_objects() {
    let endpoint = std::env::var("AGENTFS_TEST_S3_ENDPOINT").expect("set AGENTFS_TEST_S3_ENDPOINT");
    let bucket = "agentfs-integration";
    let client = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_region("us-east-1")
        .with_endpoint(&endpoint)
        .with_allow_http(true)
        .build()
        .unwrap();
    let url = client
        .signed_url(Method::PUT, &Path::from(""), Duration::from_secs(60))
        .await
        .unwrap();
    let created = reqwest::Client::new()
        .put(url)
        .send()
        .await
        .map_err(|error| error.without_url())
        .unwrap();
    assert!(
        created.status().is_success() || created.status().as_u16() == 409,
        "bucket creation returned {}",
        created.status()
    );
    let backend = S3Backend::new(S3Config {
        bucket: bucket.into(),
        prefix: format!("tests/{}", uuid::Uuid::new_v4()),
        region: "us-east-1".into(),
        endpoint: Some(endpoint),
        allow_http: true,
    })
    .unwrap();
    let workspace = WorkspaceId::new();
    let branch = BranchId::new();
    let key = RefKey::Branch { workspace, branch };
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..16 {
        let backend = backend.clone();
        let key = key.clone();
        tasks.spawn(async move {
            backend
                .compare_exchange(RefUpdate {
                    key,
                    expected_version: None,
                    bytes: index.to_string().into(),
                })
                .await
                .unwrap()
        });
    }
    let mut winners = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            CasOutcome::Applied { .. } => winners += 1,
            CasOutcome::Conflict => (),
            CasOutcome::Unknown => panic!("unexpected unknown result"),
        }
    }
    assert_eq!(winners, 1);
    let before = backend.get(&key).await.unwrap().unwrap();
    assert!(matches!(
        backend
            .compare_exchange(RefUpdate {
                key: key.clone(),
                expected_version: Some(before.version.clone()),
                bytes: Bytes::from_static(b"updated")
            })
            .await
            .unwrap(),
        CasOutcome::Applied { .. }
    ));
    assert_eq!(
        backend
            .compare_exchange(RefUpdate {
                key: key.clone(),
                expected_version: Some(before.version),
                bytes: Bytes::from_static(b"stale")
            })
            .await
            .unwrap(),
        CasOutcome::Conflict
    );
    assert_eq!(backend.get(&key).await.unwrap().unwrap().bytes, "updated");
    assert_eq!(
        backend.list_branches(workspace).await.unwrap(),
        vec![branch]
    );
    for size in [0, 1024, 20 * 1024 * 1024 + 17] {
        let bytes = Bytes::from(
            (0..size)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let reference = object_ref(ObjectKind::File, &bytes);
        backend
            .upload(
                workspace,
                ObjectStream {
                    reference: reference.clone(),
                    reader: Box::pin(std::io::Cursor::new(bytes.clone())),
                },
            )
            .await
            .unwrap();
        backend
            .upload(
                workspace,
                ObjectStream {
                    reference: reference.clone(),
                    reader: Box::pin(std::io::Cursor::new(bytes.clone())),
                },
            )
            .await
            .unwrap();
        let mut downloaded = backend.download(workspace, &reference).await.unwrap();
        let mut actual = Vec::new();
        downloaded.reader.read_to_end(&mut actual).await.unwrap();
        assert_eq!(actual, bytes);
    }
    let reference = object_ref(ObjectKind::File, b"correct");
    let error = backend
        .upload(
            workspace,
            ObjectStream {
                reference: reference.clone(),
                reader: Box::pin(std::io::Cursor::new(b"damaged")),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Integrity);
    assert!(!backend.contains(workspace, &reference).await.unwrap());
}
