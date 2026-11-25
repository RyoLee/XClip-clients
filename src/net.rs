//! HTTP protocol implementation for the XClip server.
//!
//! Wire format is unchanged from the original client:
//!   POST /<md5(username)>   header token, form value (base64)
//!   token = md5(md5(password) + (unix_time / 10))
//!
//! The server accepts the current time slice and its two neighbours, so a
//! +-20s clock skew is tolerated.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use md5::{Digest, Md5};

use crate::config::NetConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetError {
    /// Server rejected the request (bad credentials, disabled account).
    Forbidden(u16),
    /// Server returned 204 / an empty payload: nothing to paste.
    Empty,
    /// Response body was not valid base64/UTF-8.
    Corrupt(String),
    /// Transport-level failure.
    Transport(String),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::Forbidden(code) => write!(f, "server rejected the request (HTTP {code})"),
            NetError::Empty => write!(f, "no content on the server"),
            NetError::Corrupt(why) => write!(f, "malformed response: {why}"),
            NetError::Transport(why) => write!(f, "network error: {why}"),
        }
    }
}

impl std::error::Error for NetError {}

pub fn md5_hex(input: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(input);
    format!("{:x}", h.finalize())
}

/// Time-derived token. Kept byte-for-byte compatible with the original client.
pub fn token_for(password: &str, unix_secs: u64) -> String {
    let salt = unix_secs - (unix_secs % 10);
    let salt = salt / 10;
    let inner = format!("{}{}", md5_hex(password.as_bytes()), salt);
    md5_hex(inner.as_bytes())
}

pub struct Client {
    base_url: String,
    password: String,
    agent: ureq::Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResult {
    pub text: String,
}

impl Client {
    pub fn new(cfg: &NetConfig) -> Result<Self, NetError> {
        if cfg.server.is_empty() {
            return Err(NetError::Transport("main.server is not configured".into()));
        }
        if cfg.id.is_empty() {
            return Err(NetError::Transport("main.id is not configured".into()));
        }
        let agent = ureq::AgentBuilder::new()
            .timeout(cfg.http_timeout)
            .timeout_connect(Duration::from_millis(1500))
            .build();
        Ok(Self {
            base_url: format!("{}/{}", cfg.server.trim_end_matches('/'), md5_hex(cfg.id.as_bytes())),
            password: cfg.password.clone(),
            agent,
        })
    }

    fn token(&self) -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        token_for(&self.password, secs)
    }

    /// Upload text. Returns only after the server confirmed with "done".
    pub fn upload(&self, text: &str) -> Result<(), NetError> {
        let payload = B64.encode(text.as_bytes());
        let resp = self
            .agent
            .post(&self.base_url)
            .set("token", &self.token())
            .send_form(&[("value", &payload)]);

        match resp {
            Ok(r) => {
                let status = r.status();
                let body = r.into_string().unwrap_or_default();
                if status != 200 {
                    return Err(NetError::Forbidden(status));
                }
                if body.trim() != "done" {
                    return Err(NetError::Corrupt(format!(
                        "unexpected upload reply: {:?}",
                        body.chars().take(40).collect::<String>()
                    )));
                }
                Ok(())
            }
            Err(ureq::Error::Status(code, _)) => Err(NetError::Forbidden(code)),
            Err(ureq::Error::Transport(t)) => Err(NetError::Transport(t.to_string())),
        }
    }

    /// Download text. An empty body is reported as Empty, never as "".
    pub fn download(&self) -> Result<FetchResult, NetError> {
        let resp = self
            .agent
            .get(&self.base_url)
            .set("token", &self.token())
            .call();

        let body = match resp {
            Ok(r) => {
                if r.status() == 204 {
                    return Err(NetError::Empty);
                }
                r.into_string().map_err(|e| NetError::Transport(e.to_string()))?
            }
            // The server answers 204 with no body; ureq surfaces that as
            // Error::Status rather than Ok in some versions.
            Err(ureq::Error::Status(204, _)) => return Err(NetError::Empty),
            Err(ureq::Error::Status(code, _)) => return Err(NetError::Forbidden(code)),
            Err(ureq::Error::Transport(t)) => return Err(NetError::Transport(t.to_string())),
        };

        if body.trim().is_empty() {
            return Err(NetError::Empty);
        }

        let raw = B64
            .decode(body.trim())
            .map_err(|e| NetError::Corrupt(format!("invalid base64: {e}")))?;
        if raw.is_empty() {
            return Err(NetError::Empty);
        }
        let text = String::from_utf8(raw)
            .map_err(|e| NetError::Corrupt(format!("invalid UTF-8: {e}")))?;
        if text.is_empty() {
            return Err(NetError::Empty);
        }
        Ok(FetchResult { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_vector() {
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn token_matches_server_formula() {
        // Mirrors XClip.py get_tokens(): md5(md5(pw) + str(int(ts/10)))
        let pw = "test";
        let ts = 1_700_000_003u64;
        let salt = ts / 10;
        let expected = md5_hex(format!("{}{}", md5_hex(pw.as_bytes()), salt).as_bytes());
        assert_eq!(token_for(pw, ts), expected);
    }

    #[test]
    fn token_is_stable_within_a_slice() {
        let a = token_for("pw", 1_700_000_000);
        let b = token_for("pw", 1_700_000_009);
        assert_eq!(a, b);
        assert_ne!(a, token_for("pw", 1_700_000_010));
    }

    #[test]
    fn token_changes_with_password() {
        assert_ne!(token_for("a", 100), token_for("b", 100));
    }

    #[test]
    fn empty_server_is_an_error() {
        let cfg = NetConfig {
            server: String::new(),
            id: "x".into(),
            password: "p".into(),
            http_timeout: Duration::from_millis(100),
        };
        assert!(Client::new(&cfg).is_err());
    }

    #[test]
    fn base64_roundtrip_of_unicode() {
        let text = "hello \u{4f60}\u{597d} \u{1f600}";
        let enc = B64.encode(text.as_bytes());
        let dec = String::from_utf8(B64.decode(enc).unwrap()).unwrap();
        assert_eq!(dec, text);
    }
}
