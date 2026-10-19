//! Value types and parsers behind the Kubernetes CEL libraries, written to
//! accept and reject exactly what the Go implementations do: resource
//! quantities (`resource.ParseQuantity`), URLs (`url.ParseRequestURI` /
//! `url.Parse`), IP addresses and prefixes (`netip`), and the named
//! formats of `format.*` (apimachinery's name validators). The OpenAPI
//! `format` checks of CRD schemas reuse these.

use std::cmp::Ordering;
use std::net::IpAddr;

// ---------------------------------------------------------------------------
// Quantities

/// A resource quantity as an exact decimal: `mantissa * 10^exp`.
#[derive(Debug, Clone)]
pub struct Quantity {
    pub mantissa: i128,
    pub exp: i32,
    pub raw: String,
}

impl Quantity {
    pub fn parse(s: &str) -> Option<Quantity> {
        let b = s.as_bytes();
        if b.is_empty() {
            return None;
        }
        let mut i = 0;
        let negative = match b[0] {
            b'-' => {
                i += 1;
                true
            }
            b'+' => {
                i += 1;
                false
            }
            _ => false,
        };
        let int_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let int_part = &s[int_start..i];
        let mut frac_part = "";
        if i < b.len() && b[i] == b'.' {
            i += 1;
            let fs = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            frac_part = &s[fs..i];
        }
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        let suffix = &s[i..];
        let (base, exp10): (i128, i32) = match suffix {
            "" => (1, 0),
            "n" => (1, -9),
            "u" => (1, -6),
            "m" => (1, -3),
            "k" => (1, 3),
            "M" => (1, 6),
            "G" => (1, 9),
            "T" => (1, 12),
            "P" => (1, 15),
            "E" => (1, 18),
            "Ki" => (1 << 10, 0),
            "Mi" => (1 << 20, 0),
            "Gi" => (1 << 30, 0),
            "Ti" => (1 << 40, 0),
            "Pi" => (1 << 50, 0),
            "Ei" => (1 << 60, 0),
            e if e.starts_with('e') || e.starts_with('E') => {
                let digits = &e[1..];
                let unsigned = digits.strip_prefix(['+', '-']).unwrap_or(digits);
                if unsigned.is_empty() || !unsigned.bytes().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                (1, digits.parse().ok()?)
            }
            _ => return None,
        };
        // Keep at most 30 significant digits; quantities are bounded anyway.
        let digits = format!("{int_part}{frac_part}");
        let digits = digits.trim_start_matches('0');
        if digits.len() > 30 {
            return None;
        }
        let mut mantissa: i128 = if digits.is_empty() { 0 } else { digits.parse().ok()? };
        mantissa = mantissa.checked_mul(base)?;
        if negative {
            mantissa = -mantissa;
        }
        let exp = exp10.checked_sub(frac_part.len() as i32)?;
        if !(-40..=40).contains(&exp) {
            return None;
        }
        Some(Quantity { mantissa, exp, raw: s.to_string() })
    }

    fn scaled(&self, exp: i32) -> Option<i128> {
        if exp > self.exp {
            return None;
        }
        self.mantissa.checked_mul(10i128.checked_pow((self.exp - exp) as u32)?)
    }

    pub fn cmp(&self, other: &Quantity) -> Ordering {
        let e = self.exp.min(other.exp);
        match (self.scaled(e), other.scaled(e)) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => self.as_f64().partial_cmp(&other.as_f64()).unwrap_or(Ordering::Equal),
        }
    }

    pub fn as_f64(&self) -> f64 {
        self.mantissa as f64 * 10f64.powi(self.exp)
    }

    pub fn is_integer(&self) -> bool {
        self.as_integer().is_some()
    }

    /// The value as an int64, if it is a whole number in range.
    pub fn as_integer(&self) -> Option<i64> {
        let v = if self.exp >= 0 {
            self.scaled(0)?
        } else {
            let div = 10i128.checked_pow((-self.exp) as u32)?;
            if self.mantissa % div != 0 {
                return None;
            }
            self.mantissa / div
        };
        i64::try_from(v).ok()
    }

    pub fn add(&self, other: &Quantity, sign: i128) -> Option<Quantity> {
        let e = self.exp.min(other.exp);
        let m = self.scaled(e)?.checked_add(other.scaled(e)?.checked_mul(sign)?)?;
        Some(Quantity { mantissa: m, exp: e, raw: canonical(m, e) })
    }
}

fn canonical(m: i128, e: i32) -> String {
    let mut m = m;
    let mut e = e;
    while m != 0 && m % 10 == 0 && e < 0 {
        m /= 10;
        e += 1;
    }
    match e {
        0 => m.to_string(),
        -3 => format!("{m}m"),
        -6 => format!("{m}u"),
        -9 => format!("{m}n"),
        e if e > 0 => format!("{m}e{e}"),
        e => format!("{m}e{e}"),
    }
}

// ---------------------------------------------------------------------------
// URLs (Go's net/url)

#[derive(Debug, Clone, PartialEq)]
pub struct Url {
    pub raw: String,
    pub scheme: String,
    pub opaque: String,
    pub host: String,
    pub path: String,
    pub raw_query: String,
}

fn hex_val(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// `%XX` escapes must be valid; returns the unescaped string.
fn unescape(s: &str, host: bool) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let (Some(h), Some(l)) = (b.get(i + 1).copied().and_then(hex_val), b.get(i + 2).copied().and_then(hex_val)) else {
                    let end = (i + 3).min(b.len());
                    return Err(format!("invalid URL escape {:?}", &s[i..end]));
                };
                out.push(h * 16 + l);
                i += 3;
            }
            c if host && !(c.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:[]<>\"".contains(&c) || c >= 0x80) => {
                return Err(format!("invalid character {:?} in host name", c as char));
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn valid_optional_port(port: &str) -> bool {
    port.is_empty() || (port.starts_with(':') && port[1..].bytes().all(|c| c.is_ascii_digit()))
}

impl Url {
    /// `url.ParseRequestURI`: absolute URLs or absolute paths; the fragment
    /// is not split off.
    pub fn parse_request_uri(raw: &str) -> Result<Url, String> {
        Url::parse_go(raw, true)
    }

    fn parse_go(raw: &str, via_request: bool) -> Result<Url, String> {
        if raw.bytes().any(|c| c < 0x20 || c == 0x7f) {
            return Err("net/url: invalid control character in URL".into());
        }
        if raw.is_empty() && via_request {
            return Err("empty url".into());
        }
        let mut u = Url { raw: raw.to_string(), scheme: String::new(), opaque: String::new(), host: String::new(), path: String::new(), raw_query: String::new() };
        if raw == "*" {
            u.path = "*".into();
            return Ok(u);
        }
        // Scheme.
        let mut rest = raw;
        for (i, c) in raw.bytes().enumerate() {
            match c {
                b'a'..=b'z' | b'A'..=b'Z' => {}
                b'0'..=b'9' | b'+' | b'-' | b'.' if i > 0 => {}
                b':' => {
                    if i == 0 {
                        return Err("missing protocol scheme".into());
                    }
                    u.scheme = raw[..i].to_ascii_lowercase();
                    rest = &raw[i + 1..];
                    break;
                }
                _ => break,
            }
        }
        let rest_owned;
        if rest.ends_with('?') && rest.matches('?').count() == 1 {
            rest_owned = rest[..rest.len() - 1].to_string();
        } else if let Some((r, q)) = rest.split_once('?') {
            u.raw_query = q.to_string();
            rest_owned = r.to_string();
        } else {
            rest_owned = rest.to_string();
        }
        let mut rest = rest_owned.as_str();
        if !rest.starts_with('/') {
            if !u.scheme.is_empty() {
                u.opaque = rest.to_string();
                return Ok(u);
            }
            if via_request {
                return Err("invalid URI for request".into());
            }
            if let Some(colon) = rest.find(':') {
                if rest.find('/').is_none_or(|slash| colon < slash) {
                    return Err("first path segment in URL cannot contain colon".into());
                }
            }
        }
        if (!u.scheme.is_empty() || !via_request && !rest.starts_with("///")) && rest.starts_with("//") {
            let authority = &rest[2..];
            let (auth, path) = match authority.find('/') {
                Some(i) => (&authority[..i], &authority[i..]),
                None => (authority, ""),
            };
            let host = match auth.rfind('@') {
                Some(i) => {
                    let user = &auth[..i];
                    if !user.bytes().all(|c| c.is_ascii_alphanumeric() || b"-._:~!$&'()*+,;=%@".contains(&c)) {
                        return Err("net/url: invalid userinfo".into());
                    }
                    &auth[i + 1..]
                }
                None => auth,
            };
            u.host = parse_host(host)?;
            rest = path;
        }
        u.path = unescape(rest, false)?;
        Ok(u)
    }

    pub fn hostname(&self) -> String {
        let (host, _) = split_host_port(&self.host);
        host.trim_start_matches('[').trim_end_matches(']').to_string()
    }

    pub fn port(&self) -> String {
        split_host_port(&self.host).1.to_string()
    }

    /// `URL.EscapedPath`: the path re-escaped.
    pub fn escaped_path(&self) -> String {
        let mut out = String::new();
        for c in self.path.bytes() {
            if c.is_ascii_alphanumeric() || b"-._~$&+,/:;=@!'()*".contains(&c) {
                out.push(c as char);
            } else {
                out.push_str(&format!("%{c:02X}"));
            }
        }
        out
    }

    /// `URL.Query`: the query parameters (malformed pairs dropped).
    pub fn query(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        let mut out: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
        for pair in self.raw_query.split('&').filter(|p| !p.is_empty()) {
            if pair.contains(';') {
                continue;
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let dec = |s: &str| unescape(&s.replace('+', " "), false);
            if let (Ok(k), Ok(v)) = (dec(k), dec(v)) {
                out.entry(k).or_default().push(v);
            }
        }
        out
    }
}

fn split_host_port(host: &str) -> (&str, &str) {
    if let Some(close) = host.rfind(']') {
        let port = host[close + 1..].strip_prefix(':').unwrap_or("");
        return (&host[..close + 1], port);
    }
    match host.rfind(':') {
        Some(i) => (&host[..i], &host[i + 1..]),
        None => (host, ""),
    }
}

fn parse_host(host: &str) -> Result<String, String> {
    if host.starts_with('[') {
        let Some(close) = host.rfind(']') else { return Err("missing ']' in host".into()) };
        let port = &host[close + 1..];
        if !valid_optional_port(port) {
            return Err(format!("invalid port {port:?} after host"));
        }
        let inner = &host[1..close];
        let zone = inner.find("%25");
        let addr = &inner[..zone.unwrap_or(inner.len())];
        if addr.parse::<std::net::Ipv6Addr>().is_err() && zone.is_none() {
            // Go validates the address part only loosely; keep it permissive
            // but reject obviously broken literals.
            if addr.is_empty() {
                return Err("invalid IPv6 host".into());
            }
        }
        return Ok(host.to_string());
    }
    if let Some(i) = host.rfind(':') {
        let port = &host[i..];
        if !valid_optional_port(port) {
            return Err(format!("invalid port {port:?} after host"));
        }
    }
    unescape(host, true)
}

// ---------------------------------------------------------------------------
// IPs and CIDRs (netip, as the Kubernetes ip()/cidr() libraries use it)

pub fn parse_ip(s: &str) -> Result<IpAddr, String> {
    if s.contains('%') {
        return Err(format!("IP address with zone value is not allowed: {s}"));
    }
    let ip: IpAddr = s.parse().map_err(|_| format!("IP Address {s:?} parse error during conversion from string: ParseAddr({s:?}): unable to parse IP"))?;
    if let IpAddr::V6(v6) = ip {
        if v6.to_ipv4_mapped().is_some() && s.contains('.') {
            return Err(format!("IPv4-mapped IPv6 address {s:?} is not allowed"));
        }
    }
    Ok(ip)
}

pub fn parse_cidr(s: &str) -> Result<(IpAddr, u8), String> {
    let (addr, bits) = s.split_once('/').ok_or_else(|| format!("network address parse error during conversion from string: netip.ParsePrefix({s:?}): no '/'"))?;
    let ip = parse_ip(addr).map_err(|_| format!("network address parse error during conversion from string: netip.ParsePrefix({s:?}): ParseAddr({addr:?}): unable to parse IP"))?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let ok_digits = !bits.is_empty() && bits.bytes().all(|c| c.is_ascii_digit()) && !(bits.len() > 1 && bits.starts_with('0'));
    let n: u32 = bits.parse().ok().filter(|n| ok_digits && *n <= max).ok_or_else(|| {
        format!("network address parse error during conversion from string: netip.ParsePrefix({s:?}): bad bits after slash: {bits:?}")
    })?;
    Ok((ip, n as u8))
}

pub fn mask(ip: IpAddr, bits: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let m = if bits == 0 { 0 } else { u32::MAX << (32 - bits as u32) };
            IpAddr::V4((u32::from(v4) & m).into())
        }
        IpAddr::V6(v6) => {
            let m = if bits == 0 { 0 } else { u128::MAX << (128 - bits as u32) };
            IpAddr::V6((u128::from(v6) & m).into())
        }
    }
}

pub fn cidr_contains(net: IpAddr, bits: u8, ip: IpAddr) -> bool {
    net.is_ipv4() == ip.is_ipv4() && mask(net, bits) == mask(ip, bits)
}

/// net.ParseCIDR, as the OpenAPI `cidr` format uses it.
pub fn is_go_cidr(s: &str) -> bool {
    let Some((addr, bits)) = s.split_once('/') else { return false };
    let Ok(ip) = addr.parse::<IpAddr>() else { return false };
    let max = if ip.is_ipv4() { 32 } else { 128 };
    !bits.is_empty() && bits.len() <= 3 && bits.bytes().all(|c| c.is_ascii_digit()) && bits.parse::<u32>().is_ok_and(|n| n <= max)
}

pub fn is_global_unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_unspecified() || v4.is_loopback() || v4.is_multicast() || v4.is_link_local() || v4 == std::net::Ipv4Addr::BROADCAST)
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            !(v6.is_unspecified() || v6.is_loopback() || v6.is_multicast() || (seg[0] & 0xffc0) == 0xfe80)
        }
    }
}

pub fn is_link_local_unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

pub fn is_link_local_multicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 224 && v4.octets()[1] == 0 && v4.octets()[2] == 0,
        IpAddr::V6(v6) => (v6.segments()[0] & 0xff0f) == 0xff02,
    }
}

// ---------------------------------------------------------------------------
// apimachinery name validators (format.* and OpenAPI formats)

const DNS1123_LABEL_FMT: &str = "[a-z0-9]([-a-z0-9]*[a-z0-9])?";
const DNS1123_SUBDOMAIN_FMT: &str = "[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*";
const DNS1035_LABEL_FMT: &str = "[a-z]([-a-z0-9]*[a-z0-9])?";

fn is_label_char(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

fn dns1123_label_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && is_label_char(b[0]) && is_label_char(b[b.len() - 1]) && b.iter().all(|&c| is_label_char(c) || c == b'-')
}

pub fn is_dns1123_label(s: &str) -> Vec<String> {
    let mut errs = Vec::new();
    if s.len() > 63 {
        errs.push("must be no more than 63 characters".to_string());
    }
    if !dns1123_label_ok(s) {
        errs.push(format!("a lowercase RFC 1123 label must consist of lower case alphanumeric characters or '-', and must start and end with an alphanumeric character (e.g. 'my-name',  or '123-abc', regex used for validation is '{DNS1123_LABEL_FMT}')"));
    }
    errs
}

pub fn is_dns1123_subdomain(s: &str) -> Vec<String> {
    let mut errs = Vec::new();
    if s.len() > 253 {
        errs.push("must be no more than 253 characters".to_string());
    }
    if !s.split('.').all(dns1123_label_ok) {
        errs.push(format!("a lowercase RFC 1123 subdomain must consist of lower case alphanumeric characters, '-' or '.', and must start and end with an alphanumeric character (e.g. 'example.com', regex used for validation is '{DNS1123_SUBDOMAIN_FMT}')"));
    }
    errs
}

pub fn is_dns1035_label(s: &str) -> Vec<String> {
    let mut errs = Vec::new();
    if s.len() > 63 {
        errs.push("must be no more than 63 characters".to_string());
    }
    let b = s.as_bytes();
    let ok = !b.is_empty() && b[0].is_ascii_lowercase() && is_label_char(b[b.len() - 1]) && b.iter().all(|&c| is_label_char(c) || c == b'-');
    if !ok {
        errs.push(format!("a DNS-1035 label must consist of lower case alphanumeric characters or '-', start with an alphabetic character, and end with an alphanumeric character (e.g. 'my-name',  or 'abc-123', regex used for validation is '{DNS1035_LABEL_FMT}')"));
    }
    errs
}

/// `NameIs...(name, prefix=true)`: a trailing dash is allowed.
fn mask_trailing_dash(s: &str) -> String {
    if s.len() > 1 && s.ends_with('-') { format!("{}a", &s[..s.len() - 1]) } else { s.to_string() }
}

fn qualified_part_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.')
}

pub fn is_qualified_name(s: &str) -> Vec<String> {
    let mut errs = Vec::new();
    let parts: Vec<&str> = s.split('/').collect();
    let name = match parts.as_slice() {
        [name] => *name,
        [prefix, name] => {
            if prefix.is_empty() {
                errs.push("prefix part must be non-empty".to_string());
            } else {
                errs.extend(is_dns1123_subdomain(prefix).into_iter().map(|e| format!("prefix part {e}")));
            }
            name
        }
        _ => {
            return vec!["a qualified name must consist of alphanumeric characters, '-', '_' or '.', and must start and end with an alphanumeric character (e.g. 'MyName',  or 'my.name',  or '123-abc', regex used for validation is '([A-Za-z0-9][-A-Za-z0-9_.]*)?[A-Za-z0-9]') with an optional DNS subdomain prefix and '/' (e.g. 'example.com/MyName')".to_string()];
        }
    };
    if name.is_empty() {
        errs.push("name part must be non-empty".to_string());
    } else if name.len() > 63 {
        errs.push("name part must be no more than 63 characters".to_string());
    }
    if !name.is_empty() && !qualified_part_ok(name) {
        errs.push("name part must consist of alphanumeric characters, '-', '_' or '.', and must start and end with an alphanumeric character (e.g. 'MyName',  or 'my.name',  or '123-abc', regex used for validation is '([A-Za-z0-9][-A-Za-z0-9_.]*)?[A-Za-z0-9]')".to_string());
    }
    errs
}

pub fn is_label_value(s: &str) -> Vec<String> {
    let mut errs = Vec::new();
    if s.len() > 63 {
        errs.push("must be no more than 63 characters".to_string());
    }
    if !s.is_empty() && !qualified_part_ok(s) {
        errs.push("a valid label must be an empty string or consist of alphanumeric characters, '-', '_' or '.', and must start and end with an alphanumeric character (e.g. 'MyValue',  or 'my_value',  or '12345', regex used for validation is '(([A-Za-z0-9][-A-Za-z0-9_.]*)?[A-Za-z0-9])?')".to_string());
    }
    errs
}

/// The `format.<name>()` validators: errors for `s`, or none.
pub fn named_format(name: &str, s: &str) -> Option<Vec<String>> {
    Some(match name {
        "dns1123Label" => is_dns1123_label(s),
        "dns1123Subdomain" => is_dns1123_subdomain(s),
        "dns1035Label" => is_dns1035_label(s),
        "qualifiedName" => is_qualified_name(s),
        "dns1123LabelPrefix" => is_dns1123_label(&mask_trailing_dash(s)),
        "dns1123SubdomainPrefix" => is_dns1123_subdomain(&mask_trailing_dash(s)),
        "dns1035LabelPrefix" => is_dns1035_label(&mask_trailing_dash(s)),
        "labelValue" => is_label_value(s),
        "uri" => if Url::parse_request_uri(s).is_ok() { vec![] } else { vec!["invalid URI".into()] },
        "uuid" => if is_uuid(s) { vec![] } else { vec!["does not match the UUID format".into()] },
        "byte" => if is_base64(s) { vec![] } else { vec!["invalid base64".into()] },
        "date" => if is_date(s) { vec![] } else { vec!["invalid date".into()] },
        "datetime" => if parse_datetime(s).is_some() { vec![] } else { vec!["invalid datetime".into()] },
        _ => return None,
    })
}

/// Rough regex sizes the cost estimator charges for `format.*().validate`.
pub fn named_format_regex_size(_name: &str) -> u64 {
    128
}

// ---------------------------------------------------------------------------
// OpenAPI formats (kube-openapi strfmt)

pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| if matches!(i, 8 | 13 | 18 | 23) { c == b'-' } else { c.is_ascii_hexdigit() })
}

fn is_uuid_version(s: &str, v: u8) -> bool {
    is_uuid(s) && s.as_bytes()[14] == b'0' + v && (v == 3 || matches!(s.as_bytes()[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'))
}

pub fn is_base64(s: &str) -> bool {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).is_ok()
}

pub fn is_date(s: &str) -> bool {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() && s.len() == 10
}

/// strfmt.ParseDateTime: RFC 3339 and a few variants.
pub fn parse_datetime(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{DateTime, NaiveDateTime, Utc};
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    for layout in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M%:z", "%Y-%m-%dT%H:%M%z"] {
        if let Ok(t) = DateTime::parse_from_str(s, layout) {
            return Some(t.with_timezone(&Utc));
        }
    }
    for layout in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.fZ", "%Y-%m-%dT%H:%MZ", "%Y-%m-%dT%H:%M"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, layout) {
            return Some(t.and_utc());
        }
    }
    None
}

/// strfmt.ParseDuration: Go durations plus day/week units and spaced
/// forms ("3 days").
pub fn is_openapi_duration(s: &str) -> bool {
    if super::eval::parse_go_duration(s).is_ok() {
        return true;
    }
    let units = [
        "ns", "us", "µs", "ms", "s", "m", "h", "d", "w", "nanosecond", "nanoseconds", "microsecond", "microseconds", "millisecond",
        "milliseconds", "second", "seconds", "sec", "secs", "minute", "minutes", "min", "mins", "hour", "hours", "hr", "hrs", "day",
        "days", "week", "weeks", "wk", "wks",
    ];
    let mut rest = s.trim();
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if digits == 0 {
            return false;
        }
        rest = rest[digits..].trim_start();
        let unit_end = rest.find(|c: char| c.is_ascii_digit() || c.is_whitespace()).unwrap_or(rest.len());
        if !units.contains(&&rest[..unit_end]) {
            return false;
        }
        rest = rest[unit_end..].trim_start();
    }
    true
}

pub fn is_email(s: &str) -> bool {
    // net/mail.ParseAddress, for the common forms: `local@domain` or
    // `Name <local@domain>`.
    let addr = match (s.rfind('<'), s.ends_with('>')) {
        (Some(i), true) => &s[i + 1..s.len() - 1],
        _ => s.trim(),
    };
    let Some((local, domain)) = addr.rsplit_once('@') else { return false };
    let atext = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~".contains(c) || !c.is_ascii();
    let local_ok = (local.starts_with('"') && local.ends_with('"') && local.len() >= 2)
        || (!local.is_empty() && local.split('.').all(|p| !p.is_empty() && p.chars().all(atext)));
    let domain_ok = (domain.starts_with('[') && domain.ends_with(']'))
        || (!domain.is_empty() && domain.split('.').all(|p| !p.is_empty() && p.chars().all(atext)));
    local_ok && domain_ok
}

pub fn is_hostname(s: &str) -> bool {
    // strfmt's rxHostname: RFC 1123 labels, up to 255 chars, an optional
    // trailing dot.
    if s.is_empty() || s.len() > 255 {
        return false;
    }
    let s = s.strip_suffix('.').unwrap_or(s);
    s.split('.').all(|l| {
        let b = l.as_bytes();
        !b.is_empty()
            && b.len() <= 63
            && b[0].is_ascii_alphanumeric()
            && b[b.len() - 1].is_ascii_alphanumeric()
            && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-')
    })
}

fn is_mac(s: &str) -> bool {
    let groups: Vec<&str> = s.split([':', '-']).collect();
    if groups.len() >= 6 && groups.len() <= 20 && groups.iter().all(|g| g.len() == 2 && g.bytes().all(|c| c.is_ascii_hexdigit())) {
        return matches!(groups.len(), 6 | 8 | 20);
    }
    let dots: Vec<&str> = s.split('.').collect();
    matches!(dots.len(), 3 | 4 | 10) && dots.iter().all(|g| g.len() == 4 && g.bytes().all(|c| c.is_ascii_hexdigit()))
}

/// Whether `s` is valid for an OpenAPI string `format`; `None` for formats
/// the API server does not check.
pub fn check_format(format: &str, s: &str) -> Option<bool> {
    Some(match format {
        "bsonobjectid" => s.len() == 24 && s.bytes().all(|c| c.is_ascii_hexdigit()),
        "uri" => Url::parse_request_uri(s).is_ok(),
        "email" => is_email(s),
        "hostname" => is_hostname(s),
        "ipv4" => s.parse::<std::net::Ipv4Addr>().is_ok(),
        "ipv6" => s.contains(':') && s.parse::<std::net::Ipv6Addr>().is_ok(),
        "cidr" => is_go_cidr(s),
        "mac" => is_mac(s),
        "uuid" => is_uuid(s),
        "uuid3" => is_uuid_version(s, 3),
        "uuid4" => is_uuid_version(s, 4),
        "uuid5" => is_uuid_version(s, 5),
        "isbn" | "isbn10" | "isbn13" | "creditcard" | "ssn" => return None,
        "hexcolor" => s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|c| c.is_ascii_hexdigit()) || s.len() == 4 && s.starts_with('#') && s[1..].bytes().all(|c| c.is_ascii_hexdigit()),
        "rgbcolor" => s.starts_with("rgb(") && s.ends_with(')') && s[4..s.len() - 1].split(',').map(str::trim).filter(|p| p.parse::<u8>().is_ok()).count() == 3,
        "byte" => is_base64(s),
        "password" => true,
        "date" => is_date(s),
        "duration" => is_openapi_duration(s),
        "datetime" | "date-time" => parse_datetime(s).is_some(),
        _ => return None,
    })
}
