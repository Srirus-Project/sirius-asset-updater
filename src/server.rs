//! Optional HTTPS for the local service listener; unrelated to outbound game TLS.
use axum::Router;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use std::{
    fs,
    future::Future,
    io::{self, Read},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub certificate_file: PathBuf,
    pub private_key_file: PathBuf,
    #[serde(default = "handshake_timeout")]
    pub handshake_timeout_ms: u64,
}
fn handshake_timeout() -> u64 {
    10_000
}
impl TlsConfig {
    pub fn validate(&self) -> io::Result<()> {
        if self.certificate_file.as_os_str().is_empty()
            || self.private_key_file.as_os_str().is_empty()
            || !(100..=60_000).contains(&self.handshake_timeout_ms)
        {
            return Err(invalid_tls());
        }
        Ok(())
    }
    pub fn load(&self) -> io::Result<LoadedTls> {
        self.validate()?;
        let certificates = read_pem(&self.certificate_file, false)?;
        let private_key = read_pem(&self.private_key_file, true)?;
        let certs = CertificateDer::pem_slice_iter(&certificates)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| invalid_tls())?;
        if certs.is_empty() {
            return Err(invalid_tls());
        }
        let key = PrivateKeyDer::from_pem_slice(&private_key).map_err(|_| invalid_tls())?;
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| invalid_tls())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|_| invalid_tls())?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(LoadedTls {
            config: RustlsConfig::from_config(Arc::new(config)),
            handshake_timeout: Duration::from_millis(self.handshake_timeout_ms),
        })
    }
}
#[derive(Clone)]
pub struct LoadedTls {
    config: RustlsConfig,
    handshake_timeout: Duration,
}
fn invalid_tls() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid TLS certificate, private key or listener configuration",
    )
}
fn read_pem(path: &Path, private: bool) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path).map_err(|_| invalid_tls())?;
    let meta = file.metadata().map_err(|_| invalid_tls())?;
    if !meta.is_file() || meta.len() > 128 * 1024 {
        return Err(invalid_tls());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Permit owner access and group read (e.g. a dedicated service group).
        if private && meta.permissions().mode() & 0o027 != 0 {
            return Err(invalid_tls());
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut bytes = vec![];
    file.take(128 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid_tls())?;
    if bytes.len() > 128 * 1024 {
        return Err(invalid_tls());
    }
    Ok(bytes)
}
/// The caller validates/loads TLS before binding and before starting background work.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    tls: Option<LoadedTls>,
    shutdown: impl Future<Output = ()> + Send,
) -> io::Result<()> {
    let handle = axum_server::Handle::new();
    let server_handle = handle.clone();
    let listener = listener.into_std()?;
    let server = async move {
        let service = router.into_make_service_with_connect_info::<SocketAddr>();
        let server = axum_server::from_tcp(listener)?.handle(server_handle);
        match tls {
            Some(tls) => {
                server
                    .acceptor(
                        RustlsAcceptor::new(tls.config).handshake_timeout(tls.handshake_timeout),
                    )
                    .serve(service)
                    .await
            }
            None => server.serve(service).await,
        }
    };
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        _ = shutdown => { handle.graceful_shutdown(None); server.await }
    }
}
