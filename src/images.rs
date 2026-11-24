//! Fetching a message's remote images, and only ever because the user allowed it.
//!
//! This is the one place mail content makes the app contact a third party, so it is strict:
//! `https` only, no cookies, no referrer, a generic user agent, redirects followed by hand
//! (each hop must also be `https` and public), a time and a size limit, and an answer only
//! when the bytes really are a PNG, JPEG, GIF or WebP of sane dimensions. SVG is refused: it
//! is a document, not a picture. Hosts that resolve to loopback, private, link-local or other
//! non-public addresses are refused, so an image tag cannot probe the user's own network.
//!
//! Runs on a background thread (blocking ureq), never on the UI thread.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

use gpui::ImageFormat;
use url::Url;

/// Larger responses are refused.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// More pixels than this and the image is refused (a small file can decode to gigabytes).
pub const MAX_PIXELS: u64 = 25_000_000;
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HOPS: usize = 3;
/// A browser-shaped agent string, so the request does not single out this client.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

pub struct Fetched {
    pub format: ImageFormat,
    pub bytes: Vec<u8>,
}

/// Downloads and checks the image at `url`. The error is a short reason for the placeholder.
pub fn fetch(url: &str) -> Result<Fetched, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .user_agent(USER_AGENT)
        .build()
        .into();
    let mut url = Url::parse(url).map_err(|_| "bad address".to_string())?;
    for _ in 0..=MAX_HOPS {
        check_target(&url)?;
        let mut resp = agent
            .get(url.as_str())
            .header("Accept", "image/png,image/jpeg,image/gif,image/webp")
            .call()
            .map_err(|e| describe(&e))?;
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            let next = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url.join(l).ok())
                .ok_or("bad redirect")?;
            url = next;
            continue;
        }
        if status != 200 {
            return Err(format!("server answered {status}"));
        }
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(MAX_BYTES)
            .read_to_vec()
            .map_err(|e| match e {
                ureq::Error::BodyExceedsLimit(_) => "too large".to_string(),
                e => describe(&e),
            })?;
        return check_image(bytes);
    }
    Err("too many redirects".into())
}

fn describe(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "timed out".into(),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::Io(io) if io.kind() == ErrorKind::TimedOut => "timed out".into(),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => "certificate problem".into(),
        _ => "could not connect".into(),
    }
}

/// Refuses anything but an `https` URL on a public host.
fn check_target(url: &Url) -> Result<(), String> {
    if url.scheme() != "https" {
        return Err("not https".into());
    }
    let host = url.host_str().ok_or("no host")?;
    if is_local_name(host) {
        return Err("local address".into());
    }
    let addrs: Vec<IpAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => (host, url.port_or_known_default().unwrap_or(443))
            .to_socket_addrs()
            .map_err(|_| "host not found".to_string())?
            .map(|a| a.ip())
            .collect(),
    };
    if addrs.is_empty() || addrs.iter().any(|ip| !is_public(*ip)) {
        return Err("local address".into());
    }
    Ok(())
}

fn is_local_name(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost"
        || [
            ".localhost",
            ".local",
            ".internal",
            ".lan",
            ".home",
            ".corp",
        ]
        .iter()
        .any(|s| h.ends_with(s))
}

/// Whether `ip` is a globally routable unicast address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => public_v4(v4),
            None => public_v6(v6),
        },
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_documentation()
        // 100.64.0.0/10 carrier-grade NAT, 192.0.0.0/24 protocol assignments, 198.18.0.0/15
        // benchmarking, 240.0.0.0/4 reserved.
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || o[0] >= 240)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // fc00::/7 unique local, fe80::/10 link local, 2001:db8::/32 documentation.
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] == 0x2001 && s[1] == 0x0db8))
}

/// Accepts the bytes only when they are a supported image of sane dimensions.
fn check_image(bytes: Vec<u8>) -> Result<Fetched, String> {
    let (format, (w, h)) = sniff(&bytes).ok_or("not a supported image")?;
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err("image too large".into());
    }
    Ok(Fetched { format, bytes })
}

/// The format and pixel size of a PNG, JPEG, GIF or WebP, read from its header. `None` for
/// anything else, including a header too short or broken to give a size.
pub fn sniff(b: &[u8]) -> Option<(ImageFormat, (u32, u32))> {
    let be16 = |i: usize| {
        b.get(i..i + 2)
            .map(|s| u32::from(u16::from_be_bytes([s[0], s[1]])))
    };
    let le16 = |i: usize| {
        b.get(i..i + 2)
            .map(|s| u32::from(u16::from_le_bytes([s[0], s[1]])))
    };
    let be32 = |i: usize| {
        b.get(i..i + 4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((ImageFormat::Png, (be32(16)?, be32(20)?)));
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some((ImageFormat::Gif, (le16(6)?, le16(8)?)));
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let mut i = 2;
        while i + 4 <= b.len() {
            if b[i] != 0xff {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            if marker == 0xff {
                i += 1;
                continue;
            }
            // Start-of-frame markers carry the size; C4, C8 and CC are not frames.
            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                return Some((ImageFormat::Jpeg, (be16(i + 7)?, be16(i + 5)?)));
            }
            if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                i += 2;
                continue;
            }
            i += 2 + be16(i + 2)? as usize;
        }
        return None;
    }
    if b.len() >= 30 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return match &b[12..16] {
            b"VP8X" => {
                let w = 1 + u32::from_le_bytes([b[24], b[25], b[26], 0]);
                let h = 1 + u32::from_le_bytes([b[27], b[28], b[29], 0]);
                Some((ImageFormat::Webp, (w, h)))
            }
            b"VP8L" if b[20] == 0x2f => {
                let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
                Some((
                    ImageFormat::Webp,
                    (1 + (bits & 0x3fff), 1 + ((bits >> 14) & 0x3fff)),
                ))
            }
            b"VP8 " if b[23..26] == [0x9d, 0x01, 0x2a] => {
                Some((ImageFormat::Webp, (le16(26)? & 0x3fff, le16(28)? & 0x3fff)))
            }
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b
    }

    #[test]
    fn sizes_are_read_from_headers() {
        assert_eq!(sniff(&png(640, 480)), Some((ImageFormat::Png, (640, 480))));
        assert_eq!(
            sniff(b"GIF89a\x10\x00\x20\x00rest"),
            Some((ImageFormat::Gif, (16, 32)))
        );
        // JPEG: SOI, an APP0 segment to skip, then SOF0 (height 300, width 200).
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x01,
            0x2c, 0x00, 0xc8, 0x01, 0x01, 0x11, 0x00,
        ];
        assert_eq!(sniff(&jpeg), Some((ImageFormat::Jpeg, (200, 300))));
        // WebP VP8X: 24-bit little-endian width-1 and height-1.
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        webp.extend([0x1f, 0x01, 0x00, 0x0f, 0x01, 0x00]);
        webp.resize(32, 0);
        assert_eq!(sniff(&webp), Some((ImageFormat::Webp, (288, 272))));
    }

    #[test]
    fn anything_else_is_refused() {
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'/>"), None);
        assert_eq!(sniff(b"<html>"), None);
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n"), None);
        assert!(check_image(png(0, 10)).is_err());
        // A tiny file that claims to be 100000 x 100000 pixels.
        assert!(check_image(png(100_000, 100_000)).is_err());
        assert!(check_image(png(800, 600)).is_ok());
    }

    #[test]
    fn non_public_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "172.16.5.5",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(ip.parse().expect("ip")), "{ip}");
        }
        for ip in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public(ip.parse().expect("ip")), "{ip}");
        }
    }

    #[test]
    fn targets_must_be_https_and_not_local() {
        let bad = |u: &str| check_target(&Url::parse(u).expect("url")).is_err();
        assert!(bad("http://example.com/a.png"));
        assert!(bad("https://localhost/a.png"));
        assert!(bad("https://printer.local/a.png"));
        assert!(bad("https://127.0.0.1/a.png"));
        assert!(bad("https://[::1]/a.png"));
        assert!(bad("https://192.168.1.1/a.png"));
        assert!(bad("https://169.254.169.254/latest"));
    }
}
