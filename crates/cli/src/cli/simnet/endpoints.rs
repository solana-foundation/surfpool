use std::{
    env,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs},
    str::FromStr,
};

use surfpool_types::SurfpoolConfig;
use url::Url;

/// The addresses Surfpool listens on and the URLs it gives to users and Studio.
///
/// Binding and advertising deliberately have different inputs: a service can listen on a private
/// interface while a reverse proxy publishes it at a different host, port, or scheme. Keep
/// `SURFPOOL_STUDIO_HOST`, `SURFPOOL_PUBLIC_HOST`, and the `SURFPOOL_PUBLIC_*_URL` values here so
/// that distinction cannot be lost when adding a new consumer of these values.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ResolvedEndpoints {
    pub studio_bind_addr: String,
    pub rpc_url: String,
    pub ws_url: String,
    pub studio_url: String,
}

#[derive(Debug, Default)]
struct EndpointOverrides {
    studio_host: Option<String>,
    public_host: Option<String>,
    public_rpc_url: Option<String>,
    public_ws_url: Option<String>,
    public_studio_url: Option<String>,
}

impl EndpointOverrides {
    /// Reads endpoint overrides from the environment.
    fn from_environment() -> Result<Self, String> {
        Ok(Self {
            // Overrides Studio's bind address.
            studio_host: optional_env("SURFPOOL_STUDIO_HOST")?,
            // Supplies the advertised host for services without a specific public URL.
            public_host: optional_env("SURFPOOL_PUBLIC_HOST")?,
            // Supplies the advertised RPC URL.
            public_rpc_url: optional_env("SURFPOOL_PUBLIC_RPC_URL")?,
            // Supplies the advertised WebSocket URL.
            public_ws_url: optional_env("SURFPOOL_PUBLIC_WS_URL")?,
            // Supplies the advertised Studio URL.
            public_studio_url: optional_env("SURFPOOL_PUBLIC_STUDIO_URL")?,
        })
    }

    fn resolve(self, config: &SurfpoolConfig) -> Result<ResolvedEndpoints, String> {
        self.resolve_with(config, &resolve_host)
    }

    /// Resolves endpoints with an injectable host lookup so tests can supply a
    /// fake resolver instead of depending on ambient DNS (unavailable in
    /// sandboxes such as Nix builds or `unshare -n`).
    fn resolve_with(
        self,
        config: &SurfpoolConfig,
        lookup: &dyn Fn(&str) -> Result<Vec<IpAddr>, String>,
    ) -> Result<ResolvedEndpoints, String> {
        let studio_bind = resolve_bind_address(
            self.studio_host.as_deref(),
            &config.studio.bind_host,
            config.studio.bind_port,
            "SURFPOOL_STUDIO_HOST",
        )?;

        ensure_studio_bind_is_available(&studio_bind, config, lookup)?;

        let public_host = self
            .public_host
            .as_deref()
            .map(validate_public_host)
            .transpose()?;

        let rpc_url = resolve_public_url(
            self.public_rpc_url.as_deref(),
            public_host.as_deref(),
            &["http", "https"],
            "http",
            &config.rpc.bind_host,
            config.rpc.bind_port,
            "SURFPOOL_PUBLIC_RPC_URL",
            true,
        )?;
        let ws_url = resolve_public_url(
            self.public_ws_url.as_deref(),
            public_host.as_deref(),
            &["ws", "wss"],
            "ws",
            &config.rpc.bind_host,
            config.rpc.ws_port,
            "SURFPOOL_PUBLIC_WS_URL",
            true,
        )?;
        let studio_url = resolve_public_url(
            self.public_studio_url.as_deref(),
            public_host.as_deref(),
            &["http", "https"],
            "http",
            &studio_bind.host,
            studio_bind.port,
            "SURFPOOL_PUBLIC_STUDIO_URL",
            false,
        )?;

        Ok(ResolvedEndpoints {
            studio_bind_addr: studio_bind.as_socket_string(),
            rpc_url,
            ws_url,
            studio_url,
        })
    }
}

pub(super) fn resolve_endpoints(config: &SurfpoolConfig) -> Result<ResolvedEndpoints, String> {
    EndpointOverrides::from_environment()?.resolve(config)
}

fn optional_env(name: &str) -> Result<Option<String>, String> {
    match env::var(name) {
        Ok(value) if value.is_empty() || value != value.trim() => Err(format!(
            "{name} must be non-empty and contain no leading or trailing whitespace"
        )),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} must contain valid Unicode")),
    }
}

#[derive(Debug)]
struct BindAddress {
    host: String,
    port: u16,
}

impl BindAddress {
    fn as_socket_string(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn resolve_bind_address(
    override_host: Option<&str>,
    default_host: &str,
    default_port: u16,
    variable: &str,
) -> Result<BindAddress, String> {
    let value = override_host.unwrap_or(default_host);

    if let Some(bracketed) = value.strip_prefix('[') {
        let (host, port) = bracketed
            .split_once(']')
            .ok_or_else(|| format!("{variable} has an unterminated IPv6 address"))?;
        Ipv6Addr::from_str(host)
            .map_err(|_| format!("{variable} must contain a valid IPv6 address"))?;
        let port = parse_optional_port(port, default_port, variable)?;
        return Ok(BindAddress {
            host: host.to_string(),
            port,
        });
    }

    if Ipv6Addr::from_str(value).is_ok() {
        return Ok(BindAddress {
            host: value.to_string(),
            port: default_port,
        });
    }

    let (host, port) = match value.split_once(':') {
        Some((host, port)) => (host, parse_port(port, variable)?),
        None => (value, default_port),
    };
    validate_host(host, variable)?;
    Ok(BindAddress {
        host: host.to_string(),
        port,
    })
}

fn parse_optional_port(value: &str, default_port: u16, variable: &str) -> Result<u16, String> {
    match value {
        "" => Ok(default_port),
        value => parse_port(
            value
                .strip_prefix(':')
                .ok_or_else(|| format!("{variable} must use [IPv6]:PORT syntax"))?,
            variable,
        ),
    }
}

fn parse_port(value: &str, variable: &str) -> Result<u16, String> {
    let port: u16 = value
        .parse()
        .map_err(|_| format!("{variable} has an invalid port: {value}"))?;
    if port == 0 {
        return Err(format!("{variable} must use a non-zero port"));
    }
    Ok(port)
}

fn validate_public_host(value: &str) -> Result<String, String> {
    validate_host(value, "SURFPOOL_PUBLIC_HOST")?;
    if value.contains(':') && !value.starts_with('[') {
        return Err(
            "SURFPOOL_PUBLIC_HOST must bracket IPv6 addresses, for example [::1]".to_string(),
        );
    }

    let url = Url::parse(&format!("http://{value}")).map_err(|_| {
        "SURFPOOL_PUBLIC_HOST must be a host name or IP address, not a URL".to_string()
    })?;
    if url.host().is_none() || url.port().is_some() || url.path() != "/" {
        return Err("SURFPOOL_PUBLIC_HOST must not include a scheme, port, or path".to_string());
    }
    Ok(value.to_string())
}

fn validate_host(value: &str, variable: &str) -> Result<(), String> {
    if value.is_empty()
        || value.contains(['/', '?', '#', '@'])
        || value.contains("://")
        || value.chars().any(char::is_whitespace)
    {
        return Err(format!("{variable} must be a host name or IP address"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn resolve_public_url(
    explicit_url: Option<&str>,
    public_host: Option<&str>,
    allowed_schemes: &[&str],
    default_scheme: &str,
    bind_host: &str,
    port: u16,
    variable: &str,
    allow_query: bool,
) -> Result<String, String> {
    if let Some(url) = explicit_url {
        let parsed = Url::parse(url).map_err(|_| format!("{variable} must be an absolute URL"))?;
        if !allowed_schemes.contains(&parsed.scheme())
            || parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(format!(
                "{variable} must use one of {} and include a host without credentials or a fragment",
                allowed_schemes.join(", ")
            ));
        }
        if !allow_query && parsed.query().is_some() {
            return Err(format!("{variable} must not include a query"));
        }
        return Ok(url.to_string());
    }

    let host = public_host.unwrap_or_else(|| default_public_host(bind_host));
    Ok(format!("{default_scheme}://{}:{port}", url_host(host)))
}

fn default_public_host(bind_host: &str) -> &str {
    match bind_host {
        "0.0.0.0" | "::" => "127.0.0.1",
        _ => bind_host,
    }
}

fn url_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn ensure_studio_bind_is_available(
    studio: &BindAddress,
    config: &SurfpoolConfig,
    lookup: &dyn Fn(&str) -> Result<Vec<IpAddr>, String>,
) -> Result<(), String> {
    for (name, host, port) in [
        ("RPC", config.rpc.bind_host.as_str(), config.rpc.bind_port),
        (
            "WebSocket",
            config.rpc.bind_host.as_str(),
            config.rpc.ws_port,
        ),
    ] {
        if studio.port == port && hosts_overlap(&studio.host, host, lookup)? {
            return Err(format!(
                "SURFPOOL_STUDIO_HOST resolves to {}, which conflicts with the {name} listener",
                studio.as_socket_string()
            ));
        }
    }
    Ok(())
}

fn hosts_overlap(
    left: &str,
    right: &str,
    lookup: &dyn Fn(&str) -> Result<Vec<IpAddr>, String>,
) -> Result<bool, String> {
    if left == right {
        return Ok(true);
    }

    let left_addresses = lookup(left)?;
    let right_addresses = lookup(right)?;
    Ok(left_addresses.iter().any(|left| {
        right_addresses
            .iter()
            .any(|right| addresses_overlap(*left, *right))
    }))
}

fn addresses_overlap(left: IpAddr, right: IpAddr) -> bool {
    match (left, right) {
        (IpAddr::V4(left), IpAddr::V4(right)) => {
            left == right || left.is_unspecified() || right.is_unspecified()
        }
        (IpAddr::V6(left), IpAddr::V6(right)) => {
            left == right || left.is_unspecified() || right.is_unspecified()
        }
        // An IPv6 wildcard can accept IPv4 connections through IPv4-mapped addresses on some
        // platforms, so reject that portable ambiguity. An IPv4 wildcard never occupies IPv6.
        (IpAddr::V6(left), IpAddr::V4(_)) => left.is_unspecified(),
        (IpAddr::V4(_), IpAddr::V6(right)) => right.is_unspecified(),
    }
}

fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

/// Resolves hosts that need no DNS lookup: literal IPs (optionally bracketed)
/// and `localhost`. Shared with the test resolver so production and test
/// behavior cannot drift apart.
fn resolve_without_dns(host: &str) -> Option<Vec<IpAddr>> {
    let lookup_host = strip_brackets(host);
    if let Ok(addr) = lookup_host.parse::<IpAddr>() {
        return Some(vec![addr]);
    }
    if lookup_host.eq_ignore_ascii_case("localhost") {
        return Some(vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ]);
    }
    None
}

fn resolve_host(host: &str) -> Result<Vec<IpAddr>, String> {
    if let Some(addresses) = resolve_without_dns(host) {
        return Ok(addresses);
    }

    let lookup_host = strip_brackets(host);
    let socket = if host.contains(':') {
        format!("[{lookup_host}]:0")
    } else {
        format!("{lookup_host}:0")
    };

    socket
        .to_socket_addrs()
        .map(|addresses| addresses.map(|address| address.ip()).collect())
        .map_err(|_| format!("could not resolve listener host {host}"))
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use surfpool_types::SurfpoolConfig;

    use super::{EndpointOverrides, hosts_overlap, resolve_without_dns};

    fn fake_resolver(host: &str) -> Result<Vec<IpAddr>, String> {
        resolve_without_dns(host).ok_or_else(|| format!("could not resolve listener host {host}"))
    }

    #[test]
    fn studio_bind_override_derives_its_advertised_url() {
        let config = SurfpoolConfig::default();
        let resolved = EndpointOverrides {
            studio_host: Some("0.0.0.0:9000".to_string()),
            ..Default::default()
        }
        .resolve(&config)
        .unwrap();

        assert_eq!(resolved.studio_bind_addr, "0.0.0.0:9000");
        assert_eq!(resolved.studio_url, "http://127.0.0.1:9000");
    }

    #[test]
    fn public_host_uses_the_resolved_studio_port() {
        let config = SurfpoolConfig::default();
        let resolved = EndpointOverrides {
            studio_host: Some("0.0.0.0:9000".to_string()),
            public_host: Some("staging.example.com".to_string()),
            ..Default::default()
        }
        .resolve(&config)
        .unwrap();

        assert_eq!(resolved.studio_url, "http://staging.example.com:9000");
        assert_eq!(resolved.rpc_url, "http://staging.example.com:8899");
        assert_eq!(resolved.ws_url, "ws://staging.example.com:8900");
    }

    #[test]
    fn explicit_public_urls_override_the_shared_public_host() {
        let config = SurfpoolConfig::default();
        let resolved = EndpointOverrides {
            public_host: Some("staging.example.com".to_string()),
            public_rpc_url: Some("https://rpc.example.com".to_string()),
            public_studio_url: Some("https://studio.example.com".to_string()),
            ..Default::default()
        }
        .resolve(&config)
        .unwrap();

        assert_eq!(resolved.rpc_url, "https://rpc.example.com");
        assert_eq!(resolved.studio_url, "https://studio.example.com");
        assert_eq!(resolved.ws_url, "ws://staging.example.com:8900");
    }

    #[test]
    fn rejects_a_studio_listener_collision() {
        let config = SurfpoolConfig::default();
        let error = EndpointOverrides {
            studio_host: Some("127.0.0.1:8899".to_string()),
            ..Default::default()
        }
        .resolve(&config)
        .unwrap_err();

        assert!(error.contains("conflicts with the RPC listener"));
    }

    #[test]
    fn rejects_a_studio_listener_hostname_alias_collision() {
        let config = SurfpoolConfig::default();
        let error = EndpointOverrides {
            studio_host: Some("localhost:8899".to_string()),
            ..Default::default()
        }
        .resolve_with(&config, &fake_resolver)
        .unwrap_err();

        assert!(error.contains("conflicts with the RPC listener"));
    }

    #[test]
    fn hostname_alias_overlap_does_not_require_dns() {
        assert!(hosts_overlap("localhost", "127.0.0.1", &fake_resolver).unwrap());
        assert!(!hosts_overlap("localhost", "192.0.2.1", &fake_resolver).unwrap());
    }

    #[test]
    fn reports_unresolvable_studio_hosts() {
        let config = SurfpoolConfig::default();
        let error = EndpointOverrides {
            studio_host: Some("studio.invalid:8899".to_string()),
            ..Default::default()
        }
        .resolve_with(&config, &fake_resolver)
        .unwrap_err();

        assert!(error.contains("could not resolve listener host"));
    }

    #[test]
    fn allows_studio_on_ipv4_when_rpc_uses_bracketed_ipv6() {
        let mut config = SurfpoolConfig::default();
        config.rpc.bind_host = "[::1]".to_string();

        let resolved = EndpointOverrides {
            studio_host: Some("127.0.0.1:8899".to_string()),
            ..Default::default()
        }
        .resolve(&config)
        .unwrap();

        assert_eq!(resolved.studio_bind_addr, "127.0.0.1:8899");
    }

    #[test]
    fn allows_studio_on_ipv6_when_rpc_uses_ipv4_wildcard() {
        let mut config = SurfpoolConfig::default();
        config.rpc.bind_host = "0.0.0.0".to_string();

        let resolved = EndpointOverrides {
            studio_host: Some("[::1]:8899".to_string()),
            ..Default::default()
        }
        .resolve(&config)
        .unwrap();

        assert_eq!(resolved.studio_bind_addr, "[::1]:8899");
    }

    #[test]
    fn rejects_a_query_in_the_public_studio_url() {
        let config = SurfpoolConfig::default();
        let error = EndpointOverrides {
            public_studio_url: Some("https://studio.example.com?tenant=x".to_string()),
            ..Default::default()
        }
        .resolve(&config)
        .unwrap_err();

        assert!(error.contains("SURFPOOL_PUBLIC_STUDIO_URL"));
        assert!(error.contains("query"));
    }

    #[test]
    fn rejects_a_zero_port_studio_override() {
        let error = EndpointOverrides {
            studio_host: Some("127.0.0.1:0".to_string()),
            ..Default::default()
        }
        .resolve(&SurfpoolConfig::default())
        .unwrap_err();

        assert!(error.contains("non-zero port"));
    }

    #[test]
    fn derives_valid_urls_from_an_ipv6_studio_bind() {
        let config = SurfpoolConfig::default();
        let resolved = EndpointOverrides {
            studio_host: Some("::1".to_string()),
            ..Default::default()
        }
        .resolve(&config)
        .unwrap();

        assert_eq!(resolved.studio_url, "http://[::1]:18488");
    }
}
