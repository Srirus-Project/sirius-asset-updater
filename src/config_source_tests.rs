use super::*;
use axum::{body::Body, extract::State, http::header, response::Response, Router};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
const EXAMPLE: &str = include_str!("../sirius-asset-config.example.yaml");
const ACCESS: &str = "synthetic-config-access";
const SECRET: &str = "synthetic-config-secret-value";

#[derive(Default)]
struct Fake {
    objects: Mutex<HashMap<String, Vec<u8>>>,
    stall: AtomicBool,
    requests: AtomicUsize,
    unsigned: AtomicBool,
}
async fn handle(State(state): State<Arc<Fake>>, request: axum::extract::Request) -> Response {
    state.requests.fetch_add(1, Ordering::SeqCst);
    if state.stall.load(Ordering::SeqCst) {
        std::future::pending::<()>().await;
    }
    if !request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with(&format!("AWS4-HMAC-SHA256 Credential={ACCESS}/")))
    {
        state.unsigned.store(true, Ordering::SeqCst);
        return Response::builder()
            .status(403)
            .body(Body::from("<Error><Code>AccessDenied</Code></Error>"))
            .unwrap();
    }
    let objects = state.objects.lock().unwrap();
    let Some(data) = objects.get(request.uri().path()) else {
        return Response::builder().status(404).body(Body::empty()).unwrap();
    };
    if request.method() == "HEAD" {
        return Response::builder()
            .header(header::CONTENT_LENGTH, data.len())
            .body(Body::empty())
            .unwrap();
    }
    if let Some(range) = request.headers().get(header::RANGE) {
        let range = range.to_str().unwrap().strip_prefix("bytes=").unwrap();
        let (a, b) = range.split_once('-').unwrap();
        let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
        return Response::builder()
            .status(206)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {a}-{b}/{}", data.len()),
            )
            .body(Body::from(data[a..=b].to_vec()))
            .unwrap();
    }
    Response::builder().body(Body::from(data.clone())).unwrap()
}
struct S3 {
    state: Arc<Fake>,
    vars: Vec<(String, String)>,
    task: tokio::task::JoinHandle<()>,
    env: [String; 2],
}
impl Drop for S3 {
    fn drop(&mut self) {
        self.task.abort();
        for name in &self.env {
            std::env::remove_var(name);
        }
    }
}
async fn s3(key: &str) -> S3 {
    let state = Arc::new(Fake::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .fallback(axum::routing::any(handle))
        .with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let env = [
        format!("SIRIUS_CONFIG_ACCESS_{}", uuid::Uuid::new_v4().simple()),
        format!("SIRIUS_CONFIG_SECRET_{}", uuid::Uuid::new_v4().simple()),
    ];
    std::env::set_var(&env[0], ACCESS);
    std::env::set_var(&env[1], SECRET);
    let vars = vars(&[
        (URI_ENV, &format!("opendal://s3/{key}")),
        ("SIRIUS_ASSET_CONFIG_SOURCE__S3__ENDPOINT", &endpoint),
        ("SIRIUS_ASSET_CONFIG_SOURCE__S3__BUCKET", "config-bucket"),
        ("SIRIUS_ASSET_CONFIG_SOURCE__S3__REGION", "test-region"),
        ("SIRIUS_ASSET_CONFIG_SOURCE__S3__ACCESS_KEY_ID_ENV", &env[0]),
        (
            "SIRIUS_ASSET_CONFIG_SOURCE__S3__SECRET_ACCESS_KEY_ENV",
            &env[1],
        ),
    ]);
    S3 {
        state,
        vars,
        task,
        env,
    }
}
fn fs_vars(root: &std::path::Path, key: &str) -> Vec<(String, String)> {
    vars(&[
        (URI_ENV, &format!("opendal://fs/{key}")),
        (
            "SIRIUS_ASSET_CONFIG_SOURCE__FS__ROOT",
            root.to_str().unwrap(),
        ),
    ])
}
fn with(mut base: Vec<(String, String)>, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    base.extend(vars(extra));
    base
}

#[tokio::test]
async fn fs_source_loads_download_config_and_applies_overrides_after_fetch() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("jp")).unwrap();
    std::fs::write(root.path().join("jp/sirius.yaml"), EXAMPLE).unwrap();
    let base = fs_vars(root.path(), "jp/sirius.yaml");
    let plain: crate::Config = load_with(&base).await.unwrap();
    let expected: crate::Config = yaml_serde::from_str(EXAMPLE).unwrap();
    assert_eq!(plain.output, expected.output);
    let overridden: crate::Config = load_with(&with(
        base.clone(),
        &[("SIRIUS_ASSET__OUTPUT", "/srv/overridden-output")],
    ))
    .await
    .unwrap();
    assert_eq!(
        overridden.output,
        std::path::PathBuf::from("/srv/overridden-output")
    );
    // Overrides are still typed: an unknown path fails after a successful fetch.
    assert!(matches!(
        load_with::<crate::Config>(&with(base, &[("SIRIUS_ASSET__NOT_A_FIELD", "1")])).await,
        Err(Error::Config)
    ));
}

#[tokio::test]
async fn s3_source_signs_with_referenced_credentials_and_reads_snapshot() {
    let server = s3("configs/jp.yaml").await;
    server.state.objects.lock().unwrap().insert(
        "/config-bucket/configs/jp.yaml".into(),
        EXAMPLE.as_bytes().to_vec(),
    );
    let text = read_with(&server.vars).await.unwrap();
    assert_eq!(text, EXAMPLE);
    assert!(!server.state.unsigned.load(Ordering::SeqCst));
    let config: crate::Config = load_with(&with(
        server.vars.clone(),
        &[("SIRIUS_ASSET__OUTPUT", "/srv/s3-output")],
    ))
    .await
    .unwrap();
    assert_eq!(config.output, std::path::PathBuf::from("/srv/s3-output"));
}

#[tokio::test]
async fn size_limit_missing_object_directory_and_encoding_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("big.yaml"),
        vec![b'#'; MAX_BYTES as usize + 1],
    )
    .unwrap();
    std::fs::write(root.path().join("edge.yaml"), {
        let mut v = b"a: 1\n".to_vec();
        v.resize(MAX_BYTES as usize, b'\n');
        v
    })
    .unwrap();
    std::fs::write(root.path().join("empty.yaml"), b"").unwrap();
    std::fs::write(root.path().join("latin1.yaml"), [0xff, 0xfe]).unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    let read = |key: &str| read_with_owned(fs_vars(root.path(), key));
    assert!(matches!(read("big.yaml").await, Err(Error::Size)));
    assert_eq!(read("edge.yaml").await.unwrap().len(), MAX_BYTES as usize);
    assert!(matches!(
        read("missing.yaml").await,
        Err(Error::RemoteConfig)
    ));
    assert!(matches!(read("dir").await, Err(Error::RemoteConfig)));
    assert!(matches!(read("empty.yaml").await, Err(Error::Config)));
    assert!(matches!(read("latin1.yaml").await, Err(Error::Config)));

    let server = s3("absent.yaml").await;
    server.state.objects.lock().unwrap().insert(
        "/config-bucket/huge.yaml".into(),
        vec![b'#'; MAX_BYTES as usize + 1],
    );
    assert!(matches!(
        read_with(&server.vars).await,
        Err(Error::RemoteConfig)
    ));
    let huge = with(
        server
            .vars
            .iter()
            .filter(|(k, _)| k != URI_ENV)
            .cloned()
            .collect(),
        &[(URI_ENV, "opendal://s3/huge.yaml")],
    );
    assert!(matches!(read_with(&huge).await, Err(Error::Size)));
}
async fn read_with_owned(vars: Vec<(String, String)>) -> Result<String, Error> {
    read_with(&vars).await
}

#[tokio::test]
async fn stalled_remote_source_is_bounded_by_timeout() {
    let server = s3("configs/jp.yaml").await;
    server.state.stall.store(true, Ordering::SeqCst);
    let vars = with(
        server.vars.clone(),
        &[("SIRIUS_ASSET_CONFIG_SOURCE__TIMEOUT_SECONDS", "1")],
    );
    let started = std::time::Instant::now();
    assert!(matches!(read_with(&vars).await, Err(Error::Transport)));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(server.state.requests.load(Ordering::SeqCst) >= 1);
}

#[test]
fn location_precedence_and_conflicting_variables() {
    let root = tempfile::tempdir().unwrap();
    let remote = fs_vars(root.path(), "a.yaml");
    assert!(matches!(
        locate(&[]).unwrap(),
        Location::Local(p) if p == std::path::Path::new(DEFAULT_PATH)
    ));
    assert!(matches!(
        locate(&vars(&[(PATH_ENV, "/etc/sirius.yaml")])).unwrap(),
        Location::Local(p) if p == std::path::Path::new("/etc/sirius.yaml")
    ));
    // Blank URI is treated as unset, like the original loader.
    assert!(matches!(
        locate(&vars(&[(URI_ENV, "  "), (PATH_ENV, "/etc/sirius.yaml")])).unwrap(),
        Location::Local(_)
    ));
    assert!(matches!(locate(&remote).unwrap(), Location::Remote(_)));
    let rejected: Vec<Vec<(String, String)>> = vec![
        // Both locations set: no silent precedence.
        with(remote.clone(), &[(PATH_ENV, "/etc/sirius.yaml")]),
        // Bootstrap without a URI must not fall back to a local file.
        vars(&[("SIRIUS_ASSET_CONFIG_SOURCE__FS__ROOT", "/srv")]),
        // Section must match the URI backend, and only one section is allowed.
        vars(&[
            (URI_ENV, "opendal://s3/a.yaml"),
            ("SIRIUS_ASSET_CONFIG_SOURCE__FS__ROOT", "/srv"),
        ]),
        with(
            remote.clone(),
            &[("SIRIUS_ASSET_CONFIG_SOURCE__S3__BUCKET", "b")],
        ),
        vars(&[(URI_ENV, "opendal://fs/a.yaml")]),
        // deny_unknown_fields and bounded timeout.
        with(
            remote.clone(),
            &[("SIRIUS_ASSET_CONFIG_SOURCE__FS__DIRECTORY", "/srv")],
        ),
        with(
            remote.clone(),
            &[("SIRIUS_ASSET_CONFIG_SOURCE__TIMEOUT_SECONDS", "0")],
        ),
        with(
            remote.clone(),
            &[("SIRIUS_ASSET_CONFIG_SOURCE__TIMEOUT_SECONDS", "301")],
        ),
        vars(&[
            (URI_ENV, "opendal://fs/a.yaml"),
            ("SIRIUS_ASSET_CONFIG_SOURCE__FS__ROOT", "relative/root"),
        ]),
        vars(&[
            (URI_ENV, "opendal://fs/a.yaml"),
            ("SIRIUS_ASSET_CONFIG_SOURCE__FS__ROOT", "/srv/../etc"),
        ]),
        // Remote S3 endpoints must be HTTPS; plaintext is only for loopback fixtures.
        vars(&[
            (URI_ENV, "opendal://s3/a.yaml"),
            (
                "SIRIUS_ASSET_CONFIG_SOURCE__S3__ENDPOINT",
                "http://example.com",
            ),
            ("SIRIUS_ASSET_CONFIG_SOURCE__S3__BUCKET", "b"),
            ("SIRIUS_ASSET_CONFIG_SOURCE__S3__REGION", "r"),
        ]),
    ];
    for case in rejected {
        assert!(matches!(locate(&case), Err(Error::Config)));
    }
}

#[test]
fn uri_validation_rejects_credentials_queries_and_unsafe_keys() {
    assert_eq!(
        parse_uri("opendal://s3/configs/jp.yaml").unwrap(),
        (Scheme::S3, "configs/jp.yaml".into())
    );
    assert_eq!(
        parse_uri("opendal://fs/a.yaml").unwrap(),
        (Scheme::Fs, "a.yaml".into())
    );
    let long_key = format!("opendal://s3/{}", "a/".repeat(600));
    for uri in [
        "",
        "s3://bucket/key.yaml",
        "file:///etc/sirius.yaml",
        "https://bucket.example/key.yaml",
        "opendal://s3",
        "opendal://s3/",
        "opendal:///key.yaml",
        "opendal://gcs/key.yaml",
        "opendal://S3/key.yaml",
        "opendal://AKIAEXAMPLE:very-secret@s3/key.yaml",
        "opendal://s3/key.yaml?X-Amz-Signature=very-secret",
        "opendal://s3/key.yaml#frag",
        "opendal://s3/a%2F..%2Fb.yaml",
        "opendal://s3/../key.yaml",
        "opendal://s3/a/./key.yaml",
        "opendal://s3/a//key.yaml",
        "opendal://s3/a\\key.yaml",
        "opendal://s3/a key.yaml",
        "opendal://s3/k\u{e9}y.yaml",
        long_key.as_str(),
    ] {
        assert!(matches!(parse_uri(uri), Err(Error::Config)), "{uri}");
    }
}

#[tokio::test]
async fn errors_never_leak_uri_key_endpoint_or_credentials() {
    let server = s3("private/secret-path.yaml").await;
    let endpoint = server.vars[1].1.clone();
    let mut texts = Vec::new();
    // Missing object.
    texts.push(read_with(&server.vars).await.unwrap_err().to_string());
    // Wrong credentials are refused by the server.
    std::env::set_var(&server.env[0], "wrong-access-key-id");
    texts.push(read_with(&server.vars).await.unwrap_err().to_string());
    // Unset credential reference.
    std::env::remove_var(&server.env[1]);
    let error = read_with(&server.vars).await.unwrap_err();
    assert!(matches!(error, Error::Secret));
    texts.push(error.to_string());
    // Userinfo/query secrets in the URI.
    for uri in [
        "opendal://AKIAEXAMPLE:very-secret@s3/private/secret-path.yaml",
        "opendal://s3/private/secret-path.yaml?X-Amz-Credential=very-secret",
    ] {
        let vars = with(
            server
                .vars
                .iter()
                .filter(|(k, _)| k != URI_ENV)
                .cloned()
                .collect(),
            &[(URI_ENV, uri)],
        );
        texts.push(read_with(&vars).await.unwrap_err().to_string());
    }
    for text in texts {
        for needle in [
            "secret-path",
            "very-secret",
            "AKIAEXAMPLE",
            SECRET,
            ACCESS,
            "wrong-access",
            &endpoint,
            "config-bucket",
            &server.env[0],
            &server.env[1],
        ] {
            assert!(!text.contains(needle), "{text:?} leaks {needle:?}");
        }
    }
}
