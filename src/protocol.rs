use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Apply { manifest: String },
    DeleteManifest { manifest: String },
    DeleteDeployment { name: String },
    GetDeployments,
    DescribeDeployment { name: String },
    Logs { name: String, tail: usize },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ack { message: String },
    Deployments { items: Vec<DeploymentSummary> },
    Description { item: DeploymentDescription },
    Logs { content: String },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentSummary {
    pub name: String,
    pub status: String,
    pub image: String,
    pub container_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentDescription {
    pub name: String,
    pub api_version: String,
    pub status: String,
    pub image: String,
    pub container_name: String,
    pub container_id: Option<String>,
    pub message: Option<String>,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub ports: Vec<String>,
}

pub async fn send_message<T>(stream: &mut UnixStream, message: &T) -> Result<()>
where
    T: Serialize,
{
    let payload = serde_json::to_vec(message).context("failed to serialize protocol message")?;
    stream
        .write_u32(payload.len() as u32)
        .await
        .context("failed to write message length")?;
    stream
        .write_all(&payload)
        .await
        .context("failed to write message body")?;
    Ok(())
}

pub async fn read_message<T>(stream: &mut UnixStream) -> Result<T>
where
    T: DeserializeOwned,
{
    let length = stream
        .read_u32()
        .await
        .context("failed to read message length")?;
    let mut buffer = vec![0; length as usize];
    stream
        .read_exact(&mut buffer)
        .await
        .context("failed to read message body")?;
    serde_json::from_slice(&buffer).context("failed to deserialize protocol message")
}

pub async fn request(socket_path: &Path, request: Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
    send_message(&mut stream, &request).await?;
    read_message(&mut stream).await
}
