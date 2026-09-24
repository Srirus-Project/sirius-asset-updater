//! Explicit per-client forward-proxy configuration; never origin default headers.
use crate::Error;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub url_env: String,
    #[serde(default)]
    pub authorization_env: Option<String>,
}
impl ProxyConfig {
    pub fn validate(&self) -> Result<(), Error> {
        for name in std::iter::once(&self.url_env).chain(self.authorization_env.iter()) {
            if name.is_empty()
                || name.len() > 256
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(Error::Config);
            }
        }
        Ok(())
    }
    pub(crate) fn resolve(&self) -> Result<reqwest::Proxy, Error> {
        self.validate()?;
        let value = crate::secret(&self.url_env)?;
        let url = reqwest::Url::parse(&value).map_err(|_| Error::Config)?;
        if value.len() > 4096
            || value.chars().any(char::is_whitespace)
            || value.contains('\\')
            || !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Config);
        }
        let mut proxy = reqwest::Proxy::all(url).map_err(|_| Error::Config)?;
        if let Some(name) = &self.authorization_env {
            let value = crate::secret(name)?;
            if value.len() > 4096 || value.trim().is_empty() {
                return Err(Error::Config);
            }
            let mut header =
                reqwest::header::HeaderValue::from_str(&value).map_err(|_| Error::Config)?;
            header.set_sensitive(true);
            proxy = proxy.custom_http_auth(header);
        }
        Ok(proxy)
    }
}

pub(crate) fn builder(
    network: &crate::network::Network,
    proxy: Option<&ProxyConfig>,
) -> Result<reqwest::ClientBuilder, Error> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_millis(
            network.download_timeout_ms,
        ))
        .connect_timeout(std::time::Duration::from_millis(network.connect_timeout_ms));
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy.resolve()?);
    }
    Ok(builder)
}
