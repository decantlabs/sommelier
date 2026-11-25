//! The address a key member connected to, as the text that goes into a signed sign in.
//!
//! A hostile hub at an address the member never used could pass on the nonce and hub id of the real
//! hub and use the signature there. So the sign in covers the address the client reached the hub at,
//! and a hub takes it only for an address it lists as its own (`door.public_addresses`). Loopback
//! addresses are always accepted: a connection to the own machine cannot be a relay of someone else.

use std::net::IpAddr;

/// Longest address text a sign in may carry.
pub const MAX_ADDRESS: usize = 255;

/// `host:port` of a hub URL in lower case: the port of the URL, else 443 for `wss` and `https`, else
/// 80. An IPv6 host keeps its brackets.
pub fn address_of_url(raw: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| format!("invalid hub URL `{raw}`"))?;
    let default = match url.scheme() {
        "ws" | "http" => 80,
        "wss" | "https" => 443,
        _ => return Err(format!("invalid hub URL scheme in `{raw}`")),
    };
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("the hub URL `{raw}` has no host"))?;
    let address = format!(
        "{}:{}",
        host.trim_end_matches('.').to_ascii_lowercase(),
        url.port().unwrap_or(default)
    );
    if address.len() > MAX_ADDRESS {
        return Err("the hub address is too long".into());
    }
    Ok(address)
}

/// Splits `host:port`. The host of an IPv6 address keeps its brackets. `None` for a bare host.
fn split_port(text: &str) -> Option<(&str, u16)> {
    let (host, port) = text.rsplit_once(':')?;
    if host.is_empty() || (host.contains(':') && !host.ends_with(']')) {
        return None;
    }
    Some((host, port.parse().ok()?))
}

/// True when the host of `address` (`host:port`) is the own machine.
pub fn is_loopback(address: &str) -> bool {
    let host = split_port(address).map_or(address, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// Whether a hub that lists `public` as its addresses takes a sign in that names `address`. An entry is
/// `host:port` or a bare `host`, which stands for the default ports 443 and 80. Loopback is always taken.
pub fn address_allowed(address: &str, public: &[String]) -> bool {
    let address = address.to_ascii_lowercase();
    if is_loopback(&address) {
        return true;
    }
    let Some((host, port)) = split_port(&address) else {
        return false;
    };
    public.iter().any(|entry| {
        let entry = entry.trim().to_ascii_lowercase();
        let entry = entry.trim_end_matches('.');
        match split_port(entry) {
            Some((h, p)) => h == host && p == port,
            None => entry == host && (port == 443 || port == 80),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_of_a_url() {
        assert_eq!(
            address_of_url("wss://Hub.Example.org").unwrap(),
            "hub.example.org:443"
        );
        assert_eq!(
            address_of_url("ws://hub.example.org:7300/x").unwrap(),
            "hub.example.org:7300"
        );
        assert_eq!(address_of_url("http://127.0.0.1").unwrap(), "127.0.0.1:80");
        assert_eq!(address_of_url("ws://[::1]:9").unwrap(), "[::1]:9");
        assert!(address_of_url("ftp://x").is_err());
        assert!(address_of_url("nonsense").is_err());
    }

    #[test]
    fn loopback_is_always_taken_and_others_only_when_listed() {
        let list = vec!["hub.example.org".to_string(), "other.test:7300".to_string()];
        for ok in [
            "127.0.0.1:7300",
            "localhost:1",
            "[::1]:80",
            "hub.example.org:443",
            "hub.example.org:80",
            "HUB.example.org:443",
            "other.test:7300",
        ] {
            assert!(address_allowed(ok, &list), "{ok}");
        }
        for bad in [
            "evil.example:443",
            "hub.example.org:7300",
            "other.test:443",
            "hub.example.org.evil.test:443",
            "10.0.0.1:7300",
            "hub.example.org",
        ] {
            assert!(!address_allowed(bad, &list), "{bad}");
        }
        assert!(!address_allowed("hub.example.org:443", &[]));
        assert!(address_allowed("127.0.0.1:1", &[]));
    }
}
