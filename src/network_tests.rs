use super::*;
use crate::proxy::ProxyConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct ProxyServer {
    config: ProxyConfig,
    seen: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for ProxyServer {
    fn drop(&mut self) {
        self.task.abort();
        std::env::remove_var(&self.config.url_env);
        if let Some(name) = &self.config.authorization_env {
            std::env::remove_var(name);
        }
    }
}
async fn proxy(auth: &str, response: Option<&'static str>) -> ProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let records = seen.clone();
    let task = tokio::spawn(async move {
        let mut jobs = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.unwrap(); let records = records.clone();
                    jobs.spawn(async move {
                        let mut bytes = Vec::new();
                        while !bytes.ends_with(b"\r\n\r\n") {
                            let Ok(byte) = stream.read_u8().await else { return; };
                            bytes.push(byte);
                            if bytes.len() > 8192 { return; }
                        }
                        let request = String::from_utf8(bytes).unwrap();
                        records.lock().unwrap().push(request.clone());
                        if let Some(response) = response {
                            if response == "stall" { std::future::pending::<()>().await; }
                            let _ = stream.write_all(response.as_bytes()).await; return;
                        }
                        let first = request.lines().next().unwrap();
                        let mut fields = first.split_whitespace();
                        let method = fields.next().unwrap(); let target = fields.next().unwrap();
                        if method == "CONNECT" {
                            let mut origin = tokio::net::TcpStream::connect(target).await.unwrap();
                            stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
                            let _ = tokio::io::copy_bidirectional(&mut stream, &mut origin).await;
                        } else {
                            let url = reqwest::Url::parse(target).unwrap();
                            let mut origin = tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port_or_known_default().unwrap())).await.unwrap();
                            let mut forwarded = format!("{method} {} HTTP/1.1\r\nConnection: close\r\n", url.path());
                            for line in request.lines().skip(1) {
                                let lower = line.to_ascii_lowercase();
                                if !line.is_empty() && !lower.starts_with("proxy-") && !lower.starts_with("connection:") {
                                    forwarded.push_str(line); forwarded.push_str("\r\n");
                                }
                            }
                            forwarded.push_str("\r\n");
                            origin.write_all(forwarded.as_bytes()).await.unwrap();
                            let _ = tokio::io::copy(&mut origin, &mut stream).await;
                        }
                    });
                },
                _ = jobs.join_next(), if !jobs.is_empty() => {},
            }
        }
    });
    let url_env = format!("SIRIUS_PROXY_URL_{}", uuid::Uuid::new_v4().simple());
    let authorization_env = format!("SIRIUS_PROXY_AUTH_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&url_env, url);
    std::env::set_var(&authorization_env, auth);
    ProxyServer {
        config: ProxyConfig {
            url_env,
            authorization_env: Some(authorization_env),
        },
        seen,
        task,
    }
}
#[tokio::test]
async fn download_pipeline_scopes_api_and_cdn_proxies_and_keeps_tokens_separate() {
    let api = proxy("Bearer api-proxy-only", None).await;
    let cdn = proxy("Bearer cdn-proxy-only", None).await;
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = root.path().into();
    cfg.network.api_proxy = Some(api.config.clone());
    cfg.network.cdn_proxy = Some(cdn.config.clone());
    enable_assets(&mut cfg, false);
    let refresh = format!("SIRIUS_PROXY_REFRESH_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&refresh, "public-fixture");
    cfg.refresh_token_env = Some(refresh.clone());
    let catalog = catalog_fixture(
        &[("{Fwk.Resource.RemoteAssetDir}/sound", CRI_PROVIDER)],
        false,
    );
    let (client, fixture, server) = serve(cfg, StatusCode::OK, catalog, Duration::ZERO).await;
    fixture.assets.lock().unwrap().insert(
        "sound".into(),
        (StatusCode::OK, b"synthetic CRI download".to_vec()),
    );
    let output = client.fetch().await.unwrap();
    assert!(output.join("receipt.json").is_file());
    let api_seen = api.seen.lock().unwrap().join("\n");
    let cdn_seen = cdn.seen.lock().unwrap().join("\n");
    assert!(api_seen.contains("/internal/v1/resources/snapshot"));
    assert!(
        api_seen.contains("Bearer api-proxy-only") && api_seen.contains("Bearer internal-fixture")
    );
    assert!(!api_seen.contains("cdn-proxy-only") && !api_seen.contains("/catalog_main.bin"));
    assert!(cdn_seen.contains("/catalog_main.bin") && cdn_seen.contains("Bearer cdn-proxy-only"));
    assert!(cdn_seen
        .to_ascii_lowercase()
        .contains("authorization: basic "));
    assert!(!cdn_seen.contains("api-proxy-only") && !cdn_seen.contains("internal-fixture"));
    assert!(api_seen.contains("Bearer public-fixture"));
    assert!(cdn_seen.contains("/sound") && !cdn_seen.contains("public-fixture"));
    assert_eq!(fixture.seen.lock().unwrap().len(), 6);
    std::env::remove_var(refresh);
    server.abort();
}
#[tokio::test]
async fn rejected_proxy_never_falls_back_or_follows_redirects() {
    for response in ["HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n", "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:1/leak\r\nContent-Length: 0\r\n\r\n"] {
        let proxy = proxy("Bearer only-proxy", Some(response)).await;
        let root = tempfile::tempdir().unwrap(); let mut cfg = config(); cfg.output = root.path().into();
        cfg.network.api_proxy = Some(proxy.config.clone()); cfg.network.snapshot_retry.attempts = 1;
        let (client, fixture, origin) = serve(cfg, StatusCode::OK, catalog(), Duration::ZERO).await;
        assert!(client.fetch().await.is_err());
        assert!(fixture.seen.lock().unwrap().is_empty());
        assert_eq!(proxy.seen.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        origin.abort();
    }
}
#[tokio::test]
async fn tls_tunnel_keeps_proxy_auth_outside_origin_and_preserves_certificate_checks() {
    let proxy = proxy("Bearer connect-only", None).await;
    let root = tempfile::tempdir().unwrap();
    let tls = listener_tls_config(root.path()).load().unwrap();
    let origin_headers = Arc::new(Mutex::new(Vec::new()));
    let captured = origin_headers.clone();
    let router = Router::new().route(
        "/asset",
        get(move |headers: HeaderMap| {
            captured.lock().unwrap().push(headers);
            async { "synthetic TLS payload" }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}/asset", listener.local_addr().unwrap());
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let origin = tokio::spawn(crate::server::serve(listener, router, Some(tls), async {
        let _ = shutdown.await;
    }));
    let network = crate::network::Network::default();
    let untrusted = crate::proxy::builder(&network, Some(&proxy.config))
        .unwrap()
        .build()
        .unwrap();
    assert!(untrusted
        .get(&url)
        .bearer_auth("origin-only")
        .send()
        .await
        .is_err());
    assert!(origin_headers.lock().unwrap().is_empty());
    let certificate =
        reqwest::Certificate::from_pem(include_bytes!("../tests/fixtures/listener-cert.pem"))
            .unwrap();
    let client = crate::proxy::builder(&network, Some(&proxy.config))
        .unwrap()
        .tls_certs_only([certificate])
        .build()
        .unwrap();
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("origin-only")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "synthetic TLS payload"
    );
    {
        let headers = origin_headers.lock().unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0]["authorization"], "Bearer origin-only");
        assert!(!headers[0].contains_key("proxy-authorization"));
    }
    let connections = proxy.seen.lock().unwrap().join("\n");
    assert!(connections.contains("CONNECT ") && connections.contains("Bearer connect-only"));
    assert!(!connections.contains("origin-only") && !connections.contains("/asset"));
    // An HTTPS proxy's own certificate must also be trusted before credentials are sent.
    std::env::set_var(&proxy.config.url_env, url.strip_suffix("/asset").unwrap());
    let bad_proxy = crate::proxy::builder(&network, Some(&proxy.config))
        .unwrap()
        .build()
        .unwrap();
    assert!(bad_proxy.get(&url).send().await.is_err());
    assert_eq!(origin_headers.lock().unwrap().len(), 1);
    drop(bad_proxy);
    drop(client);
    drop(untrusted);
    drop(proxy);
    stop.send(()).unwrap();
    origin.await.unwrap().unwrap();
}
#[tokio::test]
async fn proxy_configuration_and_offline_readiness_never_expose_secret_values() {
    let proxy = proxy("Bearer synthetic-secret", None).await;
    let mut cfg = config();
    cfg.network.cdn_proxy = Some(proxy.config.clone());
    for value in [
        "socks5://127.0.0.1:1080",
        "http://user:password@localhost:80",
        "https://host/path",
        "https://host/?token=secret",
        "https://host/#secret",
        "http://host\\secret",
    ] {
        std::env::set_var(&proxy.config.url_env, value);
        assert!(matches!(proxy.config.resolve(), Err(Error::Config)));
        let report = cfg.check_secrets();
        assert!(report.invalid_fields.contains(&"network.cdn_proxy"));
        let text = sonic_rs::to_string(&report).unwrap();
        assert!(!text.contains(value));
    }
    std::env::remove_var(&proxy.config.url_env);
    let report = cfg.check_secrets();
    assert!(report.missing_env.contains(&proxy.config.url_env));
    std::env::set_var(&proxy.config.url_env, "http://127.0.0.1:1");
    std::env::set_var(
        proxy.config.authorization_env.as_ref().unwrap(),
        "Bearer bad\r\nX-Leak: yes",
    );
    assert!(proxy.config.resolve().is_err());
    assert!(yaml_serde::from_str::<ProxyConfig>("url_env: URL\npassword: ignored").is_err());
}

#[tokio::test]
async fn stalled_proxy_obeys_request_deadline_without_reaching_origin() {
    let proxy = proxy("Bearer timeout-only", Some("stall")).await;
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = root.path().into();
    cfg.network.api_proxy = Some(proxy.config.clone());
    cfg.network.snapshot_timeout_ms = 100;
    cfg.network.snapshot_retry.attempts = 1;
    let (client, fixture, origin) = serve(cfg, StatusCode::OK, catalog(), Duration::ZERO).await;
    let result = tokio::time::timeout(Duration::from_secs(2), client.fetch())
        .await
        .unwrap();
    assert!(matches!(result, Err(Error::Transport)));
    assert_eq!(proxy.seen.lock().unwrap().len(), 1);
    assert!(fixture.seen.lock().unwrap().is_empty());
    origin.abort();
}

#[tokio::test]
async fn ambient_proxy_environment_is_ignored() {
    if std::env::var("SIRIUS_TEST_DIRECT_CHILD").as_deref() == Ok("1") {
        let url = std::env::var("SIRIUS_TEST_DIRECT_ORIGIN").unwrap();
        let client = crate::proxy::builder(&crate::network::Network::default(), None)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            client.get(url).send().await.unwrap().text().await.unwrap(),
            "direct origin"
        );
        return;
    }
    let proxy = proxy(
        "unused",
        Some("HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n"),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/direct", listener.local_addr().unwrap());
    let router = Router::new().route("/direct", get(|| async { "direct origin" }));
    let origin = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "tests::network_tests::ambient_proxy_environment_is_ignored",
            "--nocapture",
        ])
        .env("SIRIUS_TEST_DIRECT_CHILD", "1")
        .env("SIRIUS_TEST_DIRECT_ORIGIN", url);
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        child.env(name, std::env::var(&proxy.config.url_env).unwrap());
    }
    child
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), child.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(proxy.seen.lock().unwrap().is_empty());
    origin.abort();
}
