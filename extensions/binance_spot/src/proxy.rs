//! WebSocket proxy decision for Binance spot — mirrors the Polymarket
//! extension's proxy (`extensions/polymarket/src/proxy.rs`) but scoped to the
//! Binance `@trade` stream only. Reads `BK_PROXY` through the shared
//! `blitzkrieg_bk_contract::net` policy so the feed follows the same tunnel as
//! every other network call in this process.

use blitzkrieg_bk_contract::net::{self, ProxyEndpoint, error_chain};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, client_async_tls, tungstenite::handshake::client::Response,
};

use base64::Engine as _;

const PROXY_IO: Duration = Duration::from_secs(6);

pub async fn connect(
    url: &str,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), String> {
    let active = net::proxy_view()?.active;
    connect_with(url, active).await
}

pub async fn connect_with(
    url: &str,
    active: Option<ProxyEndpoint>,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), String> {
    let Some(ep) = active else {
        return tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| error_chain(&e));
    };
    let stream = tunnel(&ep, url).await?;
    client_async_tls(url, stream)
        .await
        .map_err(|e| error_chain(&e))
}

async fn tunnel(ep: &ProxyEndpoint, url: &str) -> Result<TcpStream, String> {
    let (host, port) = host_port(url)
        .ok_or_else(|| format!("no host readable out of {}", net::redact_secrets(url)))?;
    if ep.scheme == "https" {
        return Err(
            "websocket tunnel supports http, socks5 and socks5h proxies; REST still uses an \
             https:// proxy via reqwest. Set BK_PROXY to an http:// or socks5h:// URL, or \
             BK_PROXY=none to dial direct"
                .to_string(),
        );
    }
    let mut stream = tokio::time::timeout(PROXY_IO, TcpStream::connect(ep.dial_addr()))
        .await
        .map_err(|_| format!("proxy {} timed out", ep.display()))?
        .map_err(|e| format!("proxy {} unreachable: {e}", ep.display()))?;
    match ep.scheme.as_str() {
        "http" => http_connect(&mut stream, ep, &host, port).await?,
        "socks5" | "socks5h" => socks5_connect(&mut stream, ep, &host, port).await?,
        other => {
            return Err(format!(
                "proxy scheme {other} is not supported (http, https, socks5, socks5h)"
            ));
        }
    }
    Ok(stream)
}

async fn http_connect(
    stream: &mut TcpStream,
    ep: &ProxyEndpoint,
    host: &str,
    port: u16,
) -> Result<(), String> {
    let auth = match ep.basic_userinfo() {
        Some((user, pass)) => {
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            format!("Proxy-Authorization: Basic {token}\r\n")
        }
        None => String::new(),
    };
    let req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("proxy CONNECT write: {e}"))?;
    let header = read_http_header(stream).await?;
    let status = header.lines().next().unwrap_or("");
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(format!(
            "proxy CONNECT rejected ({})",
            net::redact_secrets(status)
        ));
    }
    Ok(())
}

async fn read_http_header(stream: &mut TcpStream) -> Result<String, String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() < 8192 {
        let n = tokio::time::timeout(PROXY_IO, stream.read(&mut byte))
            .await
            .map_err(|_| "proxy CONNECT timed out".to_string())?
            .map_err(|e| format!("proxy CONNECT read: {e}"))?;
        if n == 0 {
            return Err("proxy closed during CONNECT".to_string());
        }
        buf.push(byte[0]);
        if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
    }
    Err("proxy CONNECT response exceeded 8KiB".to_string())
}

async fn socks5_connect(
    stream: &mut TcpStream,
    ep: &ProxyEndpoint,
    host: &str,
    port: u16,
) -> Result<(), String> {
    let dest = if ep.scheme == "socks5h" {
        if host.len() > 255 {
            return Err("socks5h destination host is longer than 255 bytes".to_string());
        }
        SocksDest::Domain(host.to_string(), port)
    } else {
        let mut addrs = Vec::new();
        let looked = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| format!("socks5 dns: {e}"))?;
        addrs.extend(looked);
        if addrs.is_empty() {
            return Err("socks5 dns returned no address".to_string());
        }
        let chosen = addrs
            .iter()
            .find(|a| !net::is_fake_ip(&a.ip()))
            .unwrap_or(&addrs[0]);
        match chosen.ip() {
            std::net::IpAddr::V4(v4) => SocksDest::V4(v4.octets(), port),
            std::net::IpAddr::V6(v6) => SocksDest::V6(v6.octets(), port),
        }
    };
    let methods = if ep.basic_userinfo().is_some() {
        &[0x05, 0x01, 0x02]
    } else {
        &[0x05, 0x01, 0x00]
    };
    stream
        .write_all(methods)
        .await
        .map_err(|e| format!("socks greeting: {e}"))?;
    let chosen = read_exact(stream, 2).await?;
    if chosen[0] != 0x05 {
        return Err("socks proxy did not speak SOCKS5".to_string());
    }
    match chosen[1] {
        0x00 => {}
        0x02 => {
            let Some((user, pass)) = ep.basic_userinfo() else {
                return Err("socks proxy asked for a username and the URL has none".to_string());
            };
            if user.len() > 255 || pass.len() > 255 {
                return Err("socks username or password is longer than 255 bytes".to_string());
            };
            let mut auth = vec![0x01, user.len() as u8];
            auth.extend_from_slice(user.as_bytes());
            auth.push(pass.len() as u8);
            auth.extend_from_slice(pass.as_bytes());
            stream
                .write_all(&auth)
                .await
                .map_err(|e| format!("socks auth: {e}"))?;
            let reply = read_exact(stream, 2).await?;
            if reply[1] != 0x00 {
                return Err("socks proxy rejected the username".to_string());
            }
        }
        0xFF => return Err("socks proxy rejected every auth method".to_string()),
        other => {
            return Err(format!(
                "socks proxy picked unsupported auth method {other}"
            ));
        }
    }
    stream
        .write_all(&socks5_request(&dest))
        .await
        .map_err(|e| format!("socks request: {e}"))?;
    let head = read_exact(stream, 4).await?;
    if head[1] != 0x00 {
        return Err(format!("socks connect failed (reply code {})", head[1]));
    }
    let rest = match head[3] {
        0x01 => 4 + 2,
        0x03 => {
            let len = read_exact(stream, 1).await?;
            len[0] as usize + 2
        }
        0x04 => 16 + 2,
        other => return Err(format!("socks reply has unknown address type {other}")),
    };
    let _ = read_exact(stream, rest).await?;
    Ok(())
}

enum SocksDest {
    V4([u8; 4], u16),
    V6([u8; 16], u16),
    Domain(String, u16),
}

fn socks5_request(dest: &SocksDest) -> Vec<u8> {
    let mut out = vec![0x05, 0x01, 0x00];
    match dest {
        SocksDest::V4(octets, port) => {
            out.push(0x01);
            out.extend_from_slice(octets);
            out.extend_from_slice(&port.to_be_bytes());
        }
        SocksDest::V6(octets, port) => {
            out.push(0x04);
            out.extend_from_slice(octets);
            out.extend_from_slice(&port.to_be_bytes());
        }
        SocksDest::Domain(host, port) => {
            out.push(0x03);
            out.push(host.len() as u8);
            out.extend_from_slice(host.as_bytes());
            out.extend_from_slice(&port.to_be_bytes());
        }
    }
    out
}

async fn read_exact(stream: &mut TcpStream, n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(PROXY_IO, stream.read_exact(&mut buf))
        .await
        .map_err(|_| "socks proxy timed out".to_string())?
        .map_err(|e| format!("socks proxy read: {e}"))?;
    Ok(buf)
}

fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme {
        "https" | "wss" => 443,
        "http" | "ws" => 80,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_part = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if let Some(rest) = host_part.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some((host.to_string(), port));
    }
    match host_part.split_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
        None if host_part.is_empty() => None,
        None => Some((host_part.to_string(), default_port)),
    }
}
