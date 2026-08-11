//! mihomo REST API 客户端 + /traffic、/memory 流式读取。
//! Bearer 鉴权（secret 非空时）；流按行解析 JSON，字段缺失容忍 0。

use std::fmt::Display;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};

use crate::core::models::NetworkSettings;

/// 运行时状态（来自 GET /configs）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeConfig {
    pub mode: String,
    pub ipv6: bool,
    pub tun_enable: bool,
}

/// 单帧流量统计（来自 /traffic，camelCase 键）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrafficFrame {
    pub up: u64,
    pub down: u64,
    pub up_total: u64,
    pub down_total: u64,
}

/// 单帧内存统计（来自 /memory）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MemoryFrame {
    pub inuse: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    Http(String),
    #[error("连接失败: {0}")]
    Conn(String),
    #[error("HTTP 状态 {0}")]
    Status(u16),
    #[error("{0}")]
    Json(String),
}

impl FromStr for RuntimeConfig {
    type Err = ApiError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ApiError::Json(e.to_string()))?;
        Ok(Self {
            mode: v
                .get("mode")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            ipv6: v.get("ipv6").and_then(|x| x.as_bool()).unwrap_or(false),
            tun_enable: v
                .get("tun")
                .and_then(|t| t.get("enable"))
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
        })
    }
}

impl FromStr for TrafficFrame {
    type Err = ApiError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ApiError::Json(e.to_string()))?;
        Ok(Self {
            up: v.get("up").and_then(|x| x.as_u64()).unwrap_or(0),
            down: v.get("down").and_then(|x| x.as_u64()).unwrap_or(0),
            up_total: v.get("upTotal").and_then(|x| x.as_u64()).unwrap_or(0),
            down_total: v.get("downTotal").and_then(|x| x.as_u64()).unwrap_or(0),
        })
    }
}

impl FromStr for MemoryFrame {
    type Err = ApiError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ApiError::Json(e.to_string()))?;
        Ok(Self {
            inuse: v.get("inuse").and_then(|x| x.as_u64()).unwrap_or(0),
        })
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Client {
    base: String,
    secret: String,
    http: reqwest::Client,
}

impl Client {
    /// base = http://{external_controller}。
    pub fn new(settings: &NetworkSettings) -> Self {
        Self {
            base: format!("http://{}", settings.external_controller),
            secret: settings.secret.clone(),
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// GET /version，非 2xx 或网络错误 → Err。
    pub async fn ping(&self) -> Result<(), ApiError> {
        let resp = self
            .http
            .get(self.url("/version"))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| ApiError::Conn(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError::Status(resp.status().as_u16()));
        }
        Ok(())
    }

    /// GET /configs → 运行时状态。
    pub async fn get_configs(&self) -> Result<RuntimeConfig, ApiError> {
        let body = self.request_text(reqwest::Method::GET, "/configs").await?;
        body.parse()
    }

    /// PATCH /configs（热切换 mode/ipv6/tun.enable 等）。
    pub async fn patch_configs(&self, patch: serde_json::Value) -> Result<(), ApiError> {
        let resp = self
            .http
            .patch(self.url("/configs"))
            .timeout(REQUEST_TIMEOUT)
            .header(AUTHORIZATION, self.auth_header())
            .header(CONTENT_TYPE, "application/json")
            .body(patch.to_string())
            .send()
            .await
            .map_err(|e| ApiError::Conn(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError::Status(resp.status().as_u16()));
        }
        Ok(())
    }

    /// GET /traffic → 按行解析的流量帧流；结束/出错由调用方重连。
    pub async fn traffic_stream(
        &self,
    ) -> Result<impl Stream<Item = Result<TrafficFrame, ApiError>> + Unpin, ApiError> {
        let resp = self.stream_response("/traffic").await?;
        let stream = LineStream::new(resp.bytes_stream().map(|c| c.map(|b| b.to_vec())));
        Ok(stream.map(|line| line.and_then(|l| l.parse())))
    }

    /// GET /memory → 按行解析的内存帧流。
    pub async fn memory_stream(
        &self,
    ) -> Result<impl Stream<Item = Result<MemoryFrame, ApiError>> + Unpin, ApiError> {
        let resp = self.stream_response("/memory").await?;
        let stream = LineStream::new(resp.bytes_stream().map(|c| c.map(|b| b.to_vec())));
        Ok(stream.map(|line| line.and_then(|l| l.parse())))
    }

    async fn request_text(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<String, ApiError> {
        let resp = self
            .http
            .request(method, self.url(path))
            .timeout(REQUEST_TIMEOUT)
            .header(AUTHORIZATION, self.auth_header())
            .send()
            .await
            .map_err(|e| ApiError::Conn(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ApiError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(ApiError::Status(status.as_u16()));
        }
        Ok(text)
    }

    async fn stream_response(
        &self,
        path: &str,
    ) -> Result<reqwest::Response, ApiError> {
        let resp = self
            .http
            .get(self.url(path))
            .header(AUTHORIZATION, self.auth_header())
            .send()
            .await
            .map_err(|e| ApiError::Conn(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError::Status(resp.status().as_u16()));
        }
        Ok(resp)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.secret)
    }
}

/// 按行切分 bytes 流；跨块缓冲；忽略空行。
struct LineStream<S> {
    inner: S,
    buf: Vec<u8>,
}

impl<S> LineStream<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }
}

impl<S, E> Stream for LineStream<S>
where
    S: Stream<Item = Result<Vec<u8>, E>> + Unpin,
    E: Display,
{
    type Item = Result<String, ApiError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.inner.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.buf.extend_from_slice(&bytes);
                    while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                        let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                        if line.last() == Some(&b'\n') {
                            line.pop();
                        }
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        let s = String::from_utf8_lossy(&line).trim().to_string();
                        if !s.is_empty() {
                            return Poll::Ready(Some(Ok(s)));
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(ApiError::Conn(e.to_string()))));
                }
                Poll::Ready(None) => {
                    let rest = std::mem::take(&mut self.buf);
                    let s = String::from_utf8_lossy(&rest).trim().to_string();
                    if !s.is_empty() {
                        return Poll::Ready(Some(Ok(s)));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::NetworkSettings;
    use futures_util::StreamExt;

    // ---------- JSON 反序列化 ----------

    #[test]
    fn runtime_config_from_json() {
        let rc = RuntimeConfig::from_str(r#"{"mode":"global","ipv6":true,"tun":{"enable":true}}"#).unwrap();
        assert_eq!(rc.mode, "global");
        assert!(rc.ipv6);
        assert!(rc.tun_enable);
    }

    #[test]
    fn runtime_config_missing_fields_default() {
        let rc = RuntimeConfig::from_str(r#"{"mode":"rule"}"#).unwrap();
        assert_eq!(rc.mode, "rule");
        assert!(!rc.ipv6);
        assert!(!rc.tun_enable);
    }

    #[test]
    fn traffic_frame_from_json() {
        let f = TrafficFrame::from_str(r#"{"up":123,"down":456,"upTotal":789,"downTotal":1011}"#).unwrap();
        assert_eq!(f.up, 123);
        assert_eq!(f.down, 456);
        assert_eq!(f.up_total, 789);
        assert_eq!(f.down_total, 1011);
    }

    #[test]
    fn traffic_frame_missing_fields_zero() {
        let f = TrafficFrame::from_str(r#"{"up":1}"#).unwrap();
        assert_eq!(f.up, 1);
        assert_eq!(f.down, 0);
        assert_eq!(f.up_total, 0);
        assert_eq!(f.down_total, 0);
    }

    #[test]
    fn memory_frame_from_json() {
        let f = MemoryFrame::from_str(r#"{"inuse":999,"os":888}"#).unwrap();
        assert_eq!(f.inuse, 999);
    }

    #[test]
    fn client_base_url() {
        let s = NetworkSettings {
            external_controller: "127.0.0.1:9090".into(),
            secret: "abc123".into(),
            ..NetworkSettings::default()
        };
        let c = Client::new(&s);
        assert_eq!(c.base, "http://127.0.0.1:9090");
        assert_eq!(c.secret, "abc123");
    }

    // ---------- 与假 mihomo API 服务器联测 ----------

    async fn spawn_api_server() -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::mpsc;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let mut headers_end = false;
                    while !headers_end {
                        match sock.read(&mut tmp).await {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    headers_end = true;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let req = String::from_utf8_lossy(&buf).into_owned();
                    let _ = tx.send(req);
                    let head = buf
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|i| i + 4)
                        .unwrap_or(buf.len());
                    let line = String::from_utf8_lossy(&buf[..head]).into_owned();
                    let first = line.lines().next().unwrap_or("").to_string();
                    let path = first.split(' ').nth(1).unwrap_or("/");
                    if path == "/configs" && first.starts_with("PATCH") {
                        // PATCH /configs → 204
                        let _ = sock
                            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    let body = match path {
                        "/version" => r#"{"version":"v1.19.29"}"#.to_string(),
                        "/configs" => r#"{"mode":"rule","ipv6":true,"tun":{"enable":false}}"#.to_string(),
                        "/traffic" => {
                            let payload = "{\"up\":1,\"down\":2,\"upTotal\":3,\"downTotal\":4}\n{\"up\":5,\"down\":6,\"upTotal\":7,\"downTotal\":8}\n";
                            let _ = sock
                                .write_all(
                                    format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                                        payload.len()
                                    )
                                    .as_bytes(),
                                )
                                .await;
                            return;
                        }
                        "/memory" => {
                            let payload = "{\"inuse\":111,\"os\":222}\n";
                            let _ = sock
                                .write_all(
                                    format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                                        payload.len()
                                    )
                                    .as_bytes(),
                                )
                                .await;
                            return;
                        }
                        _ => r#"{"ok":true}"#.to_string(),
                    };
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        (port, rx)
    }

    fn client_on(port: u16) -> Client {
        let s = NetworkSettings {
            external_controller: format!("127.0.0.1:{port}"),
            secret: "testsecret".into(),
            ..NetworkSettings::default()
        };
        Client::new(&s)
    }

    #[tokio::test]
    async fn ping_ok() {
        let (port, _rx) = spawn_api_server().await;
        client_on(port).ping().await.unwrap();
    }

    #[tokio::test]
    async fn ping_connection_refused() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let e = client_on(port).ping().await.unwrap_err();
        assert!(matches!(e, ApiError::Conn(_)), "错误: {e}");
    }

    #[tokio::test]
    async fn get_configs_ok() {
        let (port, _rx) = spawn_api_server().await;
        let rc = client_on(port).get_configs().await.unwrap();
        assert_eq!(rc.mode, "rule");
        assert!(rc.ipv6);
        assert!(!rc.tun_enable);
    }

    #[tokio::test]
    async fn patch_configs_sends_bearer_auth() {
        let (port, mut rx) = spawn_api_server().await;
        client_on(port)
            .patch_configs(serde_json::json!({"mode": "global"}))
            .await
            .unwrap();
        let req = rx.recv().await.expect("服务器应收到请求");
        assert!(req.starts_with("PATCH /configs"), "请求行: {req}");
        let req_lower = req.to_lowercase();
        assert!(
            req_lower.contains("authorization: bearer testsecret"),
            "应带 Bearer 鉴权: {req}"
        );
        assert!(req_lower.contains("content-type: application/json"));
    }

    #[tokio::test]
    async fn traffic_stream_frames() {
        let (port, _rx) = spawn_api_server().await;
        let stream = client_on(port).traffic_stream().await.unwrap();
        let frames: Vec<TrafficFrame> = stream
            .take(2)
            .map(|r| r.unwrap())
            .collect()
            .await;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].up, 1);
        assert_eq!(frames[0].down_total, 4);
        assert_eq!(frames[1].up, 5);
        assert_eq!(frames[1].up_total, 7);
    }

    #[tokio::test]
    async fn memory_stream_frame() {
        let (port, _rx) = spawn_api_server().await;
        let stream = client_on(port).memory_stream().await.unwrap();
        let frames: Vec<MemoryFrame> = stream.take(1).map(|r| r.unwrap()).collect().await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].inuse, 111);
    }

    #[tokio::test]
    async fn traffic_stream_conn_error() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(client_on(port).traffic_stream().await.is_err());
    }
}
