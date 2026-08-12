//! mihomo REST API 客户端 + /traffic、/memory 流式读取。
//! Bearer 鉴权（secret 非空时）；流按行解析 JSON，字段缺失容忍 0。

use std::fmt::Display;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
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

/// 运行时策略组信息（GET /proxies 中的组条目）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupInfo {
    pub name: String,
    /// "Selector" | "URLTest" | "Fallback" | "LoadBalance" | "Relay" | "Compatible" | "Pass"
    pub group_type: String,
    /// 当前选中项（自动组为当前生效节点；无选择时 None）
    pub now: Option<String>,
    /// 全部可选成员
    pub all: Vec<String>,
}

impl FromStr for GroupInfo {
    type Err = ApiError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ApiError::Json(e.to_string()))?;
        Ok(Self {
            name: v.get("name").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
            group_type: v.get("type").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
            now: v.get("now").and_then(|x| x.as_str()).map(|s| s.to_string()),
            all: v
                .get("all")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|i| i.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// 连接快照（GET /connections，camelCase 键）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnSnapshot {
    pub download_total: u64,
    pub upload_total: u64,
    pub connections: Vec<ConnInfo>,
    /// 预留：当前内存框仍走 /memory 流，此字段解析后暂未使用。
    pub memory: u64,
}

/// 单条连接。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnInfo {
    pub id: String,
    pub meta: ConnMeta,
    pub upload: u64,
    pub download: u64,
    /// 连接建立时间（RFC3339）；缺失/解析失败为 None。
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    pub chains: Vec<String>,
    pub rule: String,
    pub rule_payload: String,
    pub dl_speed: u64,
    pub ul_speed: u64,
    pub is_alive: bool,
}

/// 连接元数据（metadata 子对象，全部字符串可缺失）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnMeta {
    pub network: String,
    pub host: String,
    pub sniff_host: String,
    pub remote_destination: String,
    pub destination_ip: String,
    pub destination_port: String,
    pub source_ip: String,
    pub source_port: String,
    pub r#type: String,
    pub process_path: String,
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

impl FromStr for ConnSnapshot {
    type Err = ApiError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ApiError::Json(e.to_string()))?;
        let conns = v
            .get("connections")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(parse_conn).collect())
            .unwrap_or_default();
        Ok(Self {
            download_total: v.get("downloadTotal").and_then(|x| x.as_u64()).unwrap_or(0),
            upload_total: v.get("uploadTotal").and_then(|x| x.as_u64()).unwrap_or(0),
            connections: conns,
            memory: v.get("memory").and_then(|x| x.as_u64()).unwrap_or(0),
        })
    }
}

/// 单条连接解析：非对象元素或 start 非法时该字段置默认/None，不整条丢弃。
fn parse_conn(c: &serde_json::Value) -> Option<ConnInfo> {
    let obj = c.as_object()?;
    let get = |key: &str| obj.get(key).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let start = obj
        .get("start")
        .and_then(|x| x.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let meta = c
        .get("metadata")
        .and_then(|m| m.as_object())
        .map(|m| {
            let mget = |key: &str| {
                m.get(key)
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            ConnMeta {
                network: mget("network"),
                host: mget("host"),
                sniff_host: mget("sniffHost"),
                remote_destination: mget("remoteDestination"),
                destination_ip: mget("destinationIP"),
                destination_port: mget("destinationPort"),
                source_ip: mget("sourceIP"),
                source_port: mget("sourcePort"),
                r#type: mget("type"),
                process_path: mget("processPath"),
            }
        })
        .unwrap_or_default();
    Some(ConnInfo {
        id: get("id"),
        meta,
        upload: obj.get("upload").and_then(|x| x.as_u64()).unwrap_or(0),
        download: obj.get("download").and_then(|x| x.as_u64()).unwrap_or(0),
        start,
        chains: obj
            .get("chains")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        rule: get("rule"),
        rule_payload: get("rulePayload"),
        dl_speed: obj.get("dlSpeed").and_then(|x| x.as_u64()).unwrap_or(0),
        ul_speed: obj.get("ulSpeed").and_then(|x| x.as_u64()).unwrap_or(0),
        is_alive: obj.get("isAlive").and_then(|x| x.as_bool()).unwrap_or(true),
    })
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// 整组延迟测试：单节点超时（ms）。超时节点 mihomo 返回 8000，UI 按 >= 此值显示超时。
pub const GROUP_DELAY_TIMEOUT_MS: u16 = 5000;
/// 整组延迟测试：测试 URL（与合并器默认测速地址一致）。
pub const GROUP_DELAY_TEST_URL: &str = "http://www.gstatic.com/generate_204";

/// 路径段百分号编码（组名可含中文/emoji/空格）。
fn encode_path_segment(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

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
        let mut req = self
            .http
            .patch(self.url("/configs"))
            .timeout(REQUEST_TIMEOUT)
            .header(CONTENT_TYPE, "application/json");
        if let Some(auth) = self.auth_header() {
            req = req.header(AUTHORIZATION, auth);
        }
        let resp = req
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

    /// GET /proxies → 全部策略组信息（仅含带 all 字段的组条目，节点被过滤）。
    pub async fn get_proxies(&self) -> Result<Vec<GroupInfo>, ApiError> {
        let body = self.request_text(reqwest::Method::GET, "/proxies").await?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| ApiError::Json(e.to_string()))?;
        let mut out = Vec::new();
        if let Some(proxies) = v.get("proxies").and_then(|x| x.as_object()) {
            for (key, entry) in proxies {
                if entry.get("all").and_then(|x| x.as_array()).is_none() {
                    continue; // 节点无 all 字段，仅策略组保留
                }
                let mut gi: GroupInfo = entry
                    .to_string()
                    .parse()
                    .map_err(|e: ApiError| ApiError::Json(format!("组「{key}」解析失败: {e}")))?;
                if gi.name.is_empty() {
                    gi.name = key.clone();
                }
                out.push(gi);
            }
        }
        Ok(out)
    }

    /// PUT /proxies/{name}，body {"name": target}：切换 select 组当前节点。
    pub async fn switch_group(&self, name: &str, target: &str) -> Result<(), ApiError> {
        let mut req = self
            .http
            .put(self.url(&format!("/proxies/{}", encode_path_segment(name))))
            .timeout(REQUEST_TIMEOUT)
            .header(CONTENT_TYPE, "application/json");
        if let Some(auth) = self.auth_header() {
            req = req.header(AUTHORIZATION, auth);
        }
        let resp = req
            .body(serde_json::json!({ "name": target }).to_string())
            .send()
            .await
            .map_err(|e| ApiError::Conn(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError::Status(resp.status().as_u16()));
        }
        Ok(())
    }

    /// GET /group/{name}/delay → 整组延迟测试，返回 (节点名, 延迟ms) 列表。
    /// 组内节点多时整体较慢，请求超时放宽到 30s。
    pub async fn test_group_delay(&self, name: &str) -> Result<Vec<(String, u16)>, ApiError> {
        let path = format!(
            "/group/{}/delay?url={}&timeout={}",
            encode_path_segment(name),
            encode_path_segment(GROUP_DELAY_TEST_URL),
            GROUP_DELAY_TIMEOUT_MS
        );
        let mut req = self
            .http
            .get(self.url(&path))
            .timeout(Duration::from_secs(30));
        if let Some(auth) = self.auth_header() {
            req = req.header(AUTHORIZATION, auth);
        }
        let resp = req.send().await.map_err(|e| ApiError::Conn(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ApiError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(ApiError::Status(status.as_u16()));
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| ApiError::Json(e.to_string()))?;
        let mut out = Vec::new();
        if let Some(proxies) = v.get("proxies").and_then(|x| x.as_object()) {
            for (n, d) in proxies {
                if let Some(ms) = d.as_u64() {
                    out.push((n.clone(), ms.min(u16::MAX as u64) as u16));
                }
            }
        }
        Ok(out)
    }

    /// GET /connections → 连接快照（全量，一次返回）。
    pub async fn get_connections(&self) -> Result<ConnSnapshot, ApiError> {
        let body = self.request_text(reqwest::Method::GET, "/connections").await?;
        body.parse()
    }

    async fn request_text(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<String, ApiError> {
        let mut req = self
            .http
            .request(method, self.url(path))
            .timeout(REQUEST_TIMEOUT);
        if let Some(auth) = self.auth_header() {
            req = req.header(AUTHORIZATION, auth);
        }
        let resp = req.send().await.map_err(|e| ApiError::Conn(e.to_string()))?;
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
        let mut req = self.http.get(self.url(path));
        if let Some(auth) = self.auth_header() {
            req = req.header(AUTHORIZATION, auth);
        }
        let resp = req.send().await.map_err(|e| ApiError::Conn(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError::Status(resp.status().as_u16()));
        }
        Ok(resp)
    }

    /// Bearer 鉴权头：secret 为空时不发送（避免发出 `Authorization: Bearer `）。
    fn auth_header(&self) -> Option<String> {
        if self.secret.is_empty() {
            None
        } else {
            Some(format!("Bearer {}", self.secret))
        }
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
                    let encoded = first.split(' ').nth(1).unwrap_or("/").to_string();
                    let path = encoded.split('?').next().unwrap_or("").to_string();
                    if first.starts_with("PUT") && path.starts_with("/proxies/") {
                        // PUT /proxies/{name} → 204（非 200，用于断言成功路径）
                        let _ = sock
                            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    if path == "/configs" && first.starts_with("PATCH") {
                        // PATCH /configs → 204
                        let _ = sock
                            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    let body = match path.as_str() {
                        "/version" => r#"{"version":"v1.19.29"}"#.to_string(),
                        "/configs" => r#"{"mode":"rule","ipv6":true,"tun":{"enable":false}}"#.to_string(),
                        "/proxies" => r#"{"proxies":{
                            "节点A":{"name":"节点A","type":"Shadowsocks","history":[]},
                            "手动选择":{"name":"手动选择","type":"Selector","now":"节点B","all":["节点A","节点B","DIRECT"]},
                            "自动选择":{"name":"自动选择","type":"URLTest","now":"节点A","all":["节点A","节点B"]}
                        }}"#.to_string(),
                        _ if path.starts_with("/group/") && path.ends_with("/delay") => {
                            r#"{"proxies":{"节点A":123,"节点B":8000}}"#.to_string()
                        }
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
                        "/connections" => {
                            let payload = "{\"downloadTotal\":9,\"uploadTotal\":8,\"memory\":7,\"connections\":[{\"id\":\"conn1\",\"metadata\":{\"network\":\"tcp\",\"host\":\"conn.example.com\",\"destinationIP\":\"9.9.9.9\",\"destinationPort\":\"443\"},\"upload\":77,\"download\":88,\"start\":\"2026-08-12T10:00:00.000Z\",\"chains\":[\"DIRECT\"],\"rule\":\"DIRECT\",\"rulePayload\":\"\"}]}";
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
    async fn patch_configs_without_secret_omits_auth_header() {
        let (port, mut rx) = spawn_api_server().await;
        let client = Client::new(&NetworkSettings {
            external_controller: format!("127.0.0.1:{port}"),
            secret: String::new(),
            ..NetworkSettings::default()
        });
        client
            .patch_configs(serde_json::json!({"mode": "global"}))
            .await
            .unwrap();
        let req = rx.recv().await.expect("服务器应收到请求");
        assert!(
            !req.to_lowercase().contains("authorization"),
            "空 secret 不应带鉴权头: {req}"
        );
    }

    /// PATCH 失败分支：假服务器只接受一个连接并对 PATCH /configs 恒返 500，
    /// 断言 patch_configs 返回 Err(Status(500))（app 层依赖此错误做失败反馈）。
    #[tokio::test]
    async fn patch_configs_http_500_returns_status_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else { return };
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
            let _ = sock
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
        });
        let e = client_on(port)
            .patch_configs(serde_json::json!({"mode": "direct"}))
            .await
            .unwrap_err();
        assert!(matches!(e, ApiError::Status(500)), "期望 Status(500)，实际: {e}");
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

    // ---------- /connections 解析 ----------

    #[test]
    fn conn_snapshot_full_json() {
        let snap = ConnSnapshot::from_str(
            r#"{"downloadTotal":111,"uploadTotal":222,"memory":333,"connections":[
                {"id":"c1","metadata":{"network":"tcp","type":"HTTP","sourceIP":"127.0.0.1",
                 "destinationIP":"1.2.3.4","sourcePort":"55555","destinationPort":"443",
                 "host":"example.com","dnsMode":"fake-ip","processPath":"/usr/bin/curl",
                 "remoteDestination":"1.2.3.4:443","sniffHost":""},
                 "upload":100,"download":200,
                 "start":"2026-08-12T10:00:00.000Z",
                 "chains":["DIRECT"],"rule":"DIRECT","rulePayload":"","dlSpeed":5,"ulSpeed":3,"isAlive":true}
            ]}"#,
        )
        .unwrap();
        assert_eq!(snap.download_total, 111);
        assert_eq!(snap.upload_total, 222);
        assert_eq!(snap.memory, 333);
        assert_eq!(snap.connections.len(), 1);
        let c = &snap.connections[0];
        assert_eq!(c.id, "c1");
        assert_eq!(c.meta.host, "example.com");
        assert_eq!(c.meta.network, "tcp");
        assert_eq!(c.meta.destination_ip, "1.2.3.4");
        assert_eq!(c.meta.process_path, "/usr/bin/curl");
        assert_eq!(c.upload, 100);
        assert_eq!(c.download, 200);
        assert!(c.start.is_some());
        assert_eq!(c.chains, vec!["DIRECT".to_string()]);
        assert_eq!(c.rule, "DIRECT");
        assert_eq!(c.dl_speed, 5);
        assert_eq!(c.ul_speed, 3);
        assert!(c.is_alive);
    }

    #[test]
    fn conn_snapshot_missing_and_empty() {
        // 顶层缺字段 + connections 缺失 + 空数组
        let snap = ConnSnapshot::from_str(r#"{}"#).unwrap();
        assert_eq!(snap.download_total, 0);
        assert!(snap.connections.is_empty());
        let snap = ConnSnapshot::from_str(r#"{"connections":[]}"#).unwrap();
        assert!(snap.connections.is_empty());
        let snap = ConnSnapshot::from_str(r#"{"connections":null}"#).unwrap();
        assert!(snap.connections.is_empty());
    }

    #[test]
    fn conn_start_parsing_variants() {
        // 合法 RFC3339
        let snap = ConnSnapshot::from_str(
            r#"{"connections":[{"id":"a","start":"2026-08-12T10:00:00.000Z"}]}"#,
        )
        .unwrap();
        assert!(snap.connections[0].start.is_some());
        // RFC3339Nano（带纳秒）
        let snap = ConnSnapshot::from_str(
            r#"{"connections":[{"id":"b","start":"2026-08-12T10:00:00.123456789Z"}]}"#,
        )
        .unwrap();
        assert!(snap.connections[0].start.is_some());
        // 缺失 / 非法 → None，不整条丢弃
        let snap = ConnSnapshot::from_str(
            r#"{"connections":[{"id":"c"},{"id":"d","start":"not-a-date"}]}"#,
        )
        .unwrap();
        assert!(snap.connections[0].start.is_none());
        assert!(snap.connections[1].start.is_none());
        assert_eq!(snap.connections.len(), 2);
    }

    #[tokio::test]
    async fn get_connections_ok() {
        let (port, _rx) = spawn_api_server().await;
        let snap = client_on(port).get_connections().await.unwrap();
        assert_eq!(snap.connections.len(), 1);
        assert_eq!(snap.connections[0].meta.host, "conn.example.com");
        assert_eq!(snap.connections[0].upload, 77);
    }

    #[tokio::test]
    async fn traffic_stream_conn_error() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(client_on(port).traffic_stream().await.is_err());
    }

    // ---------- 组信息 / 组操作 API ----------

    #[test]
    fn group_info_from_json() {
        let gi = GroupInfo::from_str(
            r#"{"name":"手动选择","type":"Selector","now":"节点B","all":["节点A","节点B","DIRECT"]}"#,
        )
        .unwrap();
        assert_eq!(gi.name, "手动选择");
        assert_eq!(gi.group_type, "Selector");
        assert_eq!(gi.now.as_deref(), Some("节点B"));
        assert_eq!(gi.all, vec!["节点A", "节点B", "DIRECT"]);
    }

    #[test]
    fn group_info_no_now_is_none() {
        let gi = GroupInfo::from_str(r#"{"name":"g","type":"URLTest","all":["a"]}"#).unwrap();
        assert_eq!(gi.now, None);
        assert_eq!(gi.all, vec!["a"]);
    }

    #[tokio::test]
    async fn get_proxies_filters_nodes_keeps_groups() {
        let (port, _rx) = spawn_api_server().await;
        let groups = client_on(port).get_proxies().await.unwrap();
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["手动选择", "自动选择"], "节点应被过滤: {names:?}");
        assert_eq!(groups[0].group_type, "Selector");
        assert_eq!(groups[0].now.as_deref(), Some("节点B"));
        assert_eq!(groups[0].all.len(), 3);
        assert_eq!(groups[1].group_type, "URLTest");
    }

    #[tokio::test]
    async fn switch_group_sends_put_with_body_and_auth() {
        let (port, mut rx) = spawn_api_server().await;
        client_on(port)
            .switch_group("手动选择", "节点A")
            .await
            .unwrap();
        let req = rx.recv().await.expect("服务器应收到请求");
        assert!(req.starts_with("PUT /proxies/"), "请求行: {req}");
        assert!(
            req.to_lowercase().contains("authorization: bearer testsecret"),
            "应带 Bearer 鉴权: {req}"
        );
        assert!(req.contains(r#"{"name":"节点A"}"#), "body 应为选择目标: {req}");
    }

    #[tokio::test]
    async fn switch_group_encodes_unicode_name() {
        let (port, mut rx) = spawn_api_server().await;
        client_on(port).switch_group("🚀 节点选择", "DIRECT").await.unwrap();
        let req = rx.recv().await.expect("服务器应收到请求");
        let first_line = req.lines().next().unwrap_or("");
        assert!(
            first_line.starts_with("PUT /proxies/%F0%9F%9A%80%20"),
            "组名应百分号编码: {first_line}"
        );
        assert!(!first_line.contains('🚀'), "不应含裸 emoji: {first_line}");
    }

    #[tokio::test]
    async fn test_group_delay_parses_proxies_map() {
        let (port, _rx) = spawn_api_server().await;
        let list = client_on(port).test_group_delay("自动选择").await.unwrap();
        assert_eq!(list, vec![("节点A".to_string(), 123), ("节点B".to_string(), 8000)]);
    }

    #[tokio::test]
    async fn switch_group_http_400_returns_status_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut tmp).await {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
            }
            let _ = sock
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
        });
        let e = client_on(port)
            .switch_group("不存在", "x")
            .await
            .unwrap_err();
        assert!(matches!(e, ApiError::Status(400)), "期望 Status(400)，实际: {e}");
    }
}
