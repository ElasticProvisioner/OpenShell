// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Check the syntax and scheme of an operator-set `grpc_endpoint`: an
/// `http://` or `https://` URL with a host. This needs no gateway context, so
/// `preflight` can run it.
pub fn validate_grpc_endpoint(endpoint: &str) -> Result<(), String> {
    let uri: http::Uri = endpoint
        .parse()
        .map_err(|error| format!("grpc_endpoint {endpoint:?} is not a valid URL: {error}"))?;
    match uri.scheme_str() {
        Some("http" | "https") => {}
        _ => {
            return Err(format!(
                "grpc_endpoint {endpoint:?} must use the http:// or https:// scheme"
            ));
        }
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err(format!("grpc_endpoint {endpoint:?} has no host"));
    }
    Ok(())
}

/// Whether sandboxes dial `endpoint` over TLS, which needs the gateway CA.
#[must_use]
pub fn endpoint_uses_tls(endpoint: &str) -> bool {
    endpoint
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_https_urls_with_a_host_are_accepted() {
        for endpoint in [
            "http://127.0.0.1:8080",
            "https://gateway.internal:8443",
            "https://[::1]:8443",
        ] {
            validate_grpc_endpoint(endpoint).unwrap();
        }
    }

    #[test]
    fn other_schemes_and_malformed_urls_are_rejected() {
        for endpoint in ["ftp://host:1", "host:8080", "http://", "not a url", "/path"] {
            assert!(validate_grpc_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }

    #[test]
    fn only_https_needs_the_gateway_ca() {
        assert!(endpoint_uses_tls("https://gateway:1"));
        assert!(endpoint_uses_tls("HTTPS://gateway:1"));
        assert!(!endpoint_uses_tls("http://gateway:1"));
    }
}
