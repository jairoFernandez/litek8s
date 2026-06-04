use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;

use crate::protocol::ProxyTarget;

pub async fn bind_listener(listen_addr: &str) -> Result<TcpListener> {
    TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind proxy listener on {listen_addr}"))
}

pub async fn serve_listener(listener: TcpListener, target: ProxyTarget) -> Result<()> {
    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (socket, _) = accept_result.context("failed to accept proxy connection")?;
                let target = target.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(socket, target).await {
                        eprintln!("proxy connection failed: {error}");
                    }
                });
            }
            signal_result = tokio::signal::ctrl_c() => {
                signal_result.context("failed to listen for ctrl-c")?;
                return Ok(());
            }
        }
    }
}

/// Resolve the upstream `(host, port)` a connection should be forwarded to.
/// Replica backends carry a `container_ip` and are reached directly at
/// `container_ip:container_port`; the legacy single-container / `litek8s proxy`
/// path carries a published `host_port` and is reached at `127.0.0.1:host_port`.
fn upstream_endpoint(target: &ProxyTarget) -> Result<(String, u16)> {
    if let Some(ip) = target.container_ip.as_deref() {
        return Ok((ip.to_string(), target.container_port));
    }
    let host_port = target
        .host_port
        .context("proxy target has neither a container IP nor a published host port")?;
    Ok(("127.0.0.1".to_string(), host_port))
}

pub async fn forward_connection(mut inbound: TcpStream, target: ProxyTarget) -> Result<()> {
    match target.transport.as_str() {
        "local" => {
            let (host, port) = upstream_endpoint(&target)?;
            let mut upstream = TcpStream::connect((host.as_str(), port))
                .await
                .with_context(|| format!("failed to connect to local upstream {host}:{port}"))?;
            tokio::io::copy_bidirectional(&mut inbound, &mut upstream)
                .await
                .context("failed to proxy TCP stream")?;
            Ok(())
        }
        "ssh" => proxy_over_ssh(inbound, &target).await,
        other => bail!("unsupported proxy transport `{other}`"),
    }
}

async fn handle_connection(inbound: TcpStream, target: ProxyTarget) -> Result<()> {
    forward_connection(inbound, target).await
}

async fn proxy_over_ssh(inbound: TcpStream, target: &ProxyTarget) -> Result<()> {
    let ssh_target = target
        .ssh_target
        .as_deref()
        .context("ssh proxy target is missing the ssh endpoint")?;

    let (host, port) = upstream_endpoint(target)?;
    let mut command = Command::new("ssh");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5");
    // Use the node's registered key, matching how the agent connects. Without
    // this the tunnel falls back to default keys and fails on hosts that only
    // accept a specific identity.
    if let Some(identity) = target.ssh_identity_file.as_deref() {
        command
            .arg("-o")
            .arg("IdentitiesOnly=yes")
            .arg("-i")
            .arg(identity);
    }
    let mut child = command
        .arg(ssh_target)
        .arg("-W")
        .arg(format!("{host}:{port}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn ssh tunnel for {ssh_target}"))?;

    let mut ssh_stdin = child
        .stdin
        .take()
        .context("ssh tunnel stdin is not available")?;
    let mut ssh_stdout = child
        .stdout
        .take()
        .context("ssh tunnel stdout is not available")?;
    let mut ssh_stderr = child
        .stderr
        .take()
        .context("ssh tunnel stderr is not available")?;

    let (mut inbound_read, mut inbound_write) = inbound.into_split();

    let client_to_remote = tokio::spawn(async move {
        tokio::io::copy(&mut inbound_read, &mut ssh_stdin)
            .await
            .context("failed to forward client bytes into ssh tunnel")?;
        ssh_stdin.shutdown().await.ok();
        Ok::<(), anyhow::Error>(())
    });

    let remote_to_client = tokio::spawn(async move {
        tokio::io::copy(&mut ssh_stdout, &mut inbound_write)
            .await
            .context("failed to forward remote bytes back to the client")?;
        inbound_write.shutdown().await.ok();
        Ok::<(), anyhow::Error>(())
    });

    let stderr_reader = tokio::spawn(async move {
        let mut stderr = Vec::new();
        ssh_stderr.read_to_end(&mut stderr).await.ok();
        stderr
    });

    let client_to_remote = client_to_remote
        .await
        .map_err(|error| anyhow!("client->ssh forwarding task failed: {error}"))?;
    let remote_to_client = remote_to_client
        .await
        .map_err(|error| anyhow!("ssh->client forwarding task failed: {error}"))?;
    let stderr = stderr_reader
        .await
        .map_err(|error| anyhow!("ssh stderr task failed: {error}"))?;

    client_to_remote?;
    remote_to_client?;

    let status = child
        .wait()
        .await
        .with_context(|| format!("failed to wait for ssh tunnel on {ssh_target}"))?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr).trim().to_string();
        if detail.is_empty() {
            bail!("ssh tunnel exited with status {status}");
        }
        bail!("{detail}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::handle_connection;
    use crate::protocol::ProxyTarget;

    #[tokio::test]
    async fn proxies_one_local_connection() -> Result<()> {
        let upstream = TcpListener::bind("127.0.0.1:0").await?;
        let upstream_addr = upstream.local_addr()?;
        let upstream_task = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await?;
            let mut request = [0; 256];
            let read = socket.read(&mut request).await?;
            let text = String::from_utf8_lossy(&request[..read]);
            assert!(text.contains("GET /"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await?;
            socket.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        });

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
        let proxy_addr = proxy_listener.local_addr()?;
        let target = ProxyTarget {
            deployment_name: "echo".to_string(),
            node_name: "local".to_string(),
            transport: "local".to_string(),
            ssh_target: None,
            ssh_identity_file: None,
            container_port: 5678,
            host_port: Some(upstream_addr.port()),
            container_ip: None,
            protocol: "TCP".to_string(),
        };

        let proxy_task = tokio::spawn(async move {
            let (socket, _) = proxy_listener.accept().await?;
            handle_connection(socket, target).await
        });

        let mut client = TcpStream::connect(proxy_addr).await?;
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await?;
        client.shutdown().await?;
        let mut response = Vec::new();
        client.read_to_end(&mut response).await?;
        let body = String::from_utf8_lossy(&response);
        assert!(body.contains("200 OK"));
        assert!(body.ends_with("ok"));

        proxy_task.await??;
        upstream_task.await??;
        Ok(())
    }
}
