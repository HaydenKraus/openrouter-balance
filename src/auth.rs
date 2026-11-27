//! OAuth PKCE (OpenRouter) и хранение ключей в Windows Credential Manager.

use std::io::ErrorKind;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::rngs::OsRng;
use rand::RngCore as _;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{timeout, Instant};

const AUTH_BASE: &str = "https://openrouter.ai";
const EXCHANGE_PATH: &str = "/api/v1/auth/keys";
/// Служебное имя сервиса в Credential Manager (боевые записи).
const KEYRING_SERVICE: &str = "OpenRouterBalance";
/// Ожидание callback в браузере.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Таймаут разговора с сервером обмена.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Запрос от браузера длиннее этого — считаем мусором.
const MAX_REQUEST_BYTES: usize = 8192;
/// Таймаут чтения запроса одного соединения.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum AuthError {
    /// Таймаут ожидания входа в браузере.
    Timeout,
    /// Пользователь отказал / браузер вернул ошибку / неверный state.
    Denied(String),
    /// Ошибка сети или ответа при обмене кода.
    Exchange(String),
    /// Ошибка локального сервера (нет свободного порта и т. п.).
    Server(String),
    /// Ошибка Credential Manager.
    Storage(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Timeout => write!(f, "время ожидания входа истекло"),
            AuthError::Denied(m) => write!(f, "вход отклонён: {m}"),
            AuthError::Exchange(m) => write!(f, "не удалось обменять код на ключ: {m}"),
            AuthError::Server(m) => write!(f, "ошибка локального сервера: {m}"),
            AuthError::Storage(m) => write!(f, "ошибка хранилища ключей: {m}"),
        }
    }
}
impl std::error::Error for AuthError {}

#[derive(Debug, Clone, PartialEq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Генерация `code_verifier` (43–128 символов, base64url без `=`) и S256-`code_challenge`.
pub fn generate_pkce() -> Pkce {
    // 32 случайных байта → 43 символа base64url без паддинга.
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = challenge_for(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// `code_challenge` = BASE64URL(SHA256(verifier)) без паддинга.
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Полный OAuth-вход: локальный сервер на свободном порту, открытие браузера,
/// ожидание `code` (таймаут 5 мин), закрытие сервера, обмен кода на ключ.
/// Возвращает ключ. Отмена — `JoinHandle::abort()` вызывающей стороны
/// (сокет закрывается при drop).
pub async fn login_oauth() -> Result<String, AuthError> {
    let pkce = generate_pkce();
    let state = random_state();
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| AuthError::Server(format!("не удалось занять локальный порт: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| AuthError::Server(format!("не удалось определить локальный порт: {e}")))?
        .port();
    let callback_url = format!("http://localhost:{port}/callback");
    let auth_url = build_auth_url(AUTH_BASE, &callback_url, &pkce.challenge, &state);

    open::that(&auth_url)
        .map_err(|e| AuthError::Server(format!("не удалось открыть браузер: {e}")))?;

    let code = wait_for_callback(listener, &state, LOGIN_TIMEOUT).await?;
    exchange_code(AUTH_BASE, &code, &pkce.verifier, EXCHANGE_TIMEOUT).await
}

fn random_state() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// URL авторизации с percent-encoding параметров.
fn build_auth_url(base: &str, callback_url: &str, challenge: &str, state: &str) -> String {
    format!(
        "{base}/auth?callback_url={}&code_challenge={}&code_challenge_method=S256&state={}",
        percent_encode(callback_url),
        percent_encode(challenge),
        percent_encode(state),
    )
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// Percent-encoding в кодировке UTF-8 (без внешних крейтов).
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if is_unreserved(*byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Percent-decoding; `+` не трогаем (в query он не пробел).
fn percent_decode(value: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Результат разбора первой строки HTTP-запроса к локальному серверу.
#[derive(Debug, PartialEq, Eq)]
enum CallbackOutcome {
    /// Пришёл код авторизации (state — как передал браузер).
    Code { code: String, state: Option<String> },
    /// Браузер вернул ошибку авторизации.
    Error(String),
    /// Путь не `/callback` (favicon и прочее).
    NotCallback,
    /// Запрос не разобран.
    Malformed,
}

/// Код авторизации OpenRouter — непрозрачная строка: проверяем только разумность
/// (непустая, не длиннее 2048 байт, без управляющих символов).
fn valid_code(input: &str) -> Option<String> {
    if input.is_empty() || input.len() > 2048 || input.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(input.to_string())
}

/// Разбор первой строки запроса: `GET /callback?code=<код>&state=... HTTP/1.1`.
fn parse_callback_request(first_line: &str) -> CallbackOutcome {
    let mut parts = first_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return CallbackOutcome::Malformed;
    };
    if method != "GET" {
        return CallbackOutcome::Malformed;
    }
    let target = target.split('#').next().unwrap_or(target);
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    if path != "/callback" {
        return CallbackOutcome::NotCallback;
    }

    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, percent_decode(value)),
            None => (pair, String::new()),
        };
        match key {
            "code" => code = Some(value),
            "state" => state = Some(value),
            "error" => error = Some(value),
            "error_description" => error_description = Some(value),
            _ => {}
        }
    }

    if let Some(error) = error {
        let message = match error_description {
            Some(description) if !description.is_empty() => format!("{error}: {description}"),
            _ => error,
        };
        return CallbackOutcome::Error(message);
    }
    match code.as_deref().and_then(valid_code) {
        Some(code) => CallbackOutcome::Code { code, state },
        None => CallbackOutcome::Malformed,
    }
}

fn html_response(status_line: &str, title: &str, body: &str) -> String {
    let page = format!(
        "<!DOCTYPE html>\n<html lang=\"ru\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>{title}</title>\n</head>\n<body style=\"font-family:Segoe UI,sans-serif;\
         margin:0;display:flex;align-items:center;justify-content:center;height:100vh;\">\n\
         <div style=\"text-align:center;\">\n<h1>{title}</h1>\n<p>{body}</p>\n</div>\n\
         </body>\n</html>\n"
    );
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    )
}

async fn write_response(mut stream: tokio::net::TcpStream, response: &str) {
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// Ожидание callback: принимает соединения, пока не получит `code`.
/// Возвращает код. Соединения не по `/callback` игнорируются.
pub(crate) async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    wait: Duration,
) -> Result<String, AuthError> {
    let deadline = Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AuthError::Timeout);
        }
        let accepted = match timeout(remaining, listener.accept()).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(AuthError::Server(format!("сбой локального сервера: {e}"))),
            Err(_) => return Err(AuthError::Timeout),
        };
        let (mut stream, _) = accepted;
        let mut buf = Vec::new();
        let read = async {
            let mut chunk = [0u8; 1024];
            loop {
                if buf.len() >= MAX_REQUEST_BYTES {
                    return;
                }
                let size = MAX_REQUEST_BYTES.saturating_sub(buf.len()).min(chunk.len());
                match stream.read(&mut chunk[..size]).await {
                    Ok(0) => return,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        // Заголовки кончились либо запрос длиннее лимита — читать больше нечего.
                        if buf.windows(4).any(|w| w == b"\r\n\r\n")
                            || buf.len() >= MAX_REQUEST_BYTES
                        {
                            return;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => return,
                }
            }
        };
        if timeout(READ_TIMEOUT, read).await.is_err() {
            // Медленный/чужой клиент — просто ждём следующий запрос.
            let _ = stream.shutdown().await;
            continue;
        }
        let request = String::from_utf8_lossy(&buf);
        let first_line = request.lines().next().unwrap_or("");
        match parse_callback_request(first_line) {
            CallbackOutcome::Code { code, state } => match state {
                Some(state) if state != expected_state => {
                    write_response(
                        stream,
                        &html_response(
                            "403 Forbidden",
                            "Вход отклонён",
                            "Не удалось подтвердить вход. Попробуйте ещё раз.",
                        ),
                    )
                    .await;
                    return Err(AuthError::Denied(
                        "параметр state не совпадает с отправленным".to_string(),
                    ));
                }
                _ => {
                    write_response(
                        stream,
                        &html_response("200 OK", "Вход выполнен", "Эту вкладку можно закрыть."),
                    )
                    .await;
                    return Ok(code);
                }
            },
            CallbackOutcome::Error(message) => {
                write_response(
                    stream,
                    &html_response(
                        "403 Forbidden",
                        "Вход отклонён",
                        "Авторизация не выдала доступ. Эту вкладку можно закрыть.",
                    ),
                )
                .await;
                return Err(AuthError::Denied(message));
            }
            CallbackOutcome::NotCallback => {
                write_response(
                    stream,
                    &html_response(
                        "404 Not Found",
                        "Страница не найдена",
                        "Откройте приложение.",
                    ),
                )
                .await;
            }
            CallbackOutcome::Malformed => {
                write_response(
                    stream,
                    &html_response(
                        "400 Bad Request",
                        "Некорректный запрос",
                        "Ожидается /callback.",
                    ),
                )
                .await;
            }
        }
    }
}

/// Обмен `code` на API-ключ. `base_url` — например `https://openrouter.ai` (без слэша на конце).
pub(crate) async fn exchange_code(
    base_url: &str,
    code: &str,
    verifier: &str,
    request_timeout: Duration,
) -> Result<String, AuthError> {
    let url = format!("{base_url}{EXCHANGE_PATH}");
    let body = serde_json::json!({
        "code": code,
        "code_verifier": verifier,
        "code_challenge_method": "S256",
    });

    let client = reqwest::Client::builder()
        .timeout(request_timeout)
        .build()
        .map_err(|_| AuthError::Exchange("не удалось создать HTTP-клиент".to_string()))?;

    let response = client.post(&url).json(&body).send().await.map_err(|e| {
        if e.is_timeout() {
            AuthError::Exchange("истёк таймаут запроса".to_string())
        } else if e.is_connect() {
            AuthError::Exchange("нет связи с сервером".to_string())
        } else {
            AuthError::Exchange("ошибка сети".to_string())
        }
    })?;

    let status = response.status();
    let text = response.text().await.map_err(|e| {
        if e.is_timeout() {
            AuthError::Exchange("истёк таймаут запроса".to_string())
        } else if e.is_decode() {
            // Ответ пришёл, но целиком не прочитался (обрыв соединения).
            AuthError::Exchange("ответ сервера не получен полностью".to_string())
        } else {
            AuthError::Exchange("не удалось прочитать ответ".to_string())
        }
    })?;

    if !status.is_success() {
        return Err(AuthError::Exchange(exchange_status_text(status.as_u16())));
    }

    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| AuthError::Exchange("сервер вернул некорректный ответ".to_string()))?;
    match parsed.get("key").and_then(|value| value.as_str()) {
        Some(key) if !key.is_empty() => Ok(key.to_string()),
        _ => Err(AuthError::Exchange(
            "сервер не вернул ключ в ответе".to_string(),
        )),
    }
}

fn exchange_status_text(status: u16) -> String {
    match status {
        400 => "сервер отклонил запрос (неверный метод challenge)".to_string(),
        401 => "сервер отклонил запрос (требуется авторизация)".to_string(),
        403 => "код неверен или истёк".to_string(),
        404 => "сервер обмена недоступен".to_string(),
        429 => "слишком много попыток, повторите позже".to_string(),
        s if (500..600).contains(&s) => "сервер OpenRouter вернул ошибку".to_string(),
        s => format!("неожиданный ответ сервера (код {s})"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// Обычный ключ (OAuth или введённый вручную).
    Main,
    /// Необязательный management-ключ.
    Management,
}

impl KeyKind {
    fn user(self) -> &'static str {
        match self {
            KeyKind::Main => "api-key",
            KeyKind::Management => "management-key",
        }
    }
}

fn entry(service: &str, kind: KeyKind) -> Result<keyring::Entry, AuthError> {
    keyring::Entry::new(service, kind.user())
        .map_err(|_| AuthError::Storage("не удалось открыть хранилище ключей".to_string()))
}

fn save_key_in(service: &str, kind: KeyKind, key: &str) -> Result<(), AuthError> {
    entry(service, kind)?
        .set_password(key)
        .map_err(|_| AuthError::Storage("не удалось сохранить ключ".to_string()))
}

fn load_key_in(service: &str, kind: KeyKind) -> Result<Option<String>, AuthError> {
    match entry(service, kind)?.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(AuthError::Storage("не удалось прочитать ключ".to_string())),
    }
}

fn delete_key_in(service: &str, kind: KeyKind) -> Result<(), AuthError> {
    match entry(service, kind)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(AuthError::Storage("не удалось удалить ключ".to_string())),
    }
}

pub fn save_key(kind: KeyKind, key: &str) -> Result<(), AuthError> {
    save_key_in(KEYRING_SERVICE, kind, key)
}

/// `Ok(None)` если ключа нет.
pub fn load_key(kind: KeyKind) -> Result<Option<String>, AuthError> {
    load_key_in(KEYRING_SERVICE, kind)
}

/// Удаление; отсутствие ключа — не ошибка.
pub fn delete_key(kind: KeyKind) -> Result<(), AuthError> {
    delete_key_in(KEYRING_SERVICE, kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    const RFC_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    fn is_base64url_nopad(value: &str) -> bool {
        !value.is_empty()
            && !value.contains('=')
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }

    // --- PKCE ---

    #[test]
    fn challenge_matches_rfc7636_appendix_b() {
        assert_eq!(challenge_for(RFC_VERIFIER), RFC_CHALLENGE);
    }

    #[test]
    fn generate_pkce_length_alphabet_and_uniqueness() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let pkce = generate_pkce();
            assert!(
                (43..=128).contains(&pkce.verifier.len()),
                "длина verifier: {}",
                pkce.verifier.len()
            );
            assert!(is_base64url_nopad(&pkce.verifier));
            assert!(is_base64url_nopad(&pkce.challenge));
            assert_eq!(pkce.challenge.len(), 43);
            assert_eq!(pkce.challenge, challenge_for(&pkce.verifier));
            assert!(seen.insert(pkce.verifier));
        }
    }

    #[test]
    fn challenge_is_s256_not_plain() {
        assert_ne!(challenge_for("YWJj"), "YWJj");
        assert_eq!(
            challenge_for(""),
            "47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU"
        );
    }

    // --- percent encoding / разбор запроса ---

    #[test]
    fn percent_encoding_roundtrip() {
        let raw = "http://localhost:51234/callback?a=1&b=т е/ст";
        let encoded = percent_encode(raw);
        assert!(!encoded.contains(':'));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('&'));
        assert_eq!(percent_decode(&encoded), raw);
    }

    #[test]
    fn percent_decode_tolerates_bad_input() {
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%41%42"), "AB");
        assert_eq!(percent_decode("a+b"), "a+b");
    }

    #[test]
    fn build_auth_url_contains_expected_params() {
        let url = build_auth_url(
            "https://openrouter.ai",
            "http://localhost:51234/callback",
            "abc_DEF-123",
            "deadbeef",
        );
        assert!(url.starts_with("https://openrouter.ai/auth?"));
        assert!(url.contains("callback_url=http%3A%2F%2Flocalhost%3A51234%2Fcallback"));
        assert!(url.contains("code_challenge=abc_DEF-123"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=deadbeef"));
    }

    #[test]
    fn parse_extracts_code_and_state() {
        let outcome = parse_callback_request("GET /callback?code=YWJjZGVm&state=XYZ HTTP/1.1\r");
        assert_eq!(
            outcome,
            CallbackOutcome::Code {
                code: "YWJjZGVm".to_string(),
                state: Some("XYZ".to_string())
            }
        );
    }

    #[test]
    fn parse_accepts_opaque_code_with_arbitrary_chars() {
        let outcome = parse_callback_request("GET /callback?code=a1b2-c3.d4~e5%2Bf6 HTTP/1.1");
        assert_eq!(
            outcome,
            CallbackOutcome::Code {
                code: "a1b2-c3.d4~e5+f6".to_string(),
                state: None
            }
        );
    }

    #[test]
    fn parse_accepts_missing_state() {
        let outcome = parse_callback_request("GET /callback?code=YWJjZGVm HTTP/1.1");
        assert_eq!(
            outcome,
            CallbackOutcome::Code {
                code: "YWJjZGVm".to_string(),
                state: None
            }
        );
    }

    #[test]
    fn parse_decodes_percent_escapes_in_state() {
        let outcome = parse_callback_request("GET /callback?code=YWJjZGVm&state=a%20b HTTP/1.1");
        match outcome {
            CallbackOutcome::Code { state, .. } => assert_eq!(state.as_deref(), Some("a b")),
            other => panic!("ожидался код, получено {other:?}"),
        }
    }

    #[test]
    fn parse_reports_error() {
        let outcome = parse_callback_request(
            "GET /callback?error=access_denied&error_description=User%20said%20no HTTP/1.1",
        );
        assert_eq!(
            outcome,
            CallbackOutcome::Error("access_denied: User said no".to_string())
        );
    }

    #[test]
    fn parse_rejects_other_paths_and_garbage() {
        assert_eq!(
            parse_callback_request("GET /favicon.ico HTTP/1.1"),
            CallbackOutcome::NotCallback
        );
        assert_eq!(
            parse_callback_request("GET /callback HTTP/1.1"),
            CallbackOutcome::Malformed
        );
        assert_eq!(
            parse_callback_request("POST /callback?code=YWJj HTTP/1.1"),
            CallbackOutcome::Malformed
        );
        assert_eq!(parse_callback_request(""), CallbackOutcome::Malformed);
        assert_eq!(parse_callback_request("мусор"), CallbackOutcome::Malformed);
        assert_eq!(
            parse_callback_request("GET /callback?code= HTTP/1.1"),
            CallbackOutcome::Malformed
        );
    }

    // --- ожидание callback ---

    async fn start_listener() -> TcpListener {
        TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("локальный порт для теста")
    }

    async fn send_raw(addr: std::net::SocketAddr, request: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("подключение к тестовому серверу");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("запрос отправлен");
        let mut response = Vec::new();
        let _ = timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await;
        String::from_utf8_lossy(&response).into_owned()
    }

    /// Соединение закрывается сразу после отправки — клиент видит EOF.
    async fn send_raw_abruptly(addr: std::net::SocketAddr, request: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("подключение к тестовому серверу");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("запрос отправлен");
        let _ = stream.shutdown().await;
        let mut response = Vec::new();
        let _ = timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await;
        String::from_utf8_lossy(&response).into_owned()
    }

    #[tokio::test]
    async fn callback_cycle_collects_code() {
        let listener = start_listener().await;
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(wait_for_callback(listener, "XYZ", Duration::from_secs(5)));
        let response = send_raw(
            addr,
            "GET /callback?code=YWJj&state=XYZ HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("charset=utf-8"));
        assert!(response.contains("Вход выполнен"));
        assert_eq!(server.await.unwrap().unwrap(), "YWJj");
    }

    #[tokio::test]
    async fn callback_with_error_is_denied() {
        let listener = start_listener().await;
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(wait_for_callback(listener, "XYZ", Duration::from_secs(5)));
        let response = send_raw(
            addr,
            "GET /callback?error=access_denied&state=XYZ HTTP/1.1\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        match server.await.unwrap() {
            Err(AuthError::Denied(message)) => assert!(message.contains("access_denied")),
            other => panic!("ожидался Denied, получено {other:?}"),
        }
    }

    #[tokio::test]
    async fn callback_with_wrong_state_is_denied() {
        let listener = start_listener().await;
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(wait_for_callback(listener, "XYZ", Duration::from_secs(5)));
        let response = send_raw(addr, "GET /callback?code=YWJj&state=OTHER HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(matches!(server.await.unwrap(), Err(AuthError::Denied(_))));
    }

    #[tokio::test]
    async fn favicon_then_callback_succeeds() {
        let listener = start_listener().await;
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(wait_for_callback(listener, "XYZ", Duration::from_secs(5)));
        let favicon = send_raw(addr, "GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(favicon.starts_with("HTTP/1.1 404 Not Found"));
        let garbage = send_raw_abruptly(addr, "мусор").await;
        assert!(garbage.starts_with("HTTP/1.1 400 Bad Request"));
        let ok = send_raw(
            addr,
            "GET /callback?code=YWJj&state=XYZ HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(server.await.unwrap().unwrap(), "YWJj");
    }

    #[tokio::test]
    async fn missing_state_is_accepted() {
        let listener = start_listener().await;
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(wait_for_callback(listener, "XYZ", Duration::from_secs(5)));
        let _ = send_raw(addr, "GET /callback?code=YWJj HTTP/1.1\r\n\r\n").await;
        assert_eq!(server.await.unwrap().unwrap(), "YWJj");
    }

    #[tokio::test]
    async fn wait_for_callback_times_out() {
        let listener = start_listener().await;
        let result = wait_for_callback(listener, "XYZ", Duration::from_millis(150)).await;
        assert_eq!(result, Err(AuthError::Timeout));
    }

    #[tokio::test]
    async fn oversized_request_is_ignored_then_callback_works() {
        let listener = start_listener().await;
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(wait_for_callback(listener, "XYZ", Duration::from_secs(10)));
        let huge = format!("GET /callback?pad={} HTTP/1.1\r\n\r\n", "a".repeat(9000));
        let response = send_raw_abruptly(addr, &huge).await;
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        let ok = send_raw(addr, "GET /callback?code=YWJj&state=XYZ HTTP/1.1\r\n\r\n").await;
        assert!(ok.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(server.await.unwrap().unwrap(), "YWJj");
    }

    // --- обмен кода ---

    /// Мини-сервер: отвечает заданным телом с корректным Content-Length на первый запрос.
    async fn spawn_stub_server(status_line: &'static str, body: &'static str) -> String {
        let listener = start_listener().await;
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = timeout(Duration::from_secs(5), stream.read(&mut buf)).await;
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
                let _ = stream.shutdown().await;
            }
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    #[tokio::test]
    async fn exchange_returns_key_on_200() {
        let base =
            spawn_stub_server("200 OK", r#"{"key":"sk-or-v1-testkey","user_id":"user_1"}"#).await;
        let key = exchange_code(&base, "abc", "verifier", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(key, "sk-or-v1-testkey");
    }

    #[tokio::test]
    async fn exchange_reports_status_without_body() {
        let base = spawn_stub_server("403 Forbidden", r#"{"error":"secret-body"}"#).await;
        match exchange_code(&base, "abc", "verifier", Duration::from_secs(5)).await {
            Err(AuthError::Exchange(message)) => {
                assert!(!message.contains("secret-body"));
                assert!(message.contains("403") || message.contains("код неверен"));
            }
            other => panic!("ожидался Exchange, получено {other:?}"),
        }
    }

    #[tokio::test]
    async fn exchange_rejects_empty_body_and_missing_key() {
        let empty = spawn_stub_server("200 OK", "").await;
        assert!(matches!(
            exchange_code(&empty, "abc", "verifier", Duration::from_secs(5)).await,
            Err(AuthError::Exchange(_))
        ));

        let no_key = spawn_stub_server("200 OK", r#"{"user_id":"user_1"}"#).await;
        assert!(matches!(
            exchange_code(&no_key, "abc", "verifier", Duration::from_secs(5)).await,
            Err(AuthError::Exchange(_))
        ));

        let blank_key = spawn_stub_server("200 OK", r#"{"key":""}"#).await;
        assert!(matches!(
            exchange_code(&blank_key, "abc", "verifier", Duration::from_secs(5)).await,
            Err(AuthError::Exchange(_))
        ));
    }

    #[tokio::test]
    async fn exchange_reports_truncated_response() {
        // Сервер обещает больше байт, чем отдал: ответ не читается целиком.
        let base = spawn_stub_server("200 OK", r#"{"key":"sk-or-v1-part"#).await;
        match exchange_code(&base, "abc", "verifier", Duration::from_secs(5)).await {
            Err(AuthError::Exchange(message)) => {
                assert!(!message.contains("sk-or-v1-part"), "секрет в тексте ошибки");
            }
            other => panic!("ожидался Exchange, получено {other:?}"),
        }
    }

    // --- хранилище ключей ---

    #[test]
    #[ignore = "требует реального Credential Manager (отдельный сервис)"]
    fn keyring_roundtrip_in_real_store() {
        let service = "OpenRouterBalanceTest";
        save_key_in(service, KeyKind::Main, "sk-or-v1-test-value").unwrap();
        assert_eq!(
            load_key_in(service, KeyKind::Main).unwrap().as_deref(),
            Some("sk-or-v1-test-value")
        );
        assert_eq!(load_key_in(service, KeyKind::Management).unwrap(), None);
        delete_key_in(service, KeyKind::Main).unwrap();
        assert_eq!(load_key_in(service, KeyKind::Main).unwrap(), None);
        delete_key_in(service, KeyKind::Main).unwrap();
        save_key_in(service, KeyKind::Management, "mgmt-test-value").unwrap();
        assert_eq!(
            load_key_in(service, KeyKind::Management)
                .unwrap()
                .as_deref(),
            Some("mgmt-test-value")
        );
        delete_key_in(service, KeyKind::Management).unwrap();
    }

    #[test]
    fn key_kind_maps_to_distinct_credential_names() {
        assert_eq!(KeyKind::Main.user(), "api-key");
        assert_eq!(KeyKind::Management.user(), "management-key");
        assert_ne!(KeyKind::Main.user(), KeyKind::Management.user());
    }
}
