//! Transactional email via Resend, plus OTP generation and hashing.
#![forbid(unsafe_code)]

mod error;
pub mod templates;

pub use error::EmailError;

use rand::Rng;
use sha2::{Digest, Sha256};
use octo_resilience::{execute, CallKind, CircuitBreaker, ResilienceError, RetryPolicy};
use std::time::Duration;

#[cfg(feature = "test-fixtures")]
use std::sync::{Arc, Mutex};

const RESEND_URL: &str = "https://api.resend.com/emails";
/// Hard cap on one `send` call across every attempt and backoff sleep.
const SEND_DEADLINE: Duration = Duration::from_secs(25);

/// A captured OTP, recorded instead of actually emailed when using [`EmailSender::new_captured`].
#[cfg(feature = "test-fixtures")]
#[derive(Debug, Clone)]
pub struct CapturedOtp {
    pub to: String,
    pub code: String,
}

/// Sends transactional email via Resend's HTTP API.
#[derive(Clone)]
pub struct EmailSender {
    api_key: String,
    from_address: String,
    http: reqwest::Client,
    endpoint: String,
    policy: RetryPolicy,
    circuit: CircuitBreaker,
    #[cfg(feature = "test-fixtures")]
    captured: Option<Arc<Mutex<Vec<CapturedOtp>>>>,
    #[cfg(feature = "test-fixtures")]
    always_fail: bool,
}

impl EmailSender {
    pub fn new(api_key: String, from_address: String) -> Self {
        Self {
            api_key,
            from_address,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            endpoint: RESEND_URL.to_string(),
            policy: RetryPolicy::default(),
            // High threshold: email outages shouldn't lock out sends for unrelated users for long.
            circuit: CircuitBreaker::new(20, Duration::from_secs(10)),
            #[cfg(feature = "test-fixtures")]
            captured: None,
            #[cfg(feature = "test-fixtures")]
            always_fail: false,
        }
    }

    /// A sender that records OTP codes in memory instead of calling Resend — lets integration
    /// tests complete the signup/withdrawal OTP flow without a real inbox.
    #[cfg(feature = "test-fixtures")]
    pub fn new_captured() -> Self {
        Self {
            captured: Some(Arc::new(Mutex::new(Vec::new()))),
            ..Self::new(String::new(), "test@octo.test".to_string())
        }
    }

    /// A sender that always fails to send — simulates a Resend rejection (e.g. unverified
    /// domain), so callers can test their rollback/error-handling behavior.
    #[cfg(feature = "test-fixtures")]
    pub fn new_failing() -> Self {
        Self {
            always_fail: true,
            ..Self::new(String::new(), "test@octo.test".to_string())
        }
    }

    /// The most recently captured OTP for `to`, if any. Test-fixtures only.
    #[cfg(feature = "test-fixtures")]
    pub fn last_otp_for(&self, to: &str) -> Option<String> {
        let captured = self.captured.as_ref()?.lock().ok()?;
        captured
            .iter()
            .rev()
            .find(|c| c.to == to)
            .map(|c| c.code.clone())
    }

    /// Send an OTP email. In captured mode, records `code` instead of calling Resend.
    pub async fn send_otp(&self, to: &str, purpose: &str, code: &str) -> Result<(), EmailError> {
        #[cfg(feature = "test-fixtures")]
        if self.always_fail {
            return Err(EmailError::Rejected("simulated failure".into()));
        }
        #[cfg(feature = "test-fixtures")]
        if let Some(captured) = &self.captured {
            captured.lock().unwrap().push(CapturedOtp {
                to: to.to_string(),
                code: code.to_string(),
            });
            return Ok(());
        }
        let html = templates::otp_email(code, purpose);
        self.send(to, "Your Octo verification code", &html).await
    }

    /// Send one HTML email, retrying transient failures (429/5xx/timeouts) with backoff within
    /// [`SEND_DEADLINE`]. Logs and propagates failure — callers on a critical path (e.g. an
    /// OTP the user has no other way to receive) need to know sending failed, not just log it.
    pub async fn send(&self, to: &str, subject: &str, html: &str) -> Result<(), EmailError> {
        let attempts = execute(&self.circuit, &self.policy, CallKind::ReadOnly, || {
            self.post_once(to, subject, html)
        });
        match tokio::time::timeout(SEND_DEADLINE, attempts).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(ResilienceError::Exhausted(e))) => Err(e),
            Ok(Err(ResilienceError::Circuit)) => {
                Err(EmailError::Unavailable("circuit breaker open".into()))
            }
            Err(_) => Err(EmailError::DeadlineExceeded),
        }
    }

    /// One POST to Resend, classifying the response as success, transient or permanent.
    async fn post_once(&self, to: &str, subject: &str, html: &str) -> Result<(), EmailError> {
        let body = serde_json::json!({
            "from": self.from_address,
            "to": to,
            "subject": subject,
            "html": html,
        });

        let resp = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let detail = resp.text().await.unwrap_or_default();
        tracing::warn!(to, subject, %status, detail, "Resend rejected an email send");
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            Err(EmailError::Unavailable(detail))
        } else {
            Err(EmailError::Rejected(detail))
        }
    }
}

/// A fresh 6-digit numeric OTP, zero-padded (e.g. "042817").
pub fn generate_otp() -> String {
    let n: u32 = rand::rngs::OsRng.gen_range(0..1_000_000);
    format!("{n:06}")
}

/// SHA-256 hex digest of an OTP — stored instead of the raw code, same principle as password
/// hashing elsewhere in this codebase.
pub fn hash_otp(code: &str) -> String {
    let digest = Sha256::digest(code.as_bytes());
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // Gross-bug smoke test: asserts digit distribution across 100k samples is roughly uniform (not a crypto audit).
    #[test]
    fn generate_otp_produces_a_roughly_uniform_distribution_of_digits_across_a_large_sample() {
        const SAMPLES: usize = 100_000;
        let mut digit_counts = [[0usize; 10]; 6];

        for _ in 0..SAMPLES {
            let otp = generate_otp();
            assert_eq!(otp.len(), 6, "OTP must always be exactly 6 characters");
            assert!(otp.chars().all(|c| c.is_ascii_digit()), "OTP must only contain digits");

            for (pos, ch) in otp.chars().enumerate() {
                let d = ch.to_digit(10).expect("valid digit") as usize;
                digit_counts[pos][d] += 1;
            }
        }

        // Each digit at each position has expected frequency 10,000; assert within 8,000..=12,000.
        for pos in 0..6 {
            for digit in 0..10 {
                let count = digit_counts[pos][digit];
                assert!(
                    (8_000..=12_000).contains(&count),
                    "digit {digit} at position {pos} occurred {count} times (expected ~10,000)"
                );
            }
        }
    }

    // Gross-bug smoke test: duplicate rate across 100k draws in 1M space matches birthday-paradox expectation.
    #[test]
    fn generate_otp_duplicate_rate_across_a_large_sample_is_consistent_with_true_uniform_randomness() {
        const SAMPLES: usize = 100_000;
        let mut seen = HashSet::with_capacity(SAMPLES);

        for _ in 0..SAMPLES {
            seen.insert(generate_otp());
        }

        // For 100,000 draws from 1,000,000 bins, expected unique is ~95,163 (assert within 92,000..=98,000).
        let unique_count = seen.len();
        assert!(
            (92_000..=98_000).contains(&unique_count),
            "expected ~95,163 unique codes, got {unique_count}"
        );
    }

    /// Serve `statuses` in order, one per connection, counting the requests received.
    async fn mock_resend(statuses: Vec<u16>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/emails", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let status = statuses.get(n).copied().unwrap_or(500);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = format!("HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (url, hits)
    }

    fn fast_sender(endpoint: String) -> EmailSender {
        let mut s = EmailSender::new("k".into(), "a@b.test".into());
        s.endpoint = endpoint;
        s.policy = RetryPolicy {
            base_delay_ms: 1,
            max_delay_ms: 5,
            ..Default::default()
        };
        s
    }

    #[tokio::test]
    async fn send_retries_transient_failures_until_success() {
        let (url, hits) = mock_resend(vec![503, 429, 200]).await;
        fast_sender(url).send("u@x.test", "s", "<p>h</p>").await.unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn send_never_retries_a_4xx_validation_failure() {
        let (url, hits) = mock_resend(vec![422, 200]).await;
        let err = fast_sender(url).send("u@x.test", "s", "<p>h</p>").await.unwrap_err();
        assert!(matches!(err, EmailError::Rejected(_)));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn send_gives_up_after_max_attempts_on_persistent_5xx() {
        let (url, hits) = mock_resend(vec![500, 500, 500, 200]).await;
        let err = fast_sender(url).send("u@x.test", "s", "<p>h</p>").await.unwrap_err();
        assert!(matches!(err, EmailError::Unavailable(_)));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
