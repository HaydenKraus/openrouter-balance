//! Запросы к OpenRouter и модели ответов.
//!
//! Необязательные поля допускают отсутствие (`#[serde(default)]`).

use std::time::{Duration, SystemTime};

use serde::Deserialize;

pub const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Бюджет ключа (элемент `budgets` / `effective_budget` в `/key`).
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct KeyBudget {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub guardrail_id: Option<String>,
    #[serde(default)]
    pub budget_id: Option<String>,
    #[serde(default)]
    pub limit_usd: Option<f64>,
    #[serde(default)]
    pub spend_usd: Option<f64>,
    #[serde(default)]
    pub remaining_usd: Option<f64>,
    #[serde(default)]
    pub reset_interval: Option<String>,
    #[serde(default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub include_byok: bool,
}

/// Поле `data` ответа `GET /key`.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct KeyInfo {
    #[serde(default)]
    pub label: Option<String>,
    /// `None` — у ключа нет лимита.
    #[serde(default)]
    pub limit: Option<f64>,
    #[serde(default)]
    pub limit_remaining: Option<f64>,
    #[serde(default)]
    pub limit_reset: Option<String>,
    pub usage: f64,
    #[serde(default)]
    pub usage_daily: f64,
    #[serde(default)]
    pub usage_weekly: f64,
    #[serde(default)]
    pub usage_monthly: f64,
    #[serde(default)]
    pub is_management_key: bool,
    #[serde(default)]
    pub budgets: Vec<KeyBudget>,
    #[serde(default)]
    pub effective_budget: Option<KeyBudget>,
}

#[derive(Debug, Deserialize)]
struct KeyEnvelope {
    data: KeyInfo,
}

/// Поле `data` ответа `GET /credits`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub struct Credits {
    pub total_credits: f64,
    pub total_usage: f64,
}

impl Credits {
    pub fn remaining(&self) -> f64 {
        self.total_credits - self.total_usage
    }
}

#[derive(Debug, Deserialize)]
struct CreditsEnvelope {
    data: Credits,
}

/// Разбор тела ответа `/key`. Поля `data` и `usage` обязательны.
pub fn parse_key(body: &str) -> Result<KeyInfo, ApiError> {
    serde_json::from_str::<KeyEnvelope>(body)
        .map(|e| e.data)
        .map_err(|e| ApiError::Parse(e.to_string()))
}

/// Разбор тела ответа `/credits`.
pub fn parse_credits(body: &str) -> Result<Credits, ApiError> {
    serde_json::from_str::<CreditsEnvelope>(body)
        .map(|e| e.data)
        .map_err(|e| ApiError::Parse(e.to_string()))
}

/// Ошибки API. Тексты никогда не содержат ключей.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// Нет сети / DNS / соединение.
    Network(String),
    Timeout,
    /// 401 — ключ недействителен.
    Unauthorized,
    /// 403.
    Forbidden,
    /// 429 (исчерпаны повторы или сервер просит подождать дольше).
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// Прочие HTTP-статусы.
    Http(u16),
    /// Невалидный JSON / неожиданная структура.
    Parse(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Network(_) => write!(f, "нет соединения с сетью"),
            ApiError::Timeout => write!(f, "таймаут запроса (10 с)"),
            ApiError::Unauthorized => write!(f, "ключ недействителен (401)"),
            ApiError::Forbidden => write!(f, "доступ запрещён (403)"),
            ApiError::RateLimited { .. } => write!(f, "слишком много запросов (429)"),
            ApiError::Http(c) => write!(f, "ошибка сервера (HTTP {c})"),
            ApiError::Parse(_) => write!(f, "неожиданный ответ сервера"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Результат попытки получить общий баланс.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CreditsStatus {
    /// Нет подходящего management-ключа или `/credits` вернул 401/403.
    NeedsManagementKey,
    Available(Credits),
    /// Временная ошибка при запросе `/credits` (сеть, 429, 5xx).
    Unavailable,
}

impl From<Result<Credits, ApiError>> for CreditsStatus {
    fn from(result: Result<Credits, ApiError>) -> Self {
        match result {
            Ok(credits) => Self::Available(credits),
            Err(ApiError::Forbidden | ApiError::Unauthorized) => Self::NeedsManagementKey,
            Err(_) => Self::Unavailable,
        }
    }
}

/// Последние данные ключа и независимо обновляемый общий баланс.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub key: KeyInfo,
    pub credits: CreditsStatus,
    pub fetched_at: SystemTime,
}

// ---------------------------------------------------------------------------
// Разбор заголовков и расчёт пауз (чистые функции — тестируются отдельно)
// ---------------------------------------------------------------------------

/// `Retry-After` в секундах или в формате HTTP-date (IMF-fixdate).
fn parse_retry_after(
    header_value: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Duration> {
    let value = header_value?.trim();
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    let retry_at = chrono::NaiveDateTime::parse_from_str(value, "%a, %d %b %Y %H:%M:%S GMT")
        .ok()?
        .and_utc();
    Some(
        retry_at
            .signed_duration_since(now)
            .to_std()
            .unwrap_or_default(),
    )
}

/// Пауза перед повтором `attempt` (нумерация с 1).
///
/// `Retry-After` имеет приоритет. Иначе —
/// экспоненциальный backoff `base * 2^(attempt-1)`, ограниченный
/// [`MAX_BACKOFF`] и разбавленный джиттером (0–25 %, чтобы повторы разных
/// экземпляров не совпадали).
pub fn backoff(attempt: u32, retry_after: Option<Duration>, base: Duration) -> Duration {
    match retry_after {
        Some(server) => server,
        None => {
            let shift = attempt.saturating_sub(1).min(20);
            let exponential = base.saturating_mul(1u32 << shift).min(MAX_BACKOFF);
            let jitter = pseudorandom() % (exponential.as_millis() as u64 / 4 + 1);
            exponential + Duration::from_millis(jitter)
        }
    }
}

/// Более долгую паузу планирует вызывающий код.
const MAX_INLINE_RETRY_WAIT: Duration = Duration::from_secs(30);
/// Потолок экспоненциального backoff без заголовка.
const MAX_BACKOFF: Duration = Duration::from_secs(4);
/// Число повторов при 429.
const MAX_RETRIES: u32 = 3;
/// Интервал между обновлениями системных часов (наносекунды).
const CLOCK_REFRESH_NANOS: u128 = 1_000_000;

/// Джиттер без внешних крейтов: `SystemTime` + счётчик-соль. Не криптография.
fn pseudorandom() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SALT: AtomicU64 = AtomicU64::new(0);
    static LAST_NANOS: AtomicU64 = AtomicU64::new(0);

    let salt = SALT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let cached = LAST_NANOS.load(Ordering::Relaxed);
    let ticks = match nanos.checked_sub(cached) {
        Some(delta) if u128::from(delta) < CLOCK_REFRESH_NANOS => 0,
        _ => {
            LAST_NANOS.store(nanos, Ordering::Relaxed);
            1
        }
    };
    nanos ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(ticks)
}

// ---------------------------------------------------------------------------
// HTTP-клиент
// ---------------------------------------------------------------------------

/// HTTP-клиент с таймаутом 10 с. Дёшево клонируется.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    /// База экспоненциального backoff (в тестах — миллисекунды).
    base_delay: Duration,
}

impl Client {
    pub fn new() -> Self {
        Self::with_base_url(BASE_URL)
    }

    /// Клиент, направленный на другой адрес (тесты против локального сервера).
    ///
    /// Ошибка конфигурации TLS/сборки не должна ронять приложение: при ней
    /// используется клиент с настройками по умолчанию.
    pub fn with_base_url(url: &str) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(user_agent())
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            http,
            base_url: url.trim_end_matches('/').to_string(),
            base_delay: DEFAULT_BASE_DELAY,
        }
    }

    /// Переопределение базы backoff (только для тестов).
    #[cfg(test)]
    fn with_base_delay(mut self, base: Duration) -> Self {
        self.base_delay = base;
        self
    }

    /// `GET /key` с повторами при 429 (экспоненциальный backoff, `Retry-After`).
    pub async fn fetch_key(&self, key: &str) -> Result<KeyInfo, ApiError> {
        self.fetch_with_retries("/key", key, parse_key).await
    }

    /// `GET /credits`. 403 → `Err(ApiError::Forbidden)`.
    pub async fn fetch_credits(&self, key: &str) -> Result<Credits, ApiError> {
        self.fetch_with_retries("/credits", key, parse_credits)
            .await
    }

    /// Один запрос: статус → разбор тела. Без повторов.
    async fn fetch_once<T>(
        &self,
        path: &str,
        key: &str,
        parse: fn(&str) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let url = format!("{}{path}", self.base_url);
        // `error` выводит только `Display`, без `Debug`: он не печатает ни URL,
        // ни заголовки, ни тело — ключ не может попасть в текст ошибки.
        let response = self
            .http
            .get(url)
            .bearer_auth(key)
            .send()
            .await
            .map_err(|e| {
                let e = e.without_url();
                if e.is_timeout() {
                    ApiError::Timeout
                } else {
                    ApiError::Network(e.to_string())
                }
            })?;

        let status = response.status();
        match status.as_u16() {
            200 => {
                let body = response
                    .text()
                    .await
                    .map_err(|_| ApiError::Network(String::new()))?;
                parse(&body)
            }
            401 => Err(ApiError::Unauthorized),
            403 => Err(ApiError::Forbidden),
            429 => Err(ApiError::RateLimited {
                retry_after: parse_retry_after(
                    response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok()),
                    chrono::Utc::now(),
                ),
            }),
            other => Err(ApiError::Http(other)),
        }
    }

    /// `fetch_once` с повторами при 429. После исчерпания попыток или при долгой
    /// паузе возвращается последняя ошибка с её `Retry-After`.
    async fn fetch_with_retries<T>(
        &self,
        path: &str,
        key: &str,
        parse: fn(&str) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        for attempt in 1..=MAX_RETRIES {
            let error = match self.fetch_once(path, key, parse).await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let retry_after = match error {
                ApiError::RateLimited { retry_after } => retry_after,
                other => return Err(other),
            };
            let delay = backoff(attempt, retry_after, self.base_delay);
            if attempt == MAX_RETRIES || delay > MAX_INLINE_RETRY_WAIT {
                return Err(ApiError::RateLimited { retry_after });
            }
            tokio::time::sleep(delay).await;
        }
        Err(ApiError::RateLimited { retry_after: None })
    }
}

/// Общий таймаут запроса.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Таймаут установки соединения.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// База экспоненциального backoff вне тестов.
const DEFAULT_BASE_DELAY: Duration = Duration::from_secs(1);

fn user_agent() -> String {
    format!("openrouter-balance/{}", env!("CARGO_PKG_VERSION"))
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // -----------------------------------------------------------------------
    // Заготовки тел ответов
    // -----------------------------------------------------------------------

    const KEY_NULL_LIMIT: &str = r#"{
        "data": {
            "label": "default",
            "limit": null,
            "limit_remaining": null,
            "usage": 12.5,
            "usage_daily": 1.0,
            "usage_weekly": 3.0,
            "usage_monthly": 8.0,
            "is_management_key": false,
            "budgets": [],
            "effective_budget": null
        }
    }"#;

    const KEY_WITH_BUDGET: &str = r#"{
        "data": {
            "label": "team key",
            "limit": 25.0,
            "limit_remaining": 17.5,
            "limit_reset": "monthly",
            "usage": 7.5,
            "is_management_key": true,
            "budgets": [
                {
                    "scope": "key",
                    "guardrail_id": "gr_1",
                    "budget_id": "bd_1",
                    "limit_usd": 25.0,
                    "spend_usd": 7.5,
                    "remaining_usd": 17.5,
                    "reset_interval": "monthly",
                    "resets_at": "2026-12-01T00:00:00Z",
                    "include_byok": true
                }
            ],
            "effective_budget": {
                "scope": "guardrail",
                "guardrail_id": null,
                "budget_id": "bd_9",
                "limit_usd": 10.0,
                "spend_usd": 9.0,
                "remaining_usd": 1.0,
                "reset_interval": null,
                "resets_at": null,
                "include_byok": false
            }
        }
    }"#;

    const CREDITS_BODY: &str = r#"{"data":{"total_credits":100.0,"total_usage":42.25}}"#;

    /// Тело с большим числом неизвестных полей на всех уровнях.
    const KEY_WITH_UNKNOWN_FIELDS: &str = r#"{
        "data": {
            "label": "x",
            "usage": 2.0,
            "limit": 5.0,
            "limit_remaining": 3.0,
            "totally_new_field": {"nested": [1, 2, 3]},
            "another": "ignored",
            "budgets": [{"scope": "key", "limit_usd": 5.0, "unknown_budget_field": 1}],
            "effective_budget": {"budget_id": "b", "limit_usd": 5.0, "unknown_eb": null}
        },
        "extra_at_top_level": true
    }"#;

    fn parse<T>(result: Result<T, ApiError>) -> T {
        match result {
            Ok(value) => value,
            Err(e) => panic!("ожидался Ok, получено Err({e:?})"),
        }
    }

    fn parse_err<T: std::fmt::Debug>(result: Result<T, ApiError>) -> ApiError {
        match result {
            Ok(value) => panic!("ожидался Err, получено Ok({value:?})"),
            Err(e) => e,
        }
    }

    // -----------------------------------------------------------------------
    // Разбор тел ответов
    // -----------------------------------------------------------------------

    #[test]
    fn key_with_null_limit_has_no_limit() {
        let key = parse(parse_key(KEY_NULL_LIMIT));
        assert_eq!(key.label.as_deref(), Some("default"));
        assert_eq!(key.limit, None);
        assert_eq!(key.limit_remaining, None);
        assert_eq!(key.usage, 12.5);
        assert_eq!(key.usage_monthly, 8.0);
        assert!(!key.is_management_key);
        assert!(key.budgets.is_empty());
        assert_eq!(key.effective_budget, None);
    }

    #[test]
    fn key_with_budgets_and_effective_budget() {
        let key = parse(parse_key(KEY_WITH_BUDGET));
        assert_eq!(key.limit, Some(25.0));
        assert_eq!(key.limit_remaining, Some(17.5));
        assert_eq!(key.limit_reset.as_deref(), Some("monthly"));
        assert!(key.is_management_key);
        assert_eq!(key.budgets.len(), 1);

        let budget = &key.budgets[0];
        assert_eq!(budget.scope.as_deref(), Some("key"));
        assert_eq!(budget.guardrail_id.as_deref(), Some("gr_1"));
        assert_eq!(budget.budget_id.as_deref(), Some("bd_1"));
        assert_eq!(budget.limit_usd, Some(25.0));
        assert_eq!(budget.spend_usd, Some(7.5));
        assert_eq!(budget.remaining_usd, Some(17.5));
        assert_eq!(budget.reset_interval.as_deref(), Some("monthly"));
        assert_eq!(budget.resets_at.as_deref(), Some("2026-12-01T00:00:00Z"));
        assert!(budget.include_byok);

        let effective = key.effective_budget.expect("effective_budget");
        assert_eq!(effective.scope.as_deref(), Some("guardrail"));
        assert_eq!(effective.guardrail_id, None);
        assert_eq!(effective.budget_id.as_deref(), Some("bd_9"));
        assert_eq!(effective.limit_usd, Some(10.0));
        assert_eq!(effective.remaining_usd, Some(1.0));
        assert_eq!(effective.reset_interval, None);
        assert_eq!(effective.resets_at, None);
        assert!(!effective.include_byok);
    }

    #[test]
    fn zero_limit_is_kept_as_zero() {
        let key = parse(parse_key(
            r#"{"data":{"limit":0,"limit_remaining":0,"usage":0}}"#,
        ));
        assert_eq!(key.limit, Some(0.0));
        assert_eq!(key.limit_remaining, Some(0.0));
        assert_eq!(key.usage, 0.0);
    }

    #[test]
    fn negative_limit_remaining_is_preserved() {
        let key = parse(parse_key(
            r#"{"data":{"limit":10.0,"limit_remaining":-2.5,"usage":12.5}}"#,
        ));
        assert_eq!(key.limit_remaining, Some(-2.5));
        assert_eq!(key.usage, 12.5);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let key = parse(parse_key(KEY_WITH_UNKNOWN_FIELDS));
        assert_eq!(key.label.as_deref(), Some("x"));
        assert_eq!(key.limit, Some(5.0));
        assert_eq!(key.budgets.len(), 1);
        assert_eq!(key.budgets[0].limit_usd, Some(5.0));
        assert_eq!(key.budgets[0].guardrail_id, None);
        assert!(!key.budgets[0].include_byok);
        assert!(key.effective_budget.is_some());

        let credits = parse(parse_credits(
            r#"{"data":{"total_credits":1,"total_usage":0},"x":[1]}"#,
        ));
        assert_eq!(credits.total_credits, 1.0);
    }

    #[test]
    fn empty_and_broken_bodies_are_parse_errors() {
        for body in [
            "",
            "{}",
            r#"{"data":{}}"#,
            r#"{"data":null}"#,
            "not json",
            r#"{"data":{"usage":"x"}}"#,
        ] {
            assert!(
                matches!(parse_key(body), Err(ApiError::Parse(_))),
                "тело {body:?} должно давать Parse"
            );
        }
    }

    #[test]
    fn empty_and_broken_credit_bodies_are_parse_errors() {
        for body in [
            "",
            "{}",
            r#"{"data":null}"#,
            r#"{"data":{"total_credits":1}}"#,
        ] {
            assert!(
                matches!(parse_credits(body), Err(ApiError::Parse(_))),
                "тело {body:?} должно давать Parse"
            );
        }
    }

    #[test]
    fn credits_parsing_and_remaining() {
        let credits = parse(parse_credits(CREDITS_BODY));
        assert_eq!(
            credits,
            Credits {
                total_credits: 100.0,
                total_usage: 42.25
            }
        );
        assert!((credits.remaining() - 57.75).abs() < 1e-9);
    }

    // -----------------------------------------------------------------------
    // Retry-After и backoff
    // -----------------------------------------------------------------------

    #[test]
    fn retry_after_parses_integer_seconds() {
        let now = chrono::DateTime::UNIX_EPOCH;
        for (header, seconds) in [("0", 0), ("5", 5), (" 7 ", 7), ("120", 120)] {
            assert_eq!(
                parse_retry_after(Some(header), now),
                Some(Duration::from_secs(seconds))
            );
        }
    }

    #[test]
    fn retry_after_parses_http_date() {
        let now = "2026-10-21T07:27:58Z".parse().unwrap();
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT"), now),
            Some(Duration::from_secs(2)),
        );
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2026 07:27:57 GMT"), now),
            Some(Duration::ZERO),
        );
    }

    #[test]
    fn retry_after_ignores_invalid_values() {
        let now = chrono::DateTime::UNIX_EPOCH;
        for header in [None, Some(""), Some("1.5"), Some("-3"), Some("abc")] {
            assert_eq!(parse_retry_after(header, now), None);
        }
    }

    #[test]
    fn backoff_prefers_retry_after() {
        let base = Duration::from_secs(1);
        assert_eq!(
            backoff(1, Some(Duration::from_secs(2)), base),
            Duration::from_secs(2)
        );
        assert_eq!(
            backoff(3, Some(Duration::from_secs(0)), base),
            Duration::from_secs(0)
        );
        assert_eq!(
            backoff(1, Some(Duration::from_secs(600)), base),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn backoff_grows_exponentially_within_cap() {
        let base = Duration::from_secs(1);
        // Джиттер ≤ 25 %, поэтому проверяем интервалы, а не точные значения.
        for (attempt, nominal) in [(1u32, 1u64), (2, 2), (3, 4), (4, 4)] {
            let delay = backoff(attempt, None, base);
            let nominal = Duration::from_secs(nominal);
            assert!(
                delay >= nominal && delay <= nominal + nominal / 4 + Duration::from_millis(1),
                "attempt {attempt}: {delay:?} вне [nominal, nominal * 1.25]"
            );
        }
        // Даже огромный attempt не превышает потолок (+джиттер).
        let huge = backoff(1000, None, base);
        assert!(huge <= Duration::from_secs(5), "потолок нарушен: {huge:?}");
    }

    #[test]
    fn backoff_without_retry_after_never_hangs_for_long() {
        // Спецификация: 1 с, 2 с, 4 с (+джиттер).
        let delays: Vec<Duration> = (1..=3)
            .map(|a| backoff(a, None, Duration::from_secs(1)))
            .collect();
        assert!(delays[0] < Duration::from_millis(1300), "{:?}", delays[0]);
        assert!(delays[1] < Duration::from_millis(2600), "{:?}", delays[1]);
        assert!(delays[2] < Duration::from_millis(5200), "{:?}", delays[2]);
    }

    // -----------------------------------------------------------------------
    // Мини-HTTP-сервер для интеграционных тестов
    // -----------------------------------------------------------------------

    /// Заготовленный ответ: статус, дополнительные заголовки, тело.
    #[derive(Clone)]
    struct Reply {
        status: u16,
        headers: Vec<String>,
        body: String,
        /// Закрыть соединение, не отвечая (имитация обрыва).
        silent: bool,
    }

    impl Reply {
        fn new(status: u16, body: &str) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.to_string(),
                silent: false,
            }
        }

        fn with_header(mut self, header: &str) -> Self {
            self.headers.push(header.to_string());
            self
        }

        fn silent() -> Self {
            Self {
                status: 0,
                headers: Vec::new(),
                body: String::new(),
                silent: true,
            }
        }
    }

    /// Сервер отдаёт ответы по очереди (последний повторяется), записывает
    /// полученные запросы. Возвращает базовый URL (`http://127.0.0.1:port`).
    async fn spawn_server(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local_addr");
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);

        tokio::spawn(async move {
            let mut index = 0usize;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(request) = read_request(&mut stream).await else {
                    continue;
                };
                seen.lock().expect("mutex").push(request);

                let reply = replies
                    .get(index.min(replies.len().saturating_sub(1)))
                    .cloned()
                    .unwrap_or_else(|| Reply::new(200, "{}"));
                index += 1;

                if reply.silent {
                    drop(stream);
                    continue;
                }
                let _ = stream.write_all(&reply_bytes(&reply)).await;
                let _ = stream.flush().await;
            }
        });

        (format!("http://{addr}"), requests)
    }

    /// Читает запрос до конца заголовков (тела у GET нет).
    async fn read_request(stream: &mut tokio::net::TcpStream) -> std::io::Result<String> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn reply_bytes(reply: &Reply) -> Vec<u8> {
        let reason = match reply.status {
            200 => "OK",
            401 => "Unauthorized",
            403 => "Forbidden",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            reply.status,
            reply.body.len()
        );
        for header in &reply.headers {
            head.push_str(header);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");

        let mut out = head.into_bytes();
        out.extend_from_slice(reply.body.as_bytes());
        out
    }

    /// Клиент против тестового сервера с миллисекундным backoff.
    fn test_client(base_url: &str) -> Client {
        Client::with_base_url(base_url).with_base_delay(Duration::from_millis(5))
    }

    fn paths_seen(requests: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        requests
            .lock()
            .expect("mutex")
            .iter()
            .filter_map(|r| r.lines().next().map(str::to_string))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Интеграционные тесты клиента
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn fetch_key_200_parses_and_sends_bearer_token() {
        let (base, requests) = spawn_server(vec![Reply::new(200, KEY_WITH_BUDGET)]).await;
        let key = parse(test_client(&base).fetch_key("sk-or-test-key").await);

        assert_eq!(key.label.as_deref(), Some("team key"));
        assert_eq!(key.budgets.len(), 1);

        let first = requests.lock().expect("mutex")[0].clone();
        assert!(
            first.starts_with("GET /key "),
            "неожиданная строка запроса: {first:?}"
        );
        assert!(
            first.contains("authorization: Bearer sk-or-test-key")
                || first.contains("Authorization: Bearer sk-or-test-key"),
            "заголовок Authorization не отправлен: {first:?}"
        );
    }

    #[tokio::test]
    async fn fetch_credits_200_parses() {
        let (base, requests) = spawn_server(vec![Reply::new(200, CREDITS_BODY)]).await;
        let credits = parse(test_client(&base).fetch_credits("mgmt-key").await);
        assert_eq!(credits.total_credits, 100.0);
        assert!((credits.remaining() - 57.75).abs() < 1e-9);
        let requests = requests.lock().expect("mutex");
        assert!(requests[0].starts_with("GET /credits "));
        assert!(requests[0].contains("Bearer mgmt-key"));
    }

    #[tokio::test]
    async fn status_401_is_unauthorized() {
        let (base, _) = spawn_server(vec![Reply::new(401, r#"{"error":{"code":401}}"#)]).await;
        assert_eq!(
            parse_err(test_client(&base).fetch_key("k").await),
            ApiError::Unauthorized
        );
    }

    #[tokio::test]
    async fn status_403_is_forbidden() {
        let (base, _) = spawn_server(vec![Reply::new(
            403,
            r#"{"error":{"code":403,"message":"Only management keys can perform this operation"}}"#,
        )])
        .await;
        let error = parse_err(test_client(&base).fetch_credits("k").await);
        assert_eq!(error, ApiError::Forbidden);
        // Сообщение сервера не попадает в текст ошибки.
        assert!(!error.to_string().contains("management"));
    }

    #[tokio::test]
    async fn status_500_is_http_error() {
        let (base, _) = spawn_server(vec![Reply::new(500, "boom")]).await;
        assert_eq!(
            parse_err(test_client(&base).fetch_key("k").await),
            ApiError::Http(500)
        );
    }

    #[tokio::test]
    async fn broken_body_is_parse_error() {
        let (base, _) = spawn_server(vec![Reply::new(200, "{")]).await;
        assert!(matches!(
            parse_err(test_client(&base).fetch_key("k").await),
            ApiError::Parse(_)
        ));
    }

    #[tokio::test]
    async fn retries_on_429_then_succeeds() {
        let (base, requests) = spawn_server(vec![
            Reply::new(429, r#"{"error":{"code":429}}"#).with_header("Retry-After: 0"),
            Reply::new(200, KEY_NULL_LIMIT),
        ])
        .await;
        let key = parse(test_client(&base).fetch_key("k").await);
        assert_eq!(key.label.as_deref(), Some("default"));
        assert_eq!(paths_seen(&requests).len(), 2, "ожидался один повтор");
    }

    #[tokio::test]
    async fn retries_respect_retry_after_header() {
        let (base, requests) = spawn_server(vec![
            Reply::new(429, "{}").with_header("Retry-After: 1"),
            Reply::new(200, KEY_NULL_LIMIT),
        ])
        .await;

        let started = std::time::Instant::now();
        // База backoff — 5 мс, но Retry-After: 1 требует ждать ~1 с.
        parse(test_client(&base).fetch_key("k").await);
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "Retry-After проигнорирован: {:?}",
            started.elapsed()
        );
        assert_eq!(paths_seen(&requests).len(), 2);
    }

    #[tokio::test]
    async fn persistent_429_becomes_rate_limited() {
        let (base, requests) =
            spawn_server(vec![Reply::new(429, "{}").with_header("Retry-After: 0")]).await;
        let error = parse_err(test_client(&base).fetch_key("k").await);
        assert_eq!(
            error,
            ApiError::RateLimited {
                retry_after: Some(Duration::from_secs(0))
            }
        );
        // 3 попытки: исходная + 2 повтора (MAX_RETRIES = 3).
        assert_eq!(paths_seen(&requests).len(), MAX_RETRIES as usize);
    }

    #[tokio::test]
    async fn long_retry_after_defers_to_caller_without_retrying() {
        let (base, requests) =
            spawn_server(vec![Reply::new(429, "{}").with_header("Retry-After: 600")]).await;
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            test_client(&base).fetch_credits("k").await
        })
        .await
        .expect("долгую задержку должен планировать вызывающий код");
        assert_eq!(
            parse_err(result),
            ApiError::RateLimited {
                retry_after: Some(Duration::from_secs(600))
            }
        );
        assert_eq!(paths_seen(&requests).len(), 1);
    }

    #[tokio::test]
    async fn persistent_429_without_header_keeps_none() {
        let (base, _) = spawn_server(vec![Reply::new(429, "{}")]).await;
        let error = parse_err(test_client(&base).fetch_key("k").await);
        assert_eq!(error, ApiError::RateLimited { retry_after: None });
    }

    #[tokio::test]
    async fn server_closing_connection_is_network_error() {
        let (base, _) = spawn_server(vec![Reply::silent()]).await;
        let error = parse_err(test_client(&base).fetch_key("k").await);
        assert!(matches!(error, ApiError::Network(_)), "{error:?}");
        // URL не должен утекать в текст ошибки.
        assert!(!error.to_string().contains("127.0.0.1"));
        assert!(!format!("{error:?}").contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn no_server_is_network_error() {
        // Порт, который никто не слушает: соединение отклоняется.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);

        let error = parse_err(test_client(&format!("http://{addr}")).fetch_key("k").await);
        assert!(matches!(error, ApiError::Network(_)), "{error:?}");
    }

    #[tokio::test]
    async fn clients_talk_to_their_own_base_url() {
        // BASE_URL не должен использоваться, если задан with_base_url.
        let (base, requests) = spawn_server(vec![Reply::new(200, KEY_NULL_LIMIT)]).await;
        parse(test_client(&base).fetch_key("k").await);
        assert!(base.starts_with("http://127.0.0.1:"));
        assert_eq!(paths_seen(&requests).len(), 1);
    }

    #[test]
    fn credits_status_classifies_results() {
        let credits = parse(parse_credits(CREDITS_BODY));
        assert_eq!(
            CreditsStatus::from(Ok(credits)),
            CreditsStatus::Available(credits)
        );
        for error in [ApiError::Unauthorized, ApiError::Forbidden] {
            assert_eq!(
                CreditsStatus::from(Err(error)),
                CreditsStatus::NeedsManagementKey
            );
        }
        for error in [
            ApiError::Http(500),
            ApiError::RateLimited { retry_after: None },
        ] {
            assert_eq!(CreditsStatus::from(Err(error)), CreditsStatus::Unavailable);
        }
    }

    #[tokio::test]
    async fn client_is_cloneable_and_reusable() {
        let (base, requests) = spawn_server(vec![Reply::new(200, KEY_NULL_LIMIT)]).await;
        let client = test_client(&base);
        let clone = client.clone();
        parse(client.fetch_key("k").await);
        parse(clone.fetch_key("k").await);
        assert_eq!(paths_seen(&requests).len(), 2);
    }

    #[test]
    fn new_uses_official_base_url() {
        // Конструктор не паникует и не требует сети.
        let _ = Client::new();
        let _ = Client::default();
        assert!(BASE_URL.starts_with("https://"));
        assert!(user_agent().starts_with("openrouter-balance/"));
    }
}
