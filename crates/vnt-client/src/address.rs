use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

pub const ADDRESS_ERROR: &str = "地址格式无效，请输入 IP 或域名，可附加 :端口（1–65535），默认端口 29875；IPv6 带端口时使用 [地址]:端口";

pub(super) fn valid_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    if host.contains(':')
        || (host.contains('.') && host.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
    {
        return false;
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

pub(super) async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let host = host.trim();
    if !valid_host(host) || port == 0 {
        return Err(ADDRESS_ERROR.into());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addresses: Vec<_> = tokio::time::timeout(
        Duration::from_secs(8),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| format!("域名解析超时: {host}"))?
    .map_err(|e| format!("域名解析失败: {host}: {e}"))?
    .collect();
    if addresses.is_empty() {
        return Err(format!("域名解析失败: {host} 没有可用地址"));
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_validation_accepts_ips_and_dns_names_only() {
        for host in [
            "192.168.1.10",
            "example.com",
            "my-pc",
            "localhost",
            "example.com.",
            "::1",
            "2001:db8::1",
            "::ffff:192.168.1.10",
        ] {
            assert!(valid_host(host), "{host}");
        }
        for host in [
            "",
            "192.168.1.10:",
            "256.1.1.1",
            "01.2.3.4",
            "127.1",
            "example.com:80",
            "https://example.com",
            "host name",
            "-host",
            "host-",
            "a..b",
            "[::1]",
            "1::2::3",
            "::ffff:192.168.1.999",
        ] {
            assert!(!valid_host(host), "{host}");
        }
    }

    #[tokio::test]
    async fn resolves_localhost_and_preserves_the_port() {
        let addresses = resolve("localhost", vnt_proto::DEFAULT_PORT).await.unwrap();
        assert!(!addresses.is_empty());
        assert!(addresses
            .iter()
            .all(|addr| addr.ip().is_loopback() && addr.port() == vnt_proto::DEFAULT_PORT));
        assert_eq!(
            resolve("::1", 31000).await.unwrap(),
            vec!["[::1]:31000".parse::<SocketAddr>().unwrap()]
        );
        assert!(resolve("localhost", 0).await.is_err());
        assert!(resolve("localhost:", 29875).await.is_err());
    }
}
