use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use reqwest::header::CONTENT_TYPE;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use uuid::Uuid;

const FEISHU_BASE_URL: &str = "https://open.feishu.cn/open-apis";
const FEISHU_WS_BASE_URL: &str = "https://open.feishu.cn";
const LARK_BASE_URL: &str = "https://open.larksuite.com/open-apis";
const LARK_WS_BASE_URL: &str = "https://open.larksuite.com";
const DEFAULT_LARK_WEBHOOK_PATH: &str = "/lark";
const DEFAULT_LARK_VERIFY_SIGNATURE: bool = false;
const DEFAULT_LARK_VERIFY_TIMESTAMP_WINDOW_SECS: u64 = 0;
const DEFAULT_FEISHU_WEBHOOK_PATH: &str = "/feishu";
const DEFAULT_FEISHU_VERIFY_SIGNATURE: bool = true;
const DEFAULT_FEISHU_VERIFY_TIMESTAMP_WINDOW_SECS: u64 = 300;
const DEFAULT_FEISHU_ACK_REACTION: &str = "auto";

const LARK_ACK_REACTIONS_ZH_CN: &[&str] = &[
    "OK", "JIAYI", "APPLAUSE", "THUMBSUP", "MUSCLE", "SMILE", "DONE",
];
const LARK_ACK_REACTIONS_ZH_TW: &[&str] = &[
    "OK",
    "JIAYI",
    "APPLAUSE",
    "THUMBSUP",
    "FINGERHEART",
    "SMILE",
    "DONE",
];
const LARK_ACK_REACTIONS_EN: &[&str] = &[
    "OK",
    "THUMBSUP",
    "THANKS",
    "MUSCLE",
    "FINGERHEART",
    "APPLAUSE",
    "SMILE",
    "DONE",
];
const LARK_ACK_REACTIONS_JA: &[&str] = &[
    "OK",
    "THUMBSUP",
    "THANKS",
    "MUSCLE",
    "FINGERHEART",
    "APPLAUSE",
    "SMILE",
    "DONE",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LarkAckLocale {
    ZhCn,
    ZhTw,
    En,
    Ja,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LarkPlatform {
    Lark,
    Feishu,
}

impl LarkPlatform {
    fn api_base(self) -> &'static str {
        match self {
            Self::Lark => LARK_BASE_URL,
            Self::Feishu => FEISHU_BASE_URL,
        }
    }

    fn ws_base(self) -> &'static str {
        match self {
            Self::Lark => LARK_WS_BASE_URL,
            Self::Feishu => FEISHU_WS_BASE_URL,
        }
    }

    fn locale_header(self) -> &'static str {
        match self {
            Self::Lark => "en",
            Self::Feishu => "zh",
        }
    }

    fn proxy_service_key(self) -> &'static str {
        match self {
            Self::Lark => "channel.lark",
            Self::Feishu => "channel.feishu",
        }
    }

    fn channel_name(self) -> &'static str {
        match self {
            Self::Lark => "lark",
            Self::Feishu => "feishu",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Feishu WebSocket long-connection: pbbp2.proto frame codec
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, prost::Message)]
struct PbHeader {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

/// Feishu WS frame (pbbp2.proto).
/// method=0 → CONTROL (ping/pong)  method=1 → DATA (events)
#[derive(Clone, PartialEq, prost::Message)]
struct PbFrame {
    #[prost(uint64, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, tag = "2")]
    pub log_id: u64,
    #[prost(int32, tag = "3")]
    pub service: i32,
    #[prost(int32, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<PbHeader>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub payload: Option<Vec<u8>>,
}

impl PbFrame {
    fn header_value<'a>(&'a self, key: &str) -> &'a str {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
            .unwrap_or("")
    }
}

/// Server-sent client config (parsed from pong payload)
#[derive(Debug, serde::Deserialize, Default, Clone)]
struct WsClientConfig {
    #[serde(rename = "PingInterval")]
    ping_interval: Option<u64>,
}

/// POST /callback/ws/endpoint response
#[derive(Debug, serde::Deserialize)]
struct WsEndpointResp {
    code: i32,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    data: Option<WsEndpoint>,
}

#[derive(Debug, serde::Deserialize)]
struct WsEndpoint {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "ClientConfig")]
    client_config: Option<WsClientConfig>,
}

/// LarkEvent envelope (method=1 / type=event payload)
#[derive(Debug, serde::Deserialize)]
struct LarkEvent {
    header: LarkEventHeader,
    event: serde_json::Value,
}

#[derive(Debug, serde::Deserialize)]
struct LarkEventHeader {
    event_type: String,
    #[allow(dead_code)]
    event_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct MsgReceivePayload {
    sender: LarkSender,
    message: LarkMessage,
}

#[derive(Debug, serde::Deserialize)]
struct LarkSender {
    sender_id: LarkSenderId,
    #[serde(default)]
    sender_type: String,
}

#[derive(Debug, serde::Deserialize, Default)]
struct LarkSenderId {
    open_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct LarkMessage {
    message_id: String,
    chat_id: String,
    chat_type: String,
    message_type: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    mentions: Vec<serde_json::Value>,
}

/// Heartbeat timeout for WS connection — must be larger than ping_interval (default 120 s).
/// If no binary frame (pong or event) is received within this window, reconnect.
const WS_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh tenant token this many seconds before the announced expiry.
const LARK_TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(120);
/// Fallback tenant token TTL when `expire`/`expires_in` is absent.
const LARK_DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(7200);
/// Feishu/Lark API business code for expired/invalid tenant access token.
const LARK_INVALID_ACCESS_TOKEN_CODE: i64 = 99_991_663;
/// Max bytes downloaded from remote URLs before uploading to Lark/Feishu.
const LARK_REMOTE_UPLOAD_MAX_BYTES: usize = 20 * 1024 * 1024;
/// Timeout for downloading remote media URLs before upload.
const LARK_REMOTE_UPLOAD_TIMEOUT_SECS: u64 = 20;
/// Max redirect hops allowed for remote URL media fetch.
const LARK_REMOTE_UPLOAD_MAX_REDIRECTS: usize = 3;
/// Timeout for host DNS resolution safety checks during remote URL fetch.
const LARK_REMOTE_UPLOAD_DNS_TIMEOUT_SECS: u64 = 3;

/// Returns true when the WebSocket frame indicates live traffic that should
/// refresh the heartbeat watchdog.
fn should_refresh_last_recv(msg: &WsMsg) -> bool {
    matches!(msg, WsMsg::Binary(_) | WsMsg::Ping(_) | WsMsg::Pong(_))
}

#[derive(Debug, Clone)]
struct CachedTenantToken {
    value: String,
    refresh_after: Instant,
}

fn extract_lark_response_code(body: &serde_json::Value) -> Option<i64> {
    body.get("code").and_then(|c| c.as_i64())
}

fn is_lark_invalid_access_token(body: &serde_json::Value) -> bool {
    extract_lark_response_code(body) == Some(LARK_INVALID_ACCESS_TOKEN_CODE)
}

fn should_refresh_lark_tenant_token(status: reqwest::StatusCode, body: &serde_json::Value) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED || is_lark_invalid_access_token(body)
}

fn extract_lark_token_ttl_seconds(body: &serde_json::Value) -> u64 {
    let ttl = body
        .get("expire")
        .or_else(|| body.get("expires_in"))
        .and_then(|v| v.as_u64())
        .or_else(|| {
            body.get("expire")
                .or_else(|| body.get("expires_in"))
                .and_then(|v| v.as_i64())
                .and_then(|v| u64::try_from(v).ok())
        })
        .unwrap_or(LARK_DEFAULT_TOKEN_TTL.as_secs());
    ttl.max(1)
}

fn next_token_refresh_deadline(now: Instant, ttl_seconds: u64) -> Instant {
    let ttl = Duration::from_secs(ttl_seconds.max(1));
    let refresh_in = ttl
        .checked_sub(LARK_TOKEN_REFRESH_SKEW)
        .unwrap_or(Duration::from_secs(1));
    now + refresh_in
}

fn ensure_lark_send_success(
    status: reqwest::StatusCode,
    body: &serde_json::Value,
    context: &str,
) -> anyhow::Result<()> {
    if !status.is_success() {
        anyhow::bail!("Lark send failed {context}: status={status}, body={body}");
    }

    let code = extract_lark_response_code(body).unwrap_or(0);
    if code != 0 {
        anyhow::bail!("Lark send failed {context}: code={code}, body={body}");
    }

    Ok(())
}

/// Lark/Feishu channel.
///
/// Supports two receive modes (configured via `receive_mode` in config):
/// - **`websocket`** (default): persistent WSS long-connection; no public URL needed.
/// - **`webhook`**: HTTP callback server; requires a public HTTPS endpoint.
#[derive(Clone)]
pub struct LarkChannel {
    app_id: String,
    app_secret: String,
    encrypt_key: String,
    verification_token: String,
    port: Option<u16>,
    allowed_users: Vec<String>,
    /// Bot open_id resolved at runtime via `/bot/v3/info`.
    resolved_bot_open_id: Arc<StdRwLock<Option<String>>>,
    mention_only: bool,
    /// When true, use Feishu (CN) endpoints; when false, use Lark (international).
    use_feishu: bool,
    /// How to receive events: WebSocket long-connection or HTTP webhook.
    receive_mode: crate::config::schema::LarkReceiveMode,
    /// Callback route path in webhook mode.
    webhook_path: String,
    /// Whether webhook signature validation is enabled.
    verify_signature: bool,
    /// Maximum accepted timestamp drift for webhook requests.
    verify_timestamp_window_secs: u64,
    /// Ack reaction strategy: "auto"/"none"/custom emoji type.
    ack_reaction: String,
    /// Strategy used when media upload/download fails.
    upload_failure_strategy: crate::config::schema::FeishuUploadFailureStrategy,
    /// Cached tenant access token
    tenant_token: Arc<RwLock<Option<CachedTenantToken>>>,
    /// Dedup set: WS message_ids seen in last ~30 min to prevent double-dispatch
    ws_seen_ids: Arc<RwLock<HashMap<String, Instant>>>,
}

impl LarkChannel {
    pub fn new(
        app_id: String,
        app_secret: String,
        encrypt_key: String,
        verification_token: String,
        port: Option<u16>,
        allowed_users: Vec<String>,
        mention_only: bool,
    ) -> Self {
        Self::new_with_platform(
            app_id,
            app_secret,
            encrypt_key,
            verification_token,
            port,
            allowed_users,
            mention_only,
            LarkPlatform::Lark,
        )
    }

    fn new_with_platform(
        app_id: String,
        app_secret: String,
        encrypt_key: String,
        verification_token: String,
        port: Option<u16>,
        allowed_users: Vec<String>,
        mention_only: bool,
        platform: LarkPlatform,
    ) -> Self {
        let (default_webhook_path, default_verify_signature, default_verify_timestamp_window_secs) =
            match platform {
                LarkPlatform::Lark => (
                    DEFAULT_LARK_WEBHOOK_PATH,
                    DEFAULT_LARK_VERIFY_SIGNATURE,
                    DEFAULT_LARK_VERIFY_TIMESTAMP_WINDOW_SECS,
                ),
                LarkPlatform::Feishu => (
                    DEFAULT_FEISHU_WEBHOOK_PATH,
                    DEFAULT_FEISHU_VERIFY_SIGNATURE,
                    DEFAULT_FEISHU_VERIFY_TIMESTAMP_WINDOW_SECS,
                ),
            };

        Self {
            app_id,
            app_secret,
            encrypt_key,
            verification_token,
            port,
            allowed_users,
            resolved_bot_open_id: Arc::new(StdRwLock::new(None)),
            mention_only,
            use_feishu: matches!(platform, LarkPlatform::Feishu),
            receive_mode: crate::config::schema::LarkReceiveMode::default(),
            webhook_path: default_webhook_path.to_string(),
            verify_signature: default_verify_signature,
            verify_timestamp_window_secs: default_verify_timestamp_window_secs,
            ack_reaction: DEFAULT_FEISHU_ACK_REACTION.to_string(),
            upload_failure_strategy: crate::config::schema::FeishuUploadFailureStrategy::Strict,
            tenant_token: Arc::new(RwLock::new(None)),
            ws_seen_ids: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Build from `LarkConfig` using legacy compatibility:
    /// when `use_feishu=true`, this instance routes to Feishu endpoints.
    pub fn from_config(config: &crate::config::schema::LarkConfig) -> Self {
        let platform = if config.use_feishu {
            LarkPlatform::Feishu
        } else {
            LarkPlatform::Lark
        };
        let mut ch = Self::new_with_platform(
            config.app_id.clone(),
            config.app_secret.clone(),
            config.encrypt_key.clone().unwrap_or_default(),
            config.verification_token.clone().unwrap_or_default(),
            config.port,
            config.allowed_users.clone(),
            config.mention_only,
            platform,
        );
        ch.receive_mode = config.receive_mode.clone();
        ch
    }

    pub fn from_lark_config(config: &crate::config::schema::LarkConfig) -> Self {
        let mut ch = Self::new_with_platform(
            config.app_id.clone(),
            config.app_secret.clone(),
            config.encrypt_key.clone().unwrap_or_default(),
            config.verification_token.clone().unwrap_or_default(),
            config.port,
            config.allowed_users.clone(),
            config.mention_only,
            LarkPlatform::Lark,
        );
        ch.receive_mode = config.receive_mode.clone();
        ch
    }

    pub fn from_feishu_config(config: &crate::config::schema::FeishuConfig) -> Self {
        let mut ch = Self::new_with_platform(
            config.app_id.clone(),
            config.app_secret.clone(),
            config.encrypt_key.clone().unwrap_or_default(),
            config.verification_token.clone().unwrap_or_default(),
            config.port,
            config.allowed_users.clone(),
            config.mention_only,
            LarkPlatform::Feishu,
        );
        ch.receive_mode = config.receive_mode.clone();
        ch.webhook_path = normalize_webhook_path(&config.webhook_path);
        ch.verify_signature = config.verify_signature;
        ch.verify_timestamp_window_secs = config.verify_timestamp_window_secs;
        ch.ack_reaction = normalize_ack_reaction(&config.ack_reaction);
        ch.upload_failure_strategy = config.upload_failure_strategy;
        ch
    }

    fn platform(&self) -> LarkPlatform {
        if self.use_feishu {
            LarkPlatform::Feishu
        } else {
            LarkPlatform::Lark
        }
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client(self.platform().proxy_service_key())
    }

    fn channel_name(&self) -> &'static str {
        self.platform().channel_name()
    }

    fn api_base(&self) -> &'static str {
        self.platform().api_base()
    }

    fn ws_base(&self) -> &'static str {
        self.platform().ws_base()
    }

    fn tenant_access_token_url(&self) -> String {
        format!("{}/auth/v3/tenant_access_token/internal", self.api_base())
    }

    fn bot_info_url(&self) -> String {
        format!("{}/bot/v3/info", self.api_base())
    }

    fn send_message_url(&self) -> String {
        format!("{}/im/v1/messages?receive_id_type=chat_id", self.api_base())
    }

    fn message_reaction_url(&self, message_id: &str) -> String {
        format!("{}/im/v1/messages/{message_id}/reactions", self.api_base())
    }

    fn message_resource_download_url(&self, message_id: &str, file_key: &str) -> String {
        format!(
            "{}/im/v1/messages/{message_id}/resources/{file_key}",
            self.api_base()
        )
    }

    fn resolved_bot_open_id(&self) -> Option<String> {
        self.resolved_bot_open_id
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn set_resolved_bot_open_id(&self, open_id: Option<String>) {
        if let Ok(mut guard) = self.resolved_bot_open_id.write() {
            *guard = open_id;
        }
    }

    async fn post_message_reaction_with_token(
        &self,
        message_id: &str,
        token: &str,
        emoji_type: &str,
    ) -> anyhow::Result<reqwest::Response> {
        let url = self.message_reaction_url(message_id);
        let body = serde_json::json!({
            "reaction_type": {
                "emoji_type": emoji_type
            }
        });

        let response = self
            .http_client()
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json; charset=utf-8")
            .json(&body)
            .send()
            .await?;

        Ok(response)
    }

    /// Best-effort "received" signal for incoming messages.
    /// Failures are logged and never block normal message handling.
    async fn try_add_ack_reaction(&self, message_id: &str, emoji_type: &str) {
        if message_id.is_empty() {
            return;
        }

        let mut token = match self.get_tenant_access_token().await {
            Ok(token) => token,
            Err(err) => {
                tracing::warn!("Lark: failed to fetch token for reaction: {err}");
                return;
            }
        };

        let mut retried = false;
        loop {
            let response = match self
                .post_message_reaction_with_token(message_id, &token, emoji_type)
                .await
            {
                Ok(resp) => resp,
                Err(err) => {
                    tracing::warn!("Lark: failed to add reaction for {message_id}: {err}");
                    return;
                }
            };

            if response.status().as_u16() == 401 && !retried {
                self.invalidate_token().await;
                token = match self.get_tenant_access_token().await {
                    Ok(new_token) => new_token,
                    Err(err) => {
                        tracing::warn!(
                            "Lark: failed to refresh token for reaction on {message_id}: {err}"
                        );
                        return;
                    }
                };
                retried = true;
                continue;
            }

            if !response.status().is_success() {
                let status = response.status();
                let err_body = response.text().await.unwrap_or_default();
                tracing::warn!(
                    "Lark: add reaction failed for {message_id}: status={status}, body={err_body}"
                );
                return;
            }

            let payload: serde_json::Value = match response.json().await {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!("Lark: add reaction decode failed for {message_id}: {err}");
                    return;
                }
            };

            let code = payload.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
            if code != 0 {
                let msg = payload
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                tracing::warn!("Lark: add reaction returned code={code} for {message_id}: {msg}");
            }
            return;
        }
    }

    /// POST /callback/ws/endpoint → (wss_url, client_config)
    async fn get_ws_endpoint(&self) -> anyhow::Result<(String, WsClientConfig)> {
        let resp = self
            .http_client()
            .post(format!("{}/callback/ws/endpoint", self.ws_base()))
            .header("locale", self.platform().locale_header())
            .json(&serde_json::json!({
                "AppID": self.app_id,
                "AppSecret": self.app_secret,
            }))
            .send()
            .await?
            .json::<WsEndpointResp>()
            .await?;
        if resp.code != 0 {
            anyhow::bail!(
                "Lark WS endpoint failed: code={} msg={}",
                resp.code,
                resp.msg.as_deref().unwrap_or("(none)")
            );
        }
        let ep = resp
            .data
            .ok_or_else(|| anyhow::anyhow!("Lark WS endpoint: empty data"))?;
        Ok((ep.url, ep.client_config.unwrap_or_default()))
    }

    /// WS long-connection event loop.  Returns Ok(()) when the connection closes
    /// (the caller reconnects).
    #[allow(clippy::too_many_lines)]
    async fn listen_ws(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        self.ensure_bot_open_id().await;
        let (wss_url, client_config) = self.get_ws_endpoint().await?;
        let service_id = wss_url
            .split('?')
            .nth(1)
            .and_then(|qs| {
                qs.split('&')
                    .find(|kv| kv.starts_with("service_id="))
                    .and_then(|kv| kv.split('=').nth(1))
                    .and_then(|v| v.parse::<i32>().ok())
            })
            .unwrap_or(0);
        tracing::info!("Lark: connecting to {wss_url}");

        let (ws_stream, _) = tokio_tungstenite::connect_async(&wss_url).await?;
        let (mut write, mut read) = ws_stream.split();
        tracing::info!("Lark: WS connected (service_id={service_id})");

        let mut ping_secs = client_config.ping_interval.unwrap_or(120).max(10);
        let mut hb_interval = tokio::time::interval(Duration::from_secs(ping_secs));
        let mut timeout_check = tokio::time::interval(Duration::from_secs(10));
        hb_interval.tick().await; // consume immediate tick

        let mut seq: u64 = 0;
        let mut last_recv = Instant::now();

        // Send initial ping immediately (like the official SDK) so the server
        // starts responding with pongs and we can calibrate the ping_interval.
        seq = seq.wrapping_add(1);
        let initial_ping = PbFrame {
            seq_id: seq,
            log_id: 0,
            service: service_id,
            method: 0,
            headers: vec![PbHeader {
                key: "type".into(),
                value: "ping".into(),
            }],
            payload: None,
        };
        if write
            .send(WsMsg::Binary(initial_ping.encode_to_vec().into()))
            .await
            .is_err()
        {
            anyhow::bail!("Lark: initial ping failed");
        }
        // message_id → (fragment_slots, created_at) for multi-part reassembly
        type FragEntry = (Vec<Option<Vec<u8>>>, Instant);
        let mut frag_cache: HashMap<String, FragEntry> = HashMap::new();

        loop {
            tokio::select! {
                biased;

                _ = hb_interval.tick() => {
                    seq = seq.wrapping_add(1);
                    let ping = PbFrame {
                        seq_id: seq, log_id: 0, service: service_id, method: 0,
                        headers: vec![PbHeader { key: "type".into(), value: "ping".into() }],
                        payload: None,
                    };
                    if write.send(WsMsg::Binary(ping.encode_to_vec().into())).await.is_err() {
                        tracing::warn!("Lark: ping failed, reconnecting");
                        break;
                    }
                    // GC stale fragments > 5 min
                    let cutoff = Instant::now().checked_sub(Duration::from_secs(300)).unwrap_or(Instant::now());
                    frag_cache.retain(|_, (_, ts)| *ts > cutoff);
                }

                _ = timeout_check.tick() => {
                    if last_recv.elapsed() > WS_HEARTBEAT_TIMEOUT {
                        tracing::warn!("Lark: heartbeat timeout, reconnecting");
                        break;
                    }
                }

                msg = read.next() => {
                    let raw = match msg {
                        Some(Ok(ws_msg)) => {
                            if should_refresh_last_recv(&ws_msg) {
                                last_recv = Instant::now();
                            }
                            match ws_msg {
                                WsMsg::Binary(b) => b,
                                WsMsg::Ping(d) => { let _ = write.send(WsMsg::Pong(d)).await; continue; }
                                WsMsg::Close(_) => { tracing::info!("Lark: WS closed — reconnecting"); break; }
                                _ => continue,
                            }
                        }
                        None => { tracing::info!("Lark: WS closed — reconnecting"); break; }
                        Some(Err(e)) => { tracing::error!("Lark: WS read error: {e}"); break; }
                    };

                    let frame = match PbFrame::decode(&raw[..]) {
                        Ok(f) => f,
                        Err(e) => { tracing::error!("Lark: proto decode: {e}"); continue; }
                    };

                    // CONTROL frame
                    if frame.method == 0 {
                        if frame.header_value("type") == "pong" {
                            if let Some(p) = &frame.payload {
                                if let Ok(cfg) = serde_json::from_slice::<WsClientConfig>(p) {
                                    if let Some(secs) = cfg.ping_interval {
                                        let secs = secs.max(10);
                                        if secs != ping_secs {
                                            ping_secs = secs;
                                            hb_interval = tokio::time::interval(Duration::from_secs(ping_secs));
                                            tracing::info!("Lark: ping_interval → {ping_secs}s");
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }

                    // DATA frame
                    let msg_type = frame.header_value("type").to_string();
                    let msg_id   = frame.header_value("message_id").to_string();
                    let sum      = frame.header_value("sum").parse::<usize>().unwrap_or(1);
                    let seq_num  = frame.header_value("seq").parse::<usize>().unwrap_or(0);

                    // ACK immediately (Feishu requires within 3 s)
                    {
                        let mut ack = frame.clone();
                        ack.payload = Some(br#"{"code":200,"headers":{},"data":[]}"#.to_vec());
                        ack.headers.push(PbHeader { key: "biz_rt".into(), value: "0".into() });
                        let _ = write.send(WsMsg::Binary(ack.encode_to_vec().into())).await;
                    }

                    // Fragment reassembly
                    let sum = if sum == 0 { 1 } else { sum };
                    let payload: Vec<u8> = if sum == 1 || msg_id.is_empty() || seq_num >= sum {
                        frame.payload.clone().unwrap_or_default()
                    } else {
                        let entry = frag_cache.entry(msg_id.clone())
                            .or_insert_with(|| (vec![None; sum], Instant::now()));
                        if entry.0.len() != sum { *entry = (vec![None; sum], Instant::now()); }
                        entry.0[seq_num] = frame.payload.clone();
                        if entry.0.iter().all(|s| s.is_some()) {
                            let full: Vec<u8> = entry.0.iter()
                                .flat_map(|s| s.as_deref().unwrap_or(&[]))
                                .copied().collect();
                            frag_cache.remove(&msg_id);
                            full
                        } else { continue; }
                    };

                    if msg_type != "event" { continue; }

                    let event: LarkEvent = match serde_json::from_slice(&payload) {
                        Ok(e) => e,
                        Err(e) => { tracing::error!("Lark: event JSON: {e}"); continue; }
                    };
                    if event.header.event_type != "im.message.receive_v1" { continue; }

                    let event_payload = event.event;

                    let recv: MsgReceivePayload = match serde_json::from_value(event_payload.clone()) {
                        Ok(r) => r,
                        Err(e) => { tracing::error!("Lark: payload parse: {e}"); continue; }
                    };

                    if recv.sender.sender_type == "app" || recv.sender.sender_type == "bot" { continue; }

                    let sender_open_id = recv.sender.sender_id.open_id.as_deref().unwrap_or("");
                    if !self.is_user_allowed(sender_open_id) {
                        tracing::warn!("Lark WS: ignoring {sender_open_id} (not in allowed_users)");
                        continue;
                    }

                    let lark_msg = &recv.message;

                    // Dedup
                    {
                        let now = Instant::now();
                        let mut seen = self.ws_seen_ids.write().await;
                        // GC
                        seen.retain(|_, t| now.duration_since(*t) < Duration::from_secs(30 * 60));
                        if seen.contains_key(&lark_msg.message_id) {
                            tracing::debug!("Lark WS: dup {}", lark_msg.message_id);
                            continue;
                        }
                        seen.insert(lark_msg.message_id.clone(), now);
                    }

                    // Decode content by type.
                    let (text, post_mentioned_open_ids) =
                        match parse_lark_message_content(&lark_msg.message_type, &lark_msg.content)
                        {
                            Some(value) => value,
                            None => {
                                tracing::debug!(
                                    "Lark WS: skipping unsupported type '{}'",
                                    lark_msg.message_type
                                );
                                continue;
                            }
                        };

                    // Strip @_user_N placeholders
                    let text = strip_at_placeholders(&text);
                    let text = text.trim().to_string();
                    let text = self
                        .resolve_incoming_media_markers(&text, Some(&lark_msg.message_id))
                        .await;
                    if text.is_empty() {
                        continue;
                    }

                    // Group-chat: only respond when explicitly @-mentioned
                    let bot_open_id = self.resolved_bot_open_id();
                    if lark_msg.chat_type == "group"
                        && !should_respond_in_group(
                            self.mention_only,
                            bot_open_id.as_deref(),
                            &lark_msg.mentions,
                            &post_mentioned_open_ids,
                        )
                    {
                        continue;
                    }

                    if let Some(ack_emoji) = self.select_ack_reaction(Some(&event_payload), &text) {
                        let reaction_channel = self.clone();
                        let reaction_message_id = lark_msg.message_id.clone();
                        tokio::spawn(async move {
                            reaction_channel
                                .try_add_ack_reaction(&reaction_message_id, &ack_emoji)
                                .await;
                        });
                    }

                    let channel_msg = ChannelMessage {
                        id: Uuid::new_v4().to_string(),
                        sender: lark_msg.chat_id.clone(),
                        reply_target: lark_msg.chat_id.clone(),
                        content: text,
                        channel: self.channel_name().to_string(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        thread_ts: None,
                    };

                    tracing::debug!("Lark WS: message in {}", lark_msg.chat_id);
                    if tx.send(channel_msg).await.is_err() { break; }
                }
            }
        }
        Ok(())
    }

    /// Check if a user open_id is allowed
    fn is_user_allowed(&self, open_id: &str) -> bool {
        self.allowed_users.iter().any(|u| u == "*" || u == open_id)
    }

    /// Get or refresh tenant access token
    async fn get_tenant_access_token(&self) -> anyhow::Result<String> {
        // Check cache first
        {
            let cached = self.tenant_token.read().await;
            if let Some(ref token) = *cached {
                if Instant::now() < token.refresh_after {
                    return Ok(token.value.clone());
                }
            }
        }

        let url = self.tenant_access_token_url();
        let body = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
        });

        let resp = self.http_client().post(&url).json(&body).send().await?;
        let status = resp.status();
        let data: serde_json::Value = resp.json().await?;

        if !status.is_success() {
            anyhow::bail!("Lark tenant_access_token request failed: status={status}, body={data}");
        }

        let code = data.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = data
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("Lark tenant_access_token failed: {msg}");
        }

        let token = data
            .get("tenant_access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing tenant_access_token in response"))?
            .to_string();

        let ttl_seconds = extract_lark_token_ttl_seconds(&data);
        let refresh_after = next_token_refresh_deadline(Instant::now(), ttl_seconds);

        // Cache it with proactive refresh metadata.
        {
            let mut cached = self.tenant_token.write().await;
            *cached = Some(CachedTenantToken {
                value: token.clone(),
                refresh_after,
            });
        }

        Ok(token)
    }

    /// Invalidate cached token (called when API reports an expired tenant token).
    async fn invalidate_token(&self) {
        let mut cached = self.tenant_token.write().await;
        *cached = None;
    }

    async fn fetch_bot_open_id_with_token(
        &self,
        token: &str,
    ) -> anyhow::Result<(reqwest::StatusCode, serde_json::Value)> {
        let resp = self
            .http_client()
            .get(self.bot_info_url())
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await?;
        let status = resp.status();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .unwrap_or_else(|_| serde_json::json!({}));
        Ok((status, body))
    }

    async fn refresh_bot_open_id(&self) -> anyhow::Result<Option<String>> {
        let token = self.get_tenant_access_token().await?;
        let (status, body) = self.fetch_bot_open_id_with_token(&token).await?;

        let body = if should_refresh_lark_tenant_token(status, &body) {
            self.invalidate_token().await;
            let refreshed = self.get_tenant_access_token().await?;
            let (retry_status, retry_body) = self.fetch_bot_open_id_with_token(&refreshed).await?;
            if !retry_status.is_success() {
                anyhow::bail!(
                    "Lark bot info request failed after token refresh: status={retry_status}, body={retry_body}"
                );
            }
            retry_body
        } else {
            if !status.is_success() {
                anyhow::bail!("Lark bot info request failed: status={status}, body={body}");
            }
            body
        };

        let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            anyhow::bail!("Lark bot info failed: code={code}, body={body}");
        }

        let bot_open_id = body
            .pointer("/bot/open_id")
            .or_else(|| body.pointer("/data/bot/open_id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);

        self.set_resolved_bot_open_id(bot_open_id.clone());
        Ok(bot_open_id)
    }

    async fn ensure_bot_open_id(&self) {
        if !self.mention_only || self.resolved_bot_open_id().is_some() {
            return;
        }

        match self.refresh_bot_open_id().await {
            Ok(Some(open_id)) => {
                tracing::info!("Lark: resolved bot open_id: {open_id}");
            }
            Ok(None) => {
                tracing::warn!(
                    "Lark: bot open_id missing from /bot/v3/info response; mention_only group messages will be ignored"
                );
            }
            Err(err) => {
                tracing::warn!(
                    "Lark: failed to resolve bot open_id: {err}; mention_only group messages will be ignored"
                );
            }
        }
    }

    fn select_ack_reaction(
        &self,
        payload: Option<&serde_json::Value>,
        fallback_text: &str,
    ) -> Option<String> {
        let strategy = self.ack_reaction.trim();
        if strategy.is_empty() || strategy.eq_ignore_ascii_case("auto") {
            return Some(random_lark_ack_reaction(payload, fallback_text).to_string());
        }
        if strategy.eq_ignore_ascii_case("none")
            || strategy.eq_ignore_ascii_case("off")
            || strategy.eq_ignore_ascii_case("disabled")
            || strategy.eq_ignore_ascii_case("false")
            || strategy == "0"
        {
            return None;
        }
        Some(strategy.to_string())
    }

    async fn send_text_once(
        &self,
        url: &str,
        token: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<(reqwest::StatusCode, serde_json::Value)> {
        let resp = self
            .http_client()
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json; charset=utf-8")
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        let raw = resp.text().await.unwrap_or_default();
        let parsed = serde_json::from_str::<serde_json::Value>(&raw)
            .unwrap_or_else(|_| serde_json::json!({ "raw": raw }));
        Ok((status, parsed))
    }

    fn image_download_url(&self, image_key: &str) -> String {
        format!("{}/im/v1/images/{image_key}", self.api_base())
    }

    async fn download_lark_image_via_images_api(
        &self,
        image_key: &str,
    ) -> anyhow::Result<(String, Vec<u8>)> {
        let mut token = self.get_tenant_access_token().await?;
        let mut retried = false;

        loop {
            let response = self
                .http_client()
                .get(self.image_download_url(image_key))
                .query(&[("image_type", "message")])
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await?;

            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !retried {
                self.invalidate_token().await;
                token = self.get_tenant_access_token().await?;
                retried = true;
                continue;
            }

            let status = response.status();
            if !status.is_success() {
                let err_body = response.text().await.unwrap_or_default();
                anyhow::bail!(
                    "Lark image download failed: status={status}, image_key={image_key}, body={err_body}"
                );
            }

            if let Some(content_len) = response.content_length() {
                if content_len > LARK_REMOTE_UPLOAD_MAX_BYTES as u64 {
                    anyhow::bail!(
                        "Lark image download exceeds max size ({} bytes), image_key={image_key}",
                        LARK_REMOTE_UPLOAD_MAX_BYTES
                    );
                }
            }

            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .map(str::trim)
                .filter(|value| value.starts_with("image/"))
                .map(str::to_string);

            let bytes = response.bytes().await?;
            if bytes.len() > LARK_REMOTE_UPLOAD_MAX_BYTES {
                anyhow::bail!(
                    "Lark image download exceeds max size ({} bytes), image_key={image_key}",
                    LARK_REMOTE_UPLOAD_MAX_BYTES
                );
            }

            let mime = content_type
                .unwrap_or_else(|| detect_lark_image_mime_from_magic(bytes.as_ref()).to_string());
            return Ok((mime, bytes.to_vec()));
        }
    }

    async fn download_lark_image_via_message_resource_api(
        &self,
        message_id: &str,
        image_key: &str,
    ) -> anyhow::Result<(String, Vec<u8>)> {
        let mut token = self.get_tenant_access_token().await?;
        let mut retried = false;

        loop {
            let response = self
                .http_client()
                .get(self.message_resource_download_url(message_id, image_key))
                .query(&[("type", "image")])
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await?;

            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !retried {
                self.invalidate_token().await;
                token = self.get_tenant_access_token().await?;
                retried = true;
                continue;
            }

            let status = response.status();
            if !status.is_success() {
                let err_body = response.text().await.unwrap_or_default();
                anyhow::bail!(
                    "Lark message resource image download failed: status={status}, message_id={message_id}, image_key={image_key}, body={err_body}"
                );
            }

            if let Some(content_len) = response.content_length() {
                if content_len > LARK_REMOTE_UPLOAD_MAX_BYTES as u64 {
                    anyhow::bail!(
                        "Lark message resource image download exceeds max size ({} bytes), message_id={message_id}, image_key={image_key}",
                        LARK_REMOTE_UPLOAD_MAX_BYTES
                    );
                }
            }

            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .map(str::trim)
                .filter(|value| value.starts_with("image/"))
                .map(str::to_string);

            let bytes = response.bytes().await?;
            if bytes.len() > LARK_REMOTE_UPLOAD_MAX_BYTES {
                anyhow::bail!(
                    "Lark message resource image download exceeds max size ({} bytes), message_id={message_id}, image_key={image_key}",
                    LARK_REMOTE_UPLOAD_MAX_BYTES
                );
            }

            let mime = content_type
                .unwrap_or_else(|| detect_lark_image_mime_from_magic(bytes.as_ref()).to_string());
            return Ok((mime, bytes.to_vec()));
        }
    }

    async fn download_lark_image_as_data_uri(
        &self,
        image_key: &str,
        message_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let direct_result = self.download_lark_image_via_images_api(image_key).await;
        let (mime, bytes) = match direct_result {
            Ok(ok) => ok,
            Err(images_api_error) => {
                let Some(msg_id) = message_id.filter(|value| !value.trim().is_empty()) else {
                    return Err(images_api_error);
                };
                tracing::warn!(
                    "Lark: image download via /im/v1/images failed; trying /im/v1/messages/{{message_id}}/resources fallback: {images_api_error}"
                );
                match self
                    .download_lark_image_via_message_resource_api(msg_id, image_key)
                    .await
                {
                    Ok(ok) => ok,
                    Err(resource_api_error) => {
                        anyhow::bail!(
                            "Lark image download failed on both APIs: images_api_error={images_api_error}; resource_api_error={resource_api_error}"
                        );
                    }
                }
            }
        };

        use base64::Engine;
        let payload = base64::engine::general_purpose::STANDARD.encode(bytes);
        Ok(format!("data:{mime};base64,{payload}"))
    }

    async fn resolve_incoming_media_markers(
        &self,
        content: &str,
        message_id: Option<&str>,
    ) -> String {
        let Some(attachment) = parse_lark_single_outgoing_attachment(content) else {
            return content.to_string();
        };

        let fallback_text = build_incoming_lark_attachment_fallback_text(
            self.channel_name(),
            attachment.kind,
            &attachment.target,
        );

        if attachment.kind == LarkOutgoingAttachmentKind::Image {
            let image_key = attachment.target.trim();
            if !is_lark_platform_image_key(image_key) {
                return content.to_string();
            }

            return match self
                .download_lark_image_as_data_uri(image_key, message_id)
                .await
            {
                Ok(data_uri) => format!("[IMAGE:{data_uri}]"),
                Err(err) => {
                    tracing::warn!(
                        "Lark: failed to resolve incoming image key as binary payload (fallback to text): {err}"
                    );
                    fallback_text
                }
            };
        }

        fallback_text
    }

    fn remote_upload_http_client(&self) -> reqwest::Client {
        let builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(LARK_REMOTE_UPLOAD_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(LARK_REMOTE_UPLOAD_DNS_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::none());
        let builder = crate::config::apply_runtime_proxy_to_builder(
            builder,
            self.platform().proxy_service_key(),
        );
        builder.build().unwrap_or_else(|error| {
            tracing::warn!("Lark: failed to build remote upload client: {error}");
            self.http_client()
        })
    }

    fn handle_upload_failure(
        &self,
        recipient: &str,
        content: &str,
        err: anyhow::Error,
    ) -> anyhow::Result<serde_json::Value> {
        match self.upload_failure_strategy {
            crate::config::schema::FeishuUploadFailureStrategy::Strict => Err(err),
            crate::config::schema::FeishuUploadFailureStrategy::FallbackText => {
                tracing::warn!(
                    "Lark: media upload failed, fallback to text because upload_failure_strategy=fallback_text: {err}"
                );
                Ok(build_lark_text_send_body(recipient, content))
            }
        }
    }

    async fn upload_lark_image_with_token(
        &self,
        token: &str,
        source: LarkUploadSource,
    ) -> anyhow::Result<String> {
        let mut part = reqwest::multipart::Part::bytes(source.bytes).file_name(source.filename);
        if let Some(mime) = source.mime.as_deref() {
            part = part
                .mime_str(mime)
                .map_err(|e| anyhow::anyhow!("Lark image MIME build failed: {e}"))?;
        }
        let form = reqwest::multipart::Form::new()
            .text("image_type", "message")
            .part("image", part);

        let url = format!("{}/im/v1/images", self.api_base());
        let resp = self
            .http_client()
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_else(|_| serde_json::json!({}));

        if !status.is_success() {
            anyhow::bail!("Lark image upload failed: status={status}, body={body}");
        }
        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            anyhow::bail!("Lark image upload failed: code={code}, body={body}");
        }
        let image_key = body
            .pointer("/data/image_key")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("Lark image upload missing data.image_key: body={body}")
            })?;
        Ok(image_key.to_string())
    }

    async fn upload_lark_file_with_token(
        &self,
        token: &str,
        source: LarkUploadSource,
    ) -> anyhow::Result<String> {
        let mut part =
            reqwest::multipart::Part::bytes(source.bytes).file_name(source.filename.clone());
        if let Some(mime) = source.mime.as_deref() {
            part = part
                .mime_str(mime)
                .map_err(|e| anyhow::anyhow!("Lark file MIME build failed: {e}"))?;
        }
        let form = reqwest::multipart::Form::new()
            .text("file_type", "stream")
            .text("file_name", source.filename)
            .part("file", part);

        let url = format!("{}/im/v1/files", self.api_base());
        let resp = self
            .http_client()
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_else(|_| serde_json::json!({}));

        if !status.is_success() {
            anyhow::bail!("Lark file upload failed: status={status}, body={body}");
        }
        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            anyhow::bail!("Lark file upload failed: code={code}, body={body}");
        }
        let file_key = body
            .pointer("/data/file_key")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("Lark file upload missing data.file_key: body={body}")
            })?;
        Ok(file_key.to_string())
    }

    async fn read_lark_upload_source_from_file(
        &self,
        kind: LarkOutgoingAttachmentKind,
        file_path: &Path,
    ) -> anyhow::Result<LarkUploadSource> {
        let bytes = tokio::fs::read(file_path).await.map_err(|e| {
            anyhow::anyhow!("Lark file read failed for '{}': {e}", file_path.display())
        })?;
        let default_name = default_lark_upload_filename(kind);
        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_name.to_string());
        let mime = Some(
            mime_guess::from_path(file_path)
                .first_or_octet_stream()
                .to_string(),
        );
        Ok(LarkUploadSource {
            bytes,
            filename,
            mime,
        })
    }

    async fn fetch_lark_upload_source_from_url(
        &self,
        kind: LarkOutgoingAttachmentKind,
        target_url: &str,
    ) -> anyhow::Result<LarkUploadSource> {
        let mut parsed = reqwest::Url::parse(target_url)
            .map_err(|e| anyhow::anyhow!("invalid media URL '{target_url}': {e}"))?;
        let client = self.remote_upload_http_client();

        let mut response = None;
        for hop in 0..=LARK_REMOTE_UPLOAD_MAX_REDIRECTS {
            if parsed.scheme() != "https" {
                anyhow::bail!("only https media URLs are allowed for Lark upload");
            }
            let host = parsed
                .host_str()
                .ok_or_else(|| anyhow::anyhow!("media URL host is required"))?;
            ensure_lark_remote_upload_host_is_public(host).await?;

            let resp = client.get(parsed.clone()).send().await?;
            let status = resp.status();
            if status.is_redirection() {
                if hop >= LARK_REMOTE_UPLOAD_MAX_REDIRECTS {
                    anyhow::bail!("too many redirects for media URL download");
                }
                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| anyhow::anyhow!("redirect response missing location header"))?;
                parsed = parsed
                    .join(location)
                    .map_err(|e| anyhow::anyhow!("invalid redirect location '{location}': {e}"))?;
                continue;
            }

            if !status.is_success() {
                anyhow::bail!("failed to download media URL for Lark upload: status={status}");
            }
            response = Some(resp);
            break;
        }
        let resp = response.ok_or_else(|| anyhow::anyhow!("failed to fetch media URL response"))?;

        if let Some(content_len) = resp.content_length() {
            if content_len > LARK_REMOTE_UPLOAD_MAX_BYTES as u64 {
                anyhow::bail!(
                    "media URL exceeds max upload size ({} bytes)",
                    LARK_REMOTE_UPLOAD_MAX_BYTES
                );
            }
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);

        let bytes = tokio::time::timeout(
            Duration::from_secs(LARK_REMOTE_UPLOAD_TIMEOUT_SECS),
            resp.bytes(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timeout downloading media URL for Lark upload"))?
        .map_err(|e| anyhow::anyhow!("failed to read media URL response: {e}"))?;

        if bytes.len() > LARK_REMOTE_UPLOAD_MAX_BYTES {
            anyhow::bail!(
                "media URL exceeds max upload size ({} bytes)",
                LARK_REMOTE_UPLOAD_MAX_BYTES
            );
        }

        let filename = infer_lark_upload_filename_from_url(&parsed, kind);
        Ok(LarkUploadSource {
            bytes: bytes.to_vec(),
            filename,
            mime: content_type,
        })
    }

    async fn build_lark_send_body_with_uploads(
        &self,
        recipient: &str,
        content: &str,
        token: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let Some(attachment) = parse_lark_single_outgoing_attachment(content) else {
            return Ok(build_lark_send_body(recipient, content));
        };

        if let Some(key) = normalize_lark_media_key_for_send(attachment.kind, &attachment.target) {
            return Ok(build_lark_media_send_body_with_key(
                recipient,
                attachment.kind,
                &key,
            ));
        }

        let source = if let Some(local_path) = parse_lark_local_file_target(&attachment.target) {
            match self
                .read_lark_upload_source_from_file(attachment.kind, &local_path)
                .await
            {
                Ok(source) => source,
                Err(err) => return self.handle_upload_failure(recipient, content, err),
            }
        } else if let Some(remote_url) = parse_lark_remote_url_target(&attachment.target) {
            match self
                .fetch_lark_upload_source_from_url(attachment.kind, &remote_url)
                .await
            {
                Ok(source) => source,
                Err(err) => return self.handle_upload_failure(recipient, content, err),
            }
        } else {
            return Ok(build_lark_send_body(recipient, content));
        };

        let uploaded_key = match attachment.kind {
            LarkOutgoingAttachmentKind::Image => {
                match self.upload_lark_image_with_token(token, source).await {
                    Ok(key) => key,
                    Err(err) => return self.handle_upload_failure(recipient, content, err),
                }
            }
            LarkOutgoingAttachmentKind::File
            | LarkOutgoingAttachmentKind::Video
            | LarkOutgoingAttachmentKind::Audio => {
                match self.upload_lark_file_with_token(token, source).await {
                    Ok(key) => key,
                    Err(err) => return self.handle_upload_failure(recipient, content, err),
                }
            }
        };

        Ok(build_lark_media_send_body_with_key(
            recipient,
            attachment.kind,
            &uploaded_key,
        ))
    }

    /// Parse an event callback payload and extract text messages
    pub fn parse_event_payload(&self, payload: &serde_json::Value) -> Vec<ChannelMessage> {
        let mut messages = Vec::new();

        // Lark event v2 structure:
        // { "header": { "event_type": "im.message.receive_v1" }, "event": { "message": { ... }, "sender": { ... } } }
        let event_type = payload
            .pointer("/header/event_type")
            .and_then(|e| e.as_str())
            .unwrap_or("");

        if event_type != "im.message.receive_v1" {
            return messages;
        }

        let event = match payload.get("event") {
            Some(e) => e,
            None => return messages,
        };

        // Extract sender open_id
        let open_id = event
            .pointer("/sender/sender_id/open_id")
            .and_then(|s| s.as_str())
            .unwrap_or("");

        if open_id.is_empty() {
            return messages;
        }

        // Check allowlist
        if !self.is_user_allowed(open_id) {
            tracing::warn!("Lark: ignoring message from unauthorized user: {open_id}");
            return messages;
        }

        // Extract message content (text/post and selected media/location markers)
        let msg_type = event
            .pointer("/message/message_type")
            .and_then(|t| t.as_str())
            .unwrap_or("");

        let chat_type = event
            .pointer("/message/chat_type")
            .and_then(|c| c.as_str())
            .unwrap_or("");

        let mentions = event
            .pointer("/message/mentions")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();

        let content_str = event
            .pointer("/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or("");

        let (text, post_mentioned_open_ids): (String, Vec<String>) =
            match parse_lark_message_content(msg_type, content_str) {
                Some(parsed) => parsed,
                None => {
                    tracing::debug!("Lark: skipping unsupported message type: {msg_type}");
                    return messages;
                }
            };

        let text = strip_at_placeholders(&text);
        let text = text.trim().to_string();
        if text.is_empty() {
            return messages;
        }

        let bot_open_id = self.resolved_bot_open_id();
        if chat_type == "group"
            && !should_respond_in_group(
                self.mention_only,
                bot_open_id.as_deref(),
                &mentions,
                &post_mentioned_open_ids,
            )
        {
            return messages;
        }

        let timestamp = event
            .pointer("/message/create_time")
            .and_then(|t| t.as_str())
            .and_then(|t| t.parse::<u64>().ok())
            // Lark timestamps are in milliseconds
            .map(|ms| ms / 1000)
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            });

        let chat_id = event
            .pointer("/message/chat_id")
            .and_then(|c| c.as_str())
            .unwrap_or(open_id);

        messages.push(ChannelMessage {
            id: Uuid::new_v4().to_string(),
            sender: chat_id.to_string(),
            reply_target: chat_id.to_string(),
            content: text,
            channel: self.channel_name().to_string(),
            timestamp,
            thread_ts: None,
        });

        messages
    }
}

#[async_trait]
impl Channel for LarkChannel {
    fn name(&self) -> &str {
        self.channel_name()
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let token = self.get_tenant_access_token().await?;
        let url = self.send_message_url();
        let body = self
            .build_lark_send_body_with_uploads(&message.recipient, &message.content, &token)
            .await?;

        let (status, response) = self.send_text_once(&url, &token, &body).await?;

        if should_refresh_lark_tenant_token(status, &response) {
            // Token expired/invalid, invalidate and retry once.
            self.invalidate_token().await;
            let new_token = self.get_tenant_access_token().await?;
            let retry_body = self
                .build_lark_send_body_with_uploads(&message.recipient, &message.content, &new_token)
                .await?;
            let (retry_status, retry_response) =
                self.send_text_once(&url, &new_token, &retry_body).await?;

            if should_refresh_lark_tenant_token(retry_status, &retry_response) {
                anyhow::bail!(
                    "Lark send failed after token refresh: status={retry_status}, body={retry_response}"
                );
            }

            ensure_lark_send_success(retry_status, &retry_response, "after token refresh")?;
            return Ok(());
        }

        ensure_lark_send_success(status, &response, "without token refresh")?;
        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        use crate::config::schema::LarkReceiveMode;
        match self.receive_mode {
            LarkReceiveMode::Websocket => self.listen_ws(tx).await,
            LarkReceiveMode::Webhook => self.listen_http(tx).await,
        }
    }

    async fn health_check(&self) -> bool {
        self.get_tenant_access_token().await.is_ok()
    }
}

impl LarkChannel {
    /// HTTP callback server (legacy — requires a public endpoint).
    /// Use `listen()` (WS long-connection) for new deployments.
    pub async fn listen_http(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
    ) -> anyhow::Result<()> {
        self.ensure_bot_open_id().await;
        use axum::{body::Bytes, extract::State, http::HeaderMap, routing::post, Json, Router};

        #[derive(Clone)]
        struct AppState {
            verification_token: String,
            app_secret: String,
            encrypt_key: String,
            verify_signature: bool,
            verify_timestamp_window_secs: u64,
            channel: Arc<LarkChannel>,
            tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        }

        async fn handle_event(
            State(state): State<AppState>,
            headers: HeaderMap,
            body: Bytes,
        ) -> axum::response::Response {
            use axum::http::StatusCode;
            use axum::response::IntoResponse;

            let timestamp = headers
                .get("X-Lark-Request-Timestamp")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            let nonce = headers
                .get("X-Lark-Request-Nonce")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if !verify_lark_request_timestamp(
                timestamp,
                state.verify_timestamp_window_secs,
                now_secs,
            ) {
                return (StatusCode::FORBIDDEN, "invalid request timestamp").into_response();
            }

            if state.verify_signature {
                let signature = headers
                    .get("X-Lark-Signature")
                    .or_else(|| headers.get("X-Lark-Request-Signature"))
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();
                let signature_ok = if state.encrypt_key.trim().is_empty() {
                    verify_lark_webhook_signature_legacy(
                        &state.app_secret,
                        timestamp,
                        &body,
                        signature,
                    )
                } else {
                    verify_lark_webhook_signature(
                        &state.encrypt_key,
                        timestamp,
                        nonce,
                        &body,
                        signature,
                    )
                };
                if !signature_ok {
                    return (StatusCode::FORBIDDEN, "invalid request signature").into_response();
                }
            }

            let raw_payload = match serde_json::from_slice::<serde_json::Value>(&body) {
                Ok(value) => value,
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, "invalid json payload").into_response();
                }
            };

            let payload = match decode_lark_callback_payload(&raw_payload, &state.encrypt_key) {
                Ok(value) => value,
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, "invalid encrypted payload").into_response();
                }
            };

            if !verify_lark_payload_token(&payload, &state.verification_token) {
                return (StatusCode::FORBIDDEN, "invalid token").into_response();
            }

            // URL verification challenge
            if let Some(challenge) = payload.get("challenge").and_then(|c| c.as_str()) {
                let resp = serde_json::json!({ "challenge": challenge });
                return (StatusCode::OK, Json(resp)).into_response();
            }

            // Parse event messages
            let messages = state.channel.parse_event_payload(&payload);
            if !messages.is_empty() {
                if let Some(message_id) = payload
                    .pointer("/event/message/message_id")
                    .and_then(|m| m.as_str())
                {
                    let ack_text = messages.first().map_or("", |msg| msg.content.as_str());
                    if let Some(ack_emoji) = state
                        .channel
                        .select_ack_reaction(payload.get("event"), ack_text)
                    {
                        let reaction_channel = Arc::clone(&state.channel);
                        let reaction_message_id = message_id.to_string();
                        tokio::spawn(async move {
                            reaction_channel
                                .try_add_ack_reaction(&reaction_message_id, &ack_emoji)
                                .await;
                        });
                    }
                }
            }

            let event_message_id = payload
                .pointer("/event/message/message_id")
                .and_then(|m| m.as_str())
                .map(str::to_string);

            for msg in messages {
                let mut msg = msg;
                msg.content = state
                    .channel
                    .resolve_incoming_media_markers(&msg.content, event_message_id.as_deref())
                    .await;
                if state.tx.send(msg).await.is_err() {
                    tracing::warn!("Lark: message channel closed");
                    break;
                }
            }

            (StatusCode::OK, "ok").into_response()
        }

        let port = self.port.ok_or_else(|| {
            anyhow::anyhow!("Lark webhook mode requires `port` to be set in [channels_config.lark]")
        })?;

        let state = AppState {
            verification_token: self.verification_token.clone(),
            app_secret: self.app_secret.clone(),
            encrypt_key: self.encrypt_key.clone(),
            verify_signature: self.verify_signature,
            verify_timestamp_window_secs: self.verify_timestamp_window_secs,
            channel: Arc::new(self.clone()),
            tx,
        };

        let webhook_path = normalize_webhook_path(&self.webhook_path);
        let app = Router::new()
            .route(&webhook_path, post(handle_event))
            .with_state(state);

        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        tracing::info!("Lark event callback server listening on {addr}{webhook_path}");

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// WS helper functions
// ─────────────────────────────────────────────────────────────────────────────

fn pick_uniform_index(len: usize) -> usize {
    debug_assert!(len > 0);
    let upper = len as u64;
    let reject_threshold = (u64::MAX / upper) * upper;

    loop {
        let value = rand::random::<u64>();
        if value < reject_threshold {
            return (value % upper) as usize;
        }
    }
}

fn random_from_pool(pool: &'static [&'static str]) -> &'static str {
    pool[pick_uniform_index(pool.len())]
}

fn lark_ack_pool(locale: LarkAckLocale) -> &'static [&'static str] {
    match locale {
        LarkAckLocale::ZhCn => LARK_ACK_REACTIONS_ZH_CN,
        LarkAckLocale::ZhTw => LARK_ACK_REACTIONS_ZH_TW,
        LarkAckLocale::En => LARK_ACK_REACTIONS_EN,
        LarkAckLocale::Ja => LARK_ACK_REACTIONS_JA,
    }
}

fn map_locale_tag(tag: &str) -> Option<LarkAckLocale> {
    let normalized = tag.trim().to_ascii_lowercase().replace('-', "_");
    if normalized.is_empty() {
        return None;
    }

    if normalized.starts_with("ja") {
        return Some(LarkAckLocale::Ja);
    }
    if normalized.starts_with("en") {
        return Some(LarkAckLocale::En);
    }
    if normalized.contains("hant")
        || normalized.starts_with("zh_tw")
        || normalized.starts_with("zh_hk")
        || normalized.starts_with("zh_mo")
    {
        return Some(LarkAckLocale::ZhTw);
    }
    if normalized.starts_with("zh") {
        return Some(LarkAckLocale::ZhCn);
    }
    None
}

fn find_locale_hint(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for key in [
                "locale",
                "language",
                "lang",
                "i18n_locale",
                "user_locale",
                "locale_id",
            ] {
                if let Some(locale) = map.get(key).and_then(serde_json::Value::as_str) {
                    return Some(locale.to_string());
                }
            }

            for child in map.values() {
                if let Some(locale) = find_locale_hint(child) {
                    return Some(locale);
                }
            }
            None
        }
        serde_json::Value::Array(items) => {
            for child in items {
                if let Some(locale) = find_locale_hint(child) {
                    return Some(locale);
                }
            }
            None
        }
        _ => None,
    }
}

fn detect_locale_from_post_content(content: &str) -> Option<LarkAckLocale> {
    let parsed = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let obj = parsed.as_object()?;
    for key in obj.keys() {
        if let Some(locale) = map_locale_tag(key) {
            return Some(locale);
        }
    }
    None
}

fn is_japanese_kana(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3040..=0x309F | // Hiragana
        0x30A0..=0x30FF | // Katakana
        0x31F0..=0x31FF // Katakana Phonetic Extensions
    )
}

fn is_cjk_han(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF | // CJK Extension A
        0x4E00..=0x9FFF // CJK Unified Ideographs
    )
}

fn is_traditional_only_han(ch: char) -> bool {
    matches!(
        ch,
        '奮' | '鬥'
            | '強'
            | '體'
            | '國'
            | '臺'
            | '萬'
            | '與'
            | '為'
            | '這'
            | '學'
            | '機'
            | '開'
            | '裡'
    )
}

fn is_simplified_only_han(ch: char) -> bool {
    matches!(
        ch,
        '奋' | '斗'
            | '强'
            | '体'
            | '国'
            | '台'
            | '万'
            | '与'
            | '为'
            | '这'
            | '学'
            | '机'
            | '开'
            | '里'
    )
}

fn detect_locale_from_text(text: &str) -> Option<LarkAckLocale> {
    if text.chars().any(is_japanese_kana) {
        return Some(LarkAckLocale::Ja);
    }
    if text.chars().any(is_traditional_only_han) {
        return Some(LarkAckLocale::ZhTw);
    }
    if text.chars().any(is_simplified_only_han) {
        return Some(LarkAckLocale::ZhCn);
    }
    if text.chars().any(is_cjk_han) {
        return Some(LarkAckLocale::ZhCn);
    }
    None
}

fn detect_lark_ack_locale(
    payload: Option<&serde_json::Value>,
    fallback_text: &str,
) -> LarkAckLocale {
    if let Some(payload) = payload {
        if let Some(locale) = find_locale_hint(payload).and_then(|hint| map_locale_tag(&hint)) {
            return locale;
        }

        let message_content = payload
            .pointer("/message/content")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                payload
                    .pointer("/event/message/content")
                    .and_then(serde_json::Value::as_str)
            });

        if let Some(locale) = message_content.and_then(detect_locale_from_post_content) {
            return locale;
        }
    }

    detect_locale_from_text(fallback_text).unwrap_or(LarkAckLocale::En)
}

fn random_lark_ack_reaction(
    payload: Option<&serde_json::Value>,
    fallback_text: &str,
) -> &'static str {
    let locale = detect_lark_ack_locale(payload, fallback_text);
    random_from_pool(lark_ack_pool(locale))
}

fn normalize_webhook_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return DEFAULT_FEISHU_WEBHOOK_PATH.to_string();
    }
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn normalize_ack_reaction(ack_reaction: &str) -> String {
    let trimmed = ack_reaction.trim();
    if trimmed.is_empty() {
        DEFAULT_FEISHU_ACK_REACTION.to_string()
    } else {
        trimmed.to_string()
    }
}

fn extract_lark_content_str_field(content: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        content
            .get(*key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn extract_lark_content_number_field(content: &serde_json::Value, key: &str) -> Option<String> {
    let value = content.get(key)?;
    if let Some(v) = value.as_i64() {
        return Some(v.to_string());
    }
    if let Some(v) = value.as_u64() {
        return Some(v.to_string());
    }
    if let Some(v) = value.as_f64() {
        return Some(v.to_string());
    }
    value
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn lark_marker(tag: &str, detail: Option<String>) -> String {
    match detail {
        Some(value) if !value.trim().is_empty() => format!("[{tag}:{}]", value.trim()),
        _ => format!("[{tag}]"),
    }
}

fn parse_lark_attachment_marker(msg_type: &str, content_str: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(content_str)
        .unwrap_or_else(|_| serde_json::json!({}));

    match msg_type {
        "image" => lark_marker(
            "IMAGE",
            extract_lark_content_str_field(&parsed, &["image_key", "file_key", "key", "url"]),
        ),
        "file" => {
            let file_name = extract_lark_content_str_field(&parsed, &["file_name", "name"]);
            let file_key = extract_lark_content_str_field(&parsed, &["file_key", "key", "url"]);
            let detail = match (file_name, file_key) {
                (Some(name), Some(key)) => Some(format!("{name}|{key}")),
                (Some(name), None) => Some(name),
                (None, Some(key)) => Some(key),
                (None, None) => None,
            };
            lark_marker("FILE", detail)
        }
        "audio" => lark_marker(
            "AUDIO",
            extract_lark_content_str_field(&parsed, &["file_key", "audio_key", "key", "url"]),
        ),
        "video" => lark_marker(
            "VIDEO",
            extract_lark_content_str_field(&parsed, &["file_key", "video_key", "key", "url"]),
        ),
        "sticker" => lark_marker(
            "STICKER",
            extract_lark_content_str_field(&parsed, &["file_key", "sticker_key", "key"]),
        ),
        "location" => {
            let name = extract_lark_content_str_field(&parsed, &["name", "title", "address"]);
            let latitude = extract_lark_content_number_field(&parsed, "latitude")
                .or_else(|| extract_lark_content_number_field(&parsed, "lat"));
            let longitude = extract_lark_content_number_field(&parsed, "longitude")
                .or_else(|| extract_lark_content_number_field(&parsed, "lng"));

            let detail = match (name, latitude, longitude) {
                (Some(name), Some(lat), Some(lng)) => Some(format!("{name} ({lat},{lng})")),
                (Some(name), _, _) => Some(name),
                (None, Some(lat), Some(lng)) => Some(format!("{lat},{lng}")),
                _ => None,
            };
            lark_marker("LOCATION", detail)
        }
        _ => lark_marker("MESSAGE", None),
    }
}

fn parse_lark_message_content(msg_type: &str, content_str: &str) -> Option<(String, Vec<String>)> {
    match msg_type {
        "text" => {
            let extracted = serde_json::from_str::<serde_json::Value>(content_str)
                .ok()
                .and_then(|v| {
                    v.get("text")
                        .and_then(|t| t.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                })?;
            Some((extracted, Vec::new()))
        }
        "post" => parse_post_content_details(content_str)
            .map(|details| (details.text, details.mentioned_open_ids)),
        "image" | "file" | "audio" | "video" | "sticker" | "location" => Some((
            parse_lark_attachment_marker(msg_type, content_str),
            Vec::new(),
        )),
        _ => None,
    }
}

fn extract_card_json_marker(content: &str) -> Option<String> {
    let marker_start = content.find("[CARD:")?;
    let remainder = &content[marker_start + "[CARD:".len()..];
    for (offset, ch) in remainder.char_indices() {
        if ch != ']' {
            continue;
        }
        let candidate = remainder[..offset].trim();
        if candidate.is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(candidate) {
            return Some(parsed.to_string());
        }
    }
    None
}

fn parse_lark_attachment_kind_marker(marker_type: &str) -> Option<LarkOutgoingAttachmentKind> {
    match marker_type.trim().to_ascii_uppercase().as_str() {
        "IMAGE" | "PHOTO" => Some(LarkOutgoingAttachmentKind::Image),
        "DOCUMENT" | "FILE" => Some(LarkOutgoingAttachmentKind::File),
        "VIDEO" => Some(LarkOutgoingAttachmentKind::Video),
        "AUDIO" | "VOICE" => Some(LarkOutgoingAttachmentKind::Audio),
        _ => None,
    }
}

fn parse_lark_single_outgoing_attachment(content: &str) -> Option<LarkOutgoingAttachment> {
    let content = content.trim();
    if !content.starts_with('[') || !content.ends_with(']') {
        return None;
    }
    let marker = &content[1..content.len() - 1];
    let (marker_type, target) = marker.split_once(':')?;
    let kind = parse_lark_attachment_kind_marker(marker_type)?;
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    Some(LarkOutgoingAttachment {
        kind,
        target: target.to_string(),
    })
}

fn split_lark_attachment_target(target: &str) -> (Option<String>, String) {
    let trimmed = target.trim();
    let Some((left, right)) = trimmed.rsplit_once('|') else {
        return (None, trimmed.to_string());
    };
    let key = right.trim();
    if key.is_empty() {
        return (None, trimmed.to_string());
    }
    let display_name = left
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    let display_name = if display_name.is_empty() {
        None
    } else {
        Some(display_name)
    };
    (display_name, key.to_string())
}

fn build_incoming_lark_attachment_fallback_text(
    channel_name: &str,
    kind: LarkOutgoingAttachmentKind,
    target: &str,
) -> String {
    let (display_name, key_or_ref) = split_lark_attachment_target(target);
    let key_or_ref = key_or_ref.trim();
    let channel_label = if channel_name.eq_ignore_ascii_case("feishu") {
        "Feishu"
    } else {
        "Lark"
    };

    match kind {
        LarkOutgoingAttachmentKind::Image => {
            format!(
                "(Image attachment received from {channel_label}; key={})",
                key_or_ref
            )
        }
        LarkOutgoingAttachmentKind::File => {
            if let Some(name) = display_name.filter(|name| !name.is_empty()) {
                format!(
                    "(File attachment received from {channel_label}; name={name}; key={})",
                    key_or_ref
                )
            } else {
                format!(
                    "(File attachment received from {channel_label}; key={})",
                    key_or_ref
                )
            }
        }
        LarkOutgoingAttachmentKind::Video => {
            format!(
                "(Video attachment received from {channel_label}; key={})",
                key_or_ref
            )
        }
        LarkOutgoingAttachmentKind::Audio => {
            format!(
                "(Audio attachment received from {channel_label}; key={})",
                key_or_ref
            )
        }
    }
}

fn parse_lark_local_file_target(marker_value: &str) -> Option<PathBuf> {
    let raw = marker_value
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`'));
    let raw = raw.rsplit('|').next().unwrap_or(raw).trim();
    if raw.is_empty() || raw.starts_with("http://") || raw.starts_with("https://") {
        return None;
    }
    let candidate = raw.strip_prefix("file://").unwrap_or(raw);
    let path = PathBuf::from(candidate);
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

fn parse_lark_remote_url_target(marker_value: &str) -> Option<String> {
    let raw = marker_value
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`'));
    let raw = raw.rsplit('|').next().unwrap_or(raw).trim();
    if raw.starts_with("http://") || raw.starts_with("https://") {
        Some(raw.to_string())
    } else {
        None
    }
}

fn build_lark_text_send_body(recipient: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "receive_id": recipient,
        "msg_type": "text",
        "content": serde_json::json!({ "text": content }).to_string(),
    })
}

fn default_lark_upload_filename(kind: LarkOutgoingAttachmentKind) -> &'static str {
    match kind {
        LarkOutgoingAttachmentKind::Image => "image.bin",
        LarkOutgoingAttachmentKind::File => "attachment.bin",
        LarkOutgoingAttachmentKind::Video => "video.bin",
        LarkOutgoingAttachmentKind::Audio => "audio.bin",
    }
}

fn infer_lark_upload_filename_from_url(
    url: &reqwest::Url,
    kind: LarkOutgoingAttachmentKind,
) -> String {
    url.path_segments()
        .and_then(|mut segments| {
            segments
                .next_back()
                .filter(|name| !name.trim().is_empty())
                .map(|name| name.to_string())
        })
        .unwrap_or_else(|| default_lark_upload_filename(kind).to_string())
}

fn is_blocked_lark_remote_upload_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        return true;
    }

    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };

    is_blocked_lark_remote_upload_ip(ip)
}

fn is_blocked_lark_remote_upload_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ipv4) => {
            ipv4.is_private()
                || ipv4.is_loopback()
                || ipv4.is_link_local()
                || ipv4.is_broadcast()
                || ipv4.is_documentation()
                || ipv4.is_unspecified()
        }
        std::net::IpAddr::V6(ipv6) => {
            ipv6.is_loopback()
                || ipv6.is_unspecified()
                || ipv6.is_unique_local()
                || ipv6.is_unicast_link_local()
        }
    }
}

fn detect_lark_image_mime_from_magic(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        return "image/png";
    }
    if bytes.len() >= 3 && bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return "image/jpeg";
    }
    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return "image/gif";
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return "image/webp";
    }
    if bytes.len() >= 2 && bytes.starts_with(b"BM") {
        return "image/bmp";
    }
    "image/png"
}

fn is_lark_platform_image_key(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    normalized.starts_with("img_v2_") || normalized.starts_with("img_v3_")
}

async fn ensure_lark_remote_upload_host_is_public(host: &str) -> anyhow::Result<()> {
    if is_blocked_lark_remote_upload_host(host) {
        anyhow::bail!("blocked media URL host for Lark upload: {host}");
    }

    let lookup = tokio::time::timeout(
        Duration::from_secs(LARK_REMOTE_UPLOAD_DNS_TIMEOUT_SECS),
        tokio::net::lookup_host((host, 443)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timeout resolving media URL host: {host}"))?
    .map_err(|e| anyhow::anyhow!("failed to resolve media URL host '{host}': {e}"))?;

    for addr in lookup {
        if is_blocked_lark_remote_upload_ip(addr.ip()) {
            anyhow::bail!("blocked media URL resolved to private/local address: {host}");
        }
    }

    Ok(())
}

fn is_valid_lark_media_key(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return false;
    }
    !(trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with('/')
        || trimmed.starts_with('.')
        || trimmed.contains('\\')
        || trimmed.contains(':'))
}

fn normalize_lark_media_key_for_send(
    kind: LarkOutgoingAttachmentKind,
    marker_value: &str,
) -> Option<String> {
    let candidate = match kind {
        LarkOutgoingAttachmentKind::File => marker_value.rsplit('|').next().unwrap_or(marker_value),
        _ => marker_value,
    }
    .trim();
    if !is_valid_lark_media_key(candidate) {
        return None;
    }
    Some(candidate.to_string())
}

fn build_lark_media_send_body_with_key(
    recipient: &str,
    kind: LarkOutgoingAttachmentKind,
    key: &str,
) -> serde_json::Value {
    let (msg_type, body) = match kind {
        LarkOutgoingAttachmentKind::Image => ("image", serde_json::json!({ "image_key": key })),
        LarkOutgoingAttachmentKind::File => ("file", serde_json::json!({ "file_key": key })),
        LarkOutgoingAttachmentKind::Video => ("video", serde_json::json!({ "file_key": key })),
        LarkOutgoingAttachmentKind::Audio => ("audio", serde_json::json!({ "file_key": key })),
    };
    serde_json::json!({
        "receive_id": recipient,
        "msg_type": msg_type,
        "content": body.to_string(),
    })
}

fn build_lark_media_send_body(recipient: &str, content: &str) -> Option<serde_json::Value> {
    let attachment = parse_lark_single_outgoing_attachment(content)?;
    let key = normalize_lark_media_key_for_send(attachment.kind, &attachment.target)?;
    Some(build_lark_media_send_body_with_key(
        recipient,
        attachment.kind,
        &key,
    ))
}

fn build_lark_send_body(recipient: &str, content: &str) -> serde_json::Value {
    if let Some(card_json) = extract_card_json_marker(content) {
        return serde_json::json!({
            "receive_id": recipient,
            "msg_type": "interactive",
            "content": card_json,
        });
    }

    if let Some(media_payload) = build_lark_media_send_body(recipient, content) {
        return media_payload;
    }

    build_lark_text_send_body(recipient, content)
}

fn verify_lark_request_timestamp(timestamp: &str, window_secs: u64, now_secs: i64) -> bool {
    if window_secs == 0 {
        return true;
    }

    let Ok(request_ts) = timestamp.trim().parse::<i64>() else {
        return false;
    };

    (now_secs - request_ts).unsigned_abs() <= window_secs
}

fn decode_lark_signature_bytes(signature: &str) -> Option<Vec<u8>> {
    let normalized = signature
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or(signature)
        .trim();
    if normalized.is_empty() {
        return None;
    }

    if let Ok(bytes) = hex::decode(normalized) {
        return Some(bytes);
    }

    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(normalized)
        .ok()
}

fn constant_time_bytes_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut diff = 0u8;
    for (&l, &r) in left.iter().zip(right.iter()) {
        diff |= l ^ r;
    }
    diff == 0
}

fn verify_lark_webhook_signature(
    encrypt_key: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
    signature: &str,
) -> bool {
    use sha2::{Digest, Sha256};

    if encrypt_key.trim().is_empty() || timestamp.trim().is_empty() || nonce.trim().is_empty() {
        return false;
    }

    let Some(provided) = decode_lark_signature_bytes(signature) else {
        return false;
    };

    let mut hasher = Sha256::new();
    hasher.update(timestamp.trim().as_bytes());
    hasher.update(nonce.trim().as_bytes());
    hasher.update(encrypt_key.as_bytes());
    hasher.update(body);
    let digest = hasher.finalize();

    constant_time_bytes_eq(digest.as_slice(), &provided)
}

fn verify_lark_webhook_signature_legacy(
    app_secret: &str,
    timestamp: &str,
    body: &[u8],
    signature: &str,
) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    if app_secret.trim().is_empty() || timestamp.trim().is_empty() {
        return false;
    }

    let Some(provided) = decode_lark_signature_bytes(signature) else {
        return false;
    };

    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(app_secret.as_bytes()) else {
        return false;
    };
    mac.update(timestamp.trim().as_bytes());
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

fn decode_lark_callback_payload(
    raw_payload: &serde_json::Value,
    encrypt_key: &str,
) -> anyhow::Result<serde_json::Value> {
    if let Some(encrypted) = raw_payload.get("encrypt").and_then(|v| v.as_str()) {
        if encrypt_key.trim().is_empty() {
            anyhow::bail!("encrypt payload received but encrypt_key is missing");
        }
        return decrypt_lark_callback_payload(encrypt_key, encrypted);
    }

    Ok(raw_payload.clone())
}

fn decrypt_lark_callback_payload(
    encrypt_key: &str,
    encrypted: &str,
) -> anyhow::Result<serde_json::Value> {
    use aes::Aes256;
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    use cbc::Decryptor;
    use sha2::{Digest, Sha256};

    let encrypted = encrypted.trim();
    if encrypted.is_empty() {
        anyhow::bail!("empty encrypt payload");
    }

    use base64::Engine;
    let encrypted_bytes = base64::engine::general_purpose::STANDARD
        .decode(encrypted)
        .map_err(|e| anyhow::anyhow!("invalid encrypt payload (base64): {e}"))?;
    if encrypted_bytes.len() <= 16 {
        anyhow::bail!("invalid encrypt payload length");
    }

    let (iv, ciphertext) = encrypted_bytes.split_at(16);
    let key = Sha256::digest(encrypt_key.as_bytes());
    let decryptor = Decryptor::<Aes256>::new_from_slices(&key, iv)
        .map_err(|e| anyhow::anyhow!("invalid decrypt key/iv: {e}"))?;
    let mut buffer = ciphertext.to_vec();
    let plaintext = decryptor
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|e| anyhow::anyhow!("decrypt callback payload failed: {e}"))?;
    let payload = serde_json::from_slice::<serde_json::Value>(plaintext)
        .map_err(|e| anyhow::anyhow!("decrypt callback payload json parse failed: {e}"))?;
    Ok(payload)
}

fn extract_lark_payload_token(payload: &serde_json::Value) -> Option<&str> {
    payload
        .pointer("/header/token")
        .and_then(|token| token.as_str())
        .or_else(|| payload.get("token").and_then(|token| token.as_str()))
}

fn verify_lark_payload_token(payload: &serde_json::Value, verification_token: &str) -> bool {
    if verification_token.trim().is_empty() {
        return true;
    }

    matches!(
        extract_lark_payload_token(payload),
        Some(token) if token == verification_token
    )
}

/// Flatten a Feishu `post` rich-text message to plain text.
///
/// Returns `None` when the content cannot be parsed or yields no usable text,
/// so callers can simply `continue` rather than forwarding a meaningless
/// placeholder string to the agent.
struct ParsedPostContent {
    text: String,
    mentioned_open_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LarkOutgoingAttachmentKind {
    Image,
    File,
    Video,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LarkOutgoingAttachment {
    kind: LarkOutgoingAttachmentKind,
    target: String,
}

#[derive(Debug, Clone)]
struct LarkUploadSource {
    bytes: Vec<u8>,
    filename: String,
    mime: Option<String>,
}

fn parse_post_content_details(content: &str) -> Option<ParsedPostContent> {
    let parsed = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let locale = parsed
        .get("zh_cn")
        .or_else(|| parsed.get("en_us"))
        .or_else(|| {
            parsed
                .as_object()
                .and_then(|m| m.values().find(|v| v.is_object()))
        })?;

    let mut text = String::new();
    let mut mentioned_open_ids = Vec::new();

    if let Some(title) = locale
        .get("title")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
    {
        text.push_str(title);
        text.push_str("\n\n");
    }

    if let Some(paragraphs) = locale.get("content").and_then(|c| c.as_array()) {
        for para in paragraphs {
            if let Some(elements) = para.as_array() {
                for el in elements {
                    match el.get("tag").and_then(|t| t.as_str()).unwrap_or("") {
                        "text" => {
                            if let Some(t) = el.get("text").and_then(|t| t.as_str()) {
                                text.push_str(t);
                            }
                        }
                        "a" => {
                            text.push_str(
                                el.get("text")
                                    .and_then(|t| t.as_str())
                                    .filter(|s| !s.is_empty())
                                    .or_else(|| el.get("href").and_then(|h| h.as_str()))
                                    .unwrap_or(""),
                            );
                        }
                        "at" => {
                            let n = el
                                .get("user_name")
                                .and_then(|n| n.as_str())
                                .or_else(|| el.get("user_id").and_then(|i| i.as_str()))
                                .unwrap_or("user");
                            text.push('@');
                            text.push_str(n);
                            if let Some(open_id) = el
                                .get("user_id")
                                .and_then(|i| i.as_str())
                                .map(str::trim)
                                .filter(|id| !id.is_empty())
                            {
                                mentioned_open_ids.push(open_id.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                text.push('\n');
            }
        }
    }

    let result = text.trim().to_string();
    if result.is_empty() {
        None
    } else {
        Some(ParsedPostContent {
            text: result,
            mentioned_open_ids,
        })
    }
}

fn parse_post_content(content: &str) -> Option<String> {
    parse_post_content_details(content).map(|details| details.text)
}

/// Remove `@_user_N` placeholder tokens injected by Feishu in group chats.
fn strip_at_placeholders(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '@' {
            let rest: String = chars.clone().map(|(_, c)| c).collect();
            if let Some(after) = rest.strip_prefix("_user_") {
                let skip =
                    "_user_".len() + after.chars().take_while(|c| c.is_ascii_digit()).count();
                for _ in 0..=skip {
                    chars.next();
                }
                if chars.peek().map(|(_, c)| *c == ' ').unwrap_or(false) {
                    chars.next();
                }
                continue;
            }
        }
        result.push(ch);
    }
    result
}

fn mention_matches_bot_open_id(mention: &serde_json::Value, bot_open_id: &str) -> bool {
    mention
        .pointer("/id/open_id")
        .or_else(|| mention.pointer("/open_id"))
        .and_then(|v| v.as_str())
        .is_some_and(|value| value == bot_open_id)
}

/// In group chats, only respond when the bot is explicitly @-mentioned.
fn should_respond_in_group(
    mention_only: bool,
    bot_open_id: Option<&str>,
    mentions: &[serde_json::Value],
    post_mentioned_open_ids: &[String],
) -> bool {
    if !mention_only {
        return true;
    }
    let Some(bot_open_id) = bot_open_id.filter(|id| !id.is_empty()) else {
        return false;
    };
    if mentions.is_empty() && post_mentioned_open_ids.is_empty() {
        return false;
    }
    mentions
        .iter()
        .any(|mention| mention_matches_bot_open_id(mention, bot_open_id))
        || post_mentioned_open_ids
            .iter()
            .any(|id| id.as_str() == bot_open_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_bot_open_id(ch: LarkChannel, bot_open_id: &str) -> LarkChannel {
        ch.set_resolved_bot_open_id(Some(bot_open_id.to_string()));
        ch
    }

    fn make_channel() -> LarkChannel {
        with_bot_open_id(
            LarkChannel::new(
                "cli_test_app_id".into(),
                "test_app_secret".into(),
                String::new(),
                "test_verification_token".into(),
                None,
                vec!["ou_testuser123".into()],
                true,
            ),
            "ou_bot",
        )
    }

    fn encrypt_lark_callback_payload_for_test(
        encrypt_key: &str,
        payload: &serde_json::Value,
    ) -> String {
        use aes::Aes256;
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        use cbc::Encryptor;
        use sha2::{Digest, Sha256};

        let iv = [7u8; 16];
        let key = Sha256::digest(encrypt_key.as_bytes());
        let encryptor = Encryptor::<Aes256>::new_from_slices(&key, &iv).unwrap();
        let plaintext = serde_json::to_vec(payload).unwrap();
        let msg_len = plaintext.len();
        let mut buffer = plaintext;
        let padding = 16 - (msg_len % 16);
        buffer.extend(std::iter::repeat_n(0u8, padding.max(1)));
        let ciphertext = encryptor
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, msg_len)
            .unwrap();

        let mut envelope = iv.to_vec();
        envelope.extend_from_slice(ciphertext);
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(envelope)
    }

    #[test]
    fn lark_channel_name() {
        let ch = make_channel();
        assert_eq!(ch.name(), "lark");
    }

    #[test]
    fn lark_ws_activity_refreshes_heartbeat_watchdog() {
        assert!(should_refresh_last_recv(&WsMsg::Binary(
            vec![1, 2, 3].into()
        )));
        assert!(should_refresh_last_recv(&WsMsg::Ping(vec![9, 9].into())));
        assert!(should_refresh_last_recv(&WsMsg::Pong(vec![8, 8].into())));
    }

    #[test]
    fn lark_ws_non_activity_frames_do_not_refresh_heartbeat_watchdog() {
        assert!(!should_refresh_last_recv(&WsMsg::Text("hello".into())));
        assert!(!should_refresh_last_recv(&WsMsg::Close(None)));
    }

    #[test]
    fn lark_group_response_requires_matching_bot_mention_when_ids_available() {
        let mentions = vec![serde_json::json!({
            "id": { "open_id": "ou_other" }
        })];
        assert!(!should_respond_in_group(
            true,
            Some("ou_bot"),
            &mentions,
            &[]
        ));

        let mentions = vec![serde_json::json!({
            "id": { "open_id": "ou_bot" }
        })];
        assert!(should_respond_in_group(
            true,
            Some("ou_bot"),
            &mentions,
            &[]
        ));
    }

    #[test]
    fn lark_group_response_requires_resolved_open_id_when_mention_only_enabled() {
        let mentions = vec![serde_json::json!({
            "id": { "open_id": "ou_any" }
        })];
        assert!(!should_respond_in_group(true, None, &mentions, &[]));
    }

    #[test]
    fn lark_group_response_allows_post_mentions_for_bot_open_id() {
        assert!(should_respond_in_group(
            true,
            Some("ou_bot"),
            &[],
            &[String::from("ou_bot")]
        ));
    }

    #[test]
    fn lark_should_refresh_token_on_http_401() {
        let body = serde_json::json!({ "code": 0 });
        assert!(should_refresh_lark_tenant_token(
            reqwest::StatusCode::UNAUTHORIZED,
            &body
        ));
    }

    #[test]
    fn lark_should_refresh_token_on_body_code_99991663() {
        let body = serde_json::json!({
            "code": LARK_INVALID_ACCESS_TOKEN_CODE,
            "msg": "Invalid access token for authorization."
        });
        assert!(should_refresh_lark_tenant_token(
            reqwest::StatusCode::OK,
            &body
        ));
    }

    #[test]
    fn lark_should_not_refresh_token_on_success_body() {
        let body = serde_json::json!({ "code": 0, "msg": "ok" });
        assert!(!should_refresh_lark_tenant_token(
            reqwest::StatusCode::OK,
            &body
        ));
    }

    #[test]
    fn lark_extract_token_ttl_seconds_supports_expire_and_expires_in() {
        let body_expire = serde_json::json!({ "expire": 7200 });
        let body_expires_in = serde_json::json!({ "expires_in": 3600 });
        let body_missing = serde_json::json!({});
        assert_eq!(extract_lark_token_ttl_seconds(&body_expire), 7200);
        assert_eq!(extract_lark_token_ttl_seconds(&body_expires_in), 3600);
        assert_eq!(
            extract_lark_token_ttl_seconds(&body_missing),
            LARK_DEFAULT_TOKEN_TTL.as_secs()
        );
    }

    #[test]
    fn lark_next_token_refresh_deadline_reserves_refresh_skew() {
        let now = Instant::now();
        let regular = next_token_refresh_deadline(now, 7200);
        let short_ttl = next_token_refresh_deadline(now, 60);

        assert_eq!(regular.duration_since(now), Duration::from_secs(7080));
        assert_eq!(short_ttl.duration_since(now), Duration::from_secs(1));
    }

    #[test]
    fn lark_ensure_send_success_rejects_non_zero_code() {
        let ok = serde_json::json!({ "code": 0 });
        let bad = serde_json::json!({ "code": 12345, "msg": "bad request" });

        assert!(ensure_lark_send_success(reqwest::StatusCode::OK, &ok, "test").is_ok());
        assert!(ensure_lark_send_success(reqwest::StatusCode::OK, &bad, "test").is_err());
    }

    #[test]
    fn lark_user_allowed_exact() {
        let ch = make_channel();
        assert!(ch.is_user_allowed("ou_testuser123"));
        assert!(!ch.is_user_allowed("ou_other"));
    }

    #[test]
    fn lark_user_allowed_wildcard() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            String::new(),
            "token".into(),
            None,
            vec!["*".into()],
            true,
        );
        assert!(ch.is_user_allowed("ou_anyone"));
    }

    #[test]
    fn lark_user_denied_empty() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            String::new(),
            "token".into(),
            None,
            vec![],
            true,
        );
        assert!(!ch.is_user_allowed("ou_anyone"));
    }

    #[test]
    fn lark_parse_challenge() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "challenge": "abc123",
            "token": "test_verification_token",
            "type": "url_verification"
        });
        // Challenge payloads should not produce messages
        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_valid_text_message() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "header": {
                "event_type": "im.message.receive_v1"
            },
            "event": {
                "sender": {
                    "sender_id": {
                        "open_id": "ou_testuser123"
                    }
                },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"Hello ZeroClaw!\"}",
                    "chat_id": "oc_chat123",
                    "create_time": "1699999999000"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Hello ZeroClaw!");
        assert_eq!(msgs[0].sender, "oc_chat123");
        assert_eq!(msgs[0].channel, "lark");
        assert_eq!(msgs[0].timestamp, 1_699_999_999);
    }

    #[test]
    fn lark_parse_unauthorized_user() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_unauthorized" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"spam\"}",
                    "chat_id": "oc_chat",
                    "create_time": "1000"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_image_message_to_marker() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            String::new(),
            "token".into(),
            None,
            vec!["*".into()],
            true,
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "image",
                    "content": "{\"image_key\":\"img_v2_123\"}",
                    "chat_id": "oc_chat"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "[IMAGE:img_v2_123]");
    }

    #[test]
    fn lark_parse_extended_message_types_to_markers() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            String::new(),
            "token".into(),
            None,
            vec!["*".into()],
            true,
        );

        let cases = vec![
            (
                "file",
                "{\"file_key\":\"file_v2_1\",\"file_name\":\"report.pdf\"}",
                "[FILE:report.pdf|file_v2_1]",
            ),
            (
                "audio",
                "{\"file_key\":\"audio_v2_1\"}",
                "[AUDIO:audio_v2_1]",
            ),
            (
                "video",
                "{\"file_key\":\"video_v2_1\"}",
                "[VIDEO:video_v2_1]",
            ),
            (
                "sticker",
                "{\"file_key\":\"sticker_v2_1\"}",
                "[STICKER:sticker_v2_1]",
            ),
            (
                "location",
                "{\"name\":\"HQ\",\"latitude\":\"31.2\",\"longitude\":\"121.5\"}",
                "[LOCATION:HQ (31.2,121.5)]",
            ),
        ];

        for (msg_type, content, expected) in cases {
            let payload = serde_json::json!({
                "header": { "event_type": "im.message.receive_v1" },
                "event": {
                    "sender": { "sender_id": { "open_id": "ou_user" } },
                    "message": {
                        "message_type": msg_type,
                        "content": content,
                        "chat_id": "oc_chat"
                    }
                }
            });
            let msgs = ch.parse_event_payload(&payload);
            assert_eq!(msgs.len(), 1, "message type {msg_type} should be parsed");
            assert_eq!(msgs[0].content, expected);
        }
    }

    #[test]
    fn lark_parse_empty_text_skipped() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            String::new(),
            "token".into(),
            None,
            vec!["*".into()],
            true,
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"\"}",
                    "chat_id": "oc_chat"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_wrong_event_type() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "header": { "event_type": "im.chat.disbanded_v1" },
            "event": {}
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_missing_sender() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            String::new(),
            "token".into(),
            None,
            vec!["*".into()],
            true,
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}",
                    "chat_id": "oc_chat"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_unicode_message() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            String::new(),
            "token".into(),
            None,
            vec!["*".into()],
            true,
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"Hello world 🌍\"}",
                    "chat_id": "oc_chat",
                    "create_time": "1000"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Hello world 🌍");
    }

    #[test]
    fn lark_parse_missing_event() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_invalid_content_json() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            String::new(),
            "token".into(),
            None,
            vec!["*".into()],
            true,
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "not valid json",
                    "chat_id": "oc_chat"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_config_serde() {
        use crate::config::schema::{LarkConfig, LarkReceiveMode};
        let lc = LarkConfig {
            app_id: "cli_app123".into(),
            app_secret: "secret456".into(),
            encrypt_key: None,
            verification_token: Some("vtoken789".into()),
            allowed_users: vec!["ou_user1".into(), "ou_user2".into()],
            mention_only: false,
            use_feishu: false,
            receive_mode: LarkReceiveMode::default(),
            port: None,
        };
        let json = serde_json::to_string(&lc).unwrap();
        let parsed: LarkConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.app_id, "cli_app123");
        assert_eq!(parsed.app_secret, "secret456");
        assert_eq!(parsed.verification_token.as_deref(), Some("vtoken789"));
        assert_eq!(parsed.allowed_users.len(), 2);
    }

    #[test]
    fn lark_config_toml_roundtrip() {
        use crate::config::schema::{LarkConfig, LarkReceiveMode};
        let lc = LarkConfig {
            app_id: "app".into(),
            app_secret: "secret".into(),
            encrypt_key: None,
            verification_token: Some("tok".into()),
            allowed_users: vec!["*".into()],
            mention_only: false,
            use_feishu: false,
            receive_mode: LarkReceiveMode::Webhook,
            port: Some(9898),
        };
        let toml_str = toml::to_string(&lc).unwrap();
        let parsed: LarkConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.app_id, "app");
        assert_eq!(parsed.verification_token.as_deref(), Some("tok"));
        assert_eq!(parsed.allowed_users, vec!["*"]);
    }

    #[test]
    fn lark_config_defaults_optional_fields() {
        use crate::config::schema::{LarkConfig, LarkReceiveMode};
        let json = r#"{"app_id":"a","app_secret":"s"}"#;
        let parsed: LarkConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.verification_token.is_none());
        assert!(parsed.allowed_users.is_empty());
        assert!(!parsed.mention_only);
        assert_eq!(parsed.receive_mode, LarkReceiveMode::Websocket);
        assert!(parsed.port.is_none());
    }

    #[test]
    fn lark_from_config_preserves_mode_and_region() {
        use crate::config::schema::{LarkConfig, LarkReceiveMode};

        let cfg = LarkConfig {
            app_id: "cli_app123".into(),
            app_secret: "secret456".into(),
            encrypt_key: None,
            verification_token: Some("vtoken789".into()),
            allowed_users: vec!["*".into()],
            mention_only: false,
            use_feishu: false,
            receive_mode: LarkReceiveMode::Webhook,
            port: Some(9898),
        };

        let ch = LarkChannel::from_config(&cfg);

        assert_eq!(ch.api_base(), LARK_BASE_URL);
        assert_eq!(ch.ws_base(), LARK_WS_BASE_URL);
        assert_eq!(ch.receive_mode, LarkReceiveMode::Webhook);
        assert_eq!(ch.port, Some(9898));
    }

    #[test]
    fn lark_from_lark_config_ignores_legacy_feishu_flag() {
        use crate::config::schema::{LarkConfig, LarkReceiveMode};

        let cfg = LarkConfig {
            app_id: "cli_app123".into(),
            app_secret: "secret456".into(),
            encrypt_key: None,
            verification_token: Some("vtoken789".into()),
            allowed_users: vec!["*".into()],
            mention_only: false,
            use_feishu: true,
            receive_mode: LarkReceiveMode::Webhook,
            port: Some(9898),
        };

        let ch = LarkChannel::from_lark_config(&cfg);

        assert_eq!(ch.api_base(), LARK_BASE_URL);
        assert_eq!(ch.ws_base(), LARK_WS_BASE_URL);
        assert_eq!(ch.name(), "lark");
    }

    #[test]
    fn lark_from_feishu_config_sets_feishu_platform() {
        use crate::config::schema::{FeishuConfig, LarkReceiveMode};

        let cfg = FeishuConfig {
            app_id: "cli_feishu_app123".into(),
            app_secret: "secret456".into(),
            encrypt_key: None,
            verification_token: Some("vtoken789".into()),
            allowed_users: vec!["*".into()],
            mention_only: true,
            webhook_path: "feishu-hook".into(),
            verify_signature: false,
            verify_timestamp_window_secs: 180,
            ack_reaction: "none".into(),
            upload_failure_strategy:
                crate::config::schema::FeishuUploadFailureStrategy::FallbackText,
            receive_mode: LarkReceiveMode::Webhook,
            port: Some(9898),
        };

        let ch = LarkChannel::from_feishu_config(&cfg);

        assert_eq!(ch.api_base(), FEISHU_BASE_URL);
        assert_eq!(ch.ws_base(), FEISHU_WS_BASE_URL);
        assert_eq!(ch.name(), "feishu");
        assert!(ch.mention_only);
        assert_eq!(ch.webhook_path, "/feishu-hook");
        assert!(!ch.verify_signature);
        assert_eq!(ch.verify_timestamp_window_secs, 180);
        assert_eq!(ch.ack_reaction, "none");
        assert_eq!(
            ch.upload_failure_strategy,
            crate::config::schema::FeishuUploadFailureStrategy::FallbackText
        );
    }

    #[test]
    fn lark_webhook_path_normalization_uses_feishu_default() {
        assert_eq!(normalize_webhook_path(""), "/feishu");
        assert_eq!(normalize_webhook_path("feishu-hook"), "/feishu-hook");
        assert_eq!(normalize_webhook_path("/custom/path"), "/custom/path");
    }

    #[test]
    fn lark_verify_request_timestamp_within_window() {
        assert!(verify_lark_request_timestamp("1000", 300, 1299));
        assert!(verify_lark_request_timestamp("1000", 300, 700));
        assert!(!verify_lark_request_timestamp("1000", 300, 1301));
        assert!(!verify_lark_request_timestamp("invalid", 300, 1301));
        assert!(verify_lark_request_timestamp("invalid", 0, 1301));
    }

    #[test]
    fn lark_verify_signature_accepts_hex_and_base64() {
        use sha2::{Digest, Sha256};

        let encrypt_key = "test_encrypt_key";
        let timestamp = "1710000000";
        let nonce = "nonce_123";
        let body = br#"{"event":"ok"}"#;

        let mut hasher = Sha256::new();
        hasher.update(timestamp.as_bytes());
        hasher.update(nonce.as_bytes());
        hasher.update(encrypt_key.as_bytes());
        hasher.update(body);
        let digest = hasher.finalize();
        let hex_signature = hex::encode(digest.as_slice());
        assert!(verify_lark_webhook_signature(
            encrypt_key,
            timestamp,
            nonce,
            body,
            &hex_signature
        ));

        use base64::Engine;
        let base64_signature = base64::engine::general_purpose::STANDARD.encode(digest);
        assert!(verify_lark_webhook_signature(
            encrypt_key,
            timestamp,
            nonce,
            body,
            &base64_signature
        ));
        assert!(!verify_lark_webhook_signature(
            encrypt_key,
            timestamp,
            nonce,
            body,
            "invalid-signature"
        ));
        assert!(!verify_lark_webhook_signature(
            encrypt_key,
            timestamp,
            "",
            body,
            &hex_signature
        ));
    }

    #[test]
    fn lark_build_send_body_supports_card_marker() {
        let body = build_lark_send_body(
            "oc_chat123",
            "header\n[CARD:{\"config\":{\"wide_screen_mode\":true},\"elements\":[]}]",
        );
        assert_eq!(
            body.get("msg_type").and_then(|v| v.as_str()),
            Some("interactive")
        );
        let card_json = body.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let parsed_card = serde_json::from_str::<serde_json::Value>(card_json).unwrap();
        assert_eq!(
            parsed_card.pointer("/config/wide_screen_mode"),
            Some(&serde_json::Value::Bool(true))
        );

        let fallback = build_lark_send_body("oc_chat123", "[CARD:{not json}] hello");
        assert_eq!(
            fallback.get("msg_type").and_then(|v| v.as_str()),
            Some("text")
        );
        let text = fallback
            .get("content")
            .and_then(|v| v.as_str())
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
            .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_default();
        assert_eq!(text, "[CARD:{not json}] hello");
    }

    #[test]
    fn lark_build_send_body_supports_media_markers() {
        let image = build_lark_send_body("oc_chat123", "[IMAGE:img_v2_100]");
        assert_eq!(
            image.pointer("/msg_type").and_then(|v| v.as_str()),
            Some("image")
        );
        let image_content = image
            .get("content")
            .and_then(|v| v.as_str())
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
            .unwrap_or_default();
        assert_eq!(
            image_content.get("image_key").and_then(|v| v.as_str()),
            Some("img_v2_100")
        );

        let file = build_lark_send_body("oc_chat123", "[DOCUMENT:file_v2_200]");
        assert_eq!(
            file.pointer("/msg_type").and_then(|v| v.as_str()),
            Some("file")
        );
        let file_content = file
            .get("content")
            .and_then(|v| v.as_str())
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
            .unwrap_or_default();
        assert_eq!(
            file_content.get("file_key").and_then(|v| v.as_str()),
            Some("file_v2_200")
        );

        let file_with_name = build_lark_send_body("oc_chat123", "[FILE:report.pdf|file_v2_201]");
        assert_eq!(
            file_with_name.pointer("/msg_type").and_then(|v| v.as_str()),
            Some("file")
        );
        let named_content = file_with_name
            .get("content")
            .and_then(|v| v.as_str())
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
            .unwrap_or_default();
        assert_eq!(
            named_content.get("file_key").and_then(|v| v.as_str()),
            Some("file_v2_201")
        );

        let voice = build_lark_send_body("oc_chat123", "[VOICE:file_v2_voice]");
        assert_eq!(
            voice.pointer("/msg_type").and_then(|v| v.as_str()),
            Some("audio")
        );

        let invalid = build_lark_send_body("oc_chat123", "[IMAGE:/tmp/local.png]");
        assert_eq!(
            invalid.pointer("/msg_type").and_then(|v| v.as_str()),
            Some("text")
        );
    }

    #[test]
    fn lark_parse_single_outgoing_attachment_parses_marker_only_message() {
        let parsed = parse_lark_single_outgoing_attachment("[DOCUMENT:report.pdf|file_v2_201]")
            .expect("attachment marker should parse");
        assert_eq!(parsed.kind, LarkOutgoingAttachmentKind::File);
        assert_eq!(parsed.target, "report.pdf|file_v2_201");
        assert!(parse_lark_single_outgoing_attachment("text [IMAGE:img_v2_x]").is_none());
    }

    #[test]
    fn lark_split_attachment_target_extracts_name_and_key() {
        let (name, key) = split_lark_attachment_target("report.pdf|file_v2_201");
        assert_eq!(name.as_deref(), Some("report.pdf"));
        assert_eq!(key, "file_v2_201");

        let (name, key) = split_lark_attachment_target("file_v2_201");
        assert!(name.is_none());
        assert_eq!(key, "file_v2_201");
    }

    #[test]
    fn lark_build_incoming_attachment_fallback_text_is_readable() {
        let file = build_incoming_lark_attachment_fallback_text(
            "feishu",
            LarkOutgoingAttachmentKind::File,
            "report.pdf|file_v2_201",
        );
        assert!(file.contains("Feishu"));
        assert!(file.contains("report.pdf"));
        assert!(file.contains("file_v2_201"));

        let video = build_incoming_lark_attachment_fallback_text(
            "lark",
            LarkOutgoingAttachmentKind::Video,
            "video_v2_101",
        );
        assert!(video.contains("Video attachment received from Lark"));
        assert!(video.contains("video_v2_101"));
    }

    #[tokio::test]
    async fn lark_resolve_incoming_non_image_marker_to_fallback_text() {
        let ch = make_channel();
        let resolved = ch
            .resolve_incoming_media_markers("[FILE:report.pdf|file_v2_201]", None)
            .await;
        assert!(resolved.contains("File attachment received from Lark"));
        assert!(resolved.contains("report.pdf"));
        assert!(resolved.contains("file_v2_201"));
    }

    #[tokio::test]
    async fn lark_resolve_incoming_non_platform_image_marker_keeps_original() {
        let ch = make_channel();
        let content = "[IMAGE:https://example.com/demo.png]";
        let resolved = ch.resolve_incoming_media_markers(content, None).await;
        assert_eq!(resolved, content);
    }

    #[test]
    fn lark_message_resource_download_url_matches_region() {
        let ch_lark = make_channel();
        assert_eq!(
            ch_lark.message_resource_download_url("om_test_message_id", "img_v3_test_key"),
            "https://open.larksuite.com/open-apis/im/v1/messages/om_test_message_id/resources/img_v3_test_key"
        );

        let feishu_cfg = crate::config::schema::FeishuConfig {
            app_id: "cli_app123".into(),
            app_secret: "secret456".into(),
            encrypt_key: None,
            verification_token: Some("vtoken789".into()),
            allowed_users: vec!["*".into()],
            mention_only: false,
            webhook_path: "/feishu".into(),
            verify_signature: true,
            verify_timestamp_window_secs: 300,
            ack_reaction: "auto".into(),
            upload_failure_strategy: crate::config::schema::FeishuUploadFailureStrategy::Strict,
            receive_mode: crate::config::schema::LarkReceiveMode::Webhook,
            port: Some(9898),
        };
        let ch_feishu = LarkChannel::from_feishu_config(&feishu_cfg);
        assert_eq!(
            ch_feishu.message_resource_download_url("om_test_message_id", "img_v3_test_key"),
            "https://open.feishu.cn/open-apis/im/v1/messages/om_test_message_id/resources/img_v3_test_key"
        );
    }

    #[test]
    fn lark_parse_local_file_target_supports_plain_and_file_scheme_paths() {
        let temp_dir = std::env::temp_dir().join(format!("zeroclaw_lark_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let file_path = temp_dir.join("sample.png");
        std::fs::write(&file_path, b"test-bytes").unwrap();

        let plain = parse_lark_local_file_target(file_path.to_string_lossy().as_ref())
            .expect("plain local path should parse");
        assert_eq!(plain, file_path);

        let file_url_input = format!("file://{}", file_path.to_string_lossy());
        let with_scheme =
            parse_lark_local_file_target(&file_url_input).expect("file:// path should parse");
        assert_eq!(with_scheme, file_path);

        assert!(parse_lark_local_file_target("https://example.com/a.png").is_none());

        let _ = std::fs::remove_file(&file_path);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn lark_parse_remote_url_target_extracts_http_urls() {
        assert_eq!(
            parse_lark_remote_url_target("https://example.com/a.png"),
            Some("https://example.com/a.png".to_string())
        );
        assert_eq!(
            parse_lark_remote_url_target("name|https://example.com/a.png"),
            Some("https://example.com/a.png".to_string())
        );
        assert!(parse_lark_remote_url_target("/tmp/a.png").is_none());
    }

    #[test]
    fn lark_blocked_remote_upload_hosts_include_localhost_and_private_ips() {
        assert!(is_blocked_lark_remote_upload_host("localhost"));
        assert!(is_blocked_lark_remote_upload_host("127.0.0.1"));
        assert!(is_blocked_lark_remote_upload_host("10.0.0.1"));
        assert!(!is_blocked_lark_remote_upload_host("8.8.8.8"));
        assert!(!is_blocked_lark_remote_upload_host("example.com"));
    }

    #[test]
    fn lark_platform_image_key_detection_supports_v2_v3() {
        assert!(is_lark_platform_image_key("img_v2_abcd"));
        assert!(is_lark_platform_image_key("img_v3_abcd"));
        assert!(!is_lark_platform_image_key("file_v2_abcd"));
        assert!(!is_lark_platform_image_key("https://example.com/a.png"));
    }

    #[test]
    fn lark_detect_image_mime_from_magic_works_for_png_and_jpeg() {
        let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        assert_eq!(detect_lark_image_mime_from_magic(&png), "image/png");

        let jpeg = [0xff, 0xd8, 0xff, 0xdb];
        assert_eq!(detect_lark_image_mime_from_magic(&jpeg), "image/jpeg");
    }

    #[test]
    fn lark_upload_failure_strategy_fallback_builds_text_message() {
        let mut ch = make_channel();
        ch.upload_failure_strategy =
            crate::config::schema::FeishuUploadFailureStrategy::FallbackText;
        let body = ch
            .handle_upload_failure(
                "oc_chat123",
                "[IMAGE:https://example.com/a.png]",
                anyhow::anyhow!("download failed"),
            )
            .expect("fallback_text should turn upload error into text send body");
        assert_eq!(
            body.pointer("/msg_type").and_then(|v| v.as_str()),
            Some("text")
        );
        let text = body
            .get("content")
            .and_then(|v| v.as_str())
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
            .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_default();
        assert_eq!(text, "[IMAGE:https://example.com/a.png]");
    }

    #[test]
    fn lark_upload_failure_strategy_strict_keeps_error() {
        let ch = make_channel();
        let result = ch.handle_upload_failure(
            "oc_chat123",
            "[IMAGE:https://example.com/a.png]",
            anyhow::anyhow!("download failed"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn lark_decode_callback_payload_supports_encrypt_envelope() {
        let plain = serde_json::json!({
            "header": {
                "event_type": "im.message.receive_v1",
                "token": "tk_123"
            },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}",
                    "chat_id": "oc_chat"
                }
            }
        });
        let encrypted = encrypt_lark_callback_payload_for_test("encrypt_key_123", &plain);
        let encrypted_envelope = serde_json::json!({
            "encrypt": encrypted
        });

        let decoded = decode_lark_callback_payload(&encrypted_envelope, "encrypt_key_123").unwrap();
        assert_eq!(decoded, plain);
        assert!(decode_lark_callback_payload(&encrypted_envelope, "").is_err());
    }

    #[test]
    fn lark_verify_payload_token_supports_v1_and_v2() {
        let v1 = serde_json::json!({
            "token": "token_v1"
        });
        let v2 = serde_json::json!({
            "header": {
                "token": "token_v2"
            }
        });
        assert!(verify_lark_payload_token(&v1, "token_v1"));
        assert!(verify_lark_payload_token(&v2, "token_v2"));
        assert!(!verify_lark_payload_token(&v2, "token_mismatch"));
        assert!(verify_lark_payload_token(&v2, ""));
    }

    #[test]
    fn lark_select_ack_reaction_respects_strategy() {
        let mut ch = make_channel();
        ch.ack_reaction = "none".into();
        assert!(ch.select_ack_reaction(None, "hello").is_none());

        ch.ack_reaction = "THUMBSUP".into();
        assert_eq!(
            ch.select_ack_reaction(None, "hello").as_deref(),
            Some("THUMBSUP")
        );

        ch.ack_reaction = "auto".into();
        let picked = ch.select_ack_reaction(None, "hello").unwrap_or_default();
        assert!(!picked.is_empty());
    }

    #[test]
    fn lark_parse_fallback_sender_to_open_id() {
        // When chat_id is missing, sender should fall back to open_id
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            String::new(),
            "token".into(),
            None,
            vec!["*".into()],
            true,
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}",
                    "create_time": "1000"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, "ou_user");
    }

    #[test]
    fn lark_parse_group_message_requires_bot_mention_when_enabled() {
        let ch = with_bot_open_id(
            LarkChannel::new(
                "cli_app123".into(),
                "secret".into(),
                String::new(),
                "token".into(),
                None,
                vec!["*".into()],
                true,
            ),
            "ou_bot_123",
        );

        let no_mention_payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}",
                    "chat_type": "group",
                    "chat_id": "oc_chat",
                    "mentions": []
                }
            }
        });
        assert!(ch.parse_event_payload(&no_mention_payload).is_empty());

        let wrong_mention_payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}",
                    "chat_type": "group",
                    "chat_id": "oc_chat",
                    "mentions": [{ "id": { "open_id": "ou_other" } }]
                }
            }
        });
        assert!(ch.parse_event_payload(&wrong_mention_payload).is_empty());

        let bot_mention_payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}",
                    "chat_type": "group",
                    "chat_id": "oc_chat",
                    "mentions": [{ "id": { "open_id": "ou_bot_123" } }]
                }
            }
        });
        assert_eq!(ch.parse_event_payload(&bot_mention_payload).len(), 1);
    }

    #[test]
    fn lark_parse_group_post_message_accepts_at_when_top_level_mentions_empty() {
        let ch = with_bot_open_id(
            LarkChannel::new(
                "cli_app123".into(),
                "secret".into(),
                String::new(),
                "token".into(),
                None,
                vec!["*".into()],
                true,
            ),
            "ou_bot_123",
        );

        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "post",
                    "chat_type": "group",
                    "chat_id": "oc_chat",
                    "mentions": [],
                    "content": "{\"zh_cn\":{\"title\":\"\",\"content\":[[{\"tag\":\"at\",\"user_id\":\"ou_bot_123\",\"user_name\":\"Bot\"},{\"tag\":\"text\",\"text\":\" hi\"}]]}}"
                }
            }
        });

        assert_eq!(ch.parse_event_payload(&payload).len(), 1);
    }

    #[test]
    fn lark_parse_group_message_allows_without_mention_when_disabled() {
        let ch = LarkChannel::new(
            "cli_app123".into(),
            "secret".into(),
            String::new(),
            "token".into(),
            None,
            vec!["*".into()],
            false,
        );

        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}",
                    "chat_type": "group",
                    "chat_id": "oc_chat",
                    "mentions": []
                }
            }
        });

        assert_eq!(ch.parse_event_payload(&payload).len(), 1);
    }

    #[test]
    fn lark_reaction_url_matches_region() {
        let ch_lark = make_channel();
        assert_eq!(
            ch_lark.message_reaction_url("om_test_message_id"),
            "https://open.larksuite.com/open-apis/im/v1/messages/om_test_message_id/reactions"
        );

        let feishu_cfg = crate::config::schema::FeishuConfig {
            app_id: "cli_app123".into(),
            app_secret: "secret456".into(),
            encrypt_key: None,
            verification_token: Some("vtoken789".into()),
            allowed_users: vec!["*".into()],
            mention_only: false,
            webhook_path: "/feishu".into(),
            verify_signature: true,
            verify_timestamp_window_secs: 300,
            ack_reaction: "auto".into(),
            upload_failure_strategy: crate::config::schema::FeishuUploadFailureStrategy::Strict,
            receive_mode: crate::config::schema::LarkReceiveMode::Webhook,
            port: Some(9898),
        };
        let ch_feishu = LarkChannel::from_feishu_config(&feishu_cfg);
        assert_eq!(
            ch_feishu.message_reaction_url("om_test_message_id"),
            "https://open.feishu.cn/open-apis/im/v1/messages/om_test_message_id/reactions"
        );
    }

    #[test]
    fn lark_reaction_locale_explicit_language_tags() {
        assert_eq!(map_locale_tag("zh-CN"), Some(LarkAckLocale::ZhCn));
        assert_eq!(map_locale_tag("zh_TW"), Some(LarkAckLocale::ZhTw));
        assert_eq!(map_locale_tag("zh-Hant"), Some(LarkAckLocale::ZhTw));
        assert_eq!(map_locale_tag("en-US"), Some(LarkAckLocale::En));
        assert_eq!(map_locale_tag("ja-JP"), Some(LarkAckLocale::Ja));
        assert_eq!(map_locale_tag("fr-FR"), None);
    }

    #[test]
    fn lark_reaction_locale_prefers_explicit_payload_locale() {
        let payload = serde_json::json!({
            "sender": {
                "locale": "ja-JP"
            },
            "message": {
                "content": "{\"text\":\"hello\"}"
            }
        });
        assert_eq!(
            detect_lark_ack_locale(Some(&payload), "你好，世界"),
            LarkAckLocale::Ja
        );
    }

    #[test]
    fn lark_reaction_locale_unsupported_payload_falls_back_to_text_script() {
        let payload = serde_json::json!({
            "sender": {
                "locale": "fr-FR"
            },
            "message": {
                "content": "{\"text\":\"頑張れ\"}"
            }
        });
        assert_eq!(
            detect_lark_ack_locale(Some(&payload), "頑張ってください"),
            LarkAckLocale::Ja
        );
    }

    #[test]
    fn lark_reaction_locale_detects_simplified_and_traditional_text() {
        assert_eq!(
            detect_lark_ack_locale(None, "继续奋斗，今天很强"),
            LarkAckLocale::ZhCn
        );
        assert_eq!(
            detect_lark_ack_locale(None, "繼續奮鬥，今天很強"),
            LarkAckLocale::ZhTw
        );
    }

    #[test]
    fn lark_reaction_locale_defaults_to_english_for_unsupported_text() {
        assert_eq!(
            detect_lark_ack_locale(None, "Bonjour tout le monde"),
            LarkAckLocale::En
        );
    }

    #[test]
    fn random_lark_ack_reaction_respects_detected_locale_pool() {
        let payload = serde_json::json!({
            "sender": {
                "locale": "zh-CN"
            }
        });
        let selected = random_lark_ack_reaction(Some(&payload), "hello");
        assert!(LARK_ACK_REACTIONS_ZH_CN.contains(&selected));

        let payload = serde_json::json!({
            "sender": {
                "locale": "zh-TW"
            }
        });
        let selected = random_lark_ack_reaction(Some(&payload), "hello");
        assert!(LARK_ACK_REACTIONS_ZH_TW.contains(&selected));

        let payload = serde_json::json!({
            "sender": {
                "locale": "en-US"
            }
        });
        let selected = random_lark_ack_reaction(Some(&payload), "hello");
        assert!(LARK_ACK_REACTIONS_EN.contains(&selected));

        let payload = serde_json::json!({
            "sender": {
                "locale": "ja-JP"
            }
        });
        let selected = random_lark_ack_reaction(Some(&payload), "hello");
        assert!(LARK_ACK_REACTIONS_JA.contains(&selected));
    }
}
