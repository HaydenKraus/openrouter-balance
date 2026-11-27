//! Ядро приложения: фоновое обновление, вход/выход, уведомления.
//! UI работает только через публичные неблокирующие методы `Core`.
//!
//! Ключи живут в памяти только в виде `String` внутри `Core` (нужны для запросов)
//! и в Windows Credential Manager. В логи и в состояние UI они не попадают.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::api::{ApiError, Client, CreditsStatus, KeyInfo, Snapshot};
use crate::auth::{self, KeyKind};
use crate::config::Config;
use crate::format::{effective_remaining, money, LimitSource, RemainingLimit, TaskbarModel};
use crate::state::{AppState, AuthPhase, ErrorKind};
use crate::taskbar::{self, Shell};

/// После ошибки пробуем снова не реже, чем через столько секунд.
const ERROR_RETRY_SECS: u64 = 20;
/// Потолок паузы при ошибках подряд (нет сети, 429): 10 минут.
const ERROR_MAX_WAIT_SECS: u64 = 600;
const CREDITS_DENIED_RETRY: Duration = Duration::from_secs(600);
const MAX_WAIT_SLICE: Duration = Duration::from_secs(3600);

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

struct Session {
    state: AppState,
    main_key: Option<String>,
    mgmt_key: Option<String>,
    login_task: Option<JoinHandle<()>>,
    generation: u64,
    credits_generation: u64,
    credits: CreditsStatus,
    notify: NotifyTracker,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            state: AppState::default(),
            main_key: None,
            mgmt_key: None,
            login_task: None,
            generation: 0,
            credits_generation: 0,
            credits: CreditsStatus::Unavailable,
            notify: NotifyTracker::default(),
        }
    }
}

impl Session {
    fn stop_login(&mut self) {
        if let Some(task) = self.login_task.take() {
            task.abort();
        }
        self.generation += 1;
    }

    fn set_credits(&mut self, credits: CreditsStatus) {
        self.credits = credits;
        if let Some(snapshot) = &mut self.state.snapshot {
            snapshot.credits = credits;
        }
    }

    fn finish_login_error(&mut self, message: Option<String>) {
        self.state.phase = if self.main_key.is_some() {
            AuthPhase::Ready
        } else {
            AuthPhase::SignedOut
        };
        self.state.login_error = message;
    }
}

pub struct Core {
    // Один mutex упорядочивает операции с ключами и фиксацию сетевых результатов.
    session: Mutex<Session>,
    config: Mutex<Config>,
    rt: Handle,
    client: Client,
    wake: Notify,
    wake_credits: Notify,
    open_settings: AtomicBool,
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
            session: Mutex::new(Session::default()),
            config: Mutex::new(config),
            rt,
            client: Client::new(),
            wake: Notify::new(),
            wake_credits: Notify::new(),
            open_settings: AtomicBool::new(false),
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
        lock(&core.session).state = st;
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
        let mut session = lock(&self.session);
        session.state.has_management_key = mgmt.is_some();
        session.state.phase = if main.is_some() {
            AuthPhase::Ready
        } else {
            AuthPhase::SignedOut
        };
        let signed_in = main.is_some();
        session.main_key = main;
        session.mgmt_key = mgmt;
        signed_in
    }

    /// Запускает фоновый цикл обновления.
    pub fn start(self: &Arc<Self>) {
        if self.demo {
            return;
        }
        let me = self.clone();
        self.rt.spawn(async move { me.refresh_loop().await });
        let me = self.clone();
        self.rt.spawn(async move { me.credits_loop().await });
    }

    pub fn is_demo(&self) -> bool {
        self.demo
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
        // Сначала говорим egui/winit «окно видимо»: оно могло быть скрыто командой
        // Visible(false), и без этого winit считает его скрытым — тогда последующее
        // скрытие (крестик) не срабатывает. Win32-показ ниже нужен для foreground.
        if let Some(ctx) = self.ctx.get() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.request_repaint();
        }
        if let Some(s) = self.shell.get() {
            s.show_window();
        }
    }

    /// Скрыть окно в трей.
    pub fn hide_window(&self) {
        if let Some(ctx) = self.ctx.get() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.request_repaint();
        }
    }

    /// Клик по иконке трея: показать или скрыть окно.
    pub fn toggle_window(&self) {
        match self.shell.get() {
            Some(s) if s.is_window_shown() => self.hide_window(),
            _ => self.show_window(),
        }
    }

    /// Выход: убрать иконку трея и завершить процесс.
    pub fn quit(&self) -> ! {
        if let Some(s) = self.shell.get() {
            s.shutdown();
        }
        // Выход из процесса не прерывает начатую операцию с хранилищем.
        let _session = lock(&self.session);
        std::process::exit(0)
    }

    /// Применить изменение автозапуска, сделанное через меню трея.
    pub fn autostart_toggled(&self, on: bool) {
        if self.demo {
            return;
        }
        {
            let mut cfg = lock(&self.config);
            cfg.autostart = on;
            let _ = cfg.save();
        }
        self.request_repaint();
    }

    /// Разослать состояние в трей/кнопку панели задач и перерисовать окно.
    fn publish(&self) {
        let sources = lock(&self.config).tray_sources.clone();
        if let Some(s) = self.shell.get() {
            let session = lock(&self.session);
            let model = TaskbarModel::from_state_with(&session.state, &sources);
            s.update(model);
        }
        self.request_repaint();
    }

    // ----- публичное API для UI -------------------------------------------

    /// Копия состояния для отрисовки кадра.
    pub fn state(&self) -> AppState {
        lock(&self.session).state.clone()
    }

    pub fn config(&self) -> Config {
        lock(&self.config).clone()
    }

    /// `true` один раз после пункта трея «Настройки» — UI должен открыть настройки.
    pub fn take_open_settings(&self) -> bool {
        self.open_settings.swap(false, Ordering::Relaxed)
    }

    pub fn request_open_settings(&self) {
        // Окно могло быть скрыто в трей — сначала показываем его.
        self.show_window();
        self.open_settings.store(true, Ordering::Relaxed);
        self.request_repaint();
    }

    /// Немедленное обновление (кнопка «Обновить»). Не блокирует.
    pub fn refresh_now(&self) {
        self.wake.notify_one();
        self.wake_credits.notify_one();
    }

    /// OAuth-вход в браузере. Не блокирует.
    pub fn login_oauth(self: &Arc<Self>) {
        self.launch_login(true, async {
            auth::login_oauth().await.map_err(|e| e.to_string())
        });
    }

    /// Отмена идущего OAuth-входа.
    pub fn cancel_login(&self) {
        {
            let mut session = lock(&self.session);
            if session.state.phase != AuthPhase::LoggingIn {
                return;
            }
            session.stop_login();
            session.finish_login_error(None);
        }
        self.publish();
        self.refresh_now();
    }

    /// Ручной ввод обычного API-ключа. Не блокирует; ошибка → `state.login_error`.
    pub fn login_manual(self: &Arc<Self>, key: String) {
        let client = self.client.clone();
        self.launch_login(false, async move {
            let key = key.trim().to_string();
            if key.is_empty() {
                return Err("Введите API-ключ".into());
            }
            match client.fetch_key(&key).await {
                Ok(_) => Ok(key),
                Err(ApiError::Unauthorized) => {
                    Err("Ключ недействителен (401). Проверьте, что ключ скопирован целиком.".into())
                }
                Err(e) => Err(format!("Не удалось проверить ключ: {e}")),
            }
        });
    }

    fn launch_login(
        self: &Arc<Self>,
        oauth: bool,
        fetch: impl Future<Output = Result<String, String>> + Send + 'static,
    ) {
        if self.demo {
            return;
        }
        {
            let mut session = lock(&self.session);
            if oauth && session.state.phase == AuthPhase::LoggingIn {
                return;
            }
            session.stop_login();
            session.state.phase = AuthPhase::LoggingIn;
            session.state.login_error = None;
            session.state.refreshing = false;
            let generation = session.generation;
            let core = self.clone();
            session.login_task = Some(self.rt.spawn(async move {
                match fetch.await {
                    Ok(key) => {
                        let result = tokio::task::spawn_blocking({
                            let core = core.clone();
                            move || {
                                core.finish_login(key, generation, |key| {
                                    auth::save_key(KeyKind::Main, key)
                                })
                            }
                        })
                        .await;
                        match result {
                            Ok(true) if oauth => core.show_window(),
                            Err(_) => {
                                core.fail_login(generation, "Не удалось сохранить ключ".into())
                            }
                            _ => {}
                        }
                    }
                    Err(message) => core.fail_login(generation, message),
                }
            }));
        }
        self.publish();
    }

    /// «Выйти»: удалить все ключи, очистить состояние.
    pub fn logout(&self) {
        self.logout_with(auth::delete_key);
    }

    fn logout_with(&self, mut delete: impl FnMut(KeyKind) -> Result<(), auth::AuthError>) {
        if self.demo {
            return;
        }
        {
            let mut session = lock(&self.session);
            session.stop_login();
            session.state.refreshing = false;
            // Если первая запись не удалится, сохраняем обычную сессию для повторной попытки.
            let result = delete(KeyKind::Management).and_then(|()| {
                session.mgmt_key = None;
                session.state.has_management_key = false;
                session.set_credits(CreditsStatus::Unavailable);
                delete(KeyKind::Main)
            });
            match result {
                Ok(()) => {
                    session.state = AppState::default();
                    session.main_key = None;
                    session.mgmt_key = None;
                    session.credits = CreditsStatus::Unavailable;
                    session.notify = NotifyTracker::default();
                }
                Err(error) => session.finish_login_error(Some(format!(
                    "Не удалось выйти: {error}. Повторите выход."
                ))),
            }
        }
        self.publish();
        self.refresh_now();
    }

    /// Применить настройки (+ автозапуск в реестре) и изменение management-ключа.
    /// Операции Credential Manager/реестра занимают миллисекунды, поэтому
    /// выполняются синхронно, чтобы вернуть ошибку в окно настроек.
    pub fn apply_settings(&self, cfg: Config, mgmt: MgmtKeyChange) -> Result<(), String> {
        let mut cfg = cfg.sanitized();
        let mut current = lock(&self.config);
        if self.demo {
            cfg.autostart = current.autostart;
            *current = cfg;
            drop(current);
            self.publish();
            return Ok(());
        }
        let mut problems: Vec<String> = Vec::new();
        let mut credits_changed = false;

        if cfg.autostart != taskbar::is_autostart() {
            if let Err(e) = taskbar::set_autostart(cfg.autostart) {
                problems.push(format!("автозапуск: {e}"));
                cfg.autostart = taskbar::is_autostart();
            }
        }
        {
            let mut session = lock(&self.session);
            match mgmt {
                MgmtKeyChange::Keep => {}
                MgmtKeyChange::Set(k) => {
                    let k = k.trim().to_string();
                    if !k.is_empty() {
                        match auth::save_key(KeyKind::Management, &k) {
                            Ok(()) => {
                                session.mgmt_key = Some(k);
                                credits_changed = true;
                            }
                            Err(e) => problems.push(e.to_string()),
                        }
                    }
                }
                MgmtKeyChange::Clear => match auth::delete_key(KeyKind::Management) {
                    Ok(()) => {
                        session.mgmt_key = None;
                        credits_changed = true;
                    }
                    Err(e) => problems.push(e.to_string()),
                },
            }
            if credits_changed {
                session.credits_generation += 1;
                session.state.has_management_key = session.mgmt_key.is_some();
                session.set_credits(CreditsStatus::Unavailable);
            }
        }

        if let Some(s) = self.shell.get() {
            s.set_autostart_checked(cfg.autostart);
        }
        if let Err(e) = cfg.save() {
            problems.push(format!("не удалось сохранить настройки: {e}"));
        }
        *current = cfg;
        drop(current);
        self.refresh_now();
        self.publish();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    // ----- внутреннее ------------------------------------------------------

    fn fail_login(&self, generation: u64, message: String) {
        {
            let mut session = lock(&self.session);
            if session.generation != generation || session.state.phase != AuthPhase::LoggingIn {
                return;
            }
            session.finish_login_error(Some(message));
        }
        self.publish();
        self.refresh_now();
    }

    fn finish_login(
        &self,
        key: String,
        generation: u64,
        save: impl FnOnce(&str) -> Result<(), auth::AuthError>,
    ) -> bool {
        let success = {
            let mut session = lock(&self.session);
            if session.generation != generation || session.state.phase != AuthPhase::LoggingIn {
                return false;
            }
            match save(&key) {
                Ok(()) => {
                    session.main_key = Some(key);
                    session.credits = CreditsStatus::Unavailable;
                    session.notify = NotifyTracker::default();
                    session.state = AppState {
                        phase: AuthPhase::Ready,
                        has_management_key: session.mgmt_key.is_some(),
                        ..AppState::default()
                    };
                    true
                }
                Err(error) => {
                    session.finish_login_error(Some(error.to_string()));
                    false
                }
            }
        };
        self.publish();
        self.refresh_now();
        success
    }

    async fn refresh_loop(self: Arc<Self>) {
        let mut forced = true;
        let mut failures = 0u32;
        let mut last_generation = None;
        let mut retry = None;
        loop {
            let request = {
                let session = lock(&self.session);
                let unauthorized = session.state.error_kind == Some(ErrorKind::Unauthorized);
                if session.state.phase == AuthPhase::Ready && (forced || !unauthorized) {
                    session
                        .main_key
                        .clone()
                        .map(|key| (session.generation, key))
                } else {
                    None
                }
            };
            let Some((generation, key)) = request else {
                self.wake.notified().await;
                forced = true;
                continue;
            };
            if last_generation != Some(generation) {
                failures = 0;
                last_generation = Some(generation);
            }
            let remaining = retry_remaining(retry, generation);
            if !remaining.is_zero() {
                forced |= wait_for_wake(&self.wake, remaining).await;
                continue;
            }

            let Some((failed, server_delay)) = self.refresh_key(&key, generation).await else {
                continue;
            };
            retry = server_delay.map(|delay| (generation, Instant::now(), delay));
            failures = if failed {
                failures.saturating_add(1)
            } else {
                0
            };
            let cfg_secs = lock(&self.config).refresh_secs;
            let wait = if failed {
                error_wait_secs(cfg_secs, failures)
            } else {
                cfg_secs
            };
            forced = wait_for_wake(&self.wake, Duration::from_secs(wait)).await;
        }
    }

    async fn refresh_key(
        self: &Arc<Self>,
        key: &str,
        generation: u64,
    ) -> Option<(bool, Option<Duration>)> {
        {
            let mut session = lock(&self.session);
            if session.generation != generation {
                return None;
            }
            session.state.refreshing = true;
        }
        self.request_repaint();

        let result = self.client.fetch_key(key).await;
        let failed = result.is_err();
        let server_delay = result.as_ref().err().and_then(retry_after);
        let threshold = lock(&self.config).notify_threshold_pct;
        let mut toast = None;
        {
            let mut session = lock(&self.session);
            if session.generation != generation {
                return None;
            }
            session.state.refreshing = false;
            match result {
                Ok(key) => {
                    let remaining = effective_remaining(&key);
                    let pct = remaining.as_ref().map(|limit| limit.fraction * 100.0);
                    if session.notify.should_notify(pct, threshold) {
                        toast = remaining.map(low_balance_text);
                    }
                    session.state.snapshot = Some(Snapshot {
                        key,
                        credits: session.credits,
                        fetched_at: SystemTime::now(),
                    });
                    session.state.error = None;
                    session.state.error_kind = None;
                }
                Err(e) => {
                    session.state.error_kind = Some(classify(&e));
                    session.state.error = Some(e.to_string());
                }
            }
        }
        self.publish();
        if let Some(text) = toast {
            let core = self.clone();
            self.rt.spawn_blocking(move || {
                let session = lock(&core.session);
                if session.generation == generation {
                    show_toast(&text);
                }
            });
        }
        Some((failed, server_delay))
    }

    async fn credits_loop(self: Arc<Self>) {
        let mut retry = None;
        loop {
            let request = {
                let session = lock(&self.session);
                if session.state.phase == AuthPhase::Ready {
                    let key = session.mgmt_key.as_ref().or(session.main_key.as_ref());
                    key.cloned()
                        .map(|key| ((session.generation, session.credits_generation), key))
                } else {
                    None
                }
            };
            let Some((generation, key)) = request else {
                self.wake_credits.notified().await;
                continue;
            };
            let remaining = retry_remaining(retry, generation);
            if !remaining.is_zero() {
                wait_for_wake(&self.wake_credits, remaining).await;
                continue;
            }
            let result = self.client.fetch_credits(&key).await;
            let delay = result.as_ref().err().and_then(retry_after);
            let credits = CreditsStatus::from(result);
            {
                let mut session = lock(&self.session);
                if (session.generation, session.credits_generation) != generation {
                    continue;
                }
                session.set_credits(credits);
            }
            self.publish();

            let delay = if credits == CreditsStatus::NeedsManagementKey {
                Some(CREDITS_DENIED_RETRY)
            } else {
                delay
            };
            retry = delay.map(|delay| (generation, Instant::now(), delay));
            let wait = Duration::from_secs(lock(&self.config).refresh_secs);
            wait_for_wake(&self.wake_credits, wait).await;
        }
    }
}

async fn wait_for_wake(wake: &Notify, delay: Duration) -> bool {
    tokio::select! {
        _ = wake.notified() => true,
        _ = tokio::time::sleep(delay.min(MAX_WAIT_SLICE)) => false,
    }
}

fn retry_after(error: &ApiError) -> Option<Duration> {
    match error {
        ApiError::RateLimited { retry_after } => *retry_after,
        _ => None,
    }
}

fn retry_remaining<G: PartialEq>(retry: Option<(G, Instant, Duration)>, generation: G) -> Duration {
    match retry {
        Some((gen, started, delay)) if gen == generation => delay.saturating_sub(started.elapsed()),
        _ => Duration::ZERO,
    }
}

/// Пауза после `failures` ошибок подряд: первая как раньше (≤20 с),
/// дальше удваивается до потолка `ERROR_MAX_WAIT_SECS`.
fn error_wait_secs(cfg_secs: u64, failures: u32) -> u64 {
    let first = cfg_secs.min(ERROR_RETRY_SECS);
    let shift = failures.saturating_sub(1).min(10);
    (first << shift).min(ERROR_MAX_WAIT_SECS.max(first))
}

fn low_balance_text(limit: RemainingLimit) -> String {
    let source = match limit.source {
        LimitSource::Key => "лимита ключа",
        LimitSource::Budget => "бюджета",
    };
    let remaining = money(limit.remaining);
    let pct = limit.fraction * 100.0;
    format!("Остаток {remaining} ({pct:.0}% {source}). Проверьте настройки лимитов.")
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
    fn error_wait_grows_and_caps() {
        assert_eq!(error_wait_secs(60, 1), 20);
        assert_eq!(error_wait_secs(60, 2), 40);
        assert_eq!(error_wait_secs(60, 3), 80);
        assert_eq!(error_wait_secs(60, 6), 600);
        assert_eq!(error_wait_secs(60, 1000), 600);
        // короткий интервал из настроек — начинаем с него
        assert_eq!(error_wait_secs(5, 1), 5);
        assert_eq!(error_wait_secs(5, 0), 5);
    }

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
        let t = low_balance_text(effective_remaining(&k).unwrap());
        assert!(t.contains("$2.50") && t.contains("5%"));
    }

    #[test]
    fn toast_uses_amount_and_percent_of_the_same_budget() {
        let key = KeyInfo {
            limit: Some(100.0),
            limit_remaining: Some(90.0),
            effective_budget: Some(crate::api::KeyBudget {
                limit_usd: Some(10.0),
                remaining_usd: Some(1.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let text = low_balance_text(effective_remaining(&key).unwrap());
        assert!(text.contains("$1.00 (10% бюджета)"));
    }

    #[tokio::test]
    async fn stale_login_cannot_save_after_logout_and_new_login() {
        let core = Core::new(Handle::current(), Config::default());
        let previous = {
            let mut session = lock(&core.session);
            session.state.phase = AuthPhase::LoggingIn;
            session.generation
        };
        core.logout_with(|_| Ok(()));
        let current = {
            let mut session = lock(&core.session);
            session.state.phase = AuthPhase::LoggingIn;
            session.generation
        };
        assert!(!core.finish_login("old".into(), previous, |_| panic!("stale save")));
        assert!(core.finish_login("new".into(), current, |key| {
            assert_eq!(key, "new");
            Ok(())
        }));
        assert_eq!(core.state().phase, AuthPhase::Ready);
    }

    #[tokio::test]
    async fn logout_reports_storage_failure_and_can_be_retried() {
        let core = Core::new(Handle::current(), Config::default());
        {
            let mut session = lock(&core.session);
            session.main_key = Some("test".into());
            session.state.phase = AuthPhase::Ready;
        }
        core.logout_with(|_| Err(auth::AuthError::Storage("test failure".into())));
        let state = core.state();
        assert_eq!(state.phase, AuthPhase::Ready);
        assert!(state.login_error.unwrap().contains("test failure"));

        core.logout_with(|_| Ok(()));
        assert_eq!(core.state().phase, AuthPhase::SignedOut);
        assert!(core.state().login_error.is_none());
    }

    #[tokio::test]
    async fn partial_logout_clears_the_deleted_management_key() {
        let core = Core::new(Handle::current(), Config::default());
        {
            let mut session = lock(&core.session);
            session.main_key = Some("main".into());
            session.mgmt_key = Some("management".into());
            session.state.phase = AuthPhase::Ready;
            session.state.has_management_key = true;
        }
        core.logout_with(|kind| match kind {
            KeyKind::Management => Ok(()),
            KeyKind::Main => Err(auth::AuthError::Storage("test failure".into())),
        });
        let session = lock(&core.session);
        assert!(session.main_key.is_some());
        assert!(session.mgmt_key.is_none());
        assert!(!session.state.has_management_key);
        assert!(session.state.login_error.is_some());
    }

    #[test]
    fn rate_limit_delay_applies_until_the_key_changes() {
        let retry = Some((1, Instant::now(), Duration::from_secs(u64::MAX)));
        assert!(retry_remaining(retry, 1) > Duration::from_secs(30));
        assert_eq!(retry_remaining(retry, 2), Duration::ZERO);
    }

    #[tokio::test]
    async fn demo_keeps_its_session_and_only_updates_settings_in_memory() {
        let core = Core::new_demo(Handle::current(), Config::default(), "green");
        core.logout_with(|_| panic!("demo must not delete credentials"));
        core.login_oauth();
        core.login_manual("test".into());
        let mut cfg = core.config();
        cfg.refresh_secs += 1;
        cfg.autostart = !cfg.autostart;
        core.apply_settings(cfg.clone(), MgmtKeyChange::Set("test".into()))
            .unwrap();
        assert_eq!(core.state().phase, AuthPhase::Ready);
        assert!(!core.state().has_management_key);
        assert_eq!(core.config().refresh_secs, cfg.refresh_secs);
        assert_ne!(core.config().autostart, cfg.autostart);
        assert!(lock(&core.session).login_task.is_none());
    }

    #[tokio::test]
    async fn key_updates_while_credits_are_pending() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let release_credits = Arc::new(Notify::new());
        let server = tokio::spawn({
            let release_credits = release_credits.clone();
            async move {
                for _ in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    let release_credits = release_credits.clone();
                    tokio::spawn(async move {
                        let mut stream = BufReader::new(stream);
                        let mut line = String::new();
                        stream.read_line(&mut line).await.unwrap();
                        let body = if line.starts_with("GET /key ") {
                            r#"{"data":{"usage":12.5}}"#
                        } else {
                            assert!(line.starts_with("GET /credits "));
                            release_credits.notified().await;
                            r#"{"data":{"total_credits":100,"total_usage":10}}"#
                        };
                        loop {
                            line.clear();
                            stream.read_line(&mut line).await.unwrap();
                            if line == "\r\n" || line.is_empty() {
                                break;
                            }
                        }
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream.get_mut().write_all(reply.as_bytes()).await.unwrap();
                    });
                }
            }
        });
        let mut core = Core::new(Handle::current(), Config::default());
        Arc::get_mut(&mut core).unwrap().client = Client::with_base_url(&base);
        {
            let mut session = lock(&core.session);
            session.main_key = Some("test".into());
            session.state.phase = AuthPhase::Ready;
        }
        core.start();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(snapshot) = core.state().snapshot {
                    assert_eq!(snapshot.key.usage, 12.5);
                    assert_eq!(snapshot.credits, CreditsStatus::Unavailable);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            release_credits.notify_one();
            loop {
                let snapshot = core.state().snapshot.unwrap();
                if matches!(snapshot.credits, CreditsStatus::Available(_)) {
                    assert_eq!(snapshot.key.usage, 12.5);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("key and credits should update independently");
        server.await.unwrap();
    }
}
