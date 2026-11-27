//! Чистая логика отображения: форматирование денег, пороги цветов, модель
//! для трея / кнопки панели задач / заголовка окна. Без зависимостей от GUI и WinAPI.

use std::time::SystemTime;

use chrono::{DateTime, Local};

use crate::api::{CreditsStatus, KeyInfo, Snapshot};
use crate::config::{TraySource, MAX_TRAY_SOURCES};
use crate::state::{AppState, AuthPhase};

/// Остаток меньше этой доли — жёлтый.
pub const WARN_FRACTION: f64 = 0.20;
/// Остаток меньше этой доли — красный.
pub const CRIT_FRACTION: f64 = 0.05;
/// Лимит длины тултипа трея в Windows (128 с нулём).
pub const TRAY_TIP_MAX: usize = 127;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Green,
    Yellow,
    Red,
    /// Лимита нет: показываем только расход (синий).
    NoLimit,
    /// Ошибка обновления / нет данных (серый).
    Error,
    /// Не выполнен вход (серый).
    Unknown,
}

impl Level {
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Level::Green => [34, 160, 80],
            Level::Yellow => [230, 170, 20],
            Level::Red => [212, 48, 48],
            Level::NoLimit => [60, 120, 200],
            Level::Error => [120, 120, 120],
            Level::Unknown => [120, 120, 120],
        }
    }

    /// Цвет текста поверх `rgb()`.
    pub fn text_rgb(self) -> [u8; 3] {
        match self {
            Level::Yellow => [20, 20, 20],
            _ => [255, 255, 255],
        }
    }
}

/// Уровень по доле остатка (0..1). `None` — доли нет.
pub fn level_for_fraction(remaining_fraction: Option<f64>) -> Level {
    match remaining_fraction {
        None => Level::NoLimit,
        Some(f) if f.is_nan() => Level::NoLimit,
        Some(f) if f < CRIT_FRACTION => Level::Red,
        Some(f) if f < WARN_FRACTION => Level::Yellow,
        Some(_) => Level::Green,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    Key,
    Budget,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RemainingLimit {
    pub source: LimitSource,
    pub limit: f64,
    pub remaining: f64,
    pub fraction: f64,
}

impl RemainingLimit {
    fn new(source: LimitSource, limit: f64, remaining: f64) -> Option<Self> {
        if !limit.is_finite() || limit < 0.0 || !remaining.is_finite() {
            return None;
        }
        Some(Self {
            source,
            limit,
            remaining,
            fraction: if limit == 0.0 {
                0.0
            } else {
                (remaining / limit).clamp(0.0, 1.0)
            },
        })
    }
}

/// Лимит с наименьшей долей остатка. Сумма и доля относятся к одному ограничению.
pub fn effective_remaining(key: &KeyInfo) -> Option<RemainingLimit> {
    let own = key_limit(key);
    let budget = key.effective_budget.as_ref().and_then(|b| {
        let limit = b.limit_usd?;
        let remaining = b
            .remaining_usd
            .or_else(|| b.spend_usd.map(|spent| limit - spent));
        let remaining = remaining.or_else(|| (limit == 0.0).then_some(0.0))?;
        RemainingLimit::new(LimitSource::Budget, limit, remaining)
    });
    match (own, budget) {
        (Some(key), Some(budget)) if key.fraction <= budget.fraction => Some(key),
        (_, Some(budget)) => Some(budget),
        (key, None) => key,
    }
}

pub fn key_limit(key: &KeyInfo) -> Option<RemainingLimit> {
    RemainingLimit::new(LimitSource::Key, key.limit?, key_remaining(key)?)
}

/// Остаток лимита ключа в USD (используем `limit_remaining`, иначе limit − usage).
pub fn key_remaining(key: &KeyInfo) -> Option<f64> {
    let limit = key.limit?;
    Some(key.limit_remaining.unwrap_or(limit - key.usage))
}

// ---------------------------------------------------------------------------
// Деньги
// ---------------------------------------------------------------------------

fn group_thousands(int_part: &str) -> String {
    let mut out = String::new();
    let len = int_part.len();
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// USD: 2 знака; для сумм по модулю меньше $1 — до 4 знаков (но не меньше 2).
/// `NaN`/inf → "—".
pub fn money(v: f64) -> String {
    if !v.is_finite() {
        return "—".to_string();
    }
    let neg = v < 0.0;
    let a = v.abs();
    let mut s = if a < 1.0 {
        let s4 = format!("{a:.4}");
        let mut t = s4.trim_end_matches('0').to_string();
        // минимум 2 знака после точки
        let dot = t.find('.').unwrap_or(t.len());
        while t.len() - dot - 1 < 2 {
            t.push('0');
        }
        t
    } else {
        format!("{a:.2}")
    };
    if a >= 1.0 {
        let (i, f) = s.split_once('.').unwrap_or((&s, ""));
        s = format!("{}.{}", group_thousands(i), f);
    }
    // «-$0.00» не показываем
    let is_zero = s.chars().all(|c| c == '0' || c == '.');
    if neg && !is_zero {
        format!("-${s}")
    } else {
        format!("${s}")
    }
}

/// Короткая запись для иконки трея / оверлея (≤4 символов): `$12`, `$9.5`, `$.85`, `$1k`.
/// Отрицательные значения показываются как `$0`.
pub fn short_money(v: f64) -> String {
    if !v.is_finite() {
        return "?".to_string();
    }
    let v = v.max(0.0);
    if v >= 1000.0 {
        let k = (v / 1000.0).round();
        if k >= 100.0 {
            "$99k".to_string()
        } else {
            format!("${k:.0}k")
        }
    } else if v >= 9.95 {
        format!("${:.0}", v.round().min(999.0))
    } else if v >= 1.0 {
        format!("${v:.1}")
    } else if v >= 0.005 {
        format!("${:.2}", v).replacen("0.", ".", 1)
    } else {
        "$0".to_string()
    }
}

/// Значение выбранной суммы; `None`, если она сейчас недоступна.
pub fn source_value(snap: &Snapshot, source: TraySource) -> Option<f64> {
    let key = &snap.key;
    match source {
        TraySource::KeyRemaining => key_remaining(key),
        TraySource::AccountBalance => match snap.credits {
            CreditsStatus::Available(c) => Some(c.remaining()),
            _ => None,
        },
        TraySource::Spent => Some(key.usage),
        TraySource::SpentDay => Some(key.usage_daily),
        TraySource::SpentWeek => Some(key.usage_weekly),
        TraySource::SpentMonth => Some(key.usage_monthly),
    }
}

/// Автоматический выбор, если ничего из выбранного недоступно:
/// остаток лимита → баланс аккаунта → потрачено.
fn fallback_value(snap: &Snapshot) -> Option<f64> {
    effective_remaining(&snap.key)
        .map(|limit| limit.remaining)
        .or_else(|| source_value(snap, TraySource::AccountBalance))
        .or_else(|| source_value(snap, TraySource::Spent))
}

/// Полная сумма для иконки трея без знака валюты: `12.53`, `0.85`, `1234`, `12k`.
/// Отрицательные (долг, перерасход) — со знаком минус: `-2.50`. Нечисловые → `?`.
pub fn tray_amount(v: f64) -> String {
    if !v.is_finite() {
        return "?".to_string();
    }
    let a = v.abs();
    let s = if a < 1000.0 {
        format!("{a:.2}")
    } else if a < 100_000.0 {
        format!("{a:.0}")
    } else {
        format!("{}k", (a / 1000.0).round().min(9999.0))
    };
    // «-0.00» не показываем
    if v < 0.0 && s != "0.00" {
        format!("-{s}")
    } else {
        s
    }
}
pub fn format_time(t: SystemTime) -> String {
    DateTime::<Local>::from(t).format("%H:%M:%S").to_string()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Модель панели задач
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressKind {
    /// Скрыть прогресс.
    None,
    Normal,
    Paused,
    Error,
}

/// Всё, что нужно трею, кнопке панели задач и заголовку окна.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskbarModel {
    pub level: Level,
    /// Текст на иконке трея (≤4 символов): `$12`, `!`, `?`.
    pub tray_text: String,
    /// Строки на иконке трея (1..=2): полные суммы вида "12.53" либо "!"/"?".
    pub tray_lines: Vec<String>,
    /// Текст на оверлее кнопки панели задач (то же, что в трее).
    pub overlay_text: String,
    /// Предупреждающий символ на оверлее вместо числа (ошибка / остаток ниже порога).
    pub overlay_warning: bool,
    pub tooltip: String,
    pub title: String,
    pub progress: ProgressKind,
    /// Доля потраченного 0..1 для `SetProgressValue`.
    pub progress_spent: f64,
}

impl TaskbarModel {
    /// Модель с учётом выбранных в настройках сумм для иконки трея.
    pub fn from_state_with(st: &AppState, sources: &[TraySource]) -> Self {
        let mut model = Self::from_state(st);
        if st.phase != AuthPhase::Ready || st.is_stale() {
            return model;
        }
        if let Some(snap) = &st.snapshot {
            let lines: Vec<String> = sources
                .iter()
                .filter_map(|s| source_value(snap, *s))
                .map(tray_amount)
                .take(MAX_TRAY_SOURCES)
                .collect();
            if !lines.is_empty() {
                model.tray_lines = lines;
            } else if let Some(v) = fallback_value(snap) {
                model.tray_lines = vec![tray_amount(v)];
            }
        }
        model
    }
    pub fn from_state(st: &AppState) -> Self {
        match st.phase {
            AuthPhase::SignedOut | AuthPhase::LoggingIn => {
                return Self {
                    level: Level::Unknown,
                    tray_text: "?".into(),
                    tray_lines: vec!["?".into()],
                    overlay_text: "?".into(),
                    overlay_warning: false,
                    tooltip: "OpenRouter Баланс — вход не выполнен".into(),
                    title: "OpenRouter Баланс — вход не выполнен".into(),
                    progress: ProgressKind::None,
                    progress_spent: 0.0,
                };
            }
            AuthPhase::Ready => {}
        }
        let Some(snap) = &st.snapshot else {
            // Данных ещё не было: НЕ показываем «0».
            let (txt, tip) = match &st.error {
                Some(e) => ("!", format!("OpenRouter Баланс — ошибка: {e}")),
                None => ("…", "OpenRouter Баланс — загрузка…".to_string()),
            };
            return Self {
                level: if st.error.is_some() {
                    Level::Error
                } else {
                    Level::Unknown
                },
                tray_text: txt.into(),
                tray_lines: vec![txt.into()],
                overlay_text: txt.into(),
                overlay_warning: st.error.is_some(),
                tooltip: truncate_chars(&tip, TRAY_TIP_MAX),
                title: truncate_chars(&tip, 120),
                progress: ProgressKind::None,
                progress_spent: 0.0,
            };
        };

        let key = &snap.key;
        let effective = effective_remaining(key);
        let frac = effective.map(|limit| limit.fraction);
        let data_level = level_for_fraction(frac);
        let stale = st.is_stale();
        let level = if stale { Level::Error } else { data_level };

        // Нет ограничения — показываем остаток баланса аккаунта, если он известен.
        let account = match snap.credits {
            CreditsStatus::Available(c) => Some(c.remaining()),
            _ => None,
        };
        let (tray_text, overlay_warning) = if stale {
            ("!".to_string(), true)
        } else if let Some(limit) = effective {
            (
                short_money(limit.remaining),
                matches!(data_level, Level::Red),
            )
        } else if let Some(a) = account {
            (short_money(a), false)
        } else {
            (short_money(key.usage), false)
        };

        let (progress, progress_spent) = match frac {
            Some(f) => {
                let kind = if stale {
                    ProgressKind::Error
                } else {
                    match data_level {
                        Level::Red => ProgressKind::Error,
                        Level::Yellow => ProgressKind::Paused,
                        _ => ProgressKind::Normal,
                    }
                };
                (kind, 1.0 - f)
            }
            None => (ProgressKind::None, 0.0),
        };

        let main = match effective {
            Some(limit) => {
                let label = match limit.source {
                    LimitSource::Key => "остаток",
                    LimitSource::Budget => "остаток бюджета",
                };
                format!(
                    "{label} {} / {}",
                    money(limit.remaining),
                    money(limit.limit)
                )
            }
            None => match account {
                Some(a) => format!("баланс {}", money(a)),
                None => format!("потрачено {}", money(key.usage)),
            },
        };
        let title = if stale {
            format!("OpenRouter — ошибка, устарело: {main}")
        } else {
            format!("OpenRouter — {main}")
        };

        Self {
            level,
            tray_text: tray_text.clone(),
            tray_lines: vec![tray_text.clone()],
            overlay_text: tray_text,
            overlay_warning,
            tooltip: truncate_chars(&tooltip_text(snap, st, effective), TRAY_TIP_MAX),
            title,
            progress,
            progress_spent,
        }
    }
}

/// Полная сводка для тултипа (потом обрезается до 127 символов).
fn tooltip_text(snap: &Snapshot, st: &AppState, effective: Option<RemainingLimit>) -> String {
    let key = &snap.key;
    let mut lines: Vec<String> = Vec::new();
    if st.is_stale() {
        lines.push("⚠ Данные устарели".to_string());
    }
    lines.push(format!("Потрачено: {}", money(key.usage)));
    if let Some(limit) = effective.filter(|limit| limit.source == LimitSource::Budget) {
        lines.push(format!(
            "Остаток бюджета: {} из {}",
            money(limit.remaining),
            money(limit.limit)
        ));
    }
    match (key.limit, key_remaining(key)) {
        (Some(l), Some(r)) => lines.push(format!("Остаток: {} из {}", money(r), money(l))),
        _ => lines.push("Лимит: без лимита".to_string()),
    }
    if let CreditsStatus::Available(c) = snap.credits {
        lines.push(format!("Баланс: {}", money(c.remaining())));
    }
    lines.push(format!("Обновлено {}", format_time(snap.fetched_at)));
    lines.join("\n")
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

    fn state_with(key: KeyInfo, error: Option<&str>) -> AppState {
        AppState {
            phase: AuthPhase::Ready,
            snapshot: Some(Snapshot {
                key,
                credits: CreditsStatus::NeedsManagementKey,
                fetched_at: UNIX_EPOCH,
            }),
            error: error.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn money_basic() {
        assert_eq!(money(12.4), "$12.40");
        assert_eq!(money(0.0), "$0.00");
        assert_eq!(money(50.0), "$50.00");
        assert_eq!(money(1234.5), "$1,234.50");
        assert_eq!(money(1_234_567.891), "$1,234,567.89");
    }

    #[test]
    fn money_small_up_to_4_digits() {
        assert_eq!(money(0.0123), "$0.0123");
        assert_eq!(money(0.5), "$0.50");
        assert_eq!(money(0.0012), "$0.0012");
        assert_eq!(money(0.00001), "$0.00");
        assert_eq!(money(0.123456), "$0.1235");
    }

    #[test]
    fn money_negative_and_invalid() {
        assert_eq!(money(-3.5), "-$3.50");
        assert_eq!(money(-0.00001), "$0.00");
        assert_eq!(money(f64::NAN), "—");
        assert_eq!(money(f64::INFINITY), "—");
    }

    #[test]
    fn short_money_cases() {
        assert_eq!(short_money(12.4), "$12");
        assert_eq!(short_money(99.6), "$100");
        assert_eq!(short_money(9.96), "$10");
        assert_eq!(short_money(9.5), "$9.5");
        assert_eq!(short_money(0.85), "$.85");
        assert_eq!(short_money(0.0), "$0");
        assert_eq!(short_money(-5.0), "$0");
        assert_eq!(short_money(1500.0), "$2k");
        assert_eq!(short_money(1_000_000.0), "$99k");
        assert_eq!(short_money(f64::NAN), "?");
        for v in [0.0, 0.004, 0.5, 5.55, 9.94, 9.95, 99.5, 999.0, 5000.0, 1e9] {
            assert!(
                short_money(v).chars().count() <= 4,
                "{v} -> {}",
                short_money(v)
            );
        }
    }

    #[test]
    fn levels_thresholds() {
        assert_eq!(level_for_fraction(Some(1.0)), Level::Green);
        assert_eq!(level_for_fraction(Some(0.20)), Level::Green);
        assert_eq!(level_for_fraction(Some(0.1999)), Level::Yellow);
        assert_eq!(level_for_fraction(Some(0.05)), Level::Yellow);
        assert_eq!(level_for_fraction(Some(0.0499)), Level::Red);
        assert_eq!(level_for_fraction(Some(0.0)), Level::Red);
        assert_eq!(level_for_fraction(None), Level::NoLimit);
    }

    #[test]
    fn fraction_edge_cases() {
        let fraction = |key| effective_remaining(&key).map(|limit| limit.fraction);
        assert_eq!(fraction(key(None, None, 5.0)), None);
        assert_eq!(fraction(key(Some(0.0), Some(0.0), 0.0)), Some(0.0));
        assert_eq!(fraction(key(Some(-5.0), None, 5.0)), None);
        assert_eq!(fraction(key(Some(f64::NAN), None, 5.0)), None);
        assert_eq!(fraction(key(Some(50.0), Some(12.5), 37.5)), Some(0.25));
        // отрицательный остаток → 0
        assert_eq!(fraction(key(Some(10.0), Some(-2.0), 12.0)), Some(0.0));
        // limit_remaining отсутствует → limit − usage
        assert_eq!(fraction(key(Some(10.0), None, 7.5)), Some(0.25));
    }

    #[test]
    fn fraction_uses_min_with_effective_budget() {
        let mut k = key(Some(100.0), Some(90.0), 10.0);
        k.effective_budget = Some(KeyBudget {
            limit_usd: Some(10.0),
            remaining_usd: Some(1.0),
            ..Default::default()
        });
        assert_eq!(
            effective_remaining(&k),
            Some(RemainingLimit {
                source: LimitSource::Budget,
                limit: 10.0,
                remaining: 1.0,
                fraction: 0.1,
            })
        );
        let model = TaskbarModel::from_state(&state_with(k, None));
        assert_eq!(model.overlay_text, "$1.0");
        assert_eq!(model.progress_spent, 0.9);
        assert_eq!(model.title, "OpenRouter — остаток бюджета $1.00 / $10.00");
        assert!(model.tooltip.contains("Остаток бюджета: $1.00 из $10.00"));

        let mut k2 = key(None, None, 3.0);
        k2.effective_budget = Some(KeyBudget {
            limit_usd: Some(10.0),
            spend_usd: Some(5.0),
            ..Default::default()
        });
        let limit = effective_remaining(&k2).unwrap();
        assert_eq!((limit.remaining, limit.fraction), (5.0, 0.5));
    }

    #[test]
    fn model_zero_limit_is_exhausted() {
        let model = TaskbarModel::from_state(&state_with(key(Some(0.0), Some(0.0), 0.0), None));
        assert_eq!(model.level, Level::Red);
        assert_eq!(model.progress_spent, 1.0);
        assert_eq!(model.overlay_text, "$0");
    }

    #[test]
    fn model_normal_with_limit() {
        let m = TaskbarModel::from_state(&state_with(key(Some(50.0), Some(12.4), 37.6), None));
        assert_eq!(m.level, Level::Green);
        assert_eq!(m.tray_text, "$12");
        assert_eq!(m.title, "OpenRouter — остаток $12.40 / $50.00");
        assert_eq!(m.progress, ProgressKind::Normal);
        assert!((m.progress_spent - 0.752).abs() < 1e-9);
        assert!(m.tooltip.contains("Потрачено: $37.60"));
        assert!(m.tooltip.contains("Остаток: $12.40 из $50.00"));
        assert!(m.tooltip.chars().count() <= TRAY_TIP_MAX);
    }

    #[test]
    fn model_levels_map_to_progress() {
        let y = TaskbarModel::from_state(&state_with(key(Some(100.0), Some(10.0), 90.0), None));
        assert_eq!((y.level, y.progress), (Level::Yellow, ProgressKind::Paused));
        let r = TaskbarModel::from_state(&state_with(key(Some(100.0), Some(2.0), 98.0), None));
        assert_eq!((r.level, r.progress), (Level::Red, ProgressKind::Error));
        assert!(r.overlay_warning);
    }

    #[test]
    fn model_no_limit_hides_progress() {
        let m = TaskbarModel::from_state(&state_with(key(None, None, 7.26), None));
        assert_eq!(m.level, Level::NoLimit);
        assert_eq!(m.progress, ProgressKind::None);
        assert_eq!(m.tray_text, "$7.3");
        assert_eq!(m.title, "OpenRouter — потрачено $7.26");
        assert!(m.tooltip.contains("без лимита"));
    }

    #[test]
    fn model_no_limit_shows_account_balance() {
        let mut st = state_with(key(None, None, 7.26), None);
        if let Some(s) = st.snapshot.as_mut() {
            s.credits = CreditsStatus::Available(Credits {
                total_credits: 100.0,
                total_usage: 88.0,
            });
        }
        let m = TaskbarModel::from_state(&st);
        assert_eq!(m.tray_text, "$12");
        assert_eq!(m.title, "OpenRouter — баланс $12.00");
    }

    #[test]
    fn model_error_with_stale_data_not_zero() {
        let m = TaskbarModel::from_state(&state_with(
            key(Some(50.0), Some(12.4), 37.6),
            Some("нет сети"),
        ));
        assert_eq!(m.level, Level::Error);
        assert_eq!(m.tray_text, "!");
        assert!(m.overlay_warning);
        assert_eq!(m.progress, ProgressKind::Error);
        assert!(m.title.contains("устарело"));
        assert!(m.tooltip.contains("устарели"));
    }

    #[test]
    fn model_error_without_data_is_not_zero() {
        let st = AppState {
            phase: AuthPhase::Ready,
            error: Some("нет сети".into()),
            ..Default::default()
        };
        let m = TaskbarModel::from_state(&st);
        assert_eq!(m.tray_text, "!");
        assert_eq!(m.level, Level::Error);
        let loading = TaskbarModel::from_state(&AppState {
            phase: AuthPhase::Ready,
            ..Default::default()
        });
        assert_eq!(loading.tray_text, "…");
    }

    #[test]
    fn model_signed_out() {
        let m = TaskbarModel::from_state(&AppState::default());
        assert_eq!(m.level, Level::Unknown);
        assert_eq!(m.progress, ProgressKind::None);
    }

    #[test]
    fn tooltip_includes_account_balance_and_is_truncated() {
        let mut st = state_with(key(Some(50.0), Some(12.4), 37.6), None);
        if let Some(s) = st.snapshot.as_mut() {
            s.credits = CreditsStatus::Available(Credits {
                total_credits: 100.0,
                total_usage: 40.0,
            });
        }
        let m = TaskbarModel::from_state(&st);
        assert!(m.tooltip.contains("Баланс: $60.00"));
        assert_eq!(
            truncate_chars(&"я".repeat(300), TRAY_TIP_MAX)
                .chars()
                .count(),
            TRAY_TIP_MAX
        );
    }

    #[test]
    fn tray_amount_formats() {
        assert_eq!(tray_amount(12.534), "12.53");
        assert_eq!(tray_amount(0.0), "0.00");
        assert_eq!(tray_amount(-3.0), "-3.00");
        assert_eq!(tray_amount(-0.001), "0.00");
        assert_eq!(tray_amount(-1500.0), "-1500");
        assert_eq!(tray_amount(999.994), "999.99");
        assert_eq!(tray_amount(1234.56), "1235");
        assert_eq!(tray_amount(250_000.0), "250k");
        assert_eq!(tray_amount(f64::NAN), "?");
    }

    #[test]
    fn tray_lines_follow_selected_sources() {
        let mut st = state_with(key(Some(50.0), Some(12.4), 37.6), None);
        if let Some(s) = st.snapshot.as_mut() {
            s.key.usage_daily = 1.234;
            s.credits = CreditsStatus::Available(Credits {
                total_credits: 100.0,
                total_usage: 60.0,
            });
        }
        let m = TaskbarModel::from_state_with(
            &st,
            &[TraySource::KeyRemaining, TraySource::AccountBalance],
        );
        assert_eq!(m.tray_lines, vec!["12.40", "40.00"]);
        let m = TaskbarModel::from_state_with(&st, &[TraySource::SpentDay]);
        assert_eq!(m.tray_lines, vec!["1.23"]);
        // недоступная сумма пропускается
        let st2 = state_with(key(None, None, 5.0), None);
        let m = TaskbarModel::from_state_with(&st2, &[TraySource::KeyRemaining, TraySource::Spent]);
        assert_eq!(m.tray_lines, vec!["5.00"]);
        // ничего не доступно → автовыбор
        let m = TaskbarModel::from_state_with(&st2, &[TraySource::AccountBalance]);
        assert_eq!(m.tray_lines, vec!["5.00"]);
    }

    #[test]
    fn tray_lines_error_and_signed_out_ignore_sources() {
        let st = state_with(key(Some(50.0), Some(12.4), 37.6), Some("нет сети"));
        let m = TaskbarModel::from_state_with(&st, &[TraySource::Spent]);
        assert_eq!(m.tray_lines, vec!["!"]);
        let m = TaskbarModel::from_state_with(&AppState::default(), &[TraySource::Spent]);
        assert_eq!(m.tray_lines, vec!["?"]);
    }
}
