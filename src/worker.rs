//! Ядро приложения: фоновое обновление, вход/выход, уведомления.
//! UI работает только через публичные неблокирующие методы `Core`.
//!
//! Ключи живут в памяти только в виде `String` внутри `Core` (нужны для запросов)
//! и в Windows Credential Manager. В логи и в состояние UI они не попадают.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::api::{ApiError, Client, CreditsStatus, KeyInfo, Snapshot};
use crate::auth::{self, KeyKind};
use crate::config::Config;
use crate::format::{money, remaining_fraction, TaskbarModel};
use crate::state::{AppState, AuthPhase, ErrorKind};
use crate::taskbar::{self, Shell};

/// После ошибки пробуем снова не реже, чем через столько секунд.
const ERROR_RETRY_SECS: u64 = 20;

/// Изменение management-ключа в настройках.
pub enum MgmtKeyChange {
    Keep,
    Set(String),
    Clear,
}

/// Отслеживание пересечения порога уведомления (без спама).
#[derive(Debug, Default)]
pub struct NotifyTracker {
    fired: bool,
}

impl NotifyTracker {
    /// `true`, если нужно показать уведомление сейчас: остаток опустился ниже порога
    /// и по этому пересечению уведомление ещё не показывалось. Повторно «взводится»,
    /// когда остаток снова не ниже порога (например, после сброса лимита).
    pub fn should_notify(&mut self, remaining_pct: Option<f64>, threshold_pct: f64) -> bool {
        let Some(p) = remaining_pct.filter(|p| p.is_finite()) else {
            self.fired = false;
            return false;
        };
        if threshold_pct.is_nan() || threshold_pct <= 0.0 {
            self.fired = false;
            return false;
        }
        if p < threshold_pct {
            if self.fired {
                false
            } else {
                self.fired = true;
                true
            }
        } else {
            self.fired = false;
            false
        }
    }
}

pub struct Core {
    state: Mutex<AppState>,
    config: Mutex<Config>,
    rt: Handle,
    client: Client,
    main_key: Mutex<Option<String>>,
    mgmt_key: Mutex<Option<String>>,
    wake: Notify,
    login_task: Mutex<Option<JoinHandle<()>>>,
    open_settings: AtomicBool,
    start_hidden: AtomicBool,
    /// Увеличивается при выходе: результаты «устаревших» запросов отбрасываются.
    generation: AtomicU64,
    notify: Mutex<NotifyTracker>,
    ctx: OnceLock<egui::Context>,
    shell: OnceLock<Arc<Shell>>,
    has_tray: AtomicBool,
    demo: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Отравленный mutex не должен валить приложение.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn classify(err: &ApiError) -> ErrorKind {
    match err {
        ApiError::Network(_) => ErrorKind::Network,
        ApiError::Timeout => ErrorKind::Timeout,
        ApiError::Unauthorized => ErrorKind::Unauthorized,
        ApiError::RateLimited { .. } => ErrorKind::RateLimited,
        ApiError::Http(_) => ErrorKind::Server,
        ApiError::Forbidden | ApiError::Parse(_) => ErrorKind::Other,
    }
}

impl Core {
    pub fn new(rt: Handle, config: Config) -> Arc<Self> {
        Self::build(rt, config, false)
    }

    fn build(rt: Handle, config: Config, demo: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(AppState::default()),
            config: Mutex::new(config),
            rt,
            client: Client::new(),
            main_key: Mutex::new(None),
            mgmt_key: Mutex::new(None),
            wake: Notify::new(),
            login_task: Mutex::new(None),
            open_settings: AtomicBool::new(false),
            start_hidden: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            notify: Mutex::new(NotifyTracker::default()),
            ctx: OnceLock::new(),
            shell: OnceLock::new(),
            has_tray: AtomicBool::new(false),
            demo,
        })
    }

    /// Демо-режим (`--demo[=green|yellow|red|nolimit|error]`): фиктивные данные,
    /// без сети и без Credential Manager. Нужен для проверки панели задач.
    pub fn new_demo(rt: Handle, config: Config, kind: &str) -> Arc<Self> {
        let key = |limit: Option<f64>, rem: Option<f64>, usage: f64| KeyInfo {
            label: Some("demo".into()),
            limit,
            limit_remaining: rem,
            usage,
            usage_daily: 1.23,
            usage_weekly: 8.9,
            usage_monthly: 37.6,
            ..Default::default()
        };
        let k = match kind {
            "yellow" => key(Some(50.0), Some(7.5), 42.5),
            "red" => key(Some(50.0), Some(1.2), 48.8),
            "nolimit" => key(None, None, 37.6),
            _ => key(Some(50.0), Some(12.4), 37.6),
        };
        let credits = crate::api::Credits {
            total_credits: 100.0,
            total_usage: 62.4,
        };
        let mut st = AppState {
            phase: AuthPhase::Ready,
            snapshot: Some(Snapshot {
                key: k,
                credits: CreditsStatus::Available(credits),
                fetched_at: std::time::SystemTime::now(),
            }),
            ..Default::default()
        };
        if kind == "error" {
            st.error = Some("нет соединения с сетью".into());
            st.error_kind = Some(ErrorKind::Network);
        }
        let core = Self::build(rt, config, true);
        *lock(&core.state) = st;
        core
    }

    /// Загружает ключи из Credential Manager. Возвращает `true`, если вход уже выполнен.
    pub fn init_keys(&self) -> bool {
        if self.demo {
            return true;
        }
        let main = auth::load_key(KeyKind::Main)
            .ok()
            .flatten()
            .filter(|k| !k.is_empty());
        let mgmt = auth::load_key(KeyKind::Management)
            .ok()
            .flatten()
            .filter(|k| !k.is_empty());
        let mut st = lock(&self.state);
        st.has_management_key = mgmt.is_some();
        st.phase = if main.is_some() {
            AuthPhase::Ready
        } else {
            AuthPhase::SignedOut
        };
        drop(st);
        let signed_in = main.is_some();
        *lock(&self.main_key) = main;
        *lock(&self.mgmt_key) = mgmt;
        signed_in
    }

    /// Запускает фоновый цикл обновления.
    pub fn start(self: &Arc<Self>) {
        if self.demo {
            return;
        }
        let me = self.clone();
        self.rt.spawn(async move { me.refresh_loop().await });
    }

    /// Подключает окно и Win32-слой; сразу публикует текущее состояние.
    pub fn attach(&self, ctx: egui::Context, shell: Option<Arc<Shell>>) {
        let _ = self.ctx.set(ctx);
        if let Some(s) = shell {
            let _ = self.shell.set(s);
            self.has_tray.store(true, Ordering::Relaxed);
        }
        self.publish();
    }

    /// Есть ли иконка трея (если нет — закрытие окна должно завершать приложение).
    pub fn has_tray(&self) -> bool {
        self.has_tray.load(Ordering::Relaxed)
    }

    pub fn request_repaint(&self) {
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }

    /// Показать окно (повторный запуск, клик по трею).
    pub fn show_window(&self) {
        if let Some(s) = self.shell.get() {
            s.show_window();
        }
    }

    /// Выход: убрать иконку трея и завершить процесс.
    pub fn quit(&self) -> ! {
        if let Some(s) = self.shell.get() {
            s.shutdown();
        }
        std::process::exit(0)
    }

    /// Применить изменение автозапуска, сделанное через меню трея.
    pub fn autostart_toggled(&self, on: bool) {
        let cfg = {
            let mut c = lock(&self.config);
            c.autostart = on;
            c.clone()
        };
        let _ = cfg.save();
        self.request_repaint();
    }

    /// Разослать состояние в трей/кнопку панели задач и перерисовать окно.
    fn publish(&self) {
        let sources = lock(&self.config).tray_sources.clone();
        let model = TaskbarModel::from_state_with(&lock(&self.state), &sources);
        if let Some(s) = self.shell.get() {
            s.update(model);
        }
        self.request_repaint();
    }

    // ----- публичное API для UI -------------------------------------------

    /// Копия состояния для отрисовки кадра.
    pub fn state(&self) -> AppState {
        lock(&self.state).clone()
    }

    pub fn config(&self) -> Config {
        lock(&self.config).clone()
    }

    /// `true` один раз после пункта трея «Настройки» — UI должен открыть настройки.
    pub fn take_open_settings(&self) -> bool {
        self.open_settings.swap(false, Ordering::Relaxed)
    }

    /// Запуск свёрнутым в трей: eframe показывает окно после первого кадра,
    /// поэтому UI скрывает его командой в первом кадре.
    pub fn set_start_hidden(&self) {
        self.start_hidden.store(true, Ordering::Relaxed);
    }

    pub fn take_start_hidden(&self) -> bool {
        self.start_hidden.swap(false, Ordering::Relaxed)
    }

    pub fn request_open_settings(&self) {
        self.open_settings.store(true, Ordering::Relaxed);
        self.request_repaint();
    }

    /// Немедленное обновление (кнопка «Обновить»). Не блокирует.
    pub fn refresh_now(&self) {
        self.wake.notify_one();
    }

    /// OAuth-вход в браузере. Не блокирует.
    pub fn login_oauth(self: &Arc<Self>) {
        if lock(&self.state).phase == AuthPhase::LoggingIn {
            return;
        }
        {
            let mut st = lock(&self.state);
            st.phase = AuthPhase::LoggingIn;
            st.login_error = None;
        }
        self.publish();
        let me = self.clone();
        let gen = self.generation.load(Ordering::SeqCst);
        let handle = self.rt.spawn(async move {
            let result = auth::login_oauth().await;
            match result {
                Ok(key) => {
                    me.finish_login(key, gen).await;
                    me.show_window();
                }
                Err(e) => me.fail_login(e.to_string()),
            }
        });
        *lock(&self.login_task) = Some(handle);
    }

    /// Отмена идущего OAuth-входа.
    pub fn cancel_login(&self) {
        if let Some(h) = lock(&self.login_task).take() {
            h.abort();
        }
        {
            let mut st = lock(&self.state);
            if st.phase == AuthPhase::LoggingIn {
                st.phase = if lock(&self.main_key).is_some() {
                    AuthPhase::Ready
                } else {
                    AuthPhase::SignedOut
                };
            }
            st.login_error = None;
        }
        self.publish();
    }

    /// Ручной ввод обычного API-ключа. Не блокирует; ошибка → `state.login_error`.
    pub fn login_manual(self: &Arc<Self>, key: String) {
        let key = key.trim().to_string();
        if key.is_empty() {
            self.fail_login("Введите API-ключ".into());
            return;
        }
        {
            let mut st = lock(&self.state);
            st.phase = AuthPhase::LoggingIn;
            st.login_error = None;
        }
        self.publish();
        let me = self.clone();
        let gen = self.generation.load(Ordering::SeqCst);
        let handle = self.rt.spawn(async move {
            match me.client.fetch_key(&key).await {
                Ok(_) => me.finish_login(key, gen).await,
                Err(ApiError::Unauthorized) => me.fail_login(
                    "Ключ недействителен (401). Проверьте, что ключ скопирован целиком.".into(),
                ),
                Err(e) => me.fail_login(format!("Не удалось проверить ключ: {e}")),
            }
        });
        *lock(&self.login_task) = Some(handle);
    }

    /// «Выйти»: удалить все ключи, очистить состояние.
    pub fn logout(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(h) = lock(&self.login_task).take() {
            h.abort();
        }
        *lock(&self.main_key) = None;
        *lock(&self.mgmt_key) = None;
        *lock(&self.notify) = NotifyTracker::default();
        *lock(&self.state) = AppState::default();
        self.publish();
        if !self.demo {
            self.rt.spawn_blocking(|| {
                let _ = auth::delete_all();
            });
        }
    }

    /// Применить настройки (+ автозапуск в реестре) и изменение management-ключа.
    /// Операции Credential Manager/реестра занимают миллисекунды, поэтому
    /// выполняются синхронно, чтобы вернуть ошибку в окно настроек.
    pub fn apply_settings(&self, cfg: Config, mgmt: MgmtKeyChange) -> Result<(), String> {
        let mut cfg = cfg.sanitized();
        let mut problems: Vec<String> = Vec::new();

        if !self.demo {
            if cfg.autostart != taskbar::is_autostart() {
                if let Err(e) = taskbar::set_autostart(cfg.autostart) {
                    problems.push(format!("автозапуск: {e}"));
                    cfg.autostart = taskbar::is_autostart();
                }
            }
            match mgmt {
                MgmtKeyChange::Keep => {}
                MgmtKeyChange::Set(k) => {
                    let k = k.trim().to_string();
                    if !k.is_empty() {
                        match auth::save_key(KeyKind::Management, &k) {
                            Ok(()) => {
                                *lock(&self.mgmt_key) = Some(k);
                                lock(&self.state).has_management_key = true;
                            }
                            Err(e) => problems.push(e.to_string()),
                        }
                    }
                }
                MgmtKeyChange::Clear => match auth::delete_key(KeyKind::Management) {
                    Ok(()) => {
                        *lock(&self.mgmt_key) = None;
                        lock(&self.state).has_management_key = false;
                    }
                    Err(e) => problems.push(e.to_string()),
                },
            }
        }

        if let Some(s) = self.shell.get() {
            s.set_autostart_checked(cfg.autostart);
        }
        if let Err(e) = cfg.save() {
            problems.push(format!("не удалось сохранить настройки: {e}"));
        }
        *lock(&self.config) = cfg;
        self.wake.notify_one();
        self.publish();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    // ----- внутреннее ------------------------------------------------------

    fn fail_login(&self, msg: String) {
        {
            let mut st = lock(&self.state);
            st.phase = if lock(&self.main_key).is_some() {
                AuthPhase::Ready
            } else {
                AuthPhase::SignedOut
            };
            st.login_error = Some(msg);
        }
        self.publish();
    }

    async fn finish_login(&self, key: String, gen: u64) {
        if self.generation.load(Ordering::SeqCst) != gen {
            return; // пока шёл вход, нажали «Выйти»
        }
        let to_save = key.clone();
        let saved =
            tokio::task::spawn_blocking(move || auth::save_key(KeyKind::Main, &to_save)).await;
        match saved {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return self.fail_login(e.to_string()),
            Err(_) => return self.fail_login("внутренняя ошибка при сохранении ключа".into()),
        }
        *lock(&self.main_key) = Some(key);
        {
            let mut st = lock(&self.state);
            st.phase = AuthPhase::Ready;
            st.login_error = None;
            st.snapshot = None;
            st.error = None;
            st.error_kind = None;
        }
        self.publish();
        self.wake.notify_one();
    }

    async fn refresh_loop(self: Arc<Self>) {
        // Первое обновление сразу, если вход уже выполнен.
        let mut forced = true;
        loop {
            let (signed_in, unauthorized) = {
                let st = lock(&self.state);
                (
                    st.phase == AuthPhase::Ready,
                    st.error_kind == Some(ErrorKind::Unauthorized),
                )
            };
            if signed_in && (forced || !unauthorized) {
                self.refresh_once().await;
            }
            let cfg_secs = lock(&self.config).refresh_secs;
            let wait = if lock(&self.state).error.is_some() {
                cfg_secs.min(ERROR_RETRY_SECS)
            } else {
                cfg_secs
            };
            tokio::select! {
                _ = self.wake.notified() => forced = true,
                _ = tokio::time::sleep(Duration::from_secs(wait)) => forced = false,
            }
        }
    }

    async fn refresh_once(&self) {
        let key = lock(&self.main_key).clone();
        let Some(key) = key else { return };
        let mgmt = lock(&self.mgmt_key).clone();
        let gen = self.generation.load(Ordering::SeqCst);
        lock(&self.state).refreshing = true;
        self.request_repaint();

        let result = self.client.fetch_snapshot(&key, mgmt.as_deref()).await;
        if self.generation.load(Ordering::SeqCst) != gen {
            return; // выход во время запроса — результат не нужен
        }
        let mut toast: Option<String> = None;
        {
            let mut st = lock(&self.state);
            st.refreshing = false;
            match result {
                Ok(snap) => {
                    let pct = remaining_fraction(&snap.key).map(|f| f * 100.0);
                    let threshold = lock(&self.config).notify_threshold_pct;
                    if lock(&self.notify).should_notify(pct, threshold) {
                        toast = Some(low_balance_text(&snap.key, pct));
                    }
                    st.snapshot = Some(snap);
                    st.error = None;
                    st.error_kind = None;
                }
                Err(e) => {
                    st.error_kind = Some(classify(&e));
                    st.error = Some(e.to_string());
                }
            }
        }
        self.publish();
        if let Some(text) = toast {
            self.rt.spawn_blocking(move || show_toast(&text));
        }
    }
}

fn low_balance_text(key: &KeyInfo, pct: Option<f64>) -> String {
    let rem = crate::format::key_remaining(key)
        .map(money)
        .unwrap_or_else(|| "—".into());
    match pct {
        Some(p) => {
            format!("Остаток {rem} ({p:.0}% лимита). Пора пополнить баланс или увеличить лимит.")
        }
        None => format!("Остаток {rem}."),
    }
}

fn show_toast(text: &str) {
    use tauri_winrt_notification::Toast;
    // Ошибка показа уведомления не критична.
    let _ = Toast::new(Toast::POWERSHELL_APP_ID)
        .title("OpenRouter: низкий остаток")
        .text1(text)
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_once_per_crossing() {
        let mut t = NotifyTracker::default();
        assert!(!t.should_notify(Some(50.0), 10.0));
        assert!(t.should_notify(Some(9.0), 10.0));
        assert!(!t.should_notify(Some(8.0), 10.0));
        assert!(!t.should_notify(Some(0.0), 10.0));
        // восстановление выше порога → снова взведено
        assert!(!t.should_notify(Some(10.0), 10.0));
        assert!(t.should_notify(Some(5.0), 10.0));
    }

    #[test]
    fn notify_disabled_or_no_limit() {
        let mut t = NotifyTracker::default();
        assert!(!t.should_notify(Some(1.0), 0.0));
        assert!(!t.should_notify(None, 10.0));
        assert!(!t.should_notify(Some(f64::NAN), 10.0));
        assert!(!t.should_notify(Some(1.0), f64::NAN));
        // после включения порога при низком остатке — одно уведомление
        assert!(t.should_notify(Some(1.0), 10.0));
    }

    #[test]
    fn notify_rearms_after_none() {
        let mut t = NotifyTracker::default();
        assert!(t.should_notify(Some(1.0), 10.0));
        assert!(!t.should_notify(None, 10.0)); // лимит убрали
        assert!(t.should_notify(Some(1.0), 10.0));
    }

    #[test]
    fn classify_errors() {
        assert_eq!(classify(&ApiError::Unauthorized), ErrorKind::Unauthorized);
        assert_eq!(classify(&ApiError::Timeout), ErrorKind::Timeout);
        assert_eq!(classify(&ApiError::Network("x".into())), ErrorKind::Network);
        assert_eq!(classify(&ApiError::Http(500)), ErrorKind::Server);
        assert_eq!(
            classify(&ApiError::RateLimited { retry_after: None }),
            ErrorKind::RateLimited
        );
    }

    #[test]
    fn toast_text_has_amount() {
        let k = KeyInfo {
            limit: Some(50.0),
            limit_remaining: Some(2.5),
            ..Default::default()
        };
        let t = low_balance_text(&k, Some(5.0));
        assert!(t.contains("$2.50") && t.contains("5%"));
    }
}
