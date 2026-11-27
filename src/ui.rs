//! egui-интерфейс приложения: экран входа, главный экран с данными, настройки.

use std::sync::Arc;

use egui::{Color32, RichText};

use crate::api::{CreditsStatus, KeyInfo, Snapshot};
use crate::config::{Config, TraySource, MAX_REFRESH_SECS, MAX_TRAY_SOURCES, MIN_REFRESH_SECS};
use crate::format::{self, Level, LimitSource, RemainingLimit};
use crate::state::{AppState, AuthPhase, ErrorKind};
use crate::worker::{Core, MgmtKeyChange};

/// Заголовок окна (само окно подписывает Win32-слой — тултип миниатюры панели задач).
pub const WINDOW_TITLE: &str = "OpenRouter Баланс";

// ---------------------------------------------------------------------------
// Чистые функции отображения (покрыты тестами)
// ---------------------------------------------------------------------------

fn spent_caption(limit: RemainingLimit) -> String {
    let label = match limit.source {
        LimitSource::Key => "Потрачено",
        LimitSource::Budget => "Потрачено из бюджета",
    };
    format!(
        "{label} {} из {} ({:.0}%)",
        format::money(limit.limit - limit.remaining),
        format::money(limit.limit),
        (1.0 - limit.fraction) * 100.0,
    )
}

/// Остаток ключа для карточки: `None` — лимита нет.
pub fn remaining_text(remaining: Option<f64>) -> String {
    match remaining {
        Some(r) => format::money(r),
        None => "—".to_string(),
    }
}

/// Остаток отрицателен — показываем красным как есть.
pub fn remaining_is_negative(remaining: Option<f64>) -> bool {
    matches!(remaining, Some(r) if r < 0.0)
}

/// Выбор цвета: единственный источник — `format::Level`.
pub fn level_color(level: Level) -> Color32 {
    let [r, g, b] = level.rgb();
    Color32::from_rgb(r, g, b)
}

/// Кнопки «Обновить» / «Настройки» / «Выйти» — только в фазе `Ready`.
pub fn show_toolbar(phase: AuthPhase) -> bool {
    matches!(phase, AuthPhase::Ready)
}

/// Текст статуса соединения: `(●, подпись, цвет)`.
pub fn connection_status(state: &AppState) -> (&'static str, String, Color32) {
    if let Some(err) = &state.error {
        return ("●", format!("Ошибка: {err}"), level_color(Level::Error));
    }
    if state.refreshing {
        return ("●", "Обновление…".to_string(), level_color(Level::Unknown));
    }
    if state.snapshot.is_some() {
        return ("●", "Подключено".to_string(), level_color(Level::Green));
    }
    ("●", "Загрузка…".to_string(), level_color(Level::Unknown))
}

/// Подсказка вместо блока баланса, когда данных нет.
pub fn credits_hint(status: &CreditsStatus) -> Option<&'static str> {
    match status {
        CreditsStatus::Available(_) => None,
        CreditsStatus::NeedsManagementKey => {
            Some("Для общего баланса нужен management-ключ (Настройки).")
        }
        CreditsStatus::Unavailable => Some("Баланс аккаунта временно недоступен."),
    }
}

/// Строка бюджета ключа, если он задан.
pub fn budget_line(b: &crate::api::KeyBudget) -> String {
    let rem = b
        .remaining_usd
        .map(format::money)
        .unwrap_or_else(|| "—".to_string());
    let lim = b
        .limit_usd
        .map(format::money)
        .unwrap_or_else(|| "—".to_string());
    match &b.reset_interval {
        Some(interval) if !interval.is_empty() => {
            format!("Бюджет: остаток {rem} из {lim} (период {interval})")
        }
        _ => format!("Бюджет: остаток {rem} из {lim}"),
    }
}

/// Метка лимита ключа: `None` — «без лимита».
pub fn limit_text(limit: Option<f64>) -> String {
    match limit {
        Some(l) if l >= 0.0 && l.is_finite() => format::money(l),
        _ => "без лимита".to_string(),
    }
}

/// Включить/выключить источник для иконки трея.
///
/// Порядок `list` — порядок строк на иконке, поэтому включение добавляет в конец.
/// Лимит `MAX_TRAY_SOURCES` и минимум один источник соблюдаются здесь.
pub fn toggle_tray_source(list: &mut Vec<TraySource>, src: TraySource, on: bool) {
    if on {
        if list.len() < MAX_TRAY_SOURCES && !list.contains(&src) {
            list.push(src);
        }
    } else if list.len() > 1 {
        list.retain(|s| *s != src);
    }
}

/// Чекбокс источника доступен, если он уже выбран или лимит ещё не достигнут.
pub fn tray_source_enabled(selected: usize, src_on: bool) -> bool {
    src_on || selected < MAX_TRAY_SOURCES
}

/// Можно ли применить клик по галочке источника: включить — пока есть место,
/// снять — пока остаётся хотя бы один источник.
pub fn tray_click_allowed(selected: usize, wanted_on: bool) -> bool {
    if wanted_on {
        selected < MAX_TRAY_SOURCES
    } else {
        selected > 1
    }
}

// ---------------------------------------------------------------------------
// Приложение
// ---------------------------------------------------------------------------

/// Черновик настроек: живёт в `App`, чтобы «Отмена» ничего не меняла.
struct SettingsDraft {
    cfg: Config,
    mgmt: String,
    /// Нажата «Удалить management-ключ»; применяется по «Сохранить».
    remove_mgmt: bool,
    error: Option<String>,
}

enum SettingsAction {
    ContinueEditing,
    Save,
    Cancel,
}

impl SettingsDraft {
    fn finish(
        mut self,
        action: SettingsAction,
        apply: impl FnOnce(Config, MgmtKeyChange) -> Result<(), String>,
    ) -> Option<Self> {
        match action {
            SettingsAction::ContinueEditing => return Some(self),
            SettingsAction::Cancel => return None,
            SettingsAction::Save => {}
        }
        // Введённый новый ключ важнее нажатого ранее «Удалить».
        let key = self.mgmt.trim();
        let change = if !key.is_empty() {
            MgmtKeyChange::Set(key.to_owned())
        } else if self.remove_mgmt {
            MgmtKeyChange::Clear
        } else {
            MgmtKeyChange::Keep
        };
        self.mgmt.clear();
        match apply(self.cfg.clone(), change) {
            Ok(()) => None,
            Err(error) => {
                self.error = Some(error);
                Some(self)
            }
        }
    }
}

pub struct App {
    core: Arc<Core>,
    login_key: String,
    settings: Option<SettingsDraft>,
    start_hidden: bool,
    /// Запрошенное скрытие окна (обрабатывается в конце кадра).
    hide_requested: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, core: Arc<Core>, start_hidden: bool) -> Self {
        // Тема — системная; размеры egui масштабирует под DPI автоматически.
        cc.egui_ctx.set_theme(egui::ThemePreference::System);
        Self {
            core,
            login_key: String::new(),
            settings: None,
            start_hidden,
            hide_requested: false,
        }
    }

    fn state(&self) -> AppState {
        self.core.state()
    }

    fn open_settings(&mut self) {
        self.settings = Some(SettingsDraft {
            cfg: self.core.config(),
            mgmt: String::new(),
            remove_mgmt: false,
            error: None,
        });
    }

    // -- экран входа --------------------------------------------------------

    fn login_screen(&mut self, ui: &mut egui::Ui, state: &AppState) {
        ui.add_space(6.0);
        ui.heading("OpenRouter Баланс");
        ui.label(RichText::new("Баланс и расход API-ключа на рабочем столе.").weak());
        ui.add_space(12.0);

        match state.phase {
            AuthPhase::LoggingIn => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Ожидание входа в браузере…");
                });
                ui.add_space(6.0);
                if ui.button("Отмена").clicked() {
                    self.core.cancel_login();
                }
            }
            _ => {
                let enter = ui.add_sized(
                    [ui.available_width(), 34.0],
                    egui::Button::new(RichText::new("Войти через OpenRouter").strong()),
                );
                if enter.clicked() {
                    self.core.login_oauth();
                }
                ui.add_space(4.0);
                ui.label(RichText::new("Откроется браузер для подтверждения доступа.").weak());
            }
        }

        if let Some(err) = &state.login_error {
            ui.add_space(8.0);
            ui.colored_label(level_color(Level::Red), err.as_str());
        }

        ui.add_space(10.0);
        egui::CollapsingHeader::new("Войти с помощью API-ключа")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "Ключ создаётся вручную на openrouter.ai/settings/keys и хранится \
                         в Диспетчере учётных данных Windows.",
                    )
                    .weak()
                    .small(),
                );
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.login_key)
                        .password(true)
                        .hint_text("sk-or-v1-…")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);
                let can_save = !self.login_key.trim().is_empty();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(can_save, egui::Button::new("Сохранить"))
                        .clicked()
                    {
                        let key = std::mem::take(&mut self.login_key);
                        self.core.login_manual(key);
                    }
                    if ui.button("Очистить").clicked() {
                        self.login_key.clear();
                    }
                });
            });

        ui.add_space(8.0);
        ui.separator();
        if ui.button("Настройки").clicked() {
            self.open_settings();
        }
    }

    // -- главный экран ------------------------------------------------------

    fn main_screen(&mut self, ui: &mut egui::Ui, state: &AppState) {
        if let Some(error) = &state.login_error {
            ui.colored_label(level_color(Level::Red), error.as_str());
            ui.add_space(4.0);
        }
        if state.is_stale() {
            ui.colored_label(level_color(Level::Error), "⚠ данные устарели");
            ui.add_space(4.0);
        }

        if state.error_kind == Some(ErrorKind::Unauthorized) {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(
                    level_color(Level::Red),
                    "Ключ недействителен — войдите заново.",
                );
                if ui.button("Войти заново").clicked() {
                    self.core.logout();
                }
            });
            ui.add_space(4.0);
        }

        match &state.snapshot {
            Some(snap) => self.snapshot_view(ui, snap, state.is_stale()),
            None => match &state.error {
                Some(err) => {
                    ui.add_space(6.0);
                    ui.colored_label(level_color(Level::Red), format!("Ошибка: {err}"));
                    ui.add_space(6.0);
                    if ui
                        .add_enabled(!state.refreshing, egui::Button::new("Повторить"))
                        .clicked()
                    {
                        self.core.refresh_now();
                    }
                }
                None => {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Загрузка…").weak());
                    });
                }
            },
        }
    }

    fn snapshot_view(&self, ui: &mut egui::Ui, snap: &Snapshot, stale: bool) {
        let key = &snap.key;

        self.limit_bar(ui, format::effective_remaining(key), stale);
        ui.add_space(10.0);
        self.cards(ui, key);
        ui.add_space(10.0);
        self.spending(ui, key);
        if let Some(b) = &key.effective_budget {
            ui.add_space(6.0);
            ui.label(budget_line(b));
        }
        ui.add_space(10.0);
        self.credits_block(ui, snap);
    }

    fn limit_bar(&self, ui: &mut egui::Ui, limit: Option<RemainingLimit>, stale: bool) {
        match limit {
            Some(limit) => {
                let level = if stale {
                    Level::Error
                } else {
                    format::level_for_fraction(Some(limit.fraction))
                };
                let bar = egui::ProgressBar::new((1.0 - limit.fraction) as f32)
                    .desired_width(ui.available_width())
                    .desired_height(18.0)
                    .fill(level_color(level))
                    .animate(false);
                ui.add(bar);
                ui.add_space(2.0);
                ui.label(RichText::new(spent_caption(limit)).strong());
            }
            None => {
                ui.label(RichText::new("Лимит: без лимита").weak());
            }
        }
    }

    fn cards(&self, ui: &mut egui::Ui, key: &KeyInfo) {
        let remaining = format::key_remaining(key);
        // Карточка показывает остаток самого ключа, независимо от бюджета.
        let rem_color = if remaining_is_negative(remaining) {
            level_color(Level::Red)
        } else {
            level_color(format::level_for_fraction(
                format::key_limit(key).map(|limit| limit.fraction),
            ))
        };
        let big = |ui: &mut egui::Ui, value: String, color: Option<Color32>| {
            let text = RichText::new(value).size(24.0).strong().monospace();
            match color {
                Some(c) => ui.label(text.color(c)),
                None => ui.label(text),
            };
        };

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let w = (ui.available_width() - 2.0 * ui.spacing().item_spacing.x) / 3.0;
                let mut col = |title: &str, value: String, color: Option<Color32>| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(w.max(60.0), 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.label(RichText::new(title).small().weak());
                            big(ui, value, color);
                        },
                    );
                };
                col("Потрачено", format::money(key.usage), None);
                col("Остаток ключа", remaining_text(remaining), Some(rem_color));
                col("Лимит ключа", limit_text(key.limit), None);
            });
        });
    }

    fn spending(&self, ui: &mut egui::Ui, key: &KeyInfo) {
        ui.label(RichText::new("Расход").small().strong());
        egui::Grid::new("ui_spending")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                for (name, v) in [
                    ("За сутки", key.usage_daily),
                    ("За неделю", key.usage_weekly),
                    ("За месяц", key.usage_monthly),
                ] {
                    ui.label(RichText::new(name).weak());
                    ui.monospace(format::money(v));
                    ui.end_row();
                }
            });
    }

    fn credits_block(&self, ui: &mut egui::Ui, snap: &Snapshot) {
        if let CreditsStatus::Available(c) = snap.credits {
            ui.separator();
            ui.label(RichText::new("Баланс аккаунта").small().strong());
            egui::Grid::new("ui_credits")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("Всего пополнено").weak());
                    ui.monospace(format::money(c.total_credits));
                    ui.end_row();
                    ui.label(RichText::new("Использовано").weak());
                    ui.monospace(format::money(c.total_usage));
                    ui.end_row();
                    ui.label(RichText::new("Остаток").weak());
                    ui.monospace(RichText::new(format::money(c.remaining())).strong());
                    ui.end_row();
                });
        } else if let Some(hint) = credits_hint(&snap.credits) {
            ui.add_space(4.0);
            ui.label(RichText::new(hint).small().weak());
        }
    }

    fn status_line(&self, ui: &mut egui::Ui, state: &AppState) {
        ui.horizontal_wrapped(|ui| {
            match &state.snapshot {
                Some(snap) => {
                    ui.label(
                        RichText::new(format!(
                            "Обновлено {}",
                            format::format_time(snap.fetched_at)
                        ))
                        .weak(),
                    );
                }
                None => {
                    ui.label(RichText::new("Данных ещё нет").weak());
                }
            }
            ui.separator();
            let (dot, text, color) = connection_status(state);
            ui.colored_label(color, dot);
            ui.colored_label(color, text);
        });
    }

    // -- настройки ----------------------------------------------------------

    fn settings_window(&mut self, ctx: &egui::Context, state: &AppState) {
        let Some(mut draft) = self.settings.take() else {
            return;
        };
        let mut action = SettingsAction::ContinueEditing;
        let demo = self.core.is_demo();

        egui::Window::new("Настройки")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_min_width(330.0);

                let mut secs = draft
                    .cfg
                    .refresh_secs
                    .clamp(MIN_REFRESH_SECS, MAX_REFRESH_SECS);
                ui.horizontal(|ui| {
                    ui.label("Интервал автообновления:");
                    ui.add(
                        egui::DragValue::new(&mut secs)
                            .range(MIN_REFRESH_SECS..=MAX_REFRESH_SECS)
                            .speed(1.0)
                            .suffix(" с"),
                    );
                });
                ui.add(
                    egui::Slider::new(&mut secs, MIN_REFRESH_SECS..=MAX_REFRESH_SECS)
                        .integer()
                        .show_value(false),
                );
                draft.cfg.refresh_secs = secs.clamp(MIN_REFRESH_SECS, MAX_REFRESH_SECS);

                ui.add_space(6.0);
                let mut pct = if draft.cfg.notify_threshold_pct.is_finite() {
                    draft.cfg.notify_threshold_pct.clamp(0.0, 100.0)
                } else {
                    0.0
                };
                ui.horizontal(|ui| {
                    ui.label("Порог уведомления:");
                    ui.add(
                        egui::DragValue::new(&mut pct)
                            .range(0.0..=100.0)
                            .speed(0.5)
                            .fixed_decimals(0)
                            .suffix(" % остатка"),
                    );
                });
                draft.cfg.notify_threshold_pct = pct;
                ui.label(RichText::new("0 % — уведомления выключены.").small().weak());

                ui.add_space(6.0);
                ui.add_enabled(
                    !demo,
                    egui::Checkbox::new(&mut draft.cfg.autostart, "Запускать с Windows"),
                );
                ui.checkbox(
                    &mut draft.cfg.start_minimized,
                    "Запускаться свёрнутым в трей",
                );

                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new("Сумма в трее").small().strong());
                ui.label(
                    RichText::new(
                        "Что показывать на иконке трея (до 2 сумм, строки идут в порядке выбора)",
                    )
                    .weak()
                    .small(),
                );
                ui.add_space(2.0);
                // Копия списка на время кадра: `draft.cfg` занят замыканием чекбоксов.
                let mut sources = draft.cfg.tray_sources.clone();
                let selected = sources.len();
                for src in TraySource::ALL {
                    let mut on = sources.contains(&src);
                    let enabled = tray_source_enabled(selected, on);
                    if ui
                        .add_enabled(enabled, egui::Checkbox::new(&mut on, src.label()))
                        .changed()
                        && tray_click_allowed(selected, on)
                    {
                        // `on` — состояние после клика; ограничения проверены выше.
                        toggle_tray_source(&mut sources, src, on);
                    }
                    if src == TraySource::AccountBalance && !state.has_management_key {
                        ui.label(
                            RichText::new(
                                "нужен management-ключ; пока недоступно — будет показан \
                                 автоматический вариант",
                            )
                            .small()
                            .weak(),
                        );
                    }
                }
                draft.cfg.tray_sources = sources;
                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new("Management-ключ").small().strong());
                if let Some(err) = &draft.error {
                    ui.colored_label(level_color(Level::Red), err.as_str());
                }
                if state.has_management_key {
                    ui.label(RichText::new("ключ сохранён").small().weak());
                }
                ui.add_enabled(
                    !demo,
                    egui::TextEdit::singleline(&mut draft.mgmt)
                        .password(true)
                        .hint_text("оставьте пустым, чтобы не менять")
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    if draft.remove_mgmt {
                        ui.label(RichText::new("ключ будет удалён после «Сохранить»").small());
                    } else if state.has_management_key
                        && ui
                            .add_enabled(!demo, egui::Button::new("Удалить management-ключ"))
                            .clicked()
                    {
                        draft.remove_mgmt = true;
                        draft.mgmt.clear();
                    }
                });
                ui.add_space(6.0);
                ui.colored_label(
                    level_color(Level::Yellow),
                    "⚠ Management-ключ даёт широкие права: создание и удаление API-ключей \
                     вашего аккаунта. Храните его только если нужен общий баланс. Ключ хранится \
                     в Диспетчере учётных данных Windows. Через OAuth management-ключ получить \
                     нельзя — создайте его вручную на openrouter.ai/settings/management-keys.",
                );

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Сохранить").clicked() {
                        action = SettingsAction::Save;
                    }
                    if ui.button("Отмена").clicked() {
                        action = SettingsAction::Cancel;
                    }
                });
            });

        self.settings = draft.finish(action, |cfg, change| self.core.apply_settings(cfg, change));
    }
}

impl eframe::App for App {
    /// Вызывается и когда окно скрыто или свёрнуто (тогда `ui` не рисуется).
    /// Поэтому крестик и сворачивание обрабатываются здесь: иначе при свёрнутом
    /// окне команды скрытия никто не отправит.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.core.has_tray() {
            return;
        }
        // Закрытие окна (крестик) не завершает приложение: живём в трее.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        // Сворачивание окна тоже уводит приложение в трей.
        if ctx.input(|i| i.viewport().minimized == Some(true)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let state = self.state();

        if self.start_hidden {
            // eframe показывает окно после отрисовки первого кадра.
            if ctx.cumulative_frame_nr() == 0 {
                ctx.request_repaint();
            } else {
                self.start_hidden = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }
        if self.core.take_open_settings() {
            self.open_settings();
        }

        egui::Panel::top("ui_top").show(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(WINDOW_TITLE).strong());
                if !show_toolbar(state.phase) {
                    return;
                }
                ui.separator();
                if ui
                    .add_enabled(!state.refreshing, egui::Button::new("Обновить"))
                    .clicked()
                {
                    self.core.refresh_now();
                }
                if ui.button("Настройки").clicked() {
                    self.open_settings();
                }
                if !self.core.is_demo() && ui.button("Выйти").clicked() {
                    self.settings = None;
                    self.core.logout();
                }
            });
            ui.add_space(2.0);
        });

        egui::Panel::bottom("ui_bottom").show(ui, |ui| {
            ui.add_space(2.0);
            self.status_line(ui, &state);
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match state.phase {
                    AuthPhase::SignedOut | AuthPhase::LoggingIn => self.login_screen(ui, &state),
                    AuthPhase::Ready => self.main_screen(ui, &state),
                });
        });

        self.settings_window(&ctx, &state);

        if self.hide_requested {
            self.hide_requested = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Credits, KeyBudget};
    use std::time::UNIX_EPOCH;

    fn key(limit: Option<f64>, rem: Option<f64>, usage: f64) -> KeyInfo {
        KeyInfo {
            limit,
            limit_remaining: rem,
            usage,
            ..Default::default()
        }
    }

    fn state_with(key: KeyInfo, credits: CreditsStatus) -> AppState {
        AppState {
            phase: AuthPhase::Ready,
            snapshot: Some(Snapshot {
                key,
                credits,
                fetched_at: UNIX_EPOCH,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn spent_caption_uses_remaining_for_current_period() {
        // Лимит сбрасывается по периоду: за всё время $90, в этом периоде $10.
        assert_eq!(
            spent_caption(format::key_limit(&key(Some(50.0), Some(40.0), 90.0)).unwrap()),
            "Потрачено $10.00 из $50.00 (20%)"
        );
    }

    #[test]
    fn spent_caption_handles_overspending_and_zero_limit() {
        assert_eq!(
            spent_caption(format::key_limit(&key(Some(10.0), None, 15.0)).unwrap()),
            "Потрачено $15.00 из $10.00 (100%)"
        );
        assert_eq!(
            spent_caption(format::key_limit(&key(Some(0.0), Some(0.0), 0.0)).unwrap()),
            "Потрачено $0.00 из $0.00 (100%)"
        );
    }

    #[test]
    fn spent_caption_rounds_percent() {
        assert_eq!(
            spent_caption(format::key_limit(&key(Some(50.0), None, 37.6)).unwrap()),
            "Потрачено $37.60 из $50.00 (75%)"
        );
        assert_eq!(
            spent_caption(format::key_limit(&key(Some(3.0), None, 1.0)).unwrap()),
            "Потрачено $1.00 из $3.00 (33%)"
        );
    }

    #[test]
    fn cancel_settings_discards_changes_and_pending_key_removal() {
        let draft = SettingsDraft {
            cfg: Config {
                refresh_secs: MAX_REFRESH_SECS,
                autostart: true,
                ..Config::default()
            },
            mgmt: String::new(),
            remove_mgmt: true,
            error: None,
        };
        let next = draft.finish(SettingsAction::Cancel, |_, _| {
            panic!("отмена не должна сохранять настройки или удалять ключ")
        });
        assert!(next.is_none());
    }

    #[test]
    fn remaining_text_and_negative_flag() {
        assert_eq!(remaining_text(Some(12.4)), "$12.40");
        assert_eq!(remaining_text(Some(-2.0)), "-$2.00");
        assert_eq!(remaining_text(None), "—");
        assert!(remaining_is_negative(Some(-0.01)));
        assert!(!remaining_is_negative(Some(0.0)));
        assert!(!remaining_is_negative(None));
    }

    #[test]
    fn connection_status_by_app_state() {
        let (dot, text, color) = connection_status(&AppState::default());
        assert_eq!(dot, "●");
        assert_eq!(text, "Загрузка…");
        assert_eq!(color, level_color(Level::Unknown));

        let loading = AppState {
            refreshing: true,
            ..Default::default()
        };
        assert_eq!(connection_status(&loading).1, "Обновление…");

        let ready = state_with(
            key(Some(50.0), Some(12.4), 37.6),
            CreditsStatus::NeedsManagementKey,
        );
        let (_, text, color) = connection_status(&ready);
        assert_eq!(text, "Подключено");
        assert_eq!(color, level_color(Level::Green));

        let failed = AppState {
            error: Some("нет соединения с сетью".into()),
            error_kind: Some(ErrorKind::Network),
            ..ready.clone()
        };
        let (_, text, color) = connection_status(&failed);
        assert_eq!(text, "Ошибка: нет соединения с сетью");
        assert_eq!(color, level_color(Level::Error));
        // ошибка важнее флага обновления
        let both = AppState {
            refreshing: true,
            ..failed.clone()
        };
        assert!(connection_status(&both).1.starts_with("Ошибка:"));
    }

    #[test]
    fn credits_block_visibility_by_status() {
        let c = Credits {
            total_credits: 100.0,
            total_usage: 40.0,
        };
        assert!(credits_hint(&CreditsStatus::Available(c)).is_none());
        assert!(credits_hint(&CreditsStatus::NeedsManagementKey)
            .unwrap()
            .contains("management"));
        assert!(credits_hint(&CreditsStatus::Unavailable)
            .unwrap()
            .contains("недоступен"));
        assert_eq!(c.remaining(), 60.0);
    }

    #[test]
    fn level_to_color32_matches_format_rgb() {
        for lvl in [
            Level::Green,
            Level::Yellow,
            Level::Red,
            Level::NoLimit,
            Level::Error,
            Level::Unknown,
        ] {
            let [r, g, b] = lvl.rgb();
            assert_eq!(level_color(lvl), Color32::from_rgb(r, g, b));
        }
        // жёлтый и красный из format.rs, а не произвольные
        assert_eq!(level_color(Level::Yellow), Color32::from_rgb(230, 170, 20));
        assert_eq!(level_color(Level::Red), Color32::from_rgb(212, 48, 48));
    }

    #[test]
    fn toolbar_and_limit_text() {
        assert!(!show_toolbar(AuthPhase::SignedOut));
        assert!(!show_toolbar(AuthPhase::LoggingIn));
        assert!(show_toolbar(AuthPhase::Ready));
        assert_eq!(limit_text(Some(50.0)), "$50.00");
        assert_eq!(limit_text(None), "без лимита");
        assert_eq!(limit_text(Some(0.0)), "$0.00");
    }

    #[test]
    fn budget_line_handles_missing_fields() {
        let full = KeyBudget {
            limit_usd: Some(10.0),
            remaining_usd: Some(2.5),
            reset_interval: Some("monthly".into()),
            ..Default::default()
        };
        assert_eq!(
            budget_line(&full),
            "Бюджет: остаток $2.50 из $10.00 (период monthly)"
        );
        let sparse = KeyBudget {
            remaining_usd: Some(1.0),
            ..Default::default()
        };
        assert_eq!(budget_line(&sparse), "Бюджет: остаток $1.00 из —");
        assert_eq!(budget_line(&KeyBudget::default()), "Бюджет: остаток — из —");
    }

    #[test]
    fn tray_toggle_appends_in_selection_order() {
        let mut list = vec![TraySource::KeyRemaining];
        toggle_tray_source(&mut list, TraySource::SpentDay, true);
        assert_eq!(list, vec![TraySource::KeyRemaining, TraySource::SpentDay]);
    }

    #[test]
    fn tray_toggle_off_removes_source() {
        let mut list = vec![TraySource::Spent, TraySource::KeyRemaining];
        toggle_tray_source(&mut list, TraySource::Spent, false);
        assert_eq!(list, vec![TraySource::KeyRemaining]);
    }

    #[test]
    fn tray_toggle_respects_max_limit() {
        let mut list = vec![TraySource::KeyRemaining, TraySource::Spent];
        toggle_tray_source(&mut list, TraySource::SpentWeek, true);
        assert_eq!(list.len(), MAX_TRAY_SOURCES);
        assert!(!list.contains(&TraySource::SpentWeek));
        assert!(!tray_source_enabled(list.len(), false));
        assert!(tray_source_enabled(list.len(), true));
    }

    #[test]
    fn tray_toggle_keeps_at_least_one() {
        let mut list = vec![TraySource::KeyRemaining];
        toggle_tray_source(&mut list, TraySource::KeyRemaining, false);
        assert_eq!(list, vec![TraySource::KeyRemaining]);
        assert!(!tray_click_allowed(1, false));
        assert!(tray_click_allowed(2, false));
    }

    #[test]
    fn tray_checkbox_click_applies_second_source() {
        // Дефолт — один источник; включение второго должно применяться (регрессия:
        // условие было инвертировано, и галочка снималась сразу после клика).
        let mut list = vec![TraySource::KeyRemaining];
        assert!(tray_click_allowed(list.len(), true));
        toggle_tray_source(&mut list, TraySource::SpentDay, true);
        assert_eq!(list, vec![TraySource::KeyRemaining, TraySource::SpentDay]);
        // Третьего уже не добавить.
        assert!(!tray_click_allowed(list.len(), true));
    }

    #[test]
    fn tray_toggle_ignores_duplicate() {
        let mut list = vec![TraySource::KeyRemaining];
        toggle_tray_source(&mut list, TraySource::KeyRemaining, true);
        assert_eq!(list, vec![TraySource::KeyRemaining]);
    }
}
