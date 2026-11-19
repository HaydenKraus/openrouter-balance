//! Общее состояние приложения (читается UI, треем и кнопкой панели задач).

use crate::api::Snapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPhase {
    /// Ключа нет — показываем экран входа.
    SignedOut,
    /// Идёт OAuth-вход в браузере.
    LoggingIn,
    /// Ключ есть, идут обновления.
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Network,
    Timeout,
    /// 401: ключ недействителен, нужен повторный вход.
    Unauthorized,
    RateLimited,
    Server,
    Other,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub phase: AuthPhase,
    /// Последние известные данные (сохраняются при ошибках).
    pub snapshot: Option<Snapshot>,
    /// Текст последней ошибки обновления (без ключей), `None` — всё хорошо.
    pub error: Option<String>,
    pub error_kind: Option<ErrorKind>,
    pub refreshing: bool,
    /// Ошибка входа (OAuth / ручной ключ) для показа на экране входа.
    pub login_error: Option<String>,
    /// Задан ли management-ключ (сам ключ в состоянии не хранится).
    pub has_management_key: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            phase: AuthPhase::SignedOut,
            snapshot: None,
            error: None,
            error_kind: None,
            refreshing: false,
            login_error: None,
            has_management_key: false,
        }
    }
}

impl AppState {
    /// Данные устарели: есть снимок, но последнее обновление не удалось.
    pub fn is_stale(&self) -> bool {
        self.snapshot.is_some() && self.error.is_some()
    }
}
