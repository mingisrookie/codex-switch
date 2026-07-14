use reqwest::{blocking::Client, redirect::Policy, Url};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

const RELAY_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RelayVerificationResult {
    pub verified: bool,
    pub status_code: u16,
    pub model_count: usize,
    pub message: String,
}

pub fn verify_relay(base_url: &str, api_key: &str) -> Result<RelayVerificationResult, String> {
    verify_relay_with_timeout(base_url, api_key, RELAY_VERIFY_TIMEOUT)
}

fn verify_relay_with_timeout(
    base_url: &str,
    api_key: &str,
    timeout: Duration,
) -> Result<RelayVerificationResult, String> {
    let url = models_url(base_url)?;
    let client = Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .redirect(Policy::none())
        .build()
        .map_err(|_| "failed to initialize relay verification client".to_string())?;
    let response = client
        .get(url)
        .bearer_auth(api_key)
        .send()
        .map_err(|error| {
            if error.is_timeout() {
                "relay verification timed out".to_string()
            } else {
                "relay verification request failed".to_string()
            }
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "relay verification failed with HTTP {}",
            status.as_u16()
        ));
    }
    let body: Value = response
        .json()
        .map_err(|_| "relay /models response was not valid JSON".to_string())?;
    let models = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "relay /models response did not contain a model list".to_string())?;

    Ok(RelayVerificationResult {
        verified: true,
        status_code: status.as_u16(),
        model_count: models.len(),
        message: "Relay connection verified".to_string(),
    })
}

fn models_url(base_url: &str) -> Result<Url, String> {
    let mut url = Url::parse(base_url.trim()).map_err(|_| "invalid relay base URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("invalid relay base URL".to_string());
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/');
    url.set_path(&format!("{path}/models"));
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc::{self, Receiver},
        thread,
        time::{Duration, Instant},
    };

    use super::{verify_relay, verify_relay_with_timeout};

    fn spawn_server(response: &'static [u8], delay: Duration) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        request.extend_from_slice(&buffer[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = sender.send(String::from_utf8_lossy(&request).into_owned());
            thread::sleep(delay);
            let _ = stream.write_all(response);
        });
        (format!("http://{address}/v1"), receiver)
    }

    #[test]
    fn verifies_models_endpoint_with_bearer_auth_without_returning_the_key() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: close\r\n\r\n{\"data\":[{\"id\":\"one\"},{\"id\":\"two\"}]}";
        let (base_url, request) = spawn_server(response, Duration::ZERO);
        let api_key = "sk-test-relay-secret";

        let result = verify_relay(&base_url, api_key).unwrap();

        let request = request.recv_timeout(Duration::from_secs(1)).unwrap();
        let request_lower = request.to_ascii_lowercase();
        assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
        assert!(request_lower.contains("authorization: bearer sk-test-relay-secret\r\n"));
        assert!(result.verified);
        assert_eq!(result.status_code, 200);
        assert_eq!(result.model_count, 2);
        assert!(!format!("{result:?}").contains(api_key));
    }

    #[test]
    fn rejects_non_success_status_without_exposing_key_or_response_body() {
        let response = b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 39\r\nConnection: close\r\n\r\n{\"error\":\"sk-test-relay-secret denied\"}";
        let (base_url, _) = spawn_server(response, Duration::ZERO);
        let api_key = "sk-test-relay-secret";

        let error = verify_relay(&base_url, api_key).unwrap_err();

        assert!(error.contains("HTTP 401"), "{error}");
        assert!(!error.contains(api_key));
        assert!(!error.contains("denied"));
    }

    #[test]
    fn rejects_invalid_url_without_echoing_input_or_key() {
        let api_key = "sk-test-relay-secret";
        let base_url = "not a url/sk-test-relay-secret";

        let error = verify_relay(base_url, api_key).unwrap_err();

        assert!(error.contains("invalid relay base URL"));
        assert!(!error.contains(base_url));
        assert!(!error.contains(api_key));
    }

    #[test]
    fn reports_timeout_without_exposing_key() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}";
        let (base_url, _) = spawn_server(response, Duration::from_millis(250));
        let api_key = "sk-test-relay-secret";
        let started = Instant::now();

        let error =
            verify_relay_with_timeout(&base_url, api_key, Duration::from_millis(40)).unwrap_err();

        assert!(error.contains("timed out"), "{error}");
        assert!(!error.contains(api_key));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
