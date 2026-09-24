use super::*;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use std::sync::{Arc, Mutex};
fn config() -> Config {
    Config {
        region: region::Region::Jp,
        platform: None,
        protocol_version: None,
        game_api_root: "http://127.0.0.1:9999".into(),
        internal_token_env: "TOKEN".into(),
        refresh_token_env: None,
        environment: "release".into(),
        client_version: "1.0.3".into(),
        output: PathBuf::from("unused"),
        assets: None,
        cdn_roots: BTreeMap::from([(
            "https://static.example".into(),
            CdnAuth {
                username_env: "USER".into(),
                credential_env: "PASSWORD".into(),
            },
        )]),
    }
}
fn snapshot() -> SnapshotResponse {
    SnapshotResponse {
        stale: false,
        snapshot: Snapshot {
            region: None,
            schema_version: 1,
            environment: "release".into(),
            platform: "iOS".into(),
            client_version: "1.0.3".into(),
            protocol_version: "1.0.3".into(),
            master_version: Some("m1".into()),
            resource_version: "r1".into(),
            platform_hash: "h1".into(),
            effective_cdn_root: "https://static.example".into(),
            credential_ref: "PASSWORD".into(),
            observed_at: Utc::now(),
            source: "remote".into(),
        },
    }
}
#[test]
fn snapshot_policy_rejects_stale_identity_paths_and_secret_substitution() {
    let cfg = config();
    let valid = snapshot();
    let now = Utc::now();
    assert_eq!(
        valid.catalog_url(&cfg, now).unwrap(),
        "https://static.example/asset/r1/iOS/h1/catalog_main.bin"
    );
    for case in 0..10 {
        let mut s = snapshot();
        s.snapshot.observed_at = now;
        match case {
            0 => s.stale = true,
            1 => s.snapshot.observed_at = now - chrono::Duration::seconds(301),
            2 => s.snapshot.observed_at = now + chrono::Duration::seconds(1),
            3 => s.snapshot.platform = "Android".into(),
            4 => s.snapshot.environment = "cbt".into(),
            5 => s.snapshot.resource_version = "../other".into(),
            6 => s.snapshot.effective_cdn_root = "https://evil.example".into(),
            7 => s.snapshot.credential_ref = "OTHER_SECRET".into(),
            8 => s.snapshot.schema_version = 2,
            _ => s.snapshot.source = "offline-bundled".into(),
        }
        assert!(s.catalog_url(&cfg, now).is_err(), "case {case}");
    }
}
#[test]
fn config_rejects_remote_plaintext_and_cdn_plaintext() {
    let mut cfg = config();
    assert!(cfg.validate().is_ok());
    cfg.game_api_root = "http://api.example".into();
    assert!(cfg.validate().is_err());
    cfg = config();
    cfg.cdn_roots = BTreeMap::from([(
        "http://127.0.0.1:99".into(),
        CdnAuth {
            username_env: "A".into(),
            credential_env: "B".into(),
        },
    )]);
    assert!(cfg.validate().is_err());
}
type FixtureAssets = Arc<Mutex<BTreeMap<String, (StatusCode, Vec<u8>)>>>;
#[derive(Clone)]
struct Fixture {
    snapshot: Arc<Mutex<String>>,
    seen: Arc<Mutex<Vec<(String, String)>>>,
    catalog: Vec<u8>,
    status: StatusCode,
    delay: Duration,
    assets: FixtureAssets,
    after_asset_snapshot: Arc<Mutex<Option<String>>>,
    system_response: Arc<Mutex<String>>,
    system_failures: Arc<Mutex<usize>>,
    stalled_assets: Arc<Mutex<std::collections::BTreeSet<String>>>,
    asset_gate: Arc<Mutex<Option<Arc<tokio::sync::Barrier>>>>,
}
async fn serve(
    mut cfg: Config,
    status: StatusCode,
    catalog: Vec<u8>,
    delay: Duration,
) -> (CatalogClient, Fixture, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let id = uuid::Uuid::new_v4().simple().to_string();
    let token = format!("TEST_TOKEN_{id}");
    let user = format!("TEST_USER_{id}");
    let pass = format!("TEST_PASS_{id}");
    std::env::set_var(&token, "internal-fixture");
    std::env::set_var(&user, "username");
    std::env::set_var(&pass, "cdn-fixture");
    cfg.game_api_root = root.clone();
    cfg.internal_token_env = token;
    cfg.cdn_roots = BTreeMap::from([(
        root.clone(),
        CdnAuth {
            username_env: user,
            credential_env: pass.clone(),
        },
    )]);
    let mut snap = snapshot();
    snap.snapshot.effective_cdn_root = root;
    snap.snapshot.credential_ref = pass;
    let fixture = Fixture {
        snapshot: Arc::new(Mutex::new(sonic_rs::to_string(&snap).unwrap())),
        seen: Arc::new(Mutex::new(Vec::new())),
        catalog,
        status,
        delay,
        assets: Arc::new(Mutex::new(BTreeMap::new())),
        after_asset_snapshot: Arc::new(Mutex::new(None)),
        system_response: Arc::new(Mutex::new(r#"{"status":"available"}"#.into())),
        system_failures: Arc::new(Mutex::new(0)),
        stalled_assets: Arc::new(Mutex::new(std::collections::BTreeSet::new())),
        asset_gate: Arc::new(Mutex::new(None)),
    };
    let app = Router::new()
        .route(
            "/api/v1/system",
            get(|State(f): State<Fixture>, headers: HeaderMap| async move {
                f.seen.lock().unwrap().push((
                    "system".into(),
                    headers["authorization"].to_str().unwrap().into(),
                ));
                {
                    let mut failures = f.system_failures.lock().unwrap();
                    if *failures > 0 {
                        *failures -= 1;
                        return (StatusCode::BAD_GATEWAY, "temporary").into_response();
                    }
                }
                let body = f.system_response.lock().unwrap().clone();
                if body == r#"{"status":"available"}"# {
                    let mut snapshot: SnapshotResponse =
                        sonic_rs::from_str(&f.snapshot.lock().unwrap()).unwrap();
                    snapshot.snapshot.observed_at = Utc::now();
                    snapshot.stale = false;
                    *f.snapshot.lock().unwrap() = sonic_rs::to_string(&snapshot).unwrap();
                }
                body.into_response()
            }),
        )
        .route(
            "/internal/v1/resources/snapshot",
            get(|State(f): State<Fixture>, headers: HeaderMap| async move {
                f.seen.lock().unwrap().push((
                    "snapshot".into(),
                    headers["authorization"].to_str().unwrap().into(),
                ));
                f.snapshot.lock().unwrap().clone()
            }),
        )
        .route(
            "/asset/r1/iOS/h1/catalog_main.bin",
            get(|State(f): State<Fixture>, headers: HeaderMap| async move {
                assert_eq!(
                    headers["user-agent"],
                    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))
                );
                f.seen.lock().unwrap().push((
                    "catalog".into(),
                    headers["authorization"].to_str().unwrap().into(),
                ));
                tokio::time::sleep(f.delay).await;
                if f.status.is_redirection() {
                    return (
                        f.status,
                        [("location", "http://127.0.0.1:1/credential-leak")],
                        "redirect",
                    )
                        .into_response();
                }
                (f.status, f.catalog).into_response()
            }),
        )
        .route(
            "/asset/r1/iOS/h1/{*path}",
            get(
                |State(f): State<Fixture>,
                 axum::extract::Path(path): axum::extract::Path<String>,
                 headers: HeaderMap| async move {
                    f.seen.lock().unwrap().push((
                        path.clone(),
                        headers["authorization"].to_str().unwrap().into(),
                    ));
                    let gate = f.asset_gate.lock().unwrap().clone();
                    if let Some(gate) = gate {
                        gate.wait().await;
                    }
                    if let Some(next) = f.after_asset_snapshot.lock().unwrap().take() {
                        *f.snapshot.lock().unwrap() = next;
                    }
                    let (status, bytes) = f
                        .assets
                        .lock()
                        .unwrap()
                        .get(&path)
                        .cloned()
                        .unwrap_or((StatusCode::NOT_FOUND, Vec::new()));
                    if f.stalled_assets.lock().unwrap().contains(&path) {
                        use futures_util::StreamExt;
                        let stream =
                            futures_util::stream::once(
                                async move { Ok::<_, std::io::Error>(bytes) },
                            )
                            .chain(futures_util::stream::pending());
                        return axum::response::Response::builder()
                            .status(status)
                            .body(axum::body::Body::from_stream(stream))
                            .unwrap();
                    }
                    (status, bytes).into_response()
                },
            ),
        )
        .with_state(fixture.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Only this test-only constructor allows local plaintext CDN fixtures.
    (CatalogClient::build(cfg).unwrap(), fixture, task)
}
fn catalog() -> Vec<u8> {
    catalog_fixture(
        &[(
            "{Fwk.Resource.RemoteAssetDir}/fixture.bundle",
            CRYPT_PROVIDER,
        )],
        false,
    )
}
#[tokio::test]
async fn complete_flow_pins_version_isolates_auth_and_publishes_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = dir.path().into();
    let (client, fixture, task) = serve(cfg, StatusCode::OK, catalog(), Duration::ZERO).await;
    let path = client.fetch().await.unwrap();
    assert_eq!(
        std::fs::read(path.join("catalog_main.bin")).unwrap(),
        catalog()
    );
    let receipt = std::fs::read_to_string(path.join("receipt.json")).unwrap();
    let value: Receipt = sonic_rs::from_str(&receipt).unwrap();
    assert_eq!(value.sha256, hex::encode(Sha256::digest(catalog())));
    assert_eq!(value.snapshot.resource_version, "r1");
    assert!(!receipt.contains("cdn-fixture"));
    assert!(!receipt.contains("internal-fixture"));
    let seen = fixture.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].1, "Bearer internal-fixture");
    assert!(seen[1].1.starts_with("Basic "));
    assert!(!seen[1].1.contains("internal-fixture"));
    task.abort();
}
#[tokio::test]
async fn stale_snapshot_fails_before_cdn_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = dir.path().into();
    let (client, fixture, task) = serve(cfg, StatusCode::OK, catalog(), Duration::ZERO).await;
    let mut snapshot: SnapshotResponse =
        sonic_rs::from_str(&fixture.snapshot.lock().unwrap()).unwrap();
    snapshot.stale = true;
    *fixture.snapshot.lock().unwrap() = sonic_rs::to_string(&snapshot).unwrap();
    assert!(matches!(client.fetch().await, Err(Error::Snapshot)));
    assert_eq!(fixture.seen.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    task.abort();
}
#[tokio::test]
async fn redirect_error_and_invalid_content_never_publish_or_replace_previous_output() {
    for (status, data) in [
        (StatusCode::FOUND, catalog()),
        (StatusCode::FORBIDDEN, catalog()),
        (StatusCode::OK, b"<html>error</html>".to_vec()),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("previous"), "keep").unwrap();
        let mut cfg = config();
        cfg.output = dir.path().into();
        let (client, fixture, task) = serve(cfg, status, data, Duration::ZERO).await;
        assert!(client.fetch().await.is_err());
        assert_eq!(fixture.seen.lock().unwrap().len(), 2);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("previous")).unwrap(),
            "keep"
        );
        task.abort();
    }
}
#[tokio::test]
async fn canceled_download_does_not_publish() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = dir.path().into();
    let (client, fixture, server) =
        serve(cfg, StatusCode::OK, catalog(), Duration::from_secs(60)).await;
    let job = tokio::spawn(async move { client.fetch().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.seen.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    job.abort();
    let _ = job.await;
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    server.abort();
}

#[tokio::test]
#[ignore = "requires SIRIUS_CATALOG_SAMPLE pointing to a local binary catalog"]
async fn real_catalog_fixture_is_preserved_byte_for_byte() {
    let bytes =
        std::fs::read(std::env::var("SIRIUS_CATALOG_SAMPLE").expect("set SIRIUS_CATALOG_SAMPLE"))
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = dir.path().into();
    let (client, _, task) = serve(cfg, StatusCode::OK, bytes.clone(), Duration::ZERO).await;
    let output = client.fetch().await.unwrap();
    assert_eq!(
        std::fs::read(output.join("catalog_main.bin")).unwrap(),
        bytes
    );
    task.abort();
}

fn catalog_fixture(entries: &[(&str, &str)], dependency: bool) -> Vec<u8> {
    fn data(bytes: &mut Vec<u8>, value: &[u8]) -> u32 {
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        let offset = bytes.len() as u32;
        bytes.extend_from_slice(value);
        offset
    }
    let mut bytes = vec![0x42, 0x89, 0xe3, 0x0d, 2, 0, 0, 0, 0, 0, 0, 0];
    let mut locations = Vec::new();
    for (internal, provider) in entries {
        let name = data(&mut bytes, b"fixture");
        let path = data(&mut bytes, internal.as_bytes());
        let provider = data(&mut bytes, provider.as_bytes());
        let offset = bytes.len() as u32;
        locations.push(offset);
        for value in [name, path, provider, u32::MAX, 0, u32::MAX] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    if dependency && locations.len() > 1 {
        let array = data(&mut bytes, &locations[1].to_le_bytes());
        let offset = locations[0] as usize + 12;
        bytes[offset..offset + 4].copy_from_slice(&array.to_le_bytes());
    }
    let all: Vec<u8> = locations.iter().flat_map(|id| id.to_le_bytes()).collect();
    let ids = data(&mut bytes, &all);
    let mut entry = u32::MAX.to_le_bytes().to_vec();
    entry.extend_from_slice(&ids.to_le_bytes());
    let keys = data(&mut bytes, &entry);
    bytes[8..12].copy_from_slice(&keys.to_le_bytes());
    bytes
}
const CRYPT_PROVIDER: &str = "Fwk.Crypt.AssetBundleCryptProvider";
const UNITY_PROVIDER: &str = "UnityEngine.ResourceManagement.ResourceProviders.AssetBundleProvider";
const CRI_PROVIDER: &str = "CriWare.Assets.CriResourceProvider";
#[test]
fn catalog_parser_preserves_dependencies_and_rejects_malformed_buffers() {
    use crate::catalog::Catalog;
    let bytes = catalog_fixture(
        &[
            ("{Fwk.Resource.RemoteAssetDir}/a.bundle", CRYPT_PROVIDER),
            ("{Fwk.Resource.RemoteAssetDir}/b.bundle", UNITY_PROVIDER),
        ],
        true,
    );
    let parsed = Catalog::parse(&bytes).unwrap();
    assert_eq!(parsed.locations.len(), 2);
    assert_eq!(parsed.locations[0].dependencies, [parsed.locations[1].id]);
    let plan = parsed
        .plan("https://static.example/asset/r1/iOS/h1")
        .unwrap();
    assert_eq!(plan.assets.len(), 2);
    assert_eq!(plan.assets[0].provider, assets::Provider::EncryptedBundle);
    for end in 0..bytes.len() {
        assert!(Catalog::parse(&bytes[..end]).is_err(), "truncated at {end}");
    }
    let mut invalid = bytes.clone();
    invalid[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Catalog::parse(&invalid).is_err());
    let offset = parsed.locations[0].id as usize;
    let mut cycle = bytes.clone();
    // Dynamic string node points to itself.
    let node = cycle.len() as u32;
    cycle.extend_from_slice(&u32::MAX.to_le_bytes());
    cycle.extend_from_slice(&node.to_le_bytes());
    cycle[offset + 4..offset + 8].copy_from_slice(&(node | 0x4000_0000).to_le_bytes());
    assert!(Catalog::parse(&cycle).is_err());
}
#[test]
fn provider_plan_rejects_cross_origin_paths_unknown_providers_and_conflicts() {
    use crate::catalog::Catalog;
    let root = "https://static.example/asset/r1/iOS/h1";
    for path in [
        "https://evil.example/a.bundle",
        "{Fwk.Resource.RemoteAssetDir}/../secret",
        "{Fwk.Resource.RemoteAssetDir}/%2e%2e/file",
        "{Fwk.Resource.RemoteAssetDir}//evil",
        "{Unknown}/asset",
        "{Fwk.Resource.RemoteAssetDir}/x?token=secret",
    ] {
        assert!(
            Catalog::parse(&catalog_fixture(&[(path, CRYPT_PROVIDER)], false))
                .unwrap()
                .plan(root)
                .is_err()
        );
    }
    for entries in [
        vec![("{Fwk.Resource.RemoteAssetDir}/a", "unknown.Provider")],
        vec![
            ("{Fwk.Resource.RemoteAssetDir}/a", CRYPT_PROVIDER),
            ("{Fwk.Resource.RemoteAssetDir}/a", CRI_PROVIDER),
        ],
        vec![
            ("{Fwk.Resource.RemoteAssetDir}/a", CRI_PROVIDER),
            ("{Fwk.Resource.RemoteAssetDir}/a/b", CRI_PROVIDER),
        ],
    ] {
        assert!(Catalog::parse(&catalog_fixture(&entries, false))
            .unwrap()
            .plan(root)
            .is_err());
    }
    for entries in [
        vec![
            ("{Fwk.Resource.RemoteAssetDir}/A", CRI_PROVIDER),
            ("{Fwk.Resource.RemoteAssetDir}/a", CRI_PROVIDER),
        ],
        vec![
            ("{Fwk.Resource.RemoteAssetDir}/A", CRI_PROVIDER),
            ("{Fwk.Resource.RemoteAssetDir}/a/b", CRI_PROVIDER),
        ],
    ] {
        assert!(Catalog::parse(&catalog_fixture(&entries, false))
            .unwrap()
            .plan(root)
            .is_err());
    }
    let local = Catalog::parse(&catalog_fixture(
        &[(
            "{UnityEngine.AddressableAssets.Addressables.RuntimePath}/iOS/local.bundle",
            CRYPT_PROVIDER,
        )],
        false,
    ))
    .unwrap()
    .plan(root)
    .unwrap();
    assert!(local.assets.is_empty());
    assert_eq!(local.embedded_locations, 1);
}
#[test]
fn bundle_ctr_matches_independent_openssl_vector_and_preserves_tail() {
    let key = assets::BundleKey::from_hex("000102030405060708090a0b0c0d0e0f", "1011121314151617")
        .unwrap();
    let raw = include_bytes!("../tests/fixtures/bundle-prefix.bin");
    let mut expected: Vec<u8> = (0..raw.len()).map(|i| (i % 251) as u8).collect();
    expected[..8].copy_from_slice(b"UnityFS\0");
    for len in [8, 15, 16, 17, 16383, 16384, raw.len()] {
        let mut decoded = raw[..len].to_vec();
        key.decrypt_prefix(&mut decoded, "fixture.bundle").unwrap();
        assert_eq!(decoded, expected[..len]);
    }
    assert_eq!(raw[16384..], expected[16384..]);
    assert!(key
        .decrypt_prefix(&mut raw.to_vec(), "wrong.bundle")
        .is_err());
    assert!(key
        .decrypt_prefix(&mut raw.to_vec(), "../fixture.bundle")
        .is_err());
}
fn enable_assets(cfg: &mut Config, decrypt: bool) {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let key = format!("BUNDLE_KEY_{id}");
    let seed = format!("BUNDLE_SEED_{id}");
    std::env::set_var(&key, "000102030405060708090a0b0c0d0e0f");
    std::env::set_var(&seed, "1011121314151617");
    cfg.assets = Some(assets::AssetConfig {
        concurrency: 1,
        cache_directory: None,
        max_file_bytes: 1024 * 1024,
        max_total_bytes: 4 * 1024 * 1024,
        decrypt: decrypt.then_some(assets::DecryptConfig {
            key_hex_env: key,
            nonce_seed_hex_env: seed,
        }),
    });
}
#[tokio::test]
async fn resource_update_downloads_by_provider_decrypts_and_pins_final_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = dir.path().into();
    enable_assets(&mut cfg, true);
    let bytes = catalog_fixture(
        &[
            (
                "{Fwk.Resource.RemoteAssetDir}/fixture.bundle",
                CRYPT_PROVIDER,
            ),
            ("{Fwk.Resource.RemoteAssetDir}/plain.bundle", UNITY_PROVIDER),
            ("{Fwk.Resource.RemoteAssetDir}/sound.acb", CRI_PROVIDER),
        ],
        true,
    );
    let (client, fixture, task) = serve(cfg, StatusCode::OK, bytes, Duration::ZERO).await;
    fixture.assets.lock().unwrap().extend([
        (
            "fixture.bundle".into(),
            (
                StatusCode::OK,
                include_bytes!("../tests/fixtures/bundle-prefix.bin").to_vec(),
            ),
        ),
        (
            "plain.bundle".into(),
            (StatusCode::OK, b"UnityFS\0plain".to_vec()),
        ),
        (
            "sound.acb".into(),
            (StatusCode::OK, b"@UTFsynthetic".to_vec()),
        ),
    ]);
    let path = client.fetch().await.unwrap();
    assert!(std::fs::read(path.join("assets/fixture.bundle"))
        .unwrap()
        .starts_with(b"UnityFS\0"));
    assert_eq!(
        std::fs::read(path.join("assets/sound.acb")).unwrap(),
        b"@UTFsynthetic"
    );
    let receipt: Receipt =
        sonic_rs::from_slice(&std::fs::read(path.join("receipt.json")).unwrap()).unwrap();
    let update = receipt.update.unwrap();
    assert_eq!(update.assets.len(), 3);
    assert!(update.assets[0].decrypted);
    assert_ne!(
        update.assets[0].downloaded_sha256,
        update.assets[0].stored_sha256
    );
    assert_eq!(
        fixture
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "snapshot")
            .count(),
        2
    );
    for (path, auth) in fixture.seen.lock().unwrap().iter() {
        assert_eq!(
            auth,
            if path == "snapshot" {
                "Bearer internal-fixture"
            } else {
                "Basic dXNlcm5hbWU6Y2RuLWZpeHR1cmU="
            }
        );
    }
    assert!(path.join("locations.json").exists());
    task.abort();
}
#[tokio::test]
async fn resource_failure_or_version_change_never_publishes_partial_tree() {
    for case in 0..6 {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("previous-success")).unwrap();
        let mut cfg = config();
        cfg.output = dir.path().into();
        enable_assets(&mut cfg, case == 2);
        if case == 3 {
            cfg.assets.as_mut().unwrap().max_file_bytes = 8;
        }
        let bytes = catalog_fixture(
            &[(
                "{Fwk.Resource.RemoteAssetDir}/fixture.bundle",
                CRYPT_PROVIDER,
            )],
            false,
        );
        let (client, fixture, task) = serve(cfg, StatusCode::OK, bytes, Duration::ZERO).await;
        let status = if case == 0 {
            StatusCode::FORBIDDEN
        } else if case == 4 {
            StatusCode::FOUND
        } else if case == 5 {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        fixture.assets.lock().unwrap().insert(
            "fixture.bundle".into(),
            (status, b"not a valid encrypted bundle".to_vec()),
        );
        if case == 1 {
            let mut changed: SnapshotResponse =
                sonic_rs::from_str(&fixture.snapshot.lock().unwrap()).unwrap();
            changed.snapshot.resource_version = "r2".into();
            *fixture.after_asset_snapshot.lock().unwrap() =
                Some(sonic_rs::to_string(&changed).unwrap());
        }
        assert!(client.fetch().await.is_err(), "case {case}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(dir.path().join("previous-success").is_dir());
        task.abort();
    }
}

#[tokio::test]
async fn asset_retry_is_bounded_and_total_budget_is_enforced() {
    for retry in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.output = dir.path().into();
        enable_assets(&mut cfg, false);
        cfg.assets.as_mut().unwrap().max_file_bytes = 8;
        cfg.assets.as_mut().unwrap().max_total_bytes = 8;
        let bytes = catalog_fixture(
            &[
                ("{Fwk.Resource.RemoteAssetDir}/one", CRI_PROVIDER),
                ("{Fwk.Resource.RemoteAssetDir}/two", CRI_PROVIDER),
            ],
            false,
        );
        let (client, fixture, task) = serve(cfg, StatusCode::OK, bytes, Duration::ZERO).await;
        let status = if retry {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::OK
        };
        fixture
            .assets
            .lock()
            .unwrap()
            .insert("one".into(), (status, vec![0; 8]));
        fixture
            .assets
            .lock()
            .unwrap()
            .insert("two".into(), (status, vec![0; 8]));
        let error = client.fetch().await.unwrap_err();
        if retry {
            assert!(matches!(error, Error::Status(503)));
        } else {
            assert!(matches!(error, Error::Size));
        }
        assert_eq!(
            fixture
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(path, _)| path == "one")
                .count(),
            if retry { 3 } else { 1 }
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        task.abort();
    }
}
#[test]
fn catalog_unicode_and_dynamic_string_chains_are_decoded() {
    use crate::catalog::Catalog;
    let mut bytes = catalog_fixture(&[("placeholder", CRI_PROVIDER)], false);
    let offset = Catalog::parse(&bytes).unwrap().locations[0].id as usize;
    let encoded: Vec<u8> = "音楽"
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    bytes.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
    let unicode = bytes.len() as u32 | 0x8000_0000;
    bytes.extend_from_slice(&encoded);
    bytes[offset..offset + 4].copy_from_slice(&unicode.to_le_bytes());
    fn string(bytes: &mut Vec<u8>, s: &str) -> u32 {
        bytes.extend_from_slice(&(s.len() as u32).to_le_bytes());
        let offset = bytes.len() as u32;
        bytes.extend_from_slice(s.as_bytes());
        offset
    }
    let head = string(&mut bytes, "{Fwk.Resource.RemoteAssetDir}");
    let tail = string(&mut bytes, "sound.acb");
    let first = bytes.len() as u32;
    bytes.extend_from_slice(&head.to_le_bytes());
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    let last = bytes.len() as u32;
    bytes.extend_from_slice(&tail.to_le_bytes());
    bytes.extend_from_slice(&first.to_le_bytes());
    bytes[offset + 4..offset + 8].copy_from_slice(&(last | 0x4000_0000).to_le_bytes());
    let parsed = Catalog::parse(&bytes).unwrap();
    assert_eq!(parsed.locations[0].primary_key, "音楽");
    assert_eq!(
        parsed.locations[0].internal_id,
        "{Fwk.Resource.RemoteAssetDir}/sound.acb"
    );
}

fn enable_refresh(cfg: &mut Config) {
    let name = format!("REFRESH_TOKEN_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&name, "api-fixture");
    cfg.refresh_token_env = Some(name);
}
#[tokio::test]
async fn refresh_renews_stale_snapshots_and_keeps_three_credential_scopes_separate() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = dir.path().into();
    enable_refresh(&mut cfg);
    enable_assets(&mut cfg, false);
    let catalog = catalog_fixture(
        &[("{Fwk.Resource.RemoteAssetDir}/sound", CRI_PROVIDER)],
        false,
    );
    let (client, f, server) = serve(cfg, StatusCode::OK, catalog, Duration::ZERO).await;
    let mut stale: SnapshotResponse = sonic_rs::from_str(&f.snapshot.lock().unwrap()).unwrap();
    stale.snapshot.observed_at -= chrono::Duration::minutes(10);
    stale.stale = true;
    *f.snapshot.lock().unwrap() = sonic_rs::to_string(&stale).unwrap();
    // Model an expired server observation at publication too.
    *f.after_asset_snapshot.lock().unwrap() = Some(sonic_rs::to_string(&stale).unwrap());
    f.assets
        .lock()
        .unwrap()
        .insert("sound".into(), (StatusCode::OK, b"@UTFfixture".to_vec()));
    assert!(client.fetch().await.unwrap().join("receipt.json").exists());
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.iter().filter(|(p, _)| p == "system").count(), 2);
    for (path, auth) in seen.iter() {
        assert_eq!(
            auth,
            match path.as_str() {
                "system" => "Bearer api-fixture",
                "snapshot" => "Bearer internal-fixture",
                _ => "Basic dXNlcm5hbWU6Y2RuLWZpeHR1cmU=",
            }
        );
    }
    server.abort();
}
#[tokio::test]
async fn maintenance_or_invalid_refresh_never_reaches_cdn() {
    for response in [
        r#"{"status":"unavailable"}"#.to_owned(),
        "not json".into(),
        "x".repeat(65537),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.output = dir.path().into();
        enable_refresh(&mut cfg);
        let (client, f, server) = serve(cfg, StatusCode::OK, catalog(), Duration::ZERO).await;
        *f.system_response.lock().unwrap() = response;
        assert!(client.fetch().await.is_err());
        assert_eq!(f.seen.lock().unwrap().len(), 1);
        assert_eq!(f.seen.lock().unwrap()[0].0, "system");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        server.abort();
    }
}
#[tokio::test]
async fn probe_reads_game_api_without_catalog_or_asset_requests() {
    let mut cfg = config();
    enable_refresh(&mut cfg);
    let (client, f, server) = serve(cfg, StatusCode::OK, catalog(), Duration::ZERO).await;
    let result = client.probe().await.unwrap();
    assert_eq!(result.snapshot.resource_version, "r1");
    assert_eq!(
        f.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.clone())
            .collect::<Vec<_>>(),
        ["system", "snapshot"]
    );
    server.abort();
}
#[test]
fn offline_check_reports_missing_and_invalid_fields_without_secret_values() {
    let mut cfg = config();
    let id = uuid::Uuid::new_v4().simple().to_string();
    cfg.internal_token_env = format!("CHECK_INTERNAL_{id}");
    cfg.refresh_token_env = Some(format!("CHECK_API_{id}"));
    cfg.cdn_roots.values_mut().for_each(|auth| {
        auth.username_env = format!("CHECK_USER_{id}");
        auth.credential_env = format!("CHECK_PASS_{id}");
    });
    let report = cfg.check().unwrap();
    assert!(!report.ready);
    assert_eq!(report.missing_env.len(), 4);
    for name in &report.missing_env {
        std::env::set_var(name, "private-fixture-value");
    }
    assert!(!cfg.check().unwrap().ready); // API and internal tokens may not be shared.
    std::env::set_var(
        cfg.refresh_token_env.as_ref().unwrap(),
        "different-api-fixture",
    );
    assert!(cfg.check().unwrap().ready);
    enable_assets(&mut cfg, true);
    let decrypt = cfg.assets.as_ref().unwrap().decrypt.as_ref().unwrap();
    std::env::set_var(&decrypt.key_hex_env, "private-invalid-key-value");
    let report = cfg.check().unwrap();
    assert!(!report.ready);
    assert!(report.invalid_fields.contains(&"bundle_key_or_nonce_seed"));
    let json = sonic_rs::to_string(&report).unwrap();
    assert!(!json.contains("private-fixture-value"));
    assert!(!json.contains("private-invalid-key-value"));
}
#[tokio::test]
async fn invalid_decryption_key_fails_before_any_request() {
    let mut cfg = config();
    enable_assets(&mut cfg, true);
    std::env::set_var(
        &cfg.assets
            .as_ref()
            .unwrap()
            .decrypt
            .as_ref()
            .unwrap()
            .key_hex_env,
        "bad",
    );
    let (client, f, server) = serve(cfg, StatusCode::OK, catalog(), Duration::ZERO).await;
    assert!(matches!(client.fetch().await, Err(Error::Preflight)));
    assert!(f.seen.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn cache_survives_failed_runs_reuses_verified_bytes_and_repairs_corruption() {
    let output = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = output.path().into();
    enable_assets(&mut cfg, false);
    cfg.assets.as_mut().unwrap().cache_directory = Some(cache.path().into());
    let bytes = catalog_fixture(
        &[
            ("{Fwk.Resource.RemoteAssetDir}/a", CRI_PROVIDER),
            ("{Fwk.Resource.RemoteAssetDir}/b", CRI_PROVIDER),
        ],
        false,
    );
    let (client, f, server) = serve(cfg, StatusCode::OK, bytes.clone(), Duration::ZERO).await;
    f.assets
        .lock()
        .unwrap()
        .insert("a".into(), (StatusCode::OK, b"first".to_vec()));
    // The second resource fails; the completed first download remains a cache entry.
    assert!(matches!(client.fetch().await, Err(Error::Status(404))));
    assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 0);
    f.assets
        .lock()
        .unwrap()
        .insert("b".into(), (StatusCode::OK, b"second".to_vec()));
    let path = client.fetch().await.unwrap();
    let receipt: Receipt =
        sonic_rs::from_slice(&std::fs::read(path.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt.update.unwrap().cache_hits, 1);
    assert_eq!(
        f.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == "a")
            .count(),
        1
    );
    let snapshot: SnapshotResponse = sonic_rs::from_str(&f.snapshot.lock().unwrap()).unwrap();
    let url = snapshot.catalog_url(&client.config, Utc::now()).unwrap();
    let id = crate::cache::identity(
        &format!("jp:release:iOS:{url}"),
        &hex::encode(Sha256::digest(&bytes)),
        "a",
        assets::Provider::Cri,
    );
    std::fs::write(cache.path().join(id).join("data"), b"wrong").unwrap();
    let repaired = client.fetch().await.unwrap();
    assert_eq!(std::fs::read(repaired.join("assets/a")).unwrap(), b"first");
    assert_eq!(
        f.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == "a")
            .count(),
        2
    );
    // Only integrity-checked complete directories exist; no pending file is published.
    assert!(!std::fs::read_dir(cache.path()).unwrap().any(|p| p
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".pending-")));
    server.abort();
}
#[tokio::test]
async fn encrypted_cache_preserves_ciphertext_and_can_be_redecrypted_on_rerun() {
    let output = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = output.path().into();
    enable_assets(&mut cfg, true);
    cfg.assets.as_mut().unwrap().cache_directory = Some(cache.path().into());
    let raw = include_bytes!("../tests/fixtures/bundle-prefix.bin");
    let bytes = catalog_fixture(
        &[(
            "{Fwk.Resource.RemoteAssetDir}/fixture.bundle",
            CRYPT_PROVIDER,
        )],
        false,
    );
    let (client, f, server) = serve(cfg, StatusCode::OK, bytes.clone(), Duration::ZERO).await;
    f.assets
        .lock()
        .unwrap()
        .insert("fixture.bundle".into(), (StatusCode::OK, raw.to_vec()));
    let first = client.fetch().await.unwrap();
    let second = client.fetch().await.unwrap();
    assert_eq!(
        std::fs::read(first.join("assets/fixture.bundle")).unwrap(),
        std::fs::read(second.join("assets/fixture.bundle")).unwrap()
    );
    assert_eq!(
        f.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == "fixture.bundle")
            .count(),
        1
    );
    let snapshot: SnapshotResponse = sonic_rs::from_str(&f.snapshot.lock().unwrap()).unwrap();
    let url = snapshot.catalog_url(&client.config, Utc::now()).unwrap();
    let id = crate::cache::identity(
        &format!("jp:release:iOS:{url}"),
        &hex::encode(Sha256::digest(&bytes)),
        "fixture.bundle",
        assets::Provider::EncryptedBundle,
    );
    assert_eq!(
        std::fs::read(cache.path().join(id).join("data")).unwrap(),
        raw
    );
    server.abort();
}
#[test]
fn cache_identity_isolated_by_origin_catalog_version_path_and_provider_and_lock_excludes_writers() {
    use crate::assets::Provider;
    let id = crate::cache::identity(
        "https://one/asset/r1/iOS/h1/catalog_main.bin",
        "hash1",
        "a",
        Provider::Cri,
    );
    for (url, hash, path, provider) in [
        (
            "https://two/asset/r1/iOS/h1/catalog_main.bin",
            "hash1",
            "a",
            Provider::Cri,
        ),
        (
            "https://one/asset/r2/iOS/h1/catalog_main.bin",
            "hash1",
            "a",
            Provider::Cri,
        ),
        (
            "https://one/asset/r1/iOS/h1/catalog_main.bin",
            "hash2",
            "a",
            Provider::Cri,
        ),
        (
            "https://one/asset/r1/iOS/h1/catalog_main.bin",
            "hash1",
            "b",
            Provider::Cri,
        ),
        (
            "https://one/asset/r1/iOS/h1/catalog_main.bin",
            "hash1",
            "a",
            Provider::EncryptedBundle,
        ),
    ] {
        assert_ne!(id, crate::cache::identity(url, hash, path, provider));
    }
    let cache = tempfile::tempdir().unwrap();
    let guard = crate::cache::Guard::acquire(cache.path()).unwrap();
    assert!(matches!(
        crate::cache::Guard::acquire(cache.path()),
        Err(Error::Busy)
    ));
    drop(guard);
    assert!(crate::cache::Guard::acquire(cache.path()).is_ok());
}

#[tokio::test]
async fn offline_verification_checks_every_file_and_exact_catalog_dependency_graph() {
    let output = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = output.path().into();
    enable_assets(&mut cfg, false);
    let bytes = catalog_fixture(
        &[("{Fwk.Resource.RemoteAssetDir}/sound.acb", CRI_PROVIDER)],
        false,
    );
    let (client, f, server) = serve(cfg, StatusCode::OK, bytes, Duration::ZERO).await;
    f.assets.lock().unwrap().insert(
        "sound.acb".into(),
        (StatusCode::OK, b"@UTFfixture".to_vec()),
    );
    let path = client.fetch().await.unwrap();
    let report = crate::verify::verify(&path).await.unwrap();
    assert_eq!(report.asset_files_verified, 1);
    let receipt = std::fs::read(path.join("receipt.json")).unwrap();
    for case in 0..4 {
        match case {
            0 => {
                std::fs::write(path.join("assets/sound.acb"), b"@UTFchanged").unwrap();
            }
            1 => {
                std::fs::write(path.join("locations.json"), b"[]").unwrap();
            }
            2 => {
                let mut value: Receipt = sonic_rs::from_slice(&receipt).unwrap();
                value.update.as_mut().unwrap().assets.clear();
                std::fs::write(path.join("receipt.json"), sonic_rs::to_vec(&value).unwrap())
                    .unwrap();
            }
            _ => {
                let mut value: Receipt = sonic_rs::from_slice(&receipt).unwrap();
                value.update.as_mut().unwrap().assets[0].relative_path = "../outside".into();
                std::fs::write(path.join("receipt.json"), sonic_rs::to_vec(&value).unwrap())
                    .unwrap();
            }
        }
        assert!(crate::verify::verify(&path).await.is_err(), "case {case}");
        std::fs::write(path.join("assets/sound.acb"), b"@UTFfixture").unwrap();
        std::fs::write(path.join("receipt.json"), &receipt).unwrap();
        let catalog =
            crate::catalog::Catalog::parse(&std::fs::read(path.join("catalog_main.bin")).unwrap())
                .unwrap();
        std::fs::write(
            path.join("locations.json"),
            sonic_rs::to_vec(&catalog).unwrap(),
        )
        .unwrap();
    }
    assert!(crate::verify::verify(&path).await.is_ok());
    server.abort();
}
#[tokio::test]
async fn catalog_only_publication_is_structurally_valid_and_offline_verifiable() {
    for good in [false, true] {
        let output = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.output = output.path().into();
        let bytes = if good {
            catalog()
        } else {
            vec![0x42, 0x89, 0xe3, 0x0d, 2, 0, 0, 0, 1, 2, 3, 4]
        };
        let (client, _, server) = serve(cfg, StatusCode::OK, bytes, Duration::ZERO).await;
        let result = client.fetch().await;
        if good {
            let report = crate::verify::verify(&result.unwrap()).await.unwrap();
            assert_eq!(report.asset_files_verified, 0);
            assert_eq!(report.planned_remote_files, 1);
        } else {
            assert!(matches!(result, Err(Error::Catalog)));
            assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 0);
        }
        server.abort();
    }
}

#[tokio::test]
async fn cancellation_during_asset_body_removes_staging_preserves_complete_cache_and_releases_lock()
{
    let output = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = output.path().into();
    enable_assets(&mut cfg, false);
    cfg.assets.as_mut().unwrap().cache_directory = Some(cache.path().into());
    let catalog = catalog_fixture(
        &[
            ("{Fwk.Resource.RemoteAssetDir}/a", CRI_PROVIDER),
            ("{Fwk.Resource.RemoteAssetDir}/b", CRI_PROVIDER),
        ],
        false,
    );
    let (client, f, server) = serve(cfg, StatusCode::OK, catalog, Duration::ZERO).await;
    f.assets
        .lock()
        .unwrap()
        .insert("a".into(), (StatusCode::OK, b"complete".to_vec()));
    f.assets
        .lock()
        .unwrap()
        .insert("b".into(), (StatusCode::OK, b"partial".to_vec()));
    f.stalled_assets.lock().unwrap().insert("b".into());
    let task = tokio::spawn(async move { client.fetch().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let partial = std::fs::read_dir(output.path())
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.path().join("assets/b").exists());
            if partial {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 0);
    assert_eq!(
        std::fs::read_dir(cache.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().unwrap().is_dir())
            .count(),
        1
    );
    assert!(crate::cache::Guard::acquire(cache.path()).is_ok());
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn offline_verifier_rejects_symlinked_assets_even_when_target_bytes_match() {
    let output = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = output.path().into();
    enable_assets(&mut cfg, false);
    let bytes = catalog_fixture(
        &[("{Fwk.Resource.RemoteAssetDir}/sound", CRI_PROVIDER)],
        false,
    );
    let (client, f, server) = serve(cfg, StatusCode::OK, bytes, Duration::ZERO).await;
    f.assets
        .lock()
        .unwrap()
        .insert("sound".into(), (StatusCode::OK, b"correct".to_vec()));
    let path = client.fetch().await.unwrap();
    let outside = external.path().join("sound");
    std::fs::write(&outside, b"correct").unwrap();
    std::fs::remove_file(path.join("assets/sound")).unwrap();
    std::os::unix::fs::symlink(&outside, path.join("assets/sound")).unwrap();
    assert!(matches!(
        crate::verify::verify(&path).await,
        Err(Error::Verification)
    ));
    std::fs::remove_file(path.join("assets/sound")).unwrap();
    std::fs::remove_dir(path.join("assets")).unwrap();
    std::os::unix::fs::symlink(external.path(), path.join("assets")).unwrap();
    assert!(matches!(
        crate::verify::verify(&path).await,
        Err(Error::Verification)
    ));
    server.abort();
}

#[tokio::test]
async fn catalog_retries_transient_status_only_and_cleans_staging() {
    for (status, attempts) in [
        (StatusCode::TOO_MANY_REQUESTS, 3),
        (StatusCode::SERVICE_UNAVAILABLE, 3),
        (StatusCode::FORBIDDEN, 1),
        (StatusCode::PARTIAL_CONTENT, 1),
    ] {
        let output = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.output = output.path().into();
        let (client, f, server) = serve(cfg, status, catalog(), Duration::ZERO).await;
        assert!(matches!(client.fetch().await, Err(Error::Status(_))));
        assert_eq!(
            f.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(p, _)| p == "catalog")
                .count(),
            attempts
        );
        assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 0);
        server.abort();
    }
}

#[test]
fn dotted_versions_are_safe_but_dot_segments_are_not() {
    assert!(component("2.3.4.567"));
    for value in [".", "..", "../x", "x/y", "%2e%2e", "x?y", "x#y"] {
        assert!(!component(value));
    }
}

#[test]
fn provider_plan_preserves_parenthesized_bundle_names() {
    let name = "folder/profile(1)_hash.bundle";
    let id = format!("{{Fwk.Resource.RemoteAssetDir}}/{name}");
    let catalog =
        catalog::Catalog::parse(&catalog_fixture(&[(&id, CRYPT_PROVIDER)], false)).unwrap();
    let plan = catalog
        .plan("https://static.example/asset/2.3.4/iOS/hash")
        .unwrap();
    assert_eq!(plan.assets[0].relative_path, name);
    for path in [
        "../profile(1)",
        "profile(1)?x",
        "profile(1)#x",
        "%2e%2e/profile(1)",
    ] {
        assert!(!assets::safe_relative(path));
    }
}

#[tokio::test]
async fn concurrent_downloads_overlap_and_respect_total_budget() {
    for (total, success) in [(24, true), (16, false)] {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.output = dir.path().into();
        enable_assets(&mut cfg, false);
        let assets = cfg.assets.as_mut().unwrap();
        assets.concurrency = 2;
        assets.max_file_bytes = 8;
        assets.max_total_bytes = total;
        let entries = [
            ("{Fwk.Resource.RemoteAssetDir}/a", CRI_PROVIDER),
            ("{Fwk.Resource.RemoteAssetDir}/b", CRI_PROVIDER),
            ("{Fwk.Resource.RemoteAssetDir}/c", CRI_PROVIDER),
        ];
        let bytes = catalog_fixture(if success { &entries[..2] } else { &entries }, false);
        let (client, fixture, server) = serve(cfg, StatusCode::OK, bytes, Duration::ZERO).await;
        *fixture.asset_gate.lock().unwrap() = Some(Arc::new(tokio::sync::Barrier::new(2)));
        for name in ["a", "b", "c"] {
            fixture
                .assets
                .lock()
                .unwrap()
                .insert(name.into(), (StatusCode::OK, b"@UTFtest".to_vec()));
        }
        let result = tokio::time::timeout(Duration::from_secs(3), client.fetch())
            .await
            .expect("two requests must overlap");
        if success {
            let report = verify::verify(&result.unwrap()).await.unwrap();
            assert_eq!(report.asset_files_verified, 2);
        } else {
            assert!(matches!(result, Err(Error::Size)));
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
        assert!(!fixture
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(path, _)| path == "c"));
        server.abort();
    }
    let mut cfg = config();
    enable_assets(&mut cfg, false);
    for n in [0, 17] {
        cfg.assets.as_mut().unwrap().concurrency = n;
        assert!(cfg.validate().is_err());
    }
}

#[tokio::test]
async fn transient_snapshot_failures_retry_with_a_fixed_bound() {
    for failures in [2, 3] {
        let mut cfg = config();
        let token = format!("REFRESH_RETRY_{}", uuid::Uuid::new_v4().simple());
        std::env::set_var(&token, "api-refresh");
        cfg.refresh_token_env = Some(token);
        let (client, fixture, server) = serve(cfg, StatusCode::OK, vec![], Duration::ZERO).await;
        *fixture.system_failures.lock().unwrap() = failures;
        let result = client.probe().await;
        if failures == 2 {
            assert!(result.is_ok());
        } else {
            assert!(matches!(result, Err(Error::Status(502))));
        }
        let seen = fixture.seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|(path, _)| path == "system").count(), 3);
        assert!(!seen.iter().any(|(path, _)| path == "catalog"));
        server.abort();
    }
}

#[tokio::test]
async fn crypt_provider_preserves_plaintext_builtin_bundles_and_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = dir.path().join("published");
    enable_assets(&mut cfg, true);
    cfg.assets.as_mut().unwrap().cache_directory = Some(dir.path().join("cache"));
    let catalog = catalog_fixture(
        &[(
            "{Fwk.Resource.RemoteAssetDir}/builtin.bundle",
            CRYPT_PROVIDER,
        )],
        false,
    );
    let (client, fixture, server) = serve(cfg, StatusCode::OK, catalog, Duration::ZERO).await;
    let raw = b"UnityFS\0synthetic-plaintext".to_vec();
    fixture
        .assets
        .lock()
        .unwrap()
        .insert("builtin.bundle".into(), (StatusCode::OK, raw.clone()));
    for expected_hits in [0, 1] {
        let output = client.fetch().await.unwrap();
        assert_eq!(
            std::fs::read(output.join("assets/builtin.bundle")).unwrap(),
            raw
        );
        let receipt: Receipt =
            sonic_rs::from_slice(&std::fs::read(output.join("receipt.json")).unwrap()).unwrap();
        let update = receipt.update.unwrap();
        assert_eq!(update.cache_hits, expected_hits);
        assert!(!update.assets[0].decrypted);
        assert_eq!(
            update.assets[0].downloaded_sha256,
            update.assets[0].stored_sha256
        );
        assert_eq!(verify::verify(&output).await.unwrap().decrypted_bundles, 0);
    }
    server.abort();
}

#[test]
fn regional_snapshots_require_explicit_identity_and_keep_cdn_prefixes() {
    let mut cfg = config();
    cfg.region = region::Region::En;
    cfg.client_version = "1.0.1".into();
    let base = "https://cdn.example/prod/en_fixture";
    cfg.cdn_roots = BTreeMap::from([(
        base.into(),
        CdnAuth {
            username_env: "U".into(),
            credential_env: "P".into(),
        },
    )]);
    assert!(cfg.validate().is_ok());
    let mut s = snapshot();
    s.snapshot.schema_version = 2;
    s.snapshot.region = Some(region::Region::En);
    s.snapshot.platform = "Android".into();
    s.snapshot.client_version = "1.0.1".into();
    s.snapshot.protocol_version = "1.0.1".into();
    s.snapshot.effective_cdn_root = base.into();
    s.snapshot.credential_ref = "P".into();
    assert_eq!(
        s.catalog_url(&cfg, Utc::now()).unwrap(),
        "https://cdn.example/prod/en_fixture/asset/r1/Android/h1/catalog_main.bin"
    );
    for region in [
        None,
        Some(region::Region::Jp),
        Some(region::Region::Tw),
        Some(region::Region::Kr),
        Some(region::Region::Cn),
    ] {
        let mut wrong = s.clone();
        wrong.snapshot.region = region;
        assert!(wrong.catalog_url(&cfg, Utc::now()).is_err());
    }
    s.snapshot.schema_version = 1;
    s.snapshot.region = None;
    assert!(s.catalog_url(&cfg, Utc::now()).is_err());
    cfg.region = region::Region::Cn;
    assert!(cfg.validate().is_err());
    for bad in [
        "https://cdn.example/prod/../en",
        "https://cdn.example/%2f/prod",
        "https://cdn.example/prod/en?x=1",
        "https://cdn.example/prod/en/",
    ] {
        assert!(!cdn_root(bad), "{bad}");
    }
}
#[tokio::test]
async fn wrong_region_never_reaches_cdn_even_when_url_and_credentials_match() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = dir.path().into();
    let (client, f, server) = serve(cfg, StatusCode::OK, vec![], Duration::ZERO).await;
    let mut s: SnapshotResponse = sonic_rs::from_str(&f.snapshot.lock().unwrap()).unwrap();
    s.snapshot.schema_version = 2;
    s.snapshot.region = Some(region::Region::Kr);
    *f.snapshot.lock().unwrap() = sonic_rs::to_string(&s).unwrap();
    assert!(matches!(client.fetch().await, Err(Error::Snapshot)));
    assert!(f.seen.lock().unwrap().iter().all(|(p, _)| p == "snapshot"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    server.abort();
}
#[test]
fn cache_identity_separates_regions_even_with_shared_cdn_and_catalog() {
    let id = cache::identity(
        "en:release:Android:https://shared/catalog",
        "same",
        "a",
        assets::Provider::Cri,
    );
    for scope in [
        "kr:release:Android:https://shared/catalog",
        "en:review:Android:https://shared/catalog",
        "en:release:iOS:https://shared/catalog",
    ] {
        assert_ne!(
            id,
            cache::identity(scope, "same", "a", assets::Provider::Cri)
        );
    }
}

#[test]
fn persisted_identity_accepts_legacy_jp_and_rejects_ambiguous_or_reserved_regions() {
    let mut s = snapshot().snapshot;
    assert_eq!(s.region_identity().unwrap(), region::Region::Jp);
    s.schema_version = 2;
    assert!(s.region_identity().is_err());
    s.region = Some(region::Region::Cn);
    assert!(s.region_identity().is_err());
    s.region = Some(region::Region::Kr);
    s.platform = "Android".into();
    assert_eq!(s.region_identity().unwrap(), region::Region::Kr);
    s.schema_version = 1;
    assert!(s.region_identity().is_err());
}

#[tokio::test]
async fn job_service_auth_queue_and_real_offline_verification() {
    use crate::{
        jobs::{Job, Status},
        service::{Profile, Service, ServiceConfig},
    };
    let directory = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.output = directory.path().join("input");
    let (client, _, upstream) = serve(cfg, StatusCode::OK, catalog(), Duration::ZERO).await;
    let publication = client.fetch().await.unwrap();
    upstream.abort();
    let env = format!("SERVICE_TEST_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&env, "service-only-token");
    let service = Service::open(ServiceConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        token_env: env,
        state_directory: directory.path().join("state"),
        output_directory: directory.path().join("jobs"),
        max_concurrent_jobs: 1,
        max_queued_jobs: 1,
        retain_terminal_jobs: 10,
        timeout_seconds: 30,
        profiles: BTreeMap::from([(
            "verify".into(),
            Profile {
                region: region::Region::Jp,
                download_config: None,
                export_config: None,
                input: Some(publication),
            },
        )]),
    })
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = service.router();
    let http = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    let endpoint = format!("{url}/api/v1/jobs");
    assert_eq!(
        client.get(&endpoint).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let body = r#"{"region":"jp","profile":"verify","operation":"verify"}"#;
    let response = client
        .post(&endpoint)
        .bearer_auth("service-only-token")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let queued: Job = sonic_rs::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(
        client
            .post(&endpoint)
            .bearer_auth("service-only-token")
            .body(body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        client
            .post(&endpoint)
            .bearer_auth("service-only-token")
            .body(body.replace("jp", "en"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .post(&endpoint)
            .bearer_auth("service-only-token")
            .body(body.replace("verify\"}", "update\"}"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let cancel = format!("{endpoint}/{}/cancel", queued.id);
    let cancelled: Job = sonic_rs::from_str(
        &client
            .post(cancel)
            .bearer_auth("service-only-token")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cancelled.status, Status::Cancelled);
    let response = client
        .post(format!("{endpoint}/{}/retry", queued.id))
        .bearer_auth("service-only-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job: Job = sonic_rs::from_str(&response.text().await.unwrap()).unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(async move { service.run_workers(rx).await });
    let complete = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let text = client
                .get(format!("{endpoint}/{}", job.id))
                .bearer_auth("service-only-token")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(!text.contains("service-only-token") && !text.contains("cdn-fixture"));
            let job: Job = sonic_rs::from_str(&text).unwrap();
            if job.status.terminal() {
                break job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(complete.status, Status::Completed);
    assert!(directory
        .path()
        .join("jobs/jp")
        .join(&job.id)
        .join("verification.json")
        .is_file());
    stop.send(true).unwrap();
    workers.await.unwrap().unwrap();
    http.abort();
}
