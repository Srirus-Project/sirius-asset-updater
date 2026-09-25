use super::*;
use axum::{
    body::Body,
    extract::{Request, State},
    http::header,
    response::Response,
    routing::any,
    Router,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{atomic::AtomicUsize, Mutex},
};
fn local(directory: PathBuf) -> Provider {
    Provider {
        name: "local".into(),
        prefix: "assets".into(),
        public_base_url: None,
        backend: Backend::Local { directory },
    }
}
fn config(provider: Provider) -> Config {
    Config {
        providers: vec![provider],
        concurrency: 2,
        attempts: 2,
        object_timeout_seconds: 5,
        retry_delay_ms: 1,
        remove_local_after_upload: false,
    }
}
fn fixture() -> tempfile::TempDir {
    crate::export_verify::tests::fixture()
}
#[tokio::test]
async fn local_publication_readback_region_scope_and_explicit_cleanup() {
    let source = fixture();
    let destination = tempfile::tempdir().unwrap();
    let mut config = config(local(destination.path().into()));
    let (_tx, rx) = watch::channel(false);
    let result = config
        .publish(source.path(), Region::Jp, rx.clone())
        .await
        .unwrap();
    assert_eq!(result.files, 3);
    assert!(!result.local_removed && source.path().exists());
    let prefix = &result.providers[0].prefix;
    assert!(prefix.starts_with("assets/jp/publications/"));
    assert_eq!(
        std::fs::read(destination.path().join(prefix).join("00000/payload.bin")).unwrap(),
        b"synthetic export"
    );
    assert!(destination
        .path()
        .join(prefix)
        .join("complete.json")
        .is_file());
    config.remove_local_after_upload = true;
    let result2 = config.publish(source.path(), Region::Jp, rx).await.unwrap();
    assert_ne!(result.id, result2.id);
    assert!(result2.local_removed && !source.path().exists());
    assert!(destination
        .path()
        .join(prefix)
        .join("complete.json")
        .is_file());
}
#[derive(Default)]
struct Fake {
    objects: Mutex<HashMap<String, Vec<u8>>>,
    acls: Mutex<HashMap<String, Option<String>>>,
    write_headers: Mutex<Vec<(String, String, String, String)>>,
    parts: Mutex<BTreeMap<usize, Vec<u8>>>,
    mode: AtomicUsize,
    puts: AtomicUsize,
    multipart_puts: AtomicUsize,
    aborts: AtomicUsize,
    unsigned: AtomicBool,
    gate: tokio::sync::Notify,
}
async fn handle(State(state): State<Arc<Fake>>, request: Request) -> Response {
    let method = request.method().clone();
    let key = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    if !request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("AWS4-HMAC-SHA256 Credential=synthetic-access/"))
    {
        state.unsigned.store(true, Ordering::SeqCst);
    }
    if method == "PUT" && !query.contains("uploadId=")
        || method == "POST" && query.contains("uploads")
    {
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        state.write_headers.lock().unwrap().push((
            query.clone(),
            header("x-amz-storage-class"),
            header("x-amz-server-side-encryption"),
            header("x-amz-server-side-encryption-aws-kms-key-id"),
        ));
        state.acls.lock().unwrap().insert(
            key.clone(),
            request
                .headers()
                .get("x-amz-acl")
                .map(|v| v.to_str().unwrap().to_string()),
        );
    }
    let mode = state.mode.load(Ordering::SeqCst);
    if mode == 6 && method == "GET" {
        return Response::builder()
            .status(403)
            .body(Body::from("<Error><Code>AccessDenied</Code></Error>"))
            .unwrap();
    }
    if method == "DELETE" {
        state.aborts.fetch_add(1, Ordering::SeqCst);
        return Response::builder().status(204).body(Body::empty()).unwrap();
    }
    if method == "POST" && query.contains("uploads") {
        return Response::builder().body(Body::from("<InitiateMultipartUploadResult><UploadId>synthetic-upload</UploadId></InitiateMultipartUploadResult>")).unwrap();
    }
    if method == "PUT" {
        let attempt = state.puts.fetch_add(1, Ordering::SeqCst);
        if mode == 1 && attempt == 0 {
            return Response::builder()
                .status(503)
                .body(Body::from("<Error><Code>ServiceUnavailable</Code></Error>"))
                .unwrap();
        }
        if mode == 3 || mode == 7 && key.ends_with("complete.json") {
            return Response::builder()
                .status(403)
                .body(Body::from("<Error><Code>AccessDenied</Code></Error>"))
                .unwrap();
        }
        if query.contains("uploadId=") {
            state.multipart_puts.fetch_add(1, Ordering::SeqCst);
        }
        if mode == 4 || mode == 8 && attempt > 0 {
            state.gate.notified().await;
        }
        if mode == 5 {
            return Response::builder()
                .status(307)
                .header(header::LOCATION, "/redirected")
                .body(Body::empty())
                .unwrap();
        }
        let data = axum::body::to_bytes(request.into_body(), 32 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        if query.contains("uploadId=") {
            let part = query
                .split('&')
                .find_map(|s| s.strip_prefix("partNumber="))
                .unwrap()
                .parse()
                .unwrap();
            state.parts.lock().unwrap().insert(part, data);
        } else {
            state.objects.lock().unwrap().insert(key, data);
        }
        return Response::builder()
            .header(header::ETAG, "\"synthetic-etag\"")
            .body(Body::empty())
            .unwrap();
    }
    if method == "POST" && query.contains("uploadId=") {
        let data = state
            .parts
            .lock()
            .unwrap()
            .values()
            .flatten()
            .copied()
            .collect();
        state.objects.lock().unwrap().insert(key, data);
        return Response::builder().body(Body::from("<CompleteMultipartUploadResult><ETag>synthetic-etag</ETag></CompleteMultipartUploadResult>")).unwrap();
    }
    let objects = state.objects.lock().unwrap();
    let Some(data) = objects.get(&key) else {
        return Response::builder().status(404).body(Body::empty()).unwrap();
    };
    if method == "HEAD" {
        return Response::builder()
            .header(header::CONTENT_LENGTH, data.len())
            .body(Body::empty())
            .unwrap();
    }
    let mut data = data.clone();
    if mode == 2 && !data.is_empty() {
        data[0] ^= 1;
    }
    if let Some(range) = request.headers().get(header::RANGE) {
        let range = range.to_str().unwrap().strip_prefix("bytes=").unwrap();
        let (a, b) = range.split_once('-').unwrap();
        let a: usize = a.parse().unwrap();
        let b: usize = if b.is_empty() {
            data.len() - 1
        } else {
            b.parse().unwrap()
        };
        return Response::builder()
            .status(206)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {a}-{b}/{}", data.len()),
            )
            .body(Body::from(data[a..=b].to_vec()))
            .unwrap();
    }
    Response::builder().body(Body::from(data)).unwrap()
}
struct Server {
    state: Arc<Fake>,
    config: Config,
    task: tokio::task::JoinHandle<()>,
    env: [String; 2],
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        for name in &self.env {
            std::env::remove_var(name);
        }
    }
}
async fn server() -> Server {
    let state = Arc::new(Fake::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .fallback(any(handle))
        .with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let env = [
        format!("SIRIUS_STORAGE_ACCESS_{}", uuid::Uuid::new_v4().simple()),
        format!("SIRIUS_STORAGE_SECRET_{}", uuid::Uuid::new_v4().simple()),
    ];
    std::env::set_var(&env[0], "synthetic-access");
    std::env::set_var(&env[1], "synthetic-secret");
    let config = config(Provider {
        name: "s3".into(),
        prefix: "assets".into(),
        public_base_url: None,
        backend: Backend::S3 {
            endpoint,
            write_options: Box::default(),
            path_style: true,
            public_read: false,
            public_read_include: vec![],
            public_read_exclude: vec![],
            bucket: "synthetic-bucket".into(),
            region: "test-region".into(),
            access_key_id_env: env[0].clone(),
            secret_access_key_env: env[1].clone(),
            session_token_env: None,
        },
    });
    Server {
        state,
        config,
        task,
        env,
    }
}
#[tokio::test]
async fn s3_signed_upload_retry_readback_and_failure_preserves_source() {
    for mode in [0, 1, 2, 3, 5, 6, 7] {
        let source = fixture();
        let mut server = server().await;
        server.state.mode.store(mode, Ordering::SeqCst);
        server.config.concurrency = 1;
        server.config.remove_local_after_upload = true;
        let (_tx, rx) = watch::channel(false);
        let result = server.config.publish(source.path(), Region::Jp, rx).await;
        if mode <= 1 {
            assert!(
                result.is_ok(),
                "mode {mode}: {}",
                result.err().map(|e| e.to_string()).unwrap_or_default()
            );
            assert!(!source.path().exists());
            assert_eq!(
                server.state.puts.load(Ordering::SeqCst),
                4 + usize::from(mode == 1)
            );
        } else {
            assert!(result.is_err());
            assert!(source.path().exists());
            assert!(!server
                .state
                .objects
                .lock()
                .unwrap()
                .keys()
                .any(|k| k.ends_with("complete.json")));
            if mode == 3 || mode == 5 {
                assert_eq!(server.state.puts.load(Ordering::SeqCst), 1);
            }
        }
        assert!(!server.state.unsigned.load(Ordering::SeqCst));
    }
}
#[tokio::test]
async fn multiple_destinations_must_pass_before_markers_or_cleanup() {
    let source = fixture();
    let local_root = tempfile::tempdir().unwrap();
    let mut server = server().await;
    server
        .config
        .providers
        .insert(0, local(local_root.path().into()));
    server.config.remove_local_after_upload = true;
    server.state.mode.store(3, Ordering::SeqCst);
    let (_tx, rx) = watch::channel(false);
    assert!(server
        .config
        .publish(source.path(), Region::Jp, rx)
        .await
        .is_err());
    assert!(source.path().join("00000/payload.bin").is_file());
    let publications = local_root.path().join("assets/jp/publications");
    let publication = std::fs::read_dir(publications)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(publication.join("00000/payload.bin").is_file());
    assert!(!publication.join("complete.json").exists());
}
fn large_source() -> tempfile::TempDir {
    let root = fixture();
    let data = vec![0x5a; 9 * 1024 * 1024];
    std::fs::write(root.path().join("00000/payload.bin"), &data).unwrap();
    let path = root.path().join("resources.jsonl");
    let mut report: crate::export::ResourceReport =
        sonic_rs::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    report.outputs[0].bytes = data.len() as u64;
    report.outputs[0].sha256 = hex::encode(Sha256::digest(&data));
    let mut bytes = sonic_rs::to_vec(&report).unwrap();
    bytes.push(b'\n');
    std::fs::write(path, bytes).unwrap();
    let path = root.path().join("summary.json");
    let mut summary: crate::export::ExportSummary =
        sonic_rs::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    summary.output_bytes = data.len() as u64;
    std::fs::write(path, sonic_rs::to_vec(&summary).unwrap()).unwrap();
    root
}
#[tokio::test]
async fn multipart_success_and_cancellation_abort_without_cleanup() {
    let source = large_source();
    let server = server().await;
    let (tx, rx) = watch::channel(false);
    server
        .config
        .publish(source.path(), Region::Jp, rx.clone())
        .await
        .unwrap();
    assert_eq!(server.state.parts.lock().unwrap().len(), 2);
    server.state.mode.store(4, Ordering::SeqCst);
    server.state.puts.store(0, Ordering::SeqCst);
    server.state.multipart_puts.store(0, Ordering::SeqCst);
    let work = server.config.publish(source.path(), Region::Jp, rx);
    tokio::pin!(work);
    let cancel = async {
        while server.state.multipart_puts.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tx.send(true).unwrap();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(work, cancel) })
            .await
            .unwrap();
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(source.path().exists());
    assert!(server.state.aborts.load(Ordering::SeqCst) > 0);
}
#[test]
fn invalid_config_fails_closed_and_source_overlap_is_rejected() {
    let source = fixture();
    let config = config(local(source.path().into()));
    assert!(config.providers[0].operator(source.path()).is_err());
    let mut config = config;
    config.concurrency = 0;
    assert!(config.validate().is_err());
    config.concurrency = 1;
    config.providers[0].prefix = "../outside".into();
    assert!(config.validate().is_err());
    assert!(
        yaml_serde::from_str::<Backend>("type: local\ndirectory: /tmp\nsecret: ignored").is_err()
    );
}

#[tokio::test]
async fn storage_timeouts_are_bounded_and_zero_byte_objects_roundtrip() {
    let source = fixture();
    let mut server = server().await;
    server.state.mode.store(4, Ordering::SeqCst);
    server.config.attempts = 1;
    server.config.object_timeout_seconds = 1;
    server.config.remove_local_after_upload = true;
    let (_tx, rx) = watch::channel(false);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        server.config.publish(source.path(), Region::Jp, rx),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Transport)));
    assert!(source.path().join("00000/payload.bin").exists());
    server.state.mode.store(0, Ordering::SeqCst);
    let op = server.config.providers[0].operator(source.path()).unwrap();
    op.write("empty", Vec::<u8>::new()).await.unwrap();
    check_remote(&op, "empty", 0, &hex::encode(Sha256::digest([])))
        .await
        .unwrap();
}

#[tokio::test]
async fn s3_address_style_changes_signed_request_and_preserves_legacy_default() {
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<axum::http::Request<opendal::Buffer>>>>);
    impl opendal::HttpTransport for Capture {
        async fn fetch(
            &self,
            request: axum::http::Request<opendal::Buffer>,
        ) -> opendal::Result<axum::http::Response<opendal::HttpBody>> {
            self.0.lock().unwrap().push(request);
            Err(opendal::Error::new(
                opendal::ErrorKind::PermissionDenied,
                "synthetic capture",
            ))
        }
    }
    let mut server = server().await;
    let source = fixture();
    for style in [true, false] {
        if let Backend::S3 {
            endpoint,
            path_style,
            ..
        } = &mut server.config.providers[0].backend
        {
            *endpoint = "https://objects.example:9443".into();
            *path_style = style;
        }
        server.config.validate().unwrap();
        let capture = Capture::default();
        let operator = server.config.providers[0]
            .operator(source.path())
            .unwrap()
            .with_context(
                OperationContext::new().with_http_transport(HttpTransporter::new(capture.clone())),
            );
        assert!(operator
            .write("assets/jp/a b.bin", "synthetic")
            .await
            .is_err());
        let requests = capture.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        let (host, path) = if style {
            (
                "objects.example:9443",
                "/synthetic-bucket/assets/jp/a%20b.bin",
            )
        } else {
            (
                "synthetic-bucket.objects.example:9443",
                "/assets/jp/a%20b.bin",
            )
        };
        assert_eq!(req.uri().authority().unwrap().as_str(), host);
        assert_eq!(req.uri().path(), path);
        let signature = req.headers()[header::AUTHORIZATION].to_str().unwrap();
        assert!(signature.starts_with("AWS4-HMAC-SHA256 "));
        assert!(signature.contains("/test-region/s3/aws4_request"));
        assert!(signature.contains("host"));
    }
    let yaml = format!("type: s3\nendpoint: https://objects.example\nbucket: synthetic-bucket\nregion: test-region\naccess_key_id_env: {}\nsecret_access_key_env: {}\n", server.env[0], server.env[1]);
    let legacy: Backend = yaml_serde::from_str(&yaml).unwrap();
    assert!(matches!(
        legacy,
        Backend::S3 {
            path_style: true,
            ..
        }
    ));
    for (endpoint_value, bucket_value) in [
        ("https://127.0.0.1", "synthetic-bucket"),
        ("https://[::1]", "synthetic-bucket"),
        ("https://objects.example", "dotted.bucket"),
    ] {
        if let Backend::S3 {
            endpoint, bucket, ..
        } = &mut server.config.providers[0].backend
        {
            *endpoint = endpoint_value.into();
            *bucket = bucket_value.into();
        }
        assert!(server.config.validate().is_err());
    }
}

#[tokio::test]
async fn s3_public_read_rules_apply_to_uploads_and_markers() {
    for mode in 0..4 {
        let source = if mode == 1 { large_source() } else { fixture() };
        let mut server = server().await;
        if let Backend::S3 {
            public_read,
            public_read_include,
            public_read_exclude,
            ..
        } = &mut server.config.providers[0].backend
        {
            *public_read = mode == 2;
            *public_read_include = if mode == 1 {
                vec![r"\.bin$".into()]
            } else if mode == 3 {
                vec![".*".into()]
            } else {
                vec![]
            };
            *public_read_exclude = if mode >= 2 {
                vec![r"^resources\.jsonl$".into(), r"^00000/".into()]
            } else {
                vec![]
            };
        }
        let (_tx, rx) = watch::channel(false);
        let publication = server
            .config
            .publish(source.path(), Region::Jp, rx)
            .await
            .unwrap();
        let acls = server.state.acls.lock().unwrap();
        for path in [
            "00000/payload.bin",
            "summary.json",
            "resources.jsonl",
            "complete.json",
        ] {
            let key = format!(
                "/synthetic-bucket/{}/{}",
                publication.providers[0].prefix, path
            );
            let expected = match mode {
                1 => path.ends_with(".bin"),
                2 | 3 => path == "summary.json" || path == "complete.json",
                _ => false,
            };
            assert_eq!(
                acls.get(&key).unwrap().as_deref(),
                expected.then_some("public-read"),
                "mode {mode}, {path}"
            );
        }
        assert!(source.path().exists());
        assert!(!server.state.unsigned.load(Ordering::SeqCst));
    }
    let mut server = server().await;
    for rules in [
        vec!["[".into()],
        vec!["".into()],
        vec!["x".repeat(4097)],
        vec!["x".into(); 129],
    ] {
        if let Backend::S3 {
            public_read_include,
            ..
        } = &mut server.config.providers[0].backend
        {
            *public_read_include = rules;
        }
        assert!(server.config.validate().is_err());
    }
    assert_eq!(server.state.puts.load(Ordering::SeqCst), 0);
    if let Backend::S3 {
        public_read,
        public_read_include,
        ..
    } = &mut server.config.providers[0].backend
    {
        *public_read = true;
        public_read_include.clear();
    }
    server.state.mode.store(3, Ordering::SeqCst);
    server.config.remove_local_after_upload = true;
    let source = fixture();
    let (_tx, rx) = watch::channel(false);
    assert!(server
        .config
        .publish(source.path(), Region::Jp, rx)
        .await
        .is_err());
    assert!(source.path().exists());
    assert!(server
        .state
        .acls
        .lock()
        .unwrap()
        .values()
        .all(|acl| acl.as_deref() == Some("public-read")));
    assert!(!server
        .state
        .objects
        .lock()
        .unwrap()
        .keys()
        .any(|key| key.ends_with("complete.json")));
}

#[tokio::test]
async fn storage_plan_and_publication_urls_share_region_scoped_targets_without_writes() {
    let source = fixture();
    let destination = tempfile::tempdir().unwrap();
    let missing = destination.path().join("not-created-by-plan");
    let mut provider = local(missing.clone());
    provider.public_base_url = Some("https://cdn.example/root%20path/".into());
    let mut config = config(provider);
    for region in [Region::Jp, Region::Tw, Region::En, Region::Kr] {
        let plan = config.plan(region).unwrap();
        let target = &plan.providers[0];
        assert!(plan.preview);
        assert_eq!(
            target.public_url.as_deref(),
            Some(format!("https://cdn.example/root%20path/{}/", target.prefix).as_str())
        );
        assert!(target
            .prefix
            .starts_with(&format!("assets/{}/publications/", region.name())));
        assert!(!missing.exists());
        let json = sonic_rs::to_string(&plan).unwrap();
        assert!(!json.contains("directory") && !json.contains("backend"));
    }
    assert!(config.plan(Region::Cn).is_err());
    let (_tx, rx) = watch::channel(false);
    let publication = config.publish(source.path(), Region::Jp, rx).await.unwrap();
    let target = &publication.providers[0];
    assert_eq!(
        target.public_url.as_deref(),
        Some(format!("https://cdn.example/root%20path/{}/", target.prefix).as_str())
    );
    assert!(missing.join(&target.prefix).join("complete.json").is_file());
    for invalid in [
        "http://cdn.example",
        "https://user:secret@cdn.example",
        "https://cdn.example/?token=secret",
        "https://cdn.example/#secret",
        "https://cdn.example/ space",
        "file:///tmp/assets",
    ] {
        config.providers[0].public_base_url = Some(invalid.into());
        assert!(config.plan(Region::Jp).is_err());
    }
    config.providers[0].public_base_url = None;
    assert!(!sonic_rs::to_string(&config.plan(Region::Jp).unwrap())
        .unwrap()
        .contains("public_url"));
}

#[tokio::test]
async fn upload_progress_counts_only_verified_objects_and_all_destinations() {
    let source = fixture();
    let mut server = server().await;
    server.config.concurrency = 1;
    server.state.mode.store(8, Ordering::SeqCst);
    let mut second = server.config.providers[0].clone();
    second.name = "second".into();
    second.prefix = "backup".into();
    server.config.providers.push(second);
    let (_stop_tx, stop_rx) = watch::channel(false);
    let (progress_tx, mut progress_rx) = watch::channel(UploadProgress::default());
    let work =
        server
            .config
            .publish_with_progress(source.path(), Region::Jp, stop_rx, Some(progress_tx));
    tokio::pin!(work);
    let partial = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result = &mut work => panic!("publication ended before partial progress: {}", result.is_ok()),
                result = progress_rx.changed() => {
                    result.unwrap();
                    let snapshot = progress_rx.borrow().clone();
                    if snapshot.completed == 1 { break snapshot; }
                }
            }
        }
    }).await.unwrap();
    assert_eq!(partial.phase, "publish_upload_1_of_2");
    assert_eq!(partial.total, 6);
    assert!(partial.bytes > 0);
    server.state.mode.store(0, Ordering::SeqCst);
    server.state.gate.notify_waiters();
    let publication = tokio::time::timeout(Duration::from_secs(5), work)
        .await
        .unwrap()
        .unwrap();
    let final_progress = progress_rx.borrow().clone();
    assert_eq!(final_progress.phase, "publish");
    assert_eq!(final_progress.completed, 2 * publication.files as u64);
    assert_eq!(final_progress.completed, final_progress.total);
    assert_eq!(final_progress.bytes, 2 * publication.bytes);

    let mut server = self::server().await;
    server.config.concurrency = 1;
    server.state.mode.store(2, Ordering::SeqCst); // Corrupt read-back must not count as success.
    let (_stop_tx, stop_rx) = watch::channel(false);
    let (progress_tx, progress_rx) = watch::channel(UploadProgress::default());
    assert!(server
        .config
        .publish_with_progress(source.path(), Region::Jp, stop_rx, Some(progress_tx))
        .await
        .is_err());
    assert_eq!(progress_rx.borrow().completed, 0);
    assert_eq!(progress_rx.borrow().bytes, 0);
    assert!(source.path().exists());
}

#[tokio::test]
async fn s3_write_options_reach_put_multipart_and_completion_markers() {
    let source = large_source();
    let mut server = server().await;
    let env = format!("SIRIUS_TEST_KMS_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&env, "alias/synthetic-key");
    if let Backend::S3 { write_options, .. } = &mut server.config.providers[0].backend {
        **write_options = S3WriteOptions {
            storage_class: Some("STANDARD_IA".into()),
            server_side_encryption: Some("aws:kms".into()),
            kms_key_id_env: Some(env.clone()),
        };
    }
    let (_tx, rx) = watch::channel(false);
    server
        .config
        .publish(source.path(), Region::Jp, rx)
        .await
        .unwrap();
    let headers = server.state.write_headers.lock().unwrap();
    assert!(headers.iter().any(|(query, ..)| query.contains("uploads")));
    assert!(headers.iter().any(|(query, ..)| query.is_empty()));
    assert!(headers.len() >= 3);
    for (_, class, encryption, key) in headers.iter() {
        assert_eq!(class, "STANDARD_IA");
        assert_eq!(encryption, "aws:kms");
        assert_eq!(key, "alias/synthetic-key");
    }
    assert!(!server.state.unsigned.load(Ordering::SeqCst));
    std::env::remove_var(env);
}
#[test]
fn s3_write_options_reject_bad_policy_without_echoing_values() {
    for text in [
        "storage_class: ''",
        "storage_class: 'bad value'",
        "server_side_encryption: unknown",
        "server_side_encryption: AES256\nkms_key_id_env: UNSET_SIRIUS_TEST_KMS",
        "kms_key_id_env: UNSET_SIRIUS_TEST_KMS",
    ] {
        let options: S3WriteOptions = yaml_serde::from_str(text).unwrap();
        assert!(options.validate().is_err());
    }
    assert!(yaml_serde::from_str::<S3WriteOptions>("endpoint: https://elsewhere.invalid").is_err());
    let env = format!("SIRIUS_TEST_KMS_{}", uuid::Uuid::new_v4().simple());
    let options = S3WriteOptions {
        server_side_encryption: Some("aws:kms".into()),
        kms_key_id_env: Some(env.clone()),
        ..Default::default()
    };
    for value in ["", "invalid\nheader", "key with spaces"] {
        std::env::set_var(&env, value);
        assert!(options.validate().is_err());
    }
    std::env::remove_var(env);
    assert!(S3WriteOptions::default().validate().is_ok());
    assert!(
        yaml_serde::from_str::<S3WriteOptions>("server_side_encryption: AES256")
            .unwrap()
            .validate()
            .is_ok()
    );
}
