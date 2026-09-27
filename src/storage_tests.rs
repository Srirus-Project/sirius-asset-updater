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
        layout: Layout::Publication,
        backend: Backend::Local { directory },
    }
}
fn config(provider: Provider) -> Config {
    Config {
        providers: vec![provider],
        concurrency: 2,
        service_upload_gate: None,
        attempts: 2,
        object_timeout_seconds: 5,
        retry_delay_ms: 1,
        remove_local_after_upload: false,
        release: None,
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
    checksums: Mutex<Vec<(String, String, String)>>,
    payer_headers: Mutex<Vec<(String, bool)>>,
    customer_headers: Mutex<Vec<(String, String, String, String)>>,
    parts: Mutex<BTreeMap<usize, Vec<u8>>>,
    mode: AtomicUsize,
    puts: AtomicUsize,
    multipart_puts: AtomicUsize,
    aborts: AtomicUsize,
    unsigned: AtomicBool,
    role_credentials: AtomicBool,
    gate: tokio::sync::Notify,
}
async fn handle(State(state): State<Arc<Fake>>, request: Request) -> Response {
    let method = request.method().clone();
    {
        let sse_header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        state.customer_headers.lock().unwrap().push((
            method.to_string(),
            sse_header("x-amz-server-side-encryption-customer-algorithm"),
            sse_header("x-amz-server-side-encryption-customer-key"),
            sse_header("x-amz-server-side-encryption-customer-key-md5"),
        ));
    }

    let payer = request
        .headers()
        .get("x-amz-request-payer")
        .is_some_and(|v| v == "requester");
    state
        .payer_headers
        .lock()
        .unwrap()
        .push((method.to_string(), payer));
    let checksum = request
        .headers()
        .get("x-amz-checksum-crc32c")
        .map(|v| v.to_str().unwrap().to_owned());
    let key = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let expected_auth = if state.role_credentials.load(Ordering::SeqCst) {
        if request
            .headers()
            .get("x-amz-security-token")
            .is_none_or(|v| v != "synthetic-role-token")
        {
            state.unsigned.store(true, Ordering::SeqCst);
        }
        "AWS4-HMAC-SHA256 Credential=ASIAFIXTURE1234567/"
    } else {
        "AWS4-HMAC-SHA256 Credential=synthetic-access/"
    };
    if !request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with(expected_auth))
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
        if let Some(checksum) = checksum {
            let actual = crc32c_reference(&data);
            state
                .checksums
                .lock()
                .unwrap()
                .push((key.clone(), query.clone(), checksum.clone()));
            if checksum != actual {
                return Response::builder()
                    .status(400)
                    .body(Body::from("<Error><Code>BadDigest</Code></Error>"))
                    .unwrap();
            }
        }
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
        layout: Layout::Publication,
        backend: Backend::S3 {
            credentials_file: None,
            assume_role: None,
            request_payer: false,
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
    for region in [Region::Jp, Region::Hk, Region::En, Region::Kr] {
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
        if region == Region::Hk {
            assert!(target.prefix.starts_with("assets/hk/publications/"));
            assert!(!json.contains("/tw/") && !json.contains(r#""tw""#));
        }
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
            default_acl: None,
            customer_key_base64_env: None,
            checksum_algorithm: None,
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

#[tokio::test]
async fn shared_upload_budget_bounds_publishers_and_releases_cancelled_waiters() {
    let source = fixture();
    let mut server = server().await;
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    server.config.service_upload_gate = Some(gate.clone());
    server.state.mode.store(4, Ordering::SeqCst);
    let (tx, rx) = watch::channel(false);
    let first = server.config.publish(source.path(), Region::Jp, rx.clone());
    let second_config = server.config.clone();
    let second = second_config.publish(source.path(), Region::Jp, rx);
    let cancel = async {
        while server.state.puts.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(server.state.puts.load(Ordering::SeqCst), 1);
        assert_eq!(gate.available_permits(), 0);
        tx.send(true).unwrap();
    };
    let (first, second, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(first, second, cancel)
    })
    .await
    .unwrap();
    assert!(matches!(first, Err(Error::Cancelled)));
    assert!(matches!(second, Err(Error::Cancelled)));
    assert_eq!(gate.available_permits(), 1);
    assert!(source.path().exists());
    server.state.mode.store(0, Ordering::SeqCst);
    server.state.gate.notify_waiters();
    let (_tx, rx) = watch::channel(false);
    server
        .config
        .publish(source.path(), Region::Jp, rx)
        .await
        .unwrap();
    assert_eq!(gate.available_permits(), 1);
}

#[tokio::test]
async fn shared_upload_admission_uses_attempt_deadline_before_any_write() {
    let source = fixture();
    let mut server = server().await;
    server.config.object_timeout_seconds = 1;
    server.config.attempts = 1;
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    let held = gate.clone().acquire_owned().await.unwrap();
    server.config.service_upload_gate = Some(gate.clone());
    let (_tx, rx) = watch::channel(false);
    let start = std::time::Instant::now();
    assert!(matches!(
        server.config.publish(source.path(), Region::Jp, rx).await,
        Err(Error::Transport)
    ));
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(server.state.puts.load(Ordering::SeqCst), 0);
    assert!(source.path().exists());
    drop(held);
    assert_eq!(gate.available_permits(), 1);
}

fn crc32c_reference(data: &[u8]) -> String {
    use base64::Engine;
    let mut crc = u32::MAX;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f63b78 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    base64::engine::general_purpose::STANDARD.encode((!crc).to_be_bytes())
}

#[tokio::test]
async fn s3_crc32c_and_requester_pays_cover_parts_markers_and_readback() {
    // Independent standard CRC32C check vector: 123456789 -> e3069283.
    assert_eq!(crc32c_reference(b"123456789"), "4waSgw==");
    let source = large_source();
    let mut server = server().await;
    if let Backend::S3 {
        request_payer,
        write_options,
        ..
    } = &mut server.config.providers[0].backend
    {
        *request_payer = true;
        write_options.checksum_algorithm = Some("crc32c".into());
    }
    let (_tx, rx) = watch::channel(false);
    let publication = server
        .config
        .publish(source.path(), Region::Jp, rx)
        .await
        .unwrap();
    assert!(publication.files >= 3);
    let checksums = server.state.checksums.lock().unwrap();
    assert!(checksums.iter().any(|(_, q, _)| q.contains("partNumber=")));
    assert!(checksums
        .iter()
        .any(|(p, _, _)| p.ends_with("complete.json")));
    assert!(checksums
        .iter()
        .any(|(p, _, _)| p.ends_with("summary.json")));
    let headers = server.state.payer_headers.lock().unwrap();
    assert!(headers
        .iter()
        .any(|(method, payer)| method == "GET" && *payer));
    assert!(headers
        .iter()
        .any(|(method, payer)| method == "PUT" && *payer));
    assert!(headers.iter().all(|(_, payer)| *payer));
    assert!(!server.state.unsigned.load(Ordering::SeqCst));
}

#[test]
fn s3_checksum_policy_rejects_multipart_incompatible_and_unknown_algorithms() {
    for algorithm in ["md5", "sha256", "CRC32C", "", "crc32c\n"] {
        let options = S3WriteOptions {
            checksum_algorithm: Some(algorithm.into()),
            ..Default::default()
        };
        assert!(options.validate().is_err());
    }
    assert!(S3WriteOptions::default().validate().is_ok());
}

#[tokio::test]
async fn s3_requester_policy_defaults_off_and_denied_readback_preserves_exports() {
    let source = fixture();
    let mut server = server().await;
    let (_tx, rx) = watch::channel(false);
    server
        .config
        .publish(source.path(), Region::Jp, rx.clone())
        .await
        .unwrap();
    assert!(server
        .state
        .payer_headers
        .lock()
        .unwrap()
        .iter()
        .all(|(_, payer)| !payer));
    assert!(server.state.checksums.lock().unwrap().is_empty());
    server.state.payer_headers.lock().unwrap().clear();
    if let Backend::S3 {
        request_payer,
        write_options,
        ..
    } = &mut server.config.providers[0].backend
    {
        *request_payer = true;
        write_options.checksum_algorithm = Some("crc32c".into());
    }
    server.config.remove_local_after_upload = true;
    server.state.mode.store(6, Ordering::SeqCst);
    assert!(server
        .config
        .publish(source.path(), Region::Jp, rx)
        .await
        .is_err());
    assert!(source.path().join("summary.json").is_file());
    assert!(server
        .state
        .payer_headers
        .lock()
        .unwrap()
        .iter()
        .all(|(_, payer)| *payer));
}

fn fixture_region(region: Region) -> tempfile::TempDir {
    let source = fixture();
    let file = source.path().join("summary.json");
    let mut summary: crate::export::ExportSummary =
        sonic_rs::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    summary.region = region;
    summary.platform = region.default_platform().name().into();
    std::fs::write(file, sonic_rs::to_vec(&summary).unwrap()).unwrap();
    source
}

#[tokio::test]
async fn storage_region_templates_match_preview_and_actual_local_and_s3_publication() {
    let destination = tempfile::tempdir().unwrap();
    let mut provider = local(destination.path().join("{region}"));
    provider.prefix = "release-{server}".into();
    provider.public_base_url = Some("https://cdn-{region}.example/store/{server}/".into());
    let local_config = config(provider);
    local_config.validate().unwrap();
    let mut server = server().await;
    if let Backend::S3 { bucket, .. } = &mut server.config.providers[0].backend {
        *bucket = "sirius-{region}".into();
    }
    server.config.providers[0].prefix = "release-{server}".into();
    for region in [Region::Jp, Region::Hk, Region::En, Region::Kr] {
        let source = fixture_region(region);
        let preview = local_config.plan(region).unwrap();
        let directory = destination.path().join(region.name());
        assert!(
            !directory.exists(),
            "preview must not create resolved destination"
        );
        let (_tx, rx) = watch::channel(false);
        let publication = local_config
            .publish(source.path(), region, rx.clone())
            .await
            .unwrap();
        let target = &publication.providers[0];
        let prefix = format!("release-{}/{}/publications/", region.name(), region.name());
        assert!(target.prefix.starts_with(&prefix));
        assert!(preview.providers[0].prefix.starts_with(&prefix));
        assert_eq!(
            target.public_url.as_deref(),
            Some(
                format!(
                    "https://cdn-{}.example/store/{}/{}/",
                    region.name(),
                    region.name(),
                    target.prefix
                )
                .as_str()
            )
        );
        assert!(directory
            .join(&target.prefix)
            .join("complete.json")
            .is_file());
        let published = server
            .config
            .publish(source.path(), region, rx)
            .await
            .unwrap();
        let key = format!(
            "/sirius-{}/{}/complete.json",
            region.name(),
            published.providers[0].prefix
        );
        assert!(server.state.objects.lock().unwrap().contains_key(&key));
    }
    assert!(!destination.path().join("{region}").exists());
    assert!(!server.state.unsigned.load(Ordering::SeqCst));
    assert!(local_config.plan(Region::Cn).is_err());
    let source = fixture();
    let (_tx, rx) = watch::channel(false);
    assert!(matches!(
        local_config.publish(source.path(), Region::Cn, rx).await,
        Err(Error::ReservedRegion)
    ));
    assert!(!destination.path().join("cn").exists());
}

#[test]
fn storage_region_templates_validate_resolved_urls_and_reject_unknown_tokens() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(local(root.path().join("{region}")));
    for value in [
        "{unknown}",
        "{REGION}",
        "{region",
        "{{region}}",
        "../{region}",
    ] {
        cfg.providers[0].prefix = value.into();
        assert!(cfg.validate().is_err());
        assert!(cfg.plan(Region::Jp).is_err());
    }
    cfg.providers[0].prefix = "assets".into();
    for value in [
        "https://{unknown}.example/",
        "https://{region}:password@example/",
        "http://cdn-{region}.example/",
        "https://cdn.example/?region={region}",
    ] {
        cfg.providers[0].public_base_url = Some(value.into());
        assert!(cfg.validate().is_err());
    }
    cfg.providers[0].public_base_url = Some("https://{region}.example/path/{server}/".into());
    cfg.validate().unwrap();
    assert_eq!(
        cfg.providers[0]
            .resolved(Region::Kr)
            .unwrap()
            .public_base_url
            .as_deref(),
        Some("https://kr.example/path/kr/")
    );
}

#[tokio::test]
async fn storage_region_templates_do_not_expand_credentials_or_bypass_overlap_checks() {
    let source = fixture();
    let provider = local(source.path().join("{region}"));
    let cfg = config(provider);
    let (_tx, rx) = watch::channel(false);
    assert!(cfg.publish(source.path(), Region::Jp, rx).await.is_err());
    assert!(!source.path().join("jp").exists());
    let mut server = server().await;
    if let Backend::S3 { endpoint, .. } = &mut server.config.providers[0].backend {
        *endpoint = "https://storage-{region}.example".into();
    }
    server.config.validate().unwrap();
    if let Backend::S3 { endpoint, .. } =
        &server.config.resolved(Region::En).unwrap().providers[0].backend
    {
        assert_eq!(endpoint, "https://storage-en.example");
    }
    if let Backend::S3 {
        access_key_id_env, ..
    } = &mut server.config.providers[0].backend
    {
        *access_key_id_env = "SIRIUS_{region}_STORAGE_KEY".into();
    }
    assert!(server.config.validate().is_err());
    assert!(server.state.objects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn s3_customer_key_reaches_multipart_and_verification_without_receipt_leaks() {
    // Synthetic bytes 0..31; independently calculated base64 and MD5/base64.
    let encoded = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
    let md5 = "tP/LI3N87DFaSk0aoqYgzg==";
    let env = format!("SIRIUS_TEST_SSEC_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&env, encoded);
    let source = large_source();
    let mut server = server().await;
    if let Backend::S3 { write_options, .. } = &mut server.config.providers[0].backend {
        write_options.customer_key_base64_env = Some(env.clone());
    }
    let (_tx, rx) = watch::channel(false);
    let publication = server
        .config
        .publish(source.path(), Region::Jp, rx)
        .await
        .unwrap();
    assert!(server.state.multipart_puts.load(Ordering::SeqCst) >= 2);
    let headers = server.state.customer_headers.lock().unwrap();
    assert!(headers.iter().any(|(method, ..)| method == "GET"));
    assert!(headers.iter().any(|(method, ..)| method == "PUT"));
    assert!(headers.iter().any(|(method, ..)| method == "POST"));
    for (_, algorithm, key, digest) in headers.iter() {
        assert_eq!(algorithm, "AES256");
        assert_eq!(key, encoded);
        assert_eq!(digest, md5);
    }
    let receipt = sonic_rs::to_string(&publication).unwrap();
    assert!(!receipt.contains(encoded) && !receipt.contains(&env));
    for (path, bytes) in server.state.objects.lock().unwrap().iter() {
        if path.ends_with(".json") {
            assert!(!String::from_utf8_lossy(bytes).contains(encoded));
        }
    }
    std::env::remove_var(env);
}

#[test]
fn s3_customer_key_rejects_wrong_size_encoding_and_mixed_encryption_modes() {
    use base64::Engine;
    let env = format!("SIRIUS_TEST_SSEC_CONFIG_{}", uuid::Uuid::new_v4().simple());
    let mut options = S3WriteOptions {
        customer_key_base64_env: Some(env.clone()),
        ..Default::default()
    };
    for value in [
        "".to_string(),
        "not-base64".into(),
        base64::engine::general_purpose::STANDARD.encode([0; 16]),
        base64::engine::general_purpose::STANDARD.encode([0; 33]),
    ] {
        std::env::set_var(&env, value);
        assert!(options.validate().is_err());
    }
    std::env::set_var(
        &env,
        base64::engine::general_purpose::STANDARD.encode([0; 32]),
    );
    options.validate().unwrap();
    for algorithm in ["AES256", "aws:kms"] {
        options.server_side_encryption = Some(algorithm.into());
        assert!(options.validate().is_err());
    }
    options.server_side_encryption = None;
    options.kms_key_id_env = Some("NO_SUCH_KEY".into());
    assert!(options.validate().is_err());
    std::env::remove_var(env);
}

#[tokio::test]
async fn s3_default_acl_policies_reach_objects_multipart_and_markers_with_exclusion_priority() {
    for acl in [
        "private",
        "public-read",
        "public-read-write",
        "authenticated-read",
        "aws-exec-read",
        "bucket-owner-read",
        "bucket-owner-full-control",
    ] {
        let public = matches!(acl, "public-read" | "public-read-write");
        let source = if acl == "bucket-owner-full-control" {
            large_source()
        } else {
            fixture()
        };
        let mut server = server().await;
        if let Backend::S3 {
            write_options,
            public_read_include,
            public_read_exclude,
            ..
        } = &mut server.config.providers[0].backend
        {
            write_options.default_acl = Some(acl.into());
            *public_read_include = vec![r"^summary\.json$".into(), r"^resources\.jsonl$".into()];
            *public_read_exclude = vec![r"^resources\.jsonl$".into()];
        }
        let (_tx, rx) = watch::channel(false);
        let result = server
            .config
            .publish(source.path(), Region::Jp, rx)
            .await
            .unwrap();
        for file in [
            "00000/payload.bin",
            "summary.json",
            "resources.jsonl",
            "complete.json",
        ] {
            let expected = if file == "resources.jsonl" {
                if public {
                    "private"
                } else {
                    acl
                }
            } else if public {
                acl
            } else if file == "summary.json" {
                "public-read"
            } else {
                acl
            };
            let key = format!("/synthetic-bucket/{}/{file}", result.providers[0].prefix);
            assert_eq!(
                server
                    .state
                    .acls
                    .lock()
                    .unwrap()
                    .get(&key)
                    .unwrap()
                    .as_deref(),
                Some(expected),
                "ACL {acl}, file {file}"
            );
        }
        if acl == "bucket-owner-full-control" {
            assert!(server.state.multipart_puts.load(Ordering::SeqCst) >= 2);
        }
        assert!(!server.state.unsigned.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn s3_default_acl_rejects_unknown_values_and_preserves_source_on_backend_denial() {
    for acl in [
        "",
        "public",
        "bucket-owner-full-control\n",
        "log-delivery-write",
    ] {
        let options = S3WriteOptions {
            default_acl: Some(acl.into()),
            ..Default::default()
        };
        assert!(options.validate().is_err());
    }
    let source = fixture();
    let mut server = server().await;
    if let Backend::S3 { write_options, .. } = &mut server.config.providers[0].backend {
        write_options.default_acl = Some("bucket-owner-full-control".into());
    }
    server.config.remove_local_after_upload = true;
    server.state.mode.store(3, Ordering::SeqCst);
    let (_tx, rx) = watch::channel(false);
    assert!(server
        .config
        .publish(source.path(), Region::Jp, rx)
        .await
        .is_err());
    assert!(source.path().join("summary.json").is_file());
    assert!(server
        .state
        .acls
        .lock()
        .unwrap()
        .values()
        .all(|acl| acl.as_deref() == Some("bucket-owner-full-control")));
    assert!(!server
        .state
        .objects
        .lock()
        .unwrap()
        .keys()
        .any(|k| k.ends_with("complete.json")));
}

#[tokio::test]
async fn s3_assume_role_publishes_with_temporary_keys_and_bounds_stalled_acquisition() {
    for stalled in [false, true] {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new().fallback({ let hits = hits.clone(); move |headers: axum::http::HeaderMap| { let hits = hits.clone(); async move {
            assert!(headers["authorization"].to_str().unwrap().contains("Credential=synthetic-access/"));
            hits.fetch_add(1, Ordering::SeqCst);
            if stalled { tokio::time::sleep(Duration::from_secs(30)).await; }
            let expiration = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
            format!("<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>ASIAFIXTURE1234567</AccessKeyId><SecretAccessKey>synthetic-role-secret</SecretAccessKey><SessionToken>synthetic-role-token</SessionToken><Expiration>{expiration}</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>")
        }}});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let sts_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let source = fixture();
        let mut storage = server().await;
        storage.state.role_credentials.store(true, Ordering::SeqCst);
        storage.config.attempts = 1;
        storage.config.object_timeout_seconds = if stalled { 1 } else { 5 };
        storage.config.remove_local_after_upload = stalled;
        if let Backend::S3 { assume_role, .. } = &mut storage.config.providers[0].backend {
            *assume_role = Some(Box::new(crate::storage_sts::Config {
                role_arn: "arn:aws:iam::123456789012:role/fixture".into(),
                region: "us-east-1".into(),
                session_name: "sirius-fixture".into(),
                external_id_env: None,
                duration_seconds: 3600,
                test_endpoint: Some(origin),
            }));
        }
        let credential_root = tempfile::tempdir().unwrap();
        if !stalled {
            let file = credential_root.path().join("credentials");
            std::fs::write(&file, "[default]\naws_access_key_id=synthetic-access\naws_secret_access_key=synthetic-secret\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            if let Backend::S3 {
                credentials_file,
                access_key_id_env,
                secret_access_key_env,
                ..
            } = &mut storage.config.providers[0].backend
            {
                *credentials_file = Some(Box::new(crate::storage_credentials::Config {
                    path: file,
                    profile: "default".into(),
                    refresh_seconds: 60,
                }));
                access_key_id_env.clear();
                secret_access_key_env.clear();
            }
        }
        storage.config.plan(Region::Jp).unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        let (_tx, rx) = watch::channel(false);
        let result = tokio::time::timeout(
            Duration::from_secs(4),
            storage.config.publish(source.path(), Region::Jp, rx),
        )
        .await
        .unwrap();
        if stalled {
            assert!(result.is_err());
            assert!(storage.state.objects.lock().unwrap().is_empty());
            assert!(source.path().join("summary.json").is_file());
        } else {
            let receipt = sonic_rs::to_string(&result.unwrap()).unwrap();
            assert!(!receipt.contains("synthetic-role") && !receipt.contains("ASIAFIXTURE"));
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            assert!(!storage.state.unsigned.load(Ordering::SeqCst));
        }
        sts_task.abort();
    }
}

#[tokio::test]
async fn s3_shared_file_publication_selects_explicit_profile_and_rejects_mixed_sources() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("credentials");
    std::fs::write(&path, "[unused]\naws_access_key_id=wrong\naws_secret_access_key=wrong\n[publisher]\naws_access_key_id=synthetic-access\naws_secret_access_key=synthetic-secret\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let source = fixture();
    let mut server = server().await;
    if let Backend::S3 {
        credentials_file,
        access_key_id_env,
        secret_access_key_env,
        ..
    } = &mut server.config.providers[0].backend
    {
        *credentials_file = Some(Box::new(crate::storage_credentials::Config {
            path,
            profile: "publisher".into(),
            refresh_seconds: 60,
        }));
        access_key_id_env.clear();
        secret_access_key_env.clear();
    }
    let (_tx, rx) = watch::channel(false);
    server
        .config
        .publish(source.path(), Region::Jp, rx)
        .await
        .unwrap();
    assert!(!server.state.unsigned.load(Ordering::SeqCst));
    if let Backend::S3 {
        access_key_id_env, ..
    } = &mut server.config.providers[0].backend
    {
        *access_key_id_env = server.env[0].clone();
    }
    assert!(server.config.validate().is_err());
}

type FixtureFile = (
    &'static str,
    &'static str,
    &'static [u8],
    &'static str,
    Option<(i32, &'static str, &'static str)>,
);
/// A synthetic export with Unity objects and one CRI ACB. `files` are (resource dir, file name,
/// bytes, kind, object: (class_id, name, container)). No game data is used.
fn stable_fixture(files: &[FixtureFile]) -> tempfile::TempDir {
    use crate::export::ExportSummary;
    let root = tempfile::tempdir().unwrap();
    let mut journal = Vec::new();
    let mut dirs: Vec<&str> = files.iter().map(|f| f.0).collect();
    dirs.dedup();
    let (mut total, mut bytes, mut objects) = (0, 0u64, 0);
    let mut payloads = BTreeMap::new();
    for dir in dirs {
        std::fs::create_dir_all(root.path().join(dir)).unwrap();
        let mut outputs = Vec::new();
        let mut source = format!("bundle_{dir}");
        for (d, name, data, kind, object) in files.iter().filter(|f| f.0 == dir) {
            std::fs::write(root.path().join(d).join(name), data).unwrap();
            let mut output = sonic_rs::json!({"path": name, "kind": kind, "bytes": data.len(),
                "sha256": hex::encode(Sha256::digest(data))});
            if let Some((class_id, object_name, container)) = object {
                objects += 1;
                output["object"] = sonic_rs::json!({"source_file": "b.bundle::CAB", "path_id": objects,
                    "class_id": class_id, "name": object_name, "container": container});
            } else {
                source = format!(
                    "cri_assets_cri/sound/voice_{}",
                    "0123456789abcdef".repeat(2)
                );
            }
            total += 1;
            bytes += data.len() as u64;
            *payloads.entry(kind.to_string()).or_insert(0) += 1;
            outputs.push(output);
        }
        let unity = outputs.iter().filter(|o| o.get("object").is_some()).count();
        let report = sonic_rs::json!({"source": source, "source_sha256": "a".repeat(64),
            "output_directory": dir, "objects": unity, "selected_objects": unity,
            "skipped_objects": 0, "errors": [], "outputs": outputs});
        journal.extend(sonic_rs::to_vec(&report).unwrap());
        journal.push(b'\n');
    }
    std::fs::write(root.path().join("resources.jsonl"), journal).unwrap();
    let resources = files.iter().map(|f| f.0).collect::<HashSet<_>>().len();
    let summary = ExportSummary {
        schema_version: 4,
        region: Region::Hk,
        platform: "Android".into(),
        complete: true,
        retained: true,
        input_files: resources,
        catalog_files: resources,
        succeeded: resources,
        output_files: total,
        output_bytes: bytes,
        unity_objects: objects,
        selected_unity_objects: objects,
        catalog_sha256: "c".repeat(64),
        full_catalog: true,
        full_export: true,
        payloads,
        ..Default::default()
    };
    std::fs::write(
        root.path().join("summary.json"),
        sonic_rs::to_vec(&summary).unwrap(),
    )
    .unwrap();
    root
}
fn stable_files(back: &'static [u8], with_extra: bool) -> Vec<FixtureFile> {
    const C: &str = "Assets/AddressableResources/Adv/Chat/back.png";
    let mut files = vec![
        ("00000", "0_1.png", back, "image_png", Some((28, "back", C))),
        (
            "00000",
            "0_2.png",
            b"sprite" as &[u8],
            "image_png",
            Some((213, "back", C)),
        ),
        ("00001", "00000.wav", b"RIFF-voice-1", "hca_wav", None),
        (
            "00001",
            "00000.cues.json",
            br#"[["voice_001",0,0]]"#,
            "cue_metadata",
            None,
        ),
    ];
    if with_extra {
        files.push((
            "00002",
            "0_3.json",
            b"{}",
            "typetree_json",
            Some((1, "Extra", "Assets/AddressableResources/Ui/Extra.prefab")),
        ));
    }
    files
}
use sonic_rs::JsonValueTrait as _;
fn stable(provider: Provider, prune: bool) -> Provider {
    Provider {
        prefix: "hk".into(),
        layout: Layout::Stable {
            strip_prefixes: stable_strip_prefixes(),
            lowercase: true,
            prune,
        },
        ..provider
    }
}
#[tokio::test]
async fn stable_layout_publishes_readable_keys_and_uploads_only_changes() {
    let destination = tempfile::tempdir().unwrap();
    let root = destination.path().join("hk");
    let mut config = config(stable(local(destination.path().into()), false));
    config.release = Some(ReleaseInfo {
        resource_version: "1.0.0.104".into(),
        platform_hash: "d".repeat(32),
        catalog_sha256: "c".repeat(64),
    });
    let (_tx, rx) = watch::channel(false);
    let first = stable_fixture(&stable_files(b"texture-v1", true));
    let result = config
        .publish(first.path(), Region::Hk, rx.clone())
        .await
        .unwrap();
    let target = &result.providers[0];
    assert_eq!((target.prefix.as_str(), target.layout), ("hk", "stable"));
    let counts = target.stable.unwrap();
    assert_eq!(
        (
            counts.files,
            counts.uploaded,
            counts.unchanged,
            counts.pruned
        ),
        (5, 5, 0, 0)
    );
    assert_eq!(
        std::fs::read(root.join("adv/chat/back.png")).unwrap(),
        b"texture-v1"
    );
    assert_eq!(
        std::fs::read(root.join("adv/chat/back.assets/sprite/back.png")).unwrap(),
        b"sprite"
    );
    assert_eq!(
        std::fs::read(root.join("cri/sound/voice/voice_001.wav")).unwrap(),
        b"RIFF-voice-1"
    );
    assert!(root.join("cri/sound/voice/voice_001.cues.json").is_file());
    assert!(root.join("ui/extra.assets/gameobject/extra.json").is_file());
    // No numbered directories, publication UUIDs or completion marker in the stable tree.
    assert!(!root.join("00000").exists() && !root.join("complete.json").exists());
    let manifest = std::fs::read_to_string(root.join("_sirius/files.jsonl")).unwrap();
    assert_eq!(manifest.lines().count(), 5);
    assert!(!manifest.contains("\"local\"") && manifest.contains("\"source\""));
    let version: sonic_rs::Value =
        sonic_rs::from_slice(&std::fs::read(root.join("_sirius/version.json")).unwrap()).unwrap();
    assert_eq!(version["layout"].as_str(), Some("stable"));
    assert_eq!(
        version["release"]["resource_version"].as_str(),
        Some("1.0.0.104")
    );
    assert_eq!(version["publication_id"].as_str(), Some(result.id.as_str()));
    assert!(root.join("_sirius/summary.json").is_file());

    // Unchanged rerun uploads nothing; a changed texture uploads exactly that file.
    let result = config
        .publish(first.path(), Region::Hk, rx.clone())
        .await
        .unwrap();
    let counts = result.providers[0].stable.unwrap();
    assert_eq!((counts.uploaded, counts.unchanged), (0, 5));
    let second = stable_fixture(&stable_files(b"texture-v2", false));
    let result = config
        .publish(second.path(), Region::Hk, rx.clone())
        .await
        .unwrap();
    let counts = result.providers[0].stable.unwrap();
    assert_eq!(
        (
            counts.files,
            counts.uploaded,
            counts.unchanged,
            counts.pruned
        ),
        (4, 1, 3, 0)
    );
    assert_eq!(
        std::fs::read(root.join("adv/chat/back.png")).unwrap(),
        b"texture-v2"
    );
    // Without pruning a removed resource stays published but leaves the manifest.
    assert!(root.join("ui/extra.assets/gameobject/extra.json").is_file());
    assert_eq!(
        std::fs::read_to_string(root.join("_sirius/files.jsonl"))
            .unwrap()
            .lines()
            .count(),
        4
    );

    // With pruning, keys from the previous manifest that are no longer produced are deleted.
    let third = stable_fixture(&stable_files(b"texture-v2", true));
    config
        .publish(third.path(), Region::Hk, rx.clone())
        .await
        .unwrap();
    config.providers[0] = stable(local(destination.path().into()), true);
    let result = config.publish(second.path(), Region::Hk, rx).await.unwrap();
    let counts = result.providers[0].stable.unwrap();
    assert_eq!((counts.uploaded, counts.pruned), (0, 1));
    assert!(!root.join("ui/extra.assets/gameobject/extra.json").exists());
    assert!(root.join("adv/chat/back.png").is_file());
}
#[tokio::test]
async fn stable_layout_rejects_plans_outside_the_verified_inventory() {
    let destination = tempfile::tempdir().unwrap();
    let config = config(stable(local(destination.path().into()), false));
    let source = stable_fixture(&stable_files(b"texture-v1", false));
    // A journal that names a file the export does not contain fails verification.
    let journal = std::fs::read_to_string(source.path().join("resources.jsonl")).unwrap();
    std::fs::write(
        source.path().join("resources.jsonl"),
        journal.replace("0_2.png", "0_9.png"),
    )
    .unwrap();
    let (_tx, rx) = watch::channel(false);
    assert!(config.publish(source.path(), Region::Hk, rx).await.is_err());
    assert!(!destination.path().join("hk/_sirius/version.json").exists());
}
#[tokio::test]
async fn stable_layout_over_s3_treats_a_missing_manifest_as_first_publication() {
    let mut server = server().await;
    let provider = server.config.providers[0].clone();
    server.config.providers[0] = stable(provider, false);
    let source = stable_fixture(&stable_files(b"texture-v1", true));
    let (_tx, rx) = watch::channel(false);
    let result = server
        .config
        .publish(source.path(), Region::Hk, rx.clone())
        .await
        .unwrap();
    assert_eq!(result.providers[0].stable.unwrap().uploaded, 5);
    {
        let objects = server.state.objects.lock().unwrap();
        for key in [
            "/synthetic-bucket/hk/adv/chat/back.png",
            "/synthetic-bucket/hk/cri/sound/voice/voice_001.wav",
            "/synthetic-bucket/hk/_sirius/files.jsonl",
            "/synthetic-bucket/hk/_sirius/version.json",
        ] {
            assert!(objects.contains_key(key), "{key}");
        }
        assert!(!objects
            .keys()
            .any(|k| k.contains("/publications/") || k.ends_with("complete.json")));
    }
    let puts = server.state.puts.load(Ordering::SeqCst);
    let result = server
        .config
        .publish(source.path(), Region::Hk, rx)
        .await
        .unwrap();
    assert_eq!(result.providers[0].stable.unwrap().uploaded, 0);
    // Only the manifest, summary and version objects are rewritten.
    assert_eq!(server.state.puts.load(Ordering::SeqCst) - puts, 3);
}
