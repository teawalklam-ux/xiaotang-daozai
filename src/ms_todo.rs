//! Microsoft To Do 只读连接。
//!
//! 桌面程序是公开客户端：授权码 + PKCE，浏览器回到 `http://localhost`，
//! 令牌请求不带 client_secret。Graph 只发 GET，权限只有委托的 `Tasks.Read`。
//! 令牌进 Windows 凭据管理器；任务缓存不含令牌。本机待办文件不参与这次同步。

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Timelike, Utc};
use eframe::egui;
use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs, io::{Read, Write}, net::TcpListener, path::{Path, PathBuf},
    sync::{mpsc, Arc, atomic::{AtomicBool, Ordering}},
    thread, time::{Duration, Instant},
};
use url::Url;

const SCOPE: &str = "openid profile offline_access Tasks.Read";
const GRAPH: &str = "https://graph.microsoft.com";
#[cfg(windows)]
const SERVICE: &str = "Daozai.MicrosoftToDo";
#[cfg(windows)]
const ACCOUNT_SLOT: &str = "session";
const CACHE_NAME: &str = "ms-todo-cache.json";
const CACHE_VERSION: u32 = 1;
const MAX_LISTS: usize = 20;
const MAX_PAGES: usize = 2;
const MAX_TASKS: usize = 200;
const SYNC_INTERVAL: i64 = 300;
const MANUAL_GAP: i64 = 15;

#[derive(Clone, Debug)]
pub(crate) struct TaskRow {
    pub title: String,
    pub list: String,
    pub due: String,
    pub hover: String,
}

#[derive(Clone, Debug)]
struct Snapshot {
    account: String,
    client_id: String,
    tenant: String,
    fetched_unix: i64,
    lists: usize,
    truncated: bool,
    tasks: Vec<TaskRow>,
}

#[derive(Clone)]
struct Session {
    refresh_token: String,
    access_token: String,
    expires_at: i64,
    account: String,
    client_id: String,
    tenant: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[derive(Debug)]
enum Kind { Auth, Offline, Throttle, Remote }

#[derive(Debug)]
struct Failure {
    message: String,
    retry_after: u64,
    kind: Kind,
    clear_session: bool,
    retry_without_filter: bool,
}

impl Failure {
    fn auth(message: impl Into<String>, clear: bool) -> Self { Self { message: message.into(), retry_after: 0, kind: Kind::Auth, clear_session: clear, retry_without_filter: false } }
    fn offline(message: impl Into<String>) -> Self { Self { message: message.into(), retry_after: 30, kind: Kind::Offline, clear_session: false, retry_without_filter: false } }
    fn throttle(retry_after: u64) -> Self { Self { message: "请求过于频繁，稍后自动重试".into(), retry_after, kind: Kind::Throttle, clear_session: false, retry_without_filter: false } }
    fn remote(message: impl Into<String>) -> Self { Self { message: message.into(), retry_after: 60, kind: Kind::Remote, clear_session: false, retry_without_filter: false } }
    fn unfiltered() -> Self { Self { message: "筛选未完成任务失败".into(), retry_after: 0, kind: Kind::Remote, clear_session: false, retry_without_filter: true } }
}

struct HttpResponse { status: u16, body: Vec<u8>, retry_after: Option<u64> }

trait Http {
    fn post_form(&self, url: &str, body: &str) -> Result<HttpResponse, String>;
    fn get(&self, url: &str, bearer: &str) -> Result<HttpResponse, String>;
}

trait Secrets {
    fn load(&self) -> Result<Option<Session>, String>;
    fn save(&self, session: &Session) -> Result<(), String>;
    fn delete(&self) -> Result<(), String>;
}

struct Job<T> {
    rx: mpsc::Receiver<T>,
    cancel: Arc<AtomicBool>,
}

impl<T> Drop for Job<T> {
    fn drop(&mut self) { self.cancel.store(true, Ordering::Relaxed); }
}

#[cfg_attr(not(windows), allow(dead_code))]
enum Phase { Idle, Login, Sync }

pub(crate) struct State {
    tasks: Vec<TaskRow>,
    list_count: usize,
    truncated: bool,
    account: Option<String>,
    fetched_unix: Option<i64>,
    signed_in: bool,
    offline: bool,
    status: String,
    phase: Phase,
    next_sync: i64,
    not_before: i64,
    last_attempt: i64,
    failures: u32,
    login: Option<Job<Result<String, String>>>,
    sync: Option<Job<Result<Snapshot, Failure>>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            tasks: Vec::new(), list_count: 0, truncated: false, account: None, fetched_unix: None,
            signed_in: false, offline: false, status: String::new(), phase: Phase::Idle,
            next_sync: 0, not_before: 0, last_attempt: 0, failures: 0, login: None, sync: None,
        }
    }
}

impl State {
    pub(crate) fn restore(settings: &crate::model::Settings) -> Self {
        #[cfg(not(all(windows, not(test))))]
        { let _ = settings; Self::default() }
        #[cfg(all(windows, not(test)))]
        {
            let mut state = Self::default();
            let client = settings.ms_todo_client_id.trim().to_ascii_lowercase();
            let tenant = authority_of(settings);
            match WindowsSecrets.load() {
                Ok(Some(session)) if session.client_id == client && session.tenant == tenant => {
                    state.signed_in = true;
                    state.account = Some(session.account.clone());
                    if let Some(cache) = load_cache(&cache_path()) {
                        if cache.account == session.account && cache.client_id == session.client_id && cache.tenant == session.tenant {
                            state.offline = true;
                            state.install(cache);
                        } else {
                            let _ = fs::remove_file(cache_path());
                        }
                    }
                }
                Ok(_) => { let _ = fs::remove_file(cache_path()); }
                Err(error) => state.status = error,
            }
            state
        }
    }

    pub(crate) fn rows(&self) -> &[TaskRow] { &self.tasks }
    pub(crate) fn task_count(&self) -> usize { self.tasks.len() }
    pub(crate) fn busy(&self) -> bool { self.login.is_some() || self.sync.is_some() }
    pub(crate) fn signed_in(&self) -> bool { self.signed_in }
    pub(crate) fn account(&self) -> Option<&str> { self.account.as_deref() }
    pub(crate) fn offline(&self) -> bool { self.offline }
    pub(crate) fn truncated(&self) -> bool { self.truncated }

    pub(crate) fn subtitle(&self) -> &'static str {
        if self.signed_in { "未完成" } else if !self.tasks.is_empty() { "上次读取" } else { "未连接" }
    }

    pub(crate) fn caption(&self) -> String {
        if self.busy() { return "正在读取".into(); }
        if !self.status.is_empty() { return trim_chars(&self.status, 18); }
        if self.signed_in { return format!("{} 个清单", self.list_count); }
        if self.fetched_unix.is_some() { return "上次读取".into(); }
        "未连接".into()
    }

    pub(crate) fn fetched_label(&self) -> String {
        self.fetched_unix.and_then(format_local_time).unwrap_or_default()
    }

    /// 界面测试注入只读列表，不写凭据、不联网。
    pub(crate) fn preview(&mut self, tasks: Vec<TaskRow>, signed_in: bool) {
        self.list_count = 1;
        self.tasks = tasks;
        self.signed_in = signed_in;
        self.offline = false;
        self.status.clear();
    }

    pub(crate) fn tick(&mut self, settings: &crate::model::Settings, enabled: bool, now: i64) {
        if !enabled {
            if self.busy() {
                self.stop_jobs();
                self.status = "待办页已关闭，已停止读取".into();
            }
            return;
        }
        self.poll(now);
        if !self.signed_in || self.busy() { return; }
        let Ok((client, tenant)) = prepare(&settings.ms_todo_client_id, &settings.ms_todo_accounts, &settings.ms_todo_tenant_id) else {
            self.status = "应用 ID 或目录 ID 无效，请修改后再连接".into();
            return;
        };
        let _ = self.request_sync(false, now, client, tenant);
    }

    pub(crate) fn connect(&mut self, client_id: &str, accounts: &str, tenant_id: &str) -> Result<String, String> {
        if self.busy() { return Err("正在连接，请稍候".into()); }
        let (client_id, tenant) = prepare(client_id, accounts, tenant_id)?;
        #[cfg(not(windows))]
        { let _ = (client_id, tenant); return Err("安全保存 Microsoft 登录需要 Windows 凭据管理器，当前系统无法连接".into()); }
        #[cfg(windows)]
        { self.connect_windows(client_id, tenant) }
    }

    pub(crate) fn request_sync(&mut self, manual: bool, now: i64, client_id: String, tenant: String) -> Result<(), String> {
        if self.busy() { return Err("正在读取，请稍候".into()); }
        if !self.signed_in { return Err("尚未连接 Microsoft 待办".into()); }
        if now < self.not_before { return Err("请求过于频繁，请稍后再试".into()); }
        if manual && now.saturating_sub(self.last_attempt) < MANUAL_GAP { return Err("刚刚请求过，请稍后再试".into()); }
        if !manual && now < self.next_sync { return Ok(()); }
        self.spawn_sync(client_id, tenant, now)
    }

    pub(crate) fn disconnect(&mut self) {
        self.stop_jobs();
        #[cfg(windows)]
        { let _ = WindowsSecrets.delete(); }
        let _ = fs::remove_file(cache_path());
        *self = Self::default();
        self.status = "已断开，登录和本机缓存都已清除".into();
    }

    pub(crate) fn cancel(&mut self) {
        self.stop_jobs();
        self.status = "登录已取消".into();
    }

    fn stop_jobs(&mut self) {
        self.login.take();
        self.sync.take();
        self.phase = Phase::Idle;
    }

    fn poll(&mut self, now: i64) {
        if let Some(job) = &self.login {
            match job.rx.try_recv() {
                Ok(result) => {
                    self.login = None;
                    self.phase = Phase::Idle;
                    match result {
                        Ok(account) => {
                            self.signed_in = true;
                            self.account = Some(account);
                            self.offline = false;
                            self.status = "已连接，正在读取未完成任务".into();
                            self.next_sync = 0;
                            self.not_before = 0;
                            self.failures = 0;
                        }
                        Err(error) => self.status = error,
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    let cancelled = self.login.as_ref().is_some_and(|job| job.cancel.load(Ordering::Relaxed));
                    self.login = None;
                    self.phase = Phase::Idle;
                    if !cancelled && self.status.is_empty() { self.status = "登录已结束，请重试".into(); }
                }
            }
        }
        if let Some(job) = &self.sync {
            match job.rx.try_recv() {
                Ok(result) => {
                    self.sync = None;
                    self.phase = Phase::Idle;
                    match result {
                        Ok(snapshot) => {
                            self.account = Some(snapshot.account.clone());
                            self.signed_in = true;
                            self.offline = false;
                            self.failures = 0;
                            self.not_before = 0;
                            self.next_sync = now + SYNC_INTERVAL;
                            self.status = if snapshot.truncated { "还有更多未完成任务，请在 Microsoft To Do 中查看".into() } else { String::new() };
                            self.install(snapshot);
                        }
                        Err(failure) => self.note_failure(failure, now),
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    let cancelled = self.sync.as_ref().is_some_and(|job| job.cancel.load(Ordering::Relaxed));
                    self.sync = None;
                    self.phase = Phase::Idle;
                    if !cancelled { self.status = "读取已中断，请重试".into(); self.next_sync = now + 30; }
                }
            }
        }
    }

    fn note_failure(&mut self, failure: Failure, now: i64) {
        if failure.clear_session {
            #[cfg(windows)]
            { let _ = WindowsSecrets.delete(); }
            self.signed_in = false;
        }
        self.offline = failure.kind == Kind::Offline;
        self.failures = self.failures.saturating_add(1);
        let wait = match failure.kind {
            Kind::Throttle => clamp_wait(Some(failure.retry_after)),
            Kind::Offline => backoff(self.failures).max(30),
            Kind::Auth => 0,
            Kind::Remote => backoff(self.failures).max(failure.retry_after),
        };
        self.not_before = now + wait as i64;
        self.next_sync = self.not_before;
        self.status = failure.message;
    }

    fn install(&mut self, snapshot: Snapshot) {
        self.list_count = snapshot.lists;
        self.truncated = snapshot.truncated;
        self.fetched_unix = Some(snapshot.fetched_unix);
        self.tasks = snapshot.tasks;
    }

    fn spawn_sync(&mut self, client_id: String, tenant: String, now: i64) -> Result<(), String> {
        if cfg!(test) { self.last_attempt = now; self.next_sync = now + SYNC_INTERVAL; return Ok(()); }
        #[cfg(windows)]
        { return self.spawn_sync_windows(client_id, tenant, now); }
        #[cfg(not(windows))]
        { let _ = (client_id, tenant, now); Err("安全保存 Microsoft 登录需要 Windows 凭据管理器".into()) }
    }

    #[cfg(windows)]
    fn connect_windows(&mut self, client_id: String, tenant: String) -> Result<String, String> {
        use ring::rand::SecureRandom;
        let (port, listeners) = bind_loopback()?;
        let mut seed = [0u8; 48];
        ring::rand::SystemRandom::new().fill(&mut seed).map_err(|_| "无法创建安全登录，请重试".to_string())?;
        let verifier = b64(&seed[..32]);
        let challenge = pkce_challenge(&verifier);
        let state = b64(&seed[32..]);
        let redirect = format!("http://localhost:{port}/");
        let url = authorization_url(&client_id, &tenant, &redirect, &challenge, &state)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let flag = cancel.clone();
        thread::spawn(move || {
            let result = (|| {
                let code = wait_callback(listeners, &state, &flag, Instant::now() + Duration::from_secs(180))?;
                if flag.load(Ordering::Relaxed) { return Err("登录已取消".into()); }
                finish_login(&UreqHttp, &WindowsSecrets, &client_id, &tenant, &redirect, &verifier, &code, Utc::now().timestamp(), &flag)
            })();
            if !flag.load(Ordering::Relaxed) { let _ = tx.send(result); }
        });
        self.login = Some(Job { rx, cancel });
        self.phase = Phase::Login;
        self.status = "请在浏览器中选择账号，并只同意读取待办".into();
        Ok(url)
    }

    #[cfg(windows)]
    fn spawn_sync_windows(&mut self, client_id: String, tenant: String, now: i64) -> Result<(), String> {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let flag = cancel.clone();
        thread::spawn(move || {
            let result = run_sync(&UreqHttp, &WindowsSecrets, &client_id, &tenant, GRAPH, Utc::now().timestamp(), &flag);
            if flag.load(Ordering::Relaxed) {
                let _ = fs::remove_file(cache_path());
                return;
            }
            if let Ok(snapshot) = &result { let _ = save_cache(&cache_path(), snapshot); }
            if flag.load(Ordering::Relaxed) {
                let _ = fs::remove_file(cache_path());
                return;
            }
            let _ = tx.send(result);
        });
        self.sync = Some(Job { rx, cancel });
        self.phase = Phase::Sync;
        self.last_attempt = now;
        Ok(())
    }
}

fn cache_path() -> PathBuf { crate::model::config_dir().join(CACHE_NAME) }

pub(crate) fn prepare(client_id: &str, accounts: &str, tenant_id: &str) -> Result<(String, String), String> {
    let client_id = client_id.trim().trim_matches(|c| c == '{' || c == '}').to_ascii_lowercase();
    if !is_guid(&client_id) { return Err("请填写本应用在 Microsoft Entra 中的应用程序（客户端）ID".into()); }
    let tenant_id = tenant_id.trim().trim_matches(|c| c == '{' || c == '}').to_ascii_lowercase();
    if !tenant_id.is_empty() && !is_guid(&tenant_id) {
        return Err("目录（租户）ID 需要是 GUID。单租户应用才填写，留空则使用上方的账号范围".into());
    }
    let tenant = if !tenant_id.is_empty() { tenant_id } else { account_audience(accounts).to_string() };
    Ok((client_id, tenant))
}

#[cfg(windows)]
fn authority_of(settings: &crate::model::Settings) -> String {
    prepare(&settings.ms_todo_client_id, &settings.ms_todo_accounts, &settings.ms_todo_tenant_id)
        .map(|(_, tenant)| tenant)
        .unwrap_or_else(|_| account_audience(&settings.ms_todo_accounts).to_string())
}

fn account_audience(accounts: &str) -> &'static str {
    match accounts.trim() {
        "organizations" => "organizations",
        "consumers" => "consumers",
        _ => "common",
    }
}

fn is_guid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 { return false; }
    bytes.iter().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) { *byte == b'-' } else { byte.is_ascii_hexdigit() }
    })
}

fn b64(bytes: &[u8]) -> String { URL_SAFE_NO_PAD.encode(bytes) }

fn pkce_challenge(verifier: &str) -> String { b64(digest(&SHA256, verifier.as_bytes()).as_ref()) }

fn authorization_url(client_id: &str, tenant: &str, redirect: &str, challenge: &str, state: &str) -> Result<String, String> {
    if !matches!(tenant, "common" | "organizations" | "consumers") && !is_guid(tenant) {
        return Err("账号范围无效".into());
    }
    let mut url = Url::parse(&format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize")).map_err(|_| "登录地址无效")?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect)
        .append_pair("response_mode", "form_post")
        .append_pair("scope", SCOPE)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("prompt", "select_account");
    Ok(url.into())
}

fn token_url(tenant: &str) -> Result<String, String> {
    if !matches!(tenant, "common" | "organizations" | "consumers") && !is_guid(tenant) {
        return Err("账号范围无效".into());
    }
    Ok(format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"))
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs { serializer.append_pair(key, value); }
    serializer.finish()
}

fn allow_token_url(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else { return false };
    if url.scheme() != "https" || url.host_str() != Some("login.microsoftonline.com") || url.username() != "" || url.password().is_some() {
        return false;
    }
    let mut parts = url.path_segments().into_iter().flatten();
    let tenant = parts.next().unwrap_or("");
    let rest: Vec<_> = parts.collect();
    (matches!(tenant, "common" | "organizations" | "consumers") || is_guid(tenant)) && rest == ["oauth2", "v2.0", "token"]
}

fn trusted_graph(url: &str, graph_base: &str) -> Option<String> {
    let base = Url::parse(graph_base).ok()?;
    let url = Url::parse(url).ok()?;
    if url.scheme() != base.scheme() || url.host_str() != base.host_str() || url.port_or_known_default() != base.port_or_known_default() {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() { return None; }
    if !url.path().starts_with("/v1.0/me/todo/") { return None; }
    Some(url.to_string())
}

fn loopback_http(url: &str) -> bool {
    Url::parse(url).ok().is_some_and(|url| {
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))
    })
}

fn token_target_allowed(url: &str) -> bool {
    if allow_token_url(url) { return true; }
    cfg!(test) && loopback_http(url)
}

fn graph_target_allowed(url: &str, graph_base: &str) -> bool {
    if trusted_graph(url, graph_base).is_some() { return true; }
    cfg!(test) && loopback_http(url) && trusted_graph(url, graph_base).is_some()
}

struct UreqHttp;

impl Http for UreqHttp {
    fn post_form(&self, url: &str, body: &str) -> Result<HttpResponse, String> {
        if !token_target_allowed(url) { return Err("已拒绝向 Microsoft 登录地址以外发送凭据".into()); }
        if body.contains("client_secret") { return Err("已拒绝发送客户端机密".into()); }
        ureq_send(http_agent().post(url).set("Content-Type", "application/x-www-form-urlencoded").set("Accept", "application/json").send_string(body))
    }
    fn get(&self, url: &str, bearer: &str) -> Result<HttpResponse, String> {
        if trusted_graph(url, GRAPH).is_none() && !(cfg!(test) && loopback_http(url)) {
            return Err("已拒绝向 Microsoft To Do 以外的地址发送登录".into());
        }
        ureq_send(http_agent().get(url).set("Authorization", &format!("Bearer {bearer}")).set("Accept", "application/json").call())
    }
}

fn http_agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).timeout_connect(Duration::from_secs(10)).redirects(0).try_proxy_from_env(false);
    let proxy = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"].iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        .or_else(crate::quota::system_proxy);
    if let Some(proxy) = proxy.and_then(|value| ureq::Proxy::new(value).ok()) { builder = builder.proxy(proxy); }
    builder.build()
}

fn ureq_send(result: Result<ureq::Response, ureq::Error>) -> Result<HttpResponse, String> {
    match result {
        Ok(response) => read_response(response.status(), response),
        Err(ureq::Error::Status(status, response)) => read_response(status, response),
        Err(_) => Err("无法连接 Microsoft，请检查网络或代理".into()),
    }
}

fn read_response(status: u16, response: ureq::Response) -> Result<HttpResponse, String> {
    let retry_after = response.header("retry-after").and_then(|value| value.parse().ok());
    let mut body = Vec::new();
    response.into_reader().take(1024 * 1024 + 1).read_to_end(&mut body).map_err(|_| "读取 Microsoft 响应失败".to_string())?;
    if body.len() > 1024 * 1024 { return Err("Microsoft 响应过大".into()); }
    Ok(HttpResponse { status, body, retry_after })
}

#[cfg(windows)]
struct WindowsSecrets;

#[cfg(windows)]
impl Secrets for WindowsSecrets {
    fn load(&self) -> Result<Option<Session>, String> {
        let entry = credential()?;
        match entry.get_password() {
            Ok(raw) => Ok(Some(session_from_json(&raw)?)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err("无法读取 Windows 凭据管理器".into()),
        }
    }
    fn save(&self, session: &Session) -> Result<(), String> {
        credential()?.set_password(&session_json(session)).map_err(|_| "无法把登录写入 Windows 凭据管理器".to_string())
    }
    fn delete(&self) -> Result<(), String> {
        match credential()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err("无法从 Windows 凭据管理器删除登录".into()),
        }
    }
}

#[cfg(windows)]
fn credential() -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, ACCOUNT_SLOT).map_err(|_| "无法打开 Windows 凭据管理器".to_string())
}

fn session_json(session: &Session) -> String {
    json!({
        "refresh_token": session.refresh_token,
        "access_token": session.access_token,
        "expires_at": session.expires_at,
        "account": session.account,
        "client_id": session.client_id,
        "tenant": session.tenant,
    }).to_string()
}

fn session_from_json(raw: &str) -> Result<Session, String> {
    let value: Value = serde_json::from_str(raw).map_err(|_| "已保存的登录无法读取，请重新连接")?;
    let mut session = token_fields(&value, None, "", "")?;
    session.expires_at = value.get("expires_at").and_then(Value::as_i64).ok_or("已保存的登录无法读取，请重新连接")?;
    session.client_id = text_field(&value, &["client_id"]).ok_or("已保存的登录无法读取，请重新连接")?;
    session.tenant = text_field(&value, &["tenant"]).ok_or("已保存的登录无法读取，请重新连接")?;
    if !is_guid(&session.client_id) || (!matches!(session.tenant.as_str(), "common" | "organizations" | "consumers") && !is_guid(&session.tenant)) {
        return Err("已保存的登录无法读取，请重新连接".into());
    }
    Ok(session)
}

fn token_fields(value: &Value, previous: Option<&Session>, client_id: &str, tenant: &str) -> Result<Session, String> {
    let access_token = text_field(value, &["access_token", "accessToken"]).ok_or("Microsoft 没有返回访问令牌")?;
    if access_token.len() > 16384 || access_token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("Microsoft 返回的登录无法使用，请重新连接".into());
    }
    let refresh_token = text_field(value, &["refresh_token", "refreshToken"])
        .or_else(|| previous.map(|session| session.refresh_token.clone()))
        .ok_or("Microsoft 没有返回可刷新的登录。请重新连接，并同意离线访问")?;
    if refresh_token.len() > 16384 || refresh_token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("Microsoft 返回的登录无法使用，请重新连接".into());
    }
    if let Some(scope) = value.get("scope").and_then(Value::as_str) {
        if !scope_is_readonly(scope) { return Err("授权范围不是只读的 Tasks.Read，岛仔没有保存登录。请在应用注册里只添加 Tasks.Read 后重新连接".into()); }
    }
    let account = value.get("id_token").and_then(Value::as_str).and_then(account_from_id_token)
        .or_else(|| text_field(value, &["account"]))
        .or_else(|| previous.map(|session| session.account.clone()))
        .unwrap_or_else(|| "Microsoft 账号".into());
    let client_id = if client_id.is_empty() { text_field(value, &["client_id"]).unwrap_or_default() } else { client_id.to_string() };
    let tenant = if tenant.is_empty() { text_field(value, &["tenant"]).unwrap_or_default() } else { tenant.to_string() };
    Ok(Session { refresh_token, access_token, expires_at: 0, account, client_id, tenant })
}

fn text_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value.get(*key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_string))
}

fn scope_is_readonly(scope: &str) -> bool {
    let parts: Vec<_> = scope.split_whitespace().collect();
    let read = parts.iter().any(|part| part.eq_ignore_ascii_case("Tasks.Read"));
    let blocked = parts.iter().any(|part| {
        let part = part.to_ascii_lowercase();
        part.contains("readwrite") || part.ends_with(".all") || part.starts_with("mail.") || part == "tasks.read.shared"
    });
    read && !blocked
}

fn account_from_id_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    if bytes.len() > 8192 { return None; }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    for key in ["preferred_username", "email", "upn"] {
        if let Some(account) = value.get(key).and_then(Value::as_str) {
            let account = account.trim();
            if (3..=320).contains(&account.len()) && account.contains('@') && !account.contains('/') && account.chars().all(|c| !c.is_control() && c != ' ') {
                return Some(account.to_string());
            }
        }
    }
    None
}

fn finish_login(http: &impl Http, secrets: &impl Secrets, client_id: &str, tenant: &str, redirect: &str, verifier: &str, code: &str, now: i64, cancel: &AtomicBool) -> Result<String, String> {
    if cancel.load(Ordering::Relaxed) { return Err("登录已取消".into()); }
    let body = form(&[("client_id", client_id), ("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect), ("code_verifier", verifier), ("scope", SCOPE)]);
    let session = token_session(http, &token_url(tenant)?, &body, None, client_id, tenant, now).map_err(|failure| failure.message)?;
    if cancel.load(Ordering::Relaxed) { return Err("登录已取消".into()); }
    secrets.save(&session)?;
    if cancel.load(Ordering::Relaxed) {
        let _ = secrets.delete();
        return Err("登录已取消".into());
    }
    Ok(session.account)
}

fn token_session(http: &impl Http, url: &str, body: &str, previous: Option<&Session>, client_id: &str, tenant: &str, now: i64) -> Result<Session, Failure> {
    if !token_target_allowed(url) { return Err(Failure::remote("已拒绝向 Microsoft 登录地址以外发送凭据")); }
    let response = http.post_form(url, body).map_err(Failure::offline)?;
    if response.status == 429 || response.status == 503 { return Err(Failure::throttle(clamp_wait(response.retry_after))); }
    if !(200..300).contains(&response.status) {
        let code = oauth_error_code(&response.body).unwrap_or_default();
        let clear = code == "invalid_grant";
        return Err(Failure::auth(explain_oauth(&code), clear));
    }
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| Failure::remote("Microsoft 登录没有返回可识别的结果"))?;
    let mut session = token_fields(&value, previous, client_id, tenant).map_err(|message| Failure::auth(message, false))?;
    let expires_in = value.get("expires_in").and_then(Value::as_i64).unwrap_or(3600).clamp(60, 86_400);
    session.expires_at = now + expires_in - 60;
    Ok(session)
}

fn explain_oauth(code: &str) -> String {
    let text = match code {
        "access_denied" => "已取消 Microsoft 授权",
        "consent_required" | "interaction_required" => "需要在浏览器中同意读取待办",
        "invalid_grant" => "登录已失效，请重新连接",
        "invalid_client" => "应用 ID 未被接受。请核对应用程序（客户端）ID，并确认注册的是公开客户端",
        "unauthorized_client" => "这个应用还不能用公开客户端登录。请添加「移动和桌面应用程序」，并允许公共客户端流",
        "invalid_request" => "登录请求被拒绝，请重新连接",
        "server_error" | "temporarily_unavailable" => "Microsoft 登录暂时不可用，请稍后再试",
        _ => "Microsoft 登录没有完成",
    };
    if code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && (1..=40).contains(&code.len()) && !text.contains(code) {
        format!("{text}（{code}）")
    } else {
        text.into()
    }
}

fn oauth_error_code(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let code = value.get("error").and_then(Value::as_str)?;
    let code: String = code.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(40).collect();
    (!code.is_empty()).then_some(code)
}

fn run_sync(http: &impl Http, secrets: &impl Secrets, client_id: &str, tenant: &str, graph: &str, now: i64, cancel: &AtomicBool) -> Result<Snapshot, Failure> {
    let Some(mut session) = secrets.load().map_err(|error| Failure::remote(error))? else {
        return Err(Failure::auth("尚未连接 Microsoft 待办", false));
    };
    if session.client_id != client_id || session.tenant != tenant {
        return Err(Failure::auth("应用 ID 或账号范围已改变，请重新连接", false));
    }
    if session.expires_at <= now + 120 {
        session = refresh(http, secrets, &session, client_id, tenant, now, cancel)?;
    }
    match fetch_all(http, &session, graph, now, cancel) {
        Err(failure) if failure.kind == Kind::Auth && !failure.clear_session => {
            session = refresh(http, secrets, &session, client_id, tenant, now, cancel)?;
            fetch_all(http, &session, graph, now, cancel)
        }
        other => other,
    }
}

fn refresh(http: &impl Http, secrets: &impl Secrets, session: &Session, client_id: &str, tenant: &str, now: i64, cancel: &AtomicBool) -> Result<Session, Failure> {
    if cancel.load(Ordering::Relaxed) { return Err(Failure::remote("已取消")); }
    let body = form(&[("client_id", client_id), ("grant_type", "refresh_token"), ("refresh_token", &session.refresh_token), ("scope", SCOPE)]);
    let refreshed = match token_session(http, &token_url(tenant).map_err(Failure::remote)?, &body, Some(session), client_id, tenant, now) {
        Ok(session) => session,
        Err(failure) if failure.clear_session => {
            let _ = secrets.delete();
            return Err(failure);
        }
        Err(failure) => return Err(failure),
    };
    if cancel.load(Ordering::Relaxed) { return Err(Failure::remote("已取消")); }
    secrets.save(&refreshed).map_err(|error| Failure::remote(error))?;
    if cancel.load(Ordering::Relaxed) {
        let _ = secrets.delete();
        return Err(Failure::remote("已取消"));
    }
    Ok(refreshed)
}

fn fetch_all(http: &impl Http, session: &Session, graph: &str, now: i64, cancel: &AtomicBool) -> Result<Snapshot, Failure> {
    if cancel.load(Ordering::Relaxed) { return Err(Failure::remote("已取消")); }
    let local = local_offset();
    let today = Utc.timestamp_opt(now, 0).single().unwrap_or_else(Utc::now).with_timezone(&local).date_naive();
    let lists_url = format!("{graph}/v1.0/me/todo/lists?$select=id,displayName,isOwner&$top={MAX_LISTS}");
    let lists_page = graph_json(http, &lists_url, &session.access_token, graph, false)?;
    let mut lists = parse_lists(&lists_page);
    if lists.len() > MAX_LISTS { lists.truncate(MAX_LISTS); }
    let mut tasks = Vec::new();
    let mut truncated = lists_page.get("@odata.nextLink").and_then(Value::as_str).is_some() || lists.len() == MAX_LISTS;
    for list in &lists {
        if cancel.load(Ordering::Relaxed) { return Err(Failure::remote("已取消")); }
        if tasks.len() >= MAX_TASKS { truncated = true; break; }
        let mut url = Some(tasks_url(graph, &list.id));
        for _ in 0..MAX_PAGES {
            let Some(current) = url.take() else { break };
            if tasks.len() >= MAX_TASKS { truncated = true; break; }
            let page = match graph_json(http, &current, &session.access_token, graph, current.contains("$filter=") || current.contains("%24filter=")) {
                Err(failure) if failure.retry_without_filter => graph_json(http, &tasks_url_unfiltered(graph, &list.id), &session.access_token, graph, false)?,
                other => other?,
            };
            let mut page_tasks = parse_tasks(&page, list, today, local);
            if tasks.len() + page_tasks.len() > MAX_TASKS {
                page_tasks.truncate(MAX_TASKS - tasks.len());
                truncated = true;
            }
            tasks.append(&mut page_tasks);
            url = page.get("@odata.nextLink").and_then(Value::as_str).map(|next| {
                trusted_graph(next, graph).ok_or_else(|| Failure::remote("待办服务返回了异常的后续地址，已停止读取"))
            }).transpose()?;
            if url.is_some() && tasks.len() >= MAX_TASKS { truncated = true; }
        }
    }
    tasks.sort_by(|a, b| a.sort.cmp(&b.sort).then_with(|| a.title.cmp(&b.title)));
    let tasks = tasks.into_iter().map(|task| TaskRow { title: task.title, list: task.list, due: task.due, hover: task.hover }).collect();
    Ok(Snapshot { account: session.account.clone(), client_id: session.client_id.clone(), tenant: session.tenant.clone(), fetched_unix: now, lists: lists.len(), truncated, tasks })
}

struct Listed { id: String, name: String, shared: bool }
struct ParsedTask { title: String, list: String, due: String, hover: String, sort: String }

fn parse_lists(value: &Value) -> Vec<Listed> {
    value.get("value").and_then(Value::as_array).into_iter().flatten().filter_map(|item| {
        let id = item.get("id").and_then(Value::as_str)?.trim();
        if id.is_empty() || id.chars().any(|c| c.is_control() || c.is_whitespace()) || id.len() > 512 { return None; }
        let name = clean_text(item.get("displayName").and_then(Value::as_str).unwrap_or("未命名清单"), 80);
        let shared = item.get("isOwner").and_then(Value::as_bool) == Some(false);
        Some(Listed { id: id.to_string(), name, shared })
    }).take(MAX_LISTS).collect()
}

fn parse_tasks(value: &Value, list: &Listed, today: NaiveDate, local: FixedOffset) -> Vec<ParsedTask> {
    value.get("value").and_then(Value::as_array).into_iter().flatten().filter_map(|item| {
        let status = item.get("status").and_then(Value::as_str).unwrap_or("notStarted");
        if status.eq_ignore_ascii_case("completed") { return None; }
        let title = clean_text(item.get("title").and_then(Value::as_str).unwrap_or("（无标题）"), 200);
        let due = item.get("dueDateTime").and_then(|due| format_due(due.get("dateTime").and_then(Value::as_str).unwrap_or(""), due.get("timeZone").and_then(Value::as_str).unwrap_or(""), today, local));
        let status_label = match status {
            "inProgress" => "进行中",
            "waitingOnOthers" => "等待他人",
            "deferred" => "已推迟",
            _ => "未开始",
        };
        let important = item.get("importance").and_then(Value::as_str).is_some_and(|value| value.eq_ignore_ascii_case("high"));
        let mut hover = format!("{}\n{}", list.name, status_label);
        if list.shared { hover.push_str("\n共享清单"); }
        if important { hover.push_str("\n重要"); }
        let (due_short, sort) = if let Some(due) = &due {
            hover.push_str("\n到期 ");
            hover.push_str(&due.full);
            if !due.note.is_empty() { hover.push('\n'); hover.push_str(&due.note); }
            (due.short.clone(), due.sort.clone())
        } else {
            (String::new(), "9999-99-99 99:99".into())
        };
        Some(ParsedTask { title, list: list.name.clone(), due: due_short, hover, sort })
    }).collect()
}

struct Due { short: String, full: String, sort: String, note: String }

fn format_due(date_time: &str, time_zone: &str, today: NaiveDate, local: FixedOffset) -> Option<Due> {
    let zulu = date_time.trim().ends_with('Z');
    let date_time = date_time.trim().trim_end_matches('Z');
    let naive = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"].iter().find_map(|format| NaiveDateTime::parse_from_str(date_time, format).ok())?;
    let zone = if zulu && time_zone.trim().is_empty() { "UTC" } else { time_zone.trim() };
    let midnight = naive.time() == NaiveTime::from_hms_opt(0, 0, 0)?;
    if midnight && (zone.is_empty() || zone.eq_ignore_ascii_case("utc")) {
        return Some(due_from(naive.date(), None, today, ""));
    }
    if let Some(shown) = convert_due(naive, zone, local) {
        return Some(due_from(shown.0, Some(shown.1), today, ""));
    }
    let note = if zone.is_empty() { String::new() } else { format!("按待办中的时间显示，未换算时区 {zone}") };
    Some(due_from(naive.date(), Some(naive.time()), today, &note))
}

fn convert_due(naive: NaiveDateTime, zone: &str, local: FixedOffset) -> Option<(NaiveDate, NaiveTime)> {
    if zone.eq_ignore_ascii_case("utc") {
        let shown = Utc.from_utc_datetime(&naive).with_timezone(&local);
        return Some((shown.date_naive(), shown.time()));
    }
    let name = windows_zone(zone).unwrap_or(zone);
    let tz: chrono_tz::Tz = name.parse().ok()?;
    let shown = naive.and_local_timezone(tz).single()?.with_timezone(&local);
    Some((shown.date_naive(), shown.time()))
}

fn due_from(date: NaiveDate, time: Option<NaiveTime>, today: NaiveDate, note: &str) -> Due {
    let date_text = if date.year() == today.year() { format!("{:02}-{:02}", date.month(), date.day()) } else { date.format("%Y-%m-%d").to_string() };
    let (short, sort_time, full_time) = match time {
        Some(time) => {
            let clock = format!("{:02}:{:02}", time.hour(), time.minute());
            (format!("{date_text} {clock}"), clock.clone(), Some(clock))
        }
        None => (date_text.clone(), "24:00".into(), None),
    };
    let full = match full_time {
        Some(clock) => format!("{} {clock}", date.format("%Y-%m-%d")),
        None => date.format("%Y-%m-%d").to_string(),
    };
    Due { short, full, sort: format!("{} {sort_time}", date.format("%Y-%m-%d")), note: note.to_string() }
}

fn windows_zone(name: &str) -> Option<&'static str> {
    Some(match name {
        "China Standard Time" => "Asia/Shanghai",
        "Taipei Standard Time" => "Asia/Taipei",
        "Tokyo Standard Time" => "Asia/Tokyo",
        "Singapore Standard Time" => "Asia/Singapore",
        "Korea Standard Time" => "Asia/Seoul",
        "Pacific Standard Time" => "America/Los_Angeles",
        "Mountain Standard Time" => "America/Denver",
        "Central Standard Time" => "America/Chicago",
        "Eastern Standard Time" => "America/New_York",
        "GMT Standard Time" => "Europe/London",
        "W. Europe Standard Time" => "Europe/Berlin",
        "AUS Eastern Standard Time" => "Australia/Sydney",
        "India Standard Time" => "Asia/Kolkata",
        _ => return None,
    })
}

fn tasks_url(graph: &str, id: &str) -> String {
    let mut url = Url::parse(&format!("{graph}/v1.0/me/todo/lists/placeholder/tasks")).expect("graph");
    url.path_segments_mut().expect("path").pop().pop();
    url.path_segments_mut().expect("path").push(id).push("tasks");
    url.query_pairs_mut().append_pair("$filter", "status ne 'completed'").append_pair("$select", "id,title,status,dueDateTime,importance").append_pair("$top", "40");
    url.into()
}

fn tasks_url_unfiltered(graph: &str, id: &str) -> String {
    let mut url = Url::parse(&tasks_url(graph, id)).expect("tasks");
    let pairs: Vec<(String, String)> = url.query_pairs().filter(|(key, _)| key != "$filter").map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
    url.set_query(None);
    for (key, value) in pairs { url.query_pairs_mut().append_pair(&key, &value); }
    url.into()
}

fn graph_json(http: &impl Http, url: &str, bearer: &str, graph: &str, filtered: bool) -> Result<Value, Failure> {
    let url = trusted_graph(url, graph).ok_or_else(|| Failure::remote("已拒绝向 Microsoft To Do 以外的地址发送登录"))?;
    if !graph_target_allowed(&url, graph) { return Err(Failure::remote("已拒绝向 Microsoft To Do 以外的地址发送登录")); }
    let response = http.get(&url, bearer).map_err(Failure::offline)?;
    interpret_graph(response, filtered)
}

fn interpret_graph(response: HttpResponse, filtered: bool) -> Result<Value, Failure> {
    if response.status == 429 || response.status == 503 { return Err(Failure::throttle(clamp_wait(response.retry_after))); }
    match response.status {
        200..=299 => serde_json::from_slice(&response.body).map_err(|_| Failure::remote("Microsoft To Do 没有返回可识别的结果")),
        401 => Err(Failure::auth("登录已失效，请重新连接", false)),
        403 => Err(Failure::remote("没有读取权限。请确认已同意 Tasks.Read，且该账号可以使用 Microsoft To Do")),
        404 => Err(Failure::remote("这个账号没有返回 To Do。全球云以外的租户（包括由世纪互联运营的 Azure）不在这一版支持范围内")),
        400 if filtered => Err(Failure::unfiltered()),
        400..=499 => Err(Failure::remote(format!("Microsoft To Do 拒绝了读取（{}）", response.status))),
        _ => Err(Failure::remote(format!("Microsoft To Do 暂时不可用（{}）", response.status))),
    }
}

fn clean_text(value: &str, limit: usize) -> String {
    let text: String = value.chars().map(|c| if c.is_control() { ' ' } else { c }).take(limit).collect();
    let text = text.trim();
    if text.is_empty() { "（无标题）".into() } else { text.to_string() }
}

fn trim_chars(value: &str, limit: usize) -> String {
    let mut text: String = value.chars().take(limit).collect();
    if value.chars().count() > limit { text.push('…'); }
    text
}

fn clamp_wait(value: Option<u64>) -> u64 { value.unwrap_or(180).clamp(30, 3600) }
fn backoff(failures: u32) -> u64 { match failures { 0 | 1 => 30, 2 => 60, 3 => 120, _ => 300 } }

fn local_offset() -> FixedOffset { chrono::Local::now().offset().fix() }

fn format_local_time(unix: i64) -> Option<String> {
    Utc.timestamp_opt(unix, 0).single().map(|time| time.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    account: String,
    client_id: String,
    tenant: String,
    fetched_unix: i64,
    lists: usize,
    truncated: bool,
    tasks: Vec<CacheTask>,
}

#[derive(Serialize, Deserialize)]
struct CacheTask { title: String, list: String, due: String, hover: String }

fn save_cache(path: &Path, snapshot: &Snapshot) -> Result<(), String> {
    let file = CacheFile {
        version: CACHE_VERSION,
        account: snapshot.account.clone(),
        client_id: snapshot.client_id.clone(),
        tenant: snapshot.tenant.clone(),
        fetched_unix: snapshot.fetched_unix,
        lists: snapshot.lists,
        truncated: snapshot.truncated,
        tasks: snapshot.tasks.iter().map(|task| CacheTask { title: task.title.clone(), list: task.list.clone(), due: task.due.clone(), hover: task.hover.clone() }).collect(),
    };
    let body = serde_json::to_vec(&file).map_err(|_| "无法准备待办缓存")?;
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).map_err(|_| "无法创建数据目录")?; }
    crate::model::atomic_write(path, &body).map_err(|_| "无法写入待办缓存".to_string())
}

fn load_cache(path: &Path) -> Option<Snapshot> {
    let raw = fs::read(path).ok()?;
    let file: CacheFile = serde_json::from_slice(&raw).ok()?;
    if file.version != CACHE_VERSION { return None; }
    Some(Snapshot {
        account: file.account,
        client_id: file.client_id,
        tenant: file.tenant,
        fetched_unix: file.fetched_unix,
        lists: file.lists,
        truncated: file.truncated,
        tasks: file.tasks.into_iter().map(|task| TaskRow { title: task.title, list: task.list, due: task.due, hover: task.hover }).collect(),
    })
}

fn bind_loopback() -> Result<(u16, Vec<TcpListener>), String> {
    let v4 = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| "无法在本机打开登录回调")?;
    let port = v4.local_addr().map_err(|_| "无法在本机打开登录回调")?.port();
    let mut listeners = vec![v4];
    if let Ok(v6) = TcpListener::bind(("::1", port)) { listeners.push(v6); }
    for listener in &listeners { listener.set_nonblocking(true).map_err(|_| "无法在本机打开登录回调")?; }
    Ok((port, listeners))
}

fn wait_callback(listeners: Vec<TcpListener>, expected_state: &str, cancel: &AtomicBool, deadline: Instant) -> Result<String, String> {
    loop {
        if cancel.load(Ordering::Relaxed) { return Err("登录已取消".into()); }
        if Instant::now() >= deadline { return Err("登录等待超时，请重试".into()); }
        for listener in &listeners {
            match listener.accept() {
                Ok((mut stream, address)) => {
                    if !address.ip().is_loopback() { continue; }
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    match read_callback(&mut stream, expected_state, deadline) {
                        Ok(None) => {
                            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                        }
                        other => {
                            let _ = write_page(&mut stream);
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            return other.map(|code| code.unwrap_or_default()).and_then(|code| if code.is_empty() { Err("登录没有返回授权码，请重试".into()) } else { Ok(code) });
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return Err("登录回调中断，请重试".into()),
            }
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn read_callback(stream: &mut impl Read, expected_state: &str, deadline: Instant) -> Result<Option<String>, String> {
    let (header, body) = read_http(stream, deadline)?;
    let mut lines = header.lines();
    let request = lines.next().unwrap_or("");
    let (method, target) = request.split_once(' ').ok_or("登录回调无法识别")?;
    if target.starts_with("/favicon") { return Ok(None); }
    let host_ok = header.lines().any(|line| {
        let Some(value) = line.split_once(':').and_then(|(name, value)| name.eq_ignore_ascii_case("host").then_some(value.trim())) else { return false };
        let host = value.rsplit_once(':').map(|(host, _)| host).unwrap_or(value).trim_matches(|c| c == '[' || c == ']');
        matches!(host, "localhost" | "127.0.0.1" | "::1")
    });
    if !host_ok { return Err("登录回调来源无法核对，请重试".into()); }
    let form = if method.eq_ignore_ascii_case("POST") {
        String::from_utf8(body).map_err(|_| "登录回调无法识别")?
    } else if method.eq_ignore_ascii_case("GET") {
        target.split_once('?').map(|(_, query)| query.to_string()).unwrap_or_default()
    } else {
        return Ok(None);
    };
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(form.as_bytes()).map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
    let value = |name: &str| pairs.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
    if value("code").is_none() && value("error").is_none() { return Ok(None); }
    if value("state").as_deref() != Some(expected_state) { return Err("登录响应无法核对，请重试".into()); }
    if let Some(error) = value("error") {
        let code: String = error.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(40).collect();
        return Err(explain_oauth(&code));
    }
    let code = value("code").unwrap_or_default();
    if code.len() > 2048 || code.chars().any(|c| c.is_whitespace() || c.is_control()) { return Err("登录没有返回可用的授权码，请重试".into()); }
    Ok(Some(code))
}

fn read_http(stream: &mut impl Read, deadline: Instant) -> Result<(String, Vec<u8>), String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if Instant::now() >= deadline { return Err("登录等待超时，请重试".into()); }
        match stream.read(&mut chunk) {
            Ok(0) => return Err("登录回调中断，请重试".into()),
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > 24 * 1024 { return Err("登录回调过大".into()); }
                if let Some(end) = buffer.windows(4).position(|part| part == b"\r\n\r\n") { break end; }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock || error.kind() == std::io::ErrorKind::TimedOut => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return Err("登录回调中断，请重试".into()),
        }
    };
    let header = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut body = buffer[header_end + 4..].to_vec();
    let length = content_length(&header)?;
    while body.len() < length {
        if Instant::now() >= deadline { return Err("登录等待超时，请重试".into()); }
        match stream.read(&mut chunk) {
            Ok(0) => return Err("登录回调中断，请重试".into()),
            Ok(count) => {
                body.extend_from_slice(&chunk[..count]);
                if body.len() > 16 * 1024 { return Err("登录回调过大".into()); }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock || error.kind() == std::io::ErrorKind::TimedOut => thread::sleep(Duration::from_millis(20)),
            Err(_) => return Err("登录回调中断，请重试".into()),
        }
    }
    body.truncate(length);
    Ok((header, body))
}

fn content_length(header: &str) -> Result<usize, String> {
    let post = header.lines().next().is_some_and(|line| line.to_ascii_uppercase().starts_with("POST "));
    if !post { return Ok(0); }
    let length = header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
    }).ok_or("登录回调无法识别")?;
    if length > 16 * 1024 { return Err("登录回调过大".into()); }
    Ok(length)
}

fn write_page(stream: &mut impl Write) -> std::io::Result<()> {
    let body = "<!doctype html><meta charset=\"utf-8\"><title>岛仔</title><p>可以回到岛仔了。</p>";
    let raw = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    stream.write_all(raw.as_bytes())
}

impl crate::app::App {
    pub(crate) fn ms_todo_tick(&mut self) {
        self.ms_todo.tick(&self.settings, self.module_on(crate::app::TODO_PAGE), Utc::now().timestamp());
    }

    pub(crate) fn ms_todo_connect(&mut self) -> Result<String, String> {
        self.ms_todo.connect(&self.settings.ms_todo_client_id, &self.settings.ms_todo_accounts, &self.settings.ms_todo_tenant_id)
    }

    pub(crate) fn ms_todo_refresh(&mut self) -> Result<(), String> {
        let (client, tenant) = prepare(&self.settings.ms_todo_client_id, &self.settings.ms_todo_accounts, &self.settings.ms_todo_tenant_id)?;
        self.ms_todo.request_sync(true, Utc::now().timestamp(), client, tenant)
    }

    pub(crate) fn ms_todo_settings(&mut self, ui: &mut egui::Ui) {
        use crate::app::Cmd;
        use crate::extras::{settings_card, settings_control_row, settings_hint, settings_label, settings_segmented};
        if self.ms_todo.busy() { ui.ctx().request_repaint_after(Duration::from_millis(200)); }
        settings_label(ui, "Microsoft To Do");
        settings_card(ui, |ui| {
            ui.label(egui::RichText::new("只读取 Microsoft 待办的清单、未完成任务和到期时间。不会创建、修改、完成或删除，也不会写入 Outlook。").size(13.0));
            settings_hint(ui, "本机待办仍只保存在这台电脑。连接用的是你自己注册的公开客户端，岛仔不附带应用 ID，也不保存客户端机密。");
            let mut audience = match self.settings.ms_todo_accounts.as_str() { "organizations" => 1u8, "consumers" => 2, _ => 0 };
            settings_control_row(ui, "账号范围", 96.0, |ui| {
                let changed = settings_segmented(ui, &mut audience, &[(0, "个人和工作"), (1, "仅工作或学校"), (2, "仅个人")], 0.0);
                if changed { self.settings.ms_todo_accounts = match audience { 1 => "organizations", 2 => "consumers", _ => "common" }.into(); }
                ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover())
            });
            settings_hint(ui, "要和工作、个人账号一起用，应用注册请选「任何组织目录中的帐户和个人 Microsoft 帐户」，这里保持「个人和工作」。只给一个单位用的单租户应用，把目录（租户）ID 填在下面。");
            settings_control_row(ui, "应用 ID", 96.0, |ui| {
                ui.add_sized([ui.available_width(), 28.0], egui::TextEdit::singleline(&mut self.settings.ms_todo_client_id).hint_text("应用程序（客户端）ID").desired_width(f32::INFINITY))
            });
            settings_control_row(ui, "目录 ID", 96.0, |ui| {
                ui.add_sized([ui.available_width(), 28.0], egui::TextEdit::singleline(&mut self.settings.ms_todo_tenant_id).hint_text("单租户才填，留空使用账号范围").desired_width(f32::INFINITY))
            });
            ui.horizontal_wrapped(|ui| {
                if ui.button("连接").clicked() {
                    match self.ms_todo_connect() {
                        Ok(url) => self.pending.push(Cmd::OpenUrl(url)),
                        Err(error) => self.ms_todo.status = error,
                    }
                }
                if self.ms_todo.login.is_some() && ui.button("取消").clicked() { self.ms_todo.cancel(); }
                if ui.add_enabled(self.ms_todo.signed_in(), egui::Button::new("刷新")).clicked() {
                    if let Err(error) = self.ms_todo_refresh() { self.ms_todo.status = error; }
                }
                if ui.add_enabled(self.ms_todo.signed_in() || self.ms_todo.account.is_some(), egui::Button::new("断开")).clicked() {
                    self.ms_todo_disconnect();
                }
            });
            let status = if self.ms_todo.status.is_empty() {
                match (self.ms_todo.signed_in(), self.ms_todo.account()) {
                    (true, Some(account)) => format!("已连接 {account}"),
                    (true, None) => "已连接".into(),
                    (false, _) => "尚未连接".into(),
                }
            } else {
                self.ms_todo.status.clone()
            };
            let quiet = self.ms_todo.status.is_empty() || self.ms_todo.status.starts_with("请在浏览器") || self.ms_todo.status.starts_with("已连接") || self.ms_todo.status.starts_with("已断开");
            ui.label(egui::RichText::new(status).size(12.5).color(if quiet { settings_quiet(ui) } else { egui::Color32::from_rgb(186, 96, 42) }));
            if let Some(account) = self.ms_todo.account() {
                let when = self.ms_todo.fetched_label();
                let when = if when.is_empty() { String::new() } else { format!("，上次读取 {when}") };
                let offline = if self.ms_todo.offline() { "。当前显示上次读取的结果" } else { "" };
                ui.label(egui::RichText::new(format!("{account}：{} 个清单，{} 项未完成{when}{offline}", self.ms_todo.list_count, self.ms_todo.task_count())).size(12.0));
            }
            if self.ms_todo.truncated() { settings_hint(ui, "未完成任务较多，岛上只保留前 200 项。"); }
            ui.horizontal_wrapped(|ui| {
                for (label, url) in [
                    ("桌面应用注册", "https://learn.microsoft.com/en-us/entra/identity-platform/scenario-desktop-app-registration"),
                    ("重定向 URI", "https://learn.microsoft.com/en-us/entra/identity-platform/reply-url"),
                    ("Tasks.Read", "https://learn.microsoft.com/en-us/graph/api/todotasklist-list-tasks?view=graph-rest-1.0"),
                ] {
                    if ui.link(label).clicked() { self.pending.push(Cmd::OpenUrl(url.into())); }
                }
            });
            settings_hint(ui, "注册步骤、账号差别和这一版没有实测登录，写在文档 docs/ms-todo.md。邮件设置里的 Outlook 邮箱授权码登录仍然不可用，这是另一件事。");
        });
    }

    fn ms_todo_disconnect(&mut self) {
        self.ms_todo.disconnect();
        self.ms_todo_view = false;
        self.ms_todo_first = 0;
    }
}

fn settings_quiet(ui: &egui::Ui) -> egui::Color32 {
    crate::settings_theme::color(ui.ctx(), egui::Color32::from_gray(180), egui::Color32::from_gray(75))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use chrono::{FixedOffset, NaiveDate, TimeZone, Utc};
    use serde_json::json;
    use std::{fs, io::{Read, Write}, net::TcpListener, sync::{atomic::AtomicBool, Mutex}, thread, time::{Duration, Instant}};
    use url::Url;

    const CLIENT: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const TENANT: &str = "11111111-2222-3333-4444-555555555555";

    fn shanghai() -> FixedOffset { FixedOffset::east_opt(8 * 3600).unwrap() }
    fn day() -> NaiveDate { NaiveDate::from_ymd_opt(2026, 11, 18).unwrap() }
    fn jwt(payload: &Value) -> String { format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(payload.to_string())) }

    struct Memory(Mutex<Option<Session>>);
    impl Secrets for Memory {
        fn load(&self) -> Result<Option<Session>, String> { Ok(self.0.lock().unwrap().clone()) }
        fn save(&self, session: &Session) -> Result<(), String> { *self.0.lock().unwrap() = Some(session.clone()); Ok(()) }
        fn delete(&self) -> Result<(), String> { *self.0.lock().unwrap() = None; Ok(()) }
    }

    fn leak(value: impl Into<String>) -> &'static str { Box::leak(value.into().into_boxed_str()) }
    enum Step { Post(u16, &'static str), Get(u16, &'static str) }
    struct Scripted { posts: Mutex<Vec<String>>, gets: Mutex<Vec<String>>, plan: Mutex<Vec<Step>> }
    impl Scripted {
        fn new(plan: Vec<Step>) -> Self { Self { posts: Mutex::new(Vec::new()), gets: Mutex::new(Vec::new()), plan: Mutex::new(plan) } }
        fn next(&self, post: bool) -> Result<HttpResponse, String> {
            match self.plan.lock().unwrap().pop() {
                Some(Step::Post(status, body)) if post => Ok(HttpResponse { status, body: body.as_bytes().to_vec(), retry_after: None }),
                Some(Step::Get(status, body)) if !post => Ok(HttpResponse { status, body: body.as_bytes().to_vec(), retry_after: (status == 429).then_some(9) }),
                other => panic!("意外的请求：post={post}, {other:?}"),
            }
        }
    }
    impl std::fmt::Debug for Step { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { match self { Step::Post(status, _) => write!(f, "post {status}"), Step::Get(status, _) => write!(f, "get {status}") } } }
    impl Http for Scripted {
        fn post_form(&self, url: &str, body: &str) -> Result<HttpResponse, String> {
            assert!(token_target_allowed(url), "{url}");
            assert!(!body.contains("client_secret"), "{body}");
            self.posts.lock().unwrap().push(body.to_string());
            self.next(true)
        }
        fn get(&self, url: &str, bearer: &str) -> Result<HttpResponse, String> {
            assert!(!bearer.is_empty() && !bearer.contains('\n') && !bearer.contains('\r'));
            self.gets.lock().unwrap().push(url.to_string());
            self.next(false)
        }
    }

    fn token_json(scope: &str) -> String {
        json!({
            "access_token":"synthetic-access",
            "refresh_token":"synthetic-refresh-rotated",
            "expires_in":3600,
            "scope":scope,
            "id_token":jwt(&json!({"preferred_username":"synthetic@contoso.com"})),
        }).to_string()
    }

    fn session(expires_at: i64) -> Session {
        Session { refresh_token: "synthetic-refresh".into(), access_token: "synthetic-access".into(), expires_at, account: "synthetic@contoso.com".into(), client_id: CLIENT.into(), tenant: "common".into() }
    }

    #[test]
    fn public_client_url_requests_only_tasks_read() {
        let url = authorization_url(CLIENT, "common", "http://localhost:51234/", "challenge", "state").unwrap();
        let url = Url::parse(&url).unwrap();
        assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
        assert_eq!(url.path(), "/common/oauth2/v2.0/authorize");
        let pairs: Vec<_> = url.query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
        let scope = pairs.iter().find(|(key, _)| key == "scope").unwrap().1.clone();
        assert_eq!(scope.split_whitespace().collect::<Vec<_>>(), ["openid", "profile", "offline_access", "Tasks.Read"]);
        assert!(pairs.iter().all(|(key, _)| key != "client_secret"));
        assert_eq!(pairs.iter().find(|(key, _)| key == "code_challenge_method").unwrap().1, "S256");
        assert_eq!(pairs.iter().find(|(key, _)| key == "response_mode").unwrap().1, "form_post");
        assert_eq!(pairs.iter().find(|(key, _)| key == "response_type").unwrap().1, "code");
        assert!(prepare("not a guid", "common", "").is_err());
        assert!(prepare(CLIENT, "common", "https://evil.example").is_err());
        assert_eq!(prepare(&format!("{{{CLIENT}}}"), "evil.example", "").unwrap(), (CLIENT.into(), "common".into()));
        assert_eq!(prepare(CLIENT, "organizations", TENANT).unwrap().1, TENANT);
        assert!(!allow_token_url("https://evil.example/common/oauth2/v2.0/token"));
        assert!(allow_token_url(&token_url("common").unwrap()));
        assert!(allow_token_url(&token_url(TENANT).unwrap()));
        assert!(trusted_graph("https://graph.microsoft.com/v1.0/me/todo/lists", GRAPH).is_some());
        assert!(trusted_graph("https://evil.example/v1.0/me/todo/lists", GRAPH).is_none());
        assert!(trusted_graph("https://graph.microsoft.com/v1.0/me/messages", GRAPH).is_none());
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn due_dates_keep_date_only_values_and_convert_utc_instants() {
        let date_only = format_due("2026-11-18T00:00:00.0000000", "UTC", day(), shanghai()).unwrap();
        assert_eq!(date_only.full, "2026-11-18");
        assert!(!date_only.short.contains(':'), "{}", date_only.short);
        let timed = format_due("2026-11-18T01:30:00.0000000", "UTC", day(), shanghai()).unwrap();
        assert_eq!(timed.full, "2026-11-18 09:30");
        let next_day = format_due("2026-11-18T16:30:00Z", "", day(), shanghai()).unwrap();
        assert_eq!(next_day.full, "2026-11-19 00:30");
        let wall = format_due("2026-11-18T09:00:00.0000000", "China Standard Time", day(), shanghai()).unwrap();
        assert_eq!(wall.full, "2026-11-18 09:00");
        let unknown = format_due("2026-11-18T09:00:00.0000000", "Romance Standard Time", day(), shanghai()).unwrap();
        assert_eq!(unknown.full, "2026-11-18 09:00");
        assert!(unknown.note.contains("Romance Standard Time"));
    }

    #[test]
    fn stored_session_keeps_tokens_out_of_the_parse_error() {
        let raw = session_json(&session(1_700_000_000));
        let restored = session_from_json(&raw).unwrap();
        assert_eq!(restored.refresh_token, "synthetic-refresh");
        assert_eq!(restored.account, "synthetic@contoso.com");
        let error = match session_from_json(r#"{"access_token":"synthetic-access","refresh_token":"synthetic-refresh"}"#) {
            Err(error) => error,
            Ok(_) => panic!("不完整的登录应被拒绝"),
        };
        assert!(!error.contains("synthetic-access") && !error.contains("synthetic-refresh"));
    }

    #[test]
    fn readonly_scope_rejects_write_mail_and_all_permissions() {
        assert!(scope_is_readonly("openid profile offline_access Tasks.Read"));
        assert!(!scope_is_readonly("Tasks.Read Tasks.ReadWrite"));
        assert!(!scope_is_readonly("Tasks.Read Mail.Read"));
        assert!(!scope_is_readonly("Tasks.Read Tasks.Read.All"));
        assert!(!scope_is_readonly("Tasks.Read Tasks.Read.Shared"));
        assert!(!scope_is_readonly("openid profile"));
    }

    #[test]
    fn sync_reads_incomplete_tasks_refreshes_and_never_posts_to_graph() {
        let memory = Memory(Mutex::new(Some(session(0))));
        let tasks = json!({"value":[
            {"title":"写周报","status":"notStarted","dueDateTime":{"dateTime":"2026-11-18T00:00:00.0000000","timeZone":"UTC"}},
            {"title":"已完成的","status":"completed","dueDateTime":{"dateTime":"2026-11-17T00:00:00.0000000","timeZone":"UTC"}},
            {"title":"开会","status":"inProgress","importance":"high","dueDateTime":{"dateTime":"2026-11-18T01:30:00.0000000","timeZone":"UTC"}}
        ]}).to_string();
        let lists = r#"{"value":[{"id":"list-1","displayName":"工作","isOwner":true},{"id":"list-2","displayName":"共享项目","isOwner":false}]}"#;
        let shared = r#"{"value":[{"title":"看共享","status":"waitingOnOthers"}]}"#;
        let script = Scripted::new(vec![
            Step::Get(200, leak(shared)),
            Step::Get(200, leak(tasks)),
            Step::Get(200, leak(lists)),
            Step::Post(200, leak(token_json("openid profile offline_access Tasks.Read"))),
        ]);
        let now = Utc.with_ymd_and_hms(2026, 11, 18, 1, 0, 0).unwrap().timestamp();
        let snapshot = run_sync(&script, &memory, CLIENT, "common", GRAPH, now, &AtomicBool::new(false)).unwrap();
        let local = local_offset();
        let today = Utc.timestamp_opt(now, 0).single().unwrap().with_timezone(&local).date_naive();
        let meeting = format_due("2026-11-18T01:30:00.0000000", "UTC", today, local).unwrap();
        let report = format_due("2026-11-18T00:00:00.0000000", "UTC", today, local).unwrap();
        assert_eq!(snapshot.tasks.iter().map(|task| task.title.as_str()).collect::<Vec<_>>(), ["开会", "写周报", "看共享"]);
        assert_eq!(snapshot.tasks[0].due, meeting.short);
        assert_eq!(snapshot.tasks[1].due, report.short);
        assert!(!report.short.contains(':'), "UTC 零点的日期不应被换算到前一天");
        assert!(snapshot.tasks[2].hover.contains("共享清单"));
        assert!(snapshot.tasks[0].hover.contains("重要"));
        let posts = script.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].contains("grant_type=refresh_token"));
        assert!(posts[0].contains("synthetic-refresh"));
        assert!(!posts[0].contains("client_secret"));
        assert!(script.gets.lock().unwrap().iter().all(|url| url.starts_with("https://graph.microsoft.com/v1.0/me/todo/")));
        assert_eq!(memory.load().unwrap().unwrap().refresh_token, "synthetic-refresh-rotated");
        let path = std::env::temp_dir().join(format!("daozai-ms-todo-{}-{}.json", std::process::id(), now));
        save_cache(&path, &snapshot).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("synthetic-refresh") && !raw.contains("synthetic-access") && !raw.contains("access_token"));
        let loaded = load_cache(&path).unwrap();
        assert_eq!(loaded.tasks.len(), 3);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn evil_next_link_is_not_requested_and_errors_hide_bodies() {
        let memory = Memory(Mutex::new(Some(session(9_999_999_999))));
        let page = r#"{"value":[{"title":"一项","status":"notStarted"}],"@odata.nextLink":"https://evil.example/v1.0/me/todo/lists/list-1/tasks"}"#;
        let script = Scripted::new(vec![
            Step::Get(200, page),
            Step::Get(200, r#"{"value":[{"id":"list-1","displayName":"工作","isOwner":true}]}"#),
        ]);
        let error = run_sync(&script, &memory, CLIENT, "common", GRAPH, 1_700_000_000, &AtomicBool::new(false)).unwrap_err();
        assert!(error.message.contains("后续地址"));
        assert_eq!(script.gets.lock().unwrap().len(), 2);
        assert!(script.posts.lock().unwrap().is_empty());
        let leaked = interpret_graph(HttpResponse { status: 401, body: b"access_token=synthetic-secret".to_vec(), retry_after: None }, false).unwrap_err();
        assert!(!leaked.message.contains("synthetic-secret"));
        let oauth = explain_oauth("invalid_grant");
        assert!(oauth.contains("重新连接"));
        assert!(!oauth.contains("secret-body"));
    }

    #[test]
    fn expired_grant_clears_the_session_and_write_scope_is_not_stored() {
        let memory = Memory(Mutex::new(Some(session(0))));
        let script = Scripted::new(vec![Step::Post(400, r#"{"error":"invalid_grant","error_description":"secret-body"}"#)]);
        let error = run_sync(&script, &memory, CLIENT, "common", GRAPH, 1_700_000_000, &AtomicBool::new(false)).unwrap_err();
        assert!(error.clear_session);
        assert!(error.message.contains("重新连接"));
        assert!(!error.message.contains("secret-body"));
        assert!(memory.load().unwrap().is_none());
        let empty = Memory(Mutex::new(None));
        let refused = Scripted::new(vec![Step::Post(200, leak(token_json("Tasks.ReadWrite")))]);
        let error = finish_login(&refused, &empty, CLIENT, "common", "http://localhost:1/", "verifier", "code", 1_700_000_000, &AtomicBool::new(false)).unwrap_err();
        assert!(error.contains("Tasks.Read"));
        assert!(empty.load().unwrap().is_none());
        assert!(refused.posts.lock().unwrap()[0].contains("code_verifier=verifier"));
        assert!(!refused.posts.lock().unwrap()[0].contains("client_secret"));
    }

    #[test]
    fn filtered_failure_retries_once_without_writing() {
        let memory = Memory(Mutex::new(Some(session(9_999_999_999))));
        let script = Scripted::new(vec![
            Step::Get(200, r#"{"value":[{"title":"补读","status":"notStarted"},{"title":"完成","status":"completed"}]}"#),
            Step::Get(400, r#"{"error":{"code":"BadRequest","message":"secret-filter"}}"#),
            Step::Get(200, r#"{"value":[{"id":"list-1","displayName":"工作","isOwner":true}]}"#),
        ]);
        let snapshot = run_sync(&script, &memory, CLIENT, "common", GRAPH, 1_700_000_000, &AtomicBool::new(false)).unwrap();
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].title, "补读");
        let gets = script.gets.lock().unwrap().clone();
        assert_eq!(gets.len(), 3);
        assert!(gets[1].contains("filter"));
        assert!(!gets[2].contains("filter"));
        assert!(script.posts.lock().unwrap().is_empty());
    }

    #[test]
    fn loopback_accepts_form_post_and_does_not_echo_the_code() {
        let (port, listeners) = bind_loopback().unwrap();
        let worker = thread::spawn(move || wait_callback(listeners, "state-1", &AtomicBool::new(false), Instant::now() + Duration::from_secs(3)));
        let mut favicon = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        favicon.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        favicon.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut ignored = [0u8; 128];
        let _ = favicon.read(&mut ignored);
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        let body = "code=synthetic-code&state=state-1";
        let request = format!("POST / HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(response.contains("可以回到岛仔了"));
        assert!(!response.contains("synthetic-code"));
        assert_eq!(worker.join().unwrap().unwrap(), "synthetic-code");
    }

    #[test]
    fn wrong_state_and_foreign_host_are_rejected() {
        let body = "code=abc&state=state-1";
        let header = format!("POST / HTTP/1.1\r\nHost: evil.example\r\nContent-Length: {}\r\n\r\n", body.len());
        let mut cursor = std::io::Cursor::new(format!("{header}{body}").into_bytes());
        let error = read_callback(&mut cursor, "state-1", Instant::now() + Duration::from_secs(1)).unwrap_err();
        assert!(!error.contains("abc"));
        assert!(error.contains("来源"));
    }

    #[test]
    fn ureq_errors_do_not_echo_secrets_or_send_a_client_secret() {
        use std::io::{BufRead, BufReader};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" { break; }
                request.push_str(&line);
            }
            let length = request.lines().find_map(|line| line.split_once(':').and_then(|(name, value)| name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap()))).unwrap();
            let mut body = vec![0; length];
            std::io::Read::read_exact(&mut reader, &mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            assert!(body.contains("grant_type=refresh_token"));
            assert!(!body.contains("client_secret"));
            let mut stream = reader.into_inner();
            let payload = br#"{"error":"invalid_client","error_description":"secret-body"}"#;
            stream.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len()).as_bytes()).unwrap();
            stream.write_all(payload).unwrap();
            body
        });
        let response = UreqHttp.post_form(&format!("http://{address}/common/oauth2/v2.0/token"), "grant_type=refresh_token&refresh_token=synthetic-refresh&client_id=app").unwrap();
        assert_eq!(response.status, 400);
        let message = explain_oauth(&oauth_error_code(&response.body).unwrap());
        assert!(message.contains("应用 ID"));
        assert!(!message.contains("secret-body") && !message.contains("synthetic-refresh"));
        let _ = server.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_test_slot_round_trips_without_using_the_production_account() {
        let slot = format!("test-{}", std::process::id());
        let entry = keyring::Entry::new(SERVICE, &slot).unwrap();
        let _ = entry.delete_credential();
        let raw = session_json(&session(1_700_000_000));
        entry.set_password(&raw).unwrap();
        let restored = session_from_json(&entry.get_password().unwrap()).unwrap();
        assert_eq!(restored.refresh_token, "synthetic-refresh");
        assert_eq!(restored.account, "synthetic@contoso.com");
        entry.delete_credential().unwrap();
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
    }
}
