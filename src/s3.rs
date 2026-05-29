use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BucketState {
    Missing,
    Empty,
    NonEmpty,
}

#[derive(Debug, Clone)]
pub struct BucketProbe {
    pub endpoint: String,
    pub access_key: String,
    pub secret_key: String,
}

impl BucketProbe {
    pub fn probe(&self) -> Result<BucketState> {
        match self.signed("HEAD", "", None).call() {
            Ok(_) => {}
            Err(ureq::Error::Status(404, _)) => return Ok(BucketState::Missing),
            Err(ureq::Error::Status(code, resp)) => {
                bail!(
                    "bucket head failed with HTTP {code}: {}",
                    resp.status_text()
                )
            }
            Err(e) => return Err(anyhow::anyhow!("bucket head failed: {e}")),
        }

        match self
            .signed(
                "GET",
                "list-type=2&max-keys=1",
                Some("list-type=2&max-keys=1"),
            )
            .call()
        {
            Ok(resp) => {
                let body = resp.into_string().context("reading bucket list response")?;
                if body.contains("<Key>") {
                    Ok(BucketState::NonEmpty)
                } else {
                    Ok(BucketState::Empty)
                }
            }
            Err(ureq::Error::Status(404, _)) => Ok(BucketState::Missing),
            Err(ureq::Error::Status(code, resp)) => {
                bail!(
                    "bucket list failed with HTTP {code}: {}",
                    resp.status_text()
                )
            }
            Err(e) => Err(anyhow::anyhow!("bucket list failed: {e}")),
        }
    }

    fn signed(
        &self,
        method: &str,
        canonical_query: &str,
        url_query: Option<&str>,
    ) -> ureq::Request {
        let parsed = parse_endpoint(&self.endpoint);
        let url = match url_query {
            Some(q) => format!("{}?{q}", self.endpoint),
            None => self.endpoint.clone(),
        };
        let (amz_date, short_date) = amz_dates();
        let payload_hash = hex::encode(Sha256::digest([]));
        // Fixed header set; order matters for the canonical request and must
        // match the SignedHeaders list in the Authorization header below.
        let headers = [
            ("host", parsed.host.as_str()),
            ("x-amz-content-sha256", payload_hash.as_str()),
            ("x-amz-date", amz_date.as_str()),
        ];
        let signature = compute_signature(
            method,
            &parsed.path,
            canonical_query,
            &headers,
            &payload_hash,
            &self.secret_key,
            "auto",
            "s3",
            &amz_date,
            &short_date,
        );
        let scope = format!("{short_date}/auto/s3/aws4_request");
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}",
            self.access_key
        );
        ureq::request(method, &url)
            .set("Host", &parsed.host)
            .set("X-Amz-Date", &amz_date)
            .set("X-Amz-Content-Sha256", &payload_hash)
            .set("Authorization", &auth)
    }
}

struct ParsedEndpoint {
    host: String,
    path: String,
}

fn parse_endpoint(endpoint: &str) -> ParsedEndpoint {
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint);
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h.to_string(), format!("/{}", p.trim_start_matches('/'))),
        None => (rest.to_string(), "/".to_string()),
    };
    ParsedEndpoint { host, path }
}

fn hmac_bytes(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn signing_key(secret: &str, short_date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_bytes(format!("AWS4{secret}").as_bytes(), short_date.as_bytes());
    let k_region = hmac_bytes(&k_date, region.as_bytes());
    let k_service = hmac_bytes(&k_region, service.as_bytes());
    hmac_bytes(&k_service, b"aws4_request")
}

/// Compute the SigV4 hex signature for one request. Pure — every input is
/// explicit (notably the timestamp via `amz_date`/`short_date`), so it's
/// reproducible against AWS's published example vectors. `headers` must already
/// be lowercased and sorted by name; the signed-headers list is derived from
/// them in order.
#[allow(clippy::too_many_arguments)]
fn compute_signature(
    method: &str,
    canonical_uri: &str,
    canonical_query: &str,
    headers: &[(&str, &str)],
    payload_hash: &str,
    secret_key: &str,
    region: &str,
    service: &str,
    amz_date: &str,
    short_date: &str,
) -> String {
    let mut canonical_headers = String::new();
    let mut signed_headers = String::new();
    for (i, (name, value)) in headers.iter().enumerate() {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value.trim());
        canonical_headers.push('\n');
        if i > 0 {
            signed_headers.push(';');
        }
        signed_headers.push_str(name);
    }
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let scope = format!("{short_date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let signing_key = signing_key(secret_key, short_date, region, service);
    hex::encode(hmac_bytes(&signing_key, string_to_sign.as_bytes()))
}

fn amz_dates() -> (String, String) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_amz_time(secs)
}

/// Format unix seconds (UTC) as `(amz_date=YYYYMMDDTHHMMSSZ, short_date=YYYYMMDD)`.
/// Split out from [`amz_dates`] so the formatting is unit-testable against a
/// known instant without mocking the clock.
fn format_amz_time(secs: u64) -> (String, String) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (year, month, day) = civil_from_days(days);
    (
        format!("{year:04}{month:02}{day:02}T{h:02}{m:02}{s:02}Z"),
        format!("{year:04}{month:02}{day:02}"),
    )
}

fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case2() {
        // RFC 4231 Test Case 2.
        let mac = hmac_bytes(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn sigv4_matches_aws_get_vanilla_vector() {
        // AWS sigv4 test-suite "get-vanilla": GET / with only host + x-amz-date
        // signed, empty payload, region us-east-1, service "service".
        let payload = hex::encode(Sha256::digest([]));
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let sig = compute_signature(
            "GET",
            "/",
            "",
            &headers,
            &payload,
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "service",
            "20150830T123600Z",
            "20150830",
        );
        assert_eq!(
            sig,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn amz_time_formats_known_instant() {
        // 2015-08-30T12:36:00Z = 1440938160 unix seconds.
        let (amz, date) = format_amz_time(1_440_938_160);
        assert_eq!(amz, "20150830T123600Z");
        assert_eq!(date, "20150830");
    }

    #[test]
    fn parse_endpoint_splits_host_and_path() {
        let p = parse_endpoint("https://acct.r2.cloudflarestorage.com/trove-notes");
        assert_eq!(p.host, "acct.r2.cloudflarestorage.com");
        assert_eq!(p.path, "/trove-notes");
        // No path component → root.
        let p = parse_endpoint("https://acct.r2.cloudflarestorage.com");
        assert_eq!(p.host, "acct.r2.cloudflarestorage.com");
        assert_eq!(p.path, "/");
    }
}
