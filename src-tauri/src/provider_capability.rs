use std::{net::IpAddr, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RelayTransport {
    #[default]
    Http,
    Websocket,
}

impl RelayTransport {
    pub const fn supports_websockets(self) -> bool {
        matches!(self, Self::Websocket)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedRelayOrigin {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProviderCapabilityError {
    #[error("relay base URL is required")]
    MissingBaseUrl,
    #[error("invalid relay base URL")]
    InvalidBaseUrl,
    #[error("relay base URL must not contain credentials, query, or fragment")]
    UnsafeBaseUrl,
    #[error("non-loopback relay URLs must use HTTPS")]
    InsecureRemoteBaseUrl,
    #[error("stored relay base URL is invalid")]
    InvalidStoredBaseUrl,
}

/// Normalize the user-entered API root without inventing a path the user did not request.
///
/// An origin-only URL receives `/v1`, which is the standard Responses API root. Any explicit
/// non-root path is preserved (apart from trailing slashes) because reverse proxies commonly
/// mount an OpenAI-compatible API below a custom prefix.
pub fn normalize_relay_base_url(raw: &str) -> Result<String, ProviderCapabilityError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ProviderCapabilityError::MissingBaseUrl);
    }

    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let mut parsed =
        Url::parse(&with_scheme).map_err(|_| ProviderCapabilityError::InvalidBaseUrl)?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(ProviderCapabilityError::InvalidBaseUrl);
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ProviderCapabilityError::UnsafeBaseUrl);
    }
    if parsed.scheme() == "http" && !is_loopback_host(parsed.host_str()) {
        return Err(ProviderCapabilityError::InsecureRemoteBaseUrl);
    }

    let explicit_path = parsed.path().trim_end_matches('/').to_string();
    parsed.set_path(if explicit_path.is_empty() {
        "/v1"
    } else {
        &explicit_path
    });
    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

pub fn normalized_relay_origin(
    value: &str,
) -> Result<NormalizedRelayOrigin, ProviderCapabilityError> {
    let normalized = normalize_relay_base_url(value)
        .map_err(|_| ProviderCapabilityError::InvalidStoredBaseUrl)?;
    let parsed =
        Url::parse(&normalized).map_err(|_| ProviderCapabilityError::InvalidStoredBaseUrl)?;
    let host = parsed
        .host_str()
        .ok_or(ProviderCapabilityError::InvalidStoredBaseUrl)?;
    let port = parsed
        .port_or_known_default()
        .ok_or(ProviderCapabilityError::InvalidStoredBaseUrl)?;
    Ok(NormalizedRelayOrigin {
        scheme: parsed.scheme().to_ascii_lowercase(),
        host: host.to_ascii_lowercase(),
        port,
    })
}

pub fn is_loopback_host(host: Option<&str>) -> bool {
    host.is_some_and(|host| {
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        host.eq_ignore_ascii_case("localhost")
            || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback())
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{
        normalize_relay_base_url, normalized_relay_origin, ProviderCapabilityError, RelayTransport,
    };

    #[test]
    fn http_is_the_compatibility_default() {
        assert_eq!(RelayTransport::default(), RelayTransport::Http);
        assert!(!RelayTransport::Http.supports_websockets());
        assert!(RelayTransport::Websocket.supports_websockets());
    }

    #[test]
    fn origin_only_urls_receive_the_standard_v1_root() {
        assert_eq!(
            normalize_relay_base_url("api.example.com").unwrap(),
            "https://api.example.com/v1"
        );
        assert_eq!(
            normalize_relay_base_url("https://api.example.com/").unwrap(),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn explicit_proxy_paths_are_preserved_instead_of_receiving_an_extra_v1() {
        assert_eq!(
            normalize_relay_base_url("https://api.example.com/openai").unwrap(),
            "https://api.example.com/openai"
        );
        assert_eq!(
            normalize_relay_base_url("https://api.example.com/openai/").unwrap(),
            "https://api.example.com/openai"
        );
        assert_eq!(
            normalize_relay_base_url("https://api.example.com/openai/v1").unwrap(),
            "https://api.example.com/openai/v1"
        );
    }

    #[test]
    fn remote_http_and_credential_bearing_urls_are_rejected() {
        assert_eq!(
            normalize_relay_base_url("http://api.example.com/v1").unwrap_err(),
            ProviderCapabilityError::InsecureRemoteBaseUrl
        );
        assert_eq!(
            normalize_relay_base_url("https://user:pass@api.example.com/v1").unwrap_err(),
            ProviderCapabilityError::UnsafeBaseUrl
        );
        assert_eq!(
            normalize_relay_base_url("https://api.example.com/v1?token=x").unwrap_err(),
            ProviderCapabilityError::UnsafeBaseUrl
        );
    }

    #[test]
    fn stored_origin_validation_rejects_unsafe_or_insecure_values() {
        assert_eq!(
            normalized_relay_origin("http://api.example.com/v1").unwrap_err(),
            ProviderCapabilityError::InvalidStoredBaseUrl
        );
        assert_eq!(
            normalized_relay_origin("https://user:pass@api.example.com/v1").unwrap_err(),
            ProviderCapabilityError::InvalidStoredBaseUrl
        );
    }

    #[test]
    fn loopback_http_and_origin_comparison_remain_supported() {
        assert_eq!(
            normalize_relay_base_url("http://127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(
            normalized_relay_origin("https://API.EXAMPLE.com/path").unwrap(),
            normalized_relay_origin("https://api.example.com:443/other").unwrap()
        );
    }
    proptest! {
        #[test]
        fn normalized_https_urls_are_idempotent(
            label in "[a-z][a-z0-9]{0,15}",
            segments in prop::collection::vec("[a-z][a-z0-9_-]{0,12}", 0..4),
        ) {
            let path = if segments.is_empty() {
                String::new()
            } else {
                format!("/{}", segments.join("/"))
            };
            let input = format!("https://{label}.example.com{path}");
            let once = normalize_relay_base_url(&input).unwrap();
            let twice = normalize_relay_base_url(&once).unwrap();
            prop_assert_eq!(once, twice);
        }

        #[test]
        fn relay_transport_json_round_trips(websocket in any::<bool>()) {
            let transport = if websocket {
                RelayTransport::Websocket
            } else {
                RelayTransport::Http
            };
            let json = serde_json::to_string(&transport).unwrap();
            let decoded: RelayTransport = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(decoded, transport);
            prop_assert_eq!(decoded.supports_websockets(), websocket);
        }
    }
}
