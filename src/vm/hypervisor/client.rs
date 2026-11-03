//! Client operations against the Cloud Hypervisor REST API.
//!
//! Every method is one request/response exchange over the Unix socket; the
//! HTTP framing lives in [`super::protocol`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::time::timeout;

use super::protocol::{
    API_PREFIX, Error, HttpResponse, check_status, parse_http_response, read_http_response,
};
use super::types::{SnapshotConfig, VmInfo};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A minimal Cloud Hypervisor API client.
#[derive(Debug, Clone)]
pub struct CloudHypervisorClient {
    socket_path: PathBuf,
}

impl CloudHypervisorClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// `PUT /api/v1/vm.info` -> state, memory and device tree.
    pub async fn info(&self) -> Result<VmInfo> {
        self.get("/vm.info").await
    }

    /// `PUT /api/v1/vm.pause`.
    pub async fn pause(&self) -> Result<()> {
        self.put_empty("/vm.pause").await
    }

    /// `PUT /api/v1/vm.resume`.
    pub async fn resume(&self) -> Result<()> {
        self.put_empty("/vm.resume").await
    }

    /// `PUT /api/v1/vm.snapshot`. The VM must be paused first.
    pub async fn snapshot(&self, spec: &SnapshotConfig) -> Result<()> {
        self.put_json("/vm.snapshot", spec).await
    }

    /// Ask the VMM to shut the guest down. hegn still verifies the process
    /// before sending signals, so this is a graceful first step only.
    pub async fn shutdown(&self) -> Result<()> {
        self.put_empty("/vm.shutdown").await
    }

    async fn put_empty(&self, path: &str) -> Result<()> {
        let response = self.request("PUT", path, Some(Vec::new())).await?;
        check_status(response.status, &response.body)
    }

    async fn put_json<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let bytes = serde_json::to_vec(body)?;
        let response = self.request("PUT", path, Some(bytes)).await?;
        check_status(response.status, &response.body)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.request("GET", path, None).await?;
        check_status(response.status, &response.body)?;
        Ok(serde_json::from_slice(&response.body)?)
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse> {
        let result = timeout(REQUEST_TIMEOUT, self.request_inner(method, path, body)).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(Error::Timeout(REQUEST_TIMEOUT).into()),
        }
    }

    async fn request_inner(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to Cloud Hypervisor API socket {}",
                    self.socket_path.display()
                )
            })?;

        let body = body.unwrap_or_default();
        let uri = format!("{API_PREFIX}{path}");
        let mut request = format!(
            "{method} {uri} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n"
        );
        if method != "GET" {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await?;
        if !body.is_empty() {
            stream.write_all(&body).await?;
        }
        stream.flush().await?;

        let raw = read_http_response(&mut stream).await?;
        parse_http_response(&raw)
    }
}

/// Convenience: is the API socket present and does it report a live VM?
pub async fn is_running(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }
    CloudHypervisorClient::new(socket_path)
        .info()
        .await
        .map(|info| info.state.is_alive())
        .unwrap_or(false)
}

/// Validate that a path is usable as a Cloud Hypervisor socket before spawning.
pub fn validate_socket_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("socket path is empty");
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        bail!("socket directory {} does not exist", parent.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::hypervisor::VmState;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Recorded {
        method: String,
        path: String,
        body: Vec<u8>,
    }

    async fn read_request(stream: &mut UnixStream) -> Recorded {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
                let length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + length {
                    break;
                }
            }
        }
        let pos = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
        let mut first = headers.lines().next().unwrap().split_whitespace();
        let method = first.next().unwrap().to_string();
        let path = first.next().unwrap().to_string();
        let body = buf[pos + 4..].to_vec();
        Recorded { method, path, body }
    }

    fn spawn_mock(
        responses: Vec<(u16, String)>,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        tokio::task::JoinHandle<Vec<Recorded>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-hypervisor.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let recorded_task = recorded.clone();
        let handle = tokio::spawn(async move {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req = read_request(&mut stream).await;
                recorded_task.lock().await.push(req);
                let reason = if status == 204 { "No Content" } else { "OK" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            recorded.lock().await.clone()
        });
        (dir, path, handle)
    }

    #[tokio::test]
    async fn pauses_and_resumes_via_the_v1_endpoints() {
        let (_dir, path, handle) = spawn_mock(vec![(204, String::new()), (204, String::new())]);
        let client = CloudHypervisorClient::new(&path);
        client.pause().await.unwrap();
        client.resume().await.unwrap();

        let requests = handle.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "PUT");
        assert_eq!(requests[0].path, "/api/v1/vm.pause");
        assert!(requests[0].body.is_empty());
        assert_eq!(requests[1].method, "PUT");
        assert_eq!(requests[1].path, "/api/v1/vm.resume");
    }

    #[tokio::test]
    async fn reads_vm_info() {
        let (_dir, path, handle) = spawn_mock(vec![(
            200,
            r#"{"state":"Running","memory_actual_size":1073741824,"config":{"payload":{"kernel":"/img/vmlinux"}}}"#
                .into(),
        )]);
        let info = CloudHypervisorClient::new(&path).info().await.unwrap();
        assert_eq!(info.state, VmState::Running);
        assert_eq!(info.memory_actual_size, Some(1024 * 1024 * 1024));
        let requests = handle.await.unwrap();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/api/v1/vm.info");
    }

    #[tokio::test]
    async fn snapshot_sends_a_file_url() {
        let (_dir, path, handle) = spawn_mock(vec![(204, String::new())]);
        CloudHypervisorClient::new(&path)
            .snapshot(&SnapshotConfig {
                destination_url: "file:///snapshots/demo".into(),
            })
            .await
            .unwrap();

        let requests = handle.await.unwrap();
        assert_eq!(requests[0].path, "/api/v1/vm.snapshot");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["destination_url"], "file:///snapshots/demo");
    }

    #[tokio::test]
    async fn reads_a_framed_response_without_waiting_for_socket_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-hypervisor.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_request(&mut stream).await;
            let body = r#"{"state":"Running"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            // Cloud Hypervisor may keep the connection open; the client must
            // not wait for this sleep/EOF to finish the response.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let info = tokio::time::timeout(
            Duration::from_millis(500),
            CloudHypervisorClient::new(&path).info(),
        )
        .await
        .expect("client waited for EOF instead of Content-Length")
        .unwrap();
        assert_eq!(info.state, VmState::Running);
        server.abort();
    }

    #[tokio::test]
    async fn no_content_responses_do_not_wait_for_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-hypervisor.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        tokio::time::timeout(
            Duration::from_millis(500),
            CloudHypervisorClient::new(&path).pause(),
        )
        .await
        .expect("client waited for EOF after a bodyless 204")
        .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn surfaces_api_errors() {
        let (_dir, path, handle) = spawn_mock(vec![(400, r#"{"error":"nope"}"#.into())]);
        let err = CloudHypervisorClient::new(&path).pause().await.unwrap_err();
        assert!(err.to_string().contains("HTTP 400"), "{err}");
        assert!(err.to_string().contains("nope"), "{err}");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn missing_socket_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = CloudHypervisorClient::new(dir.path().join("missing.sock"))
            .info()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("failed to connect"), "{err}");
    }

    #[test]
    fn socket_path_validation_rejects_missing_parent() {
        let err = validate_socket_path(Path::new("/definitely/missing/cloud-hypervisor.sock"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(validate_socket_path(Path::new("")).is_err());
    }
}
