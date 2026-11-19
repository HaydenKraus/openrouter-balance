//! Настройки приложения (без секретов!) в `%APPDATA%\OpenRouterBalance\config.json`.

use serde::{Deserialize, Serialize};

pub const MIN_REFRESH_SECS: u64 = 15;
pub const MAX_REFRESH_SECS: u64 = 3600;

/// Максимум сумм на иконке трея (строк текста).
pub const MAX_TRAY_SOURCES: usize = 2;

/// Какая сумма может показываться на иконке трея.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraySource {
    /// Остаток лимита ключа (`limit_remaining`).
    KeyRemaining,
    /// Остаток баланса аккаунта (нужен management-ключ).
    AccountBalance,
    /// Всего потрачено ключом (`usage`).
    Spent,
    SpentDay,
    SpentWeek,
    SpentMonth,
}

impl TraySource {
    pub const ALL: [TraySource; 6] = [
        TraySource::KeyRemaining,
        TraySource::AccountBalance,
        TraySource::Spent,
        TraySource::SpentDay,
        TraySource::SpentWeek,
        TraySource::SpentMonth,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TraySource::KeyRemaining => "Остаток лимита ключа",
            TraySource::AccountBalance => "Баланс аккаунта (нужен management-ключ)",
            TraySource::Spent => "Потрачено всего",
            TraySource::SpentDay => "Потрачено за сутки",
            TraySource::SpentWeek => "Потрачено за неделю",
            TraySource::SpentMonth => "Потрачено за месяц",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Интервал автообновления, секунды.
    pub refresh_secs: u64,
    /// Порог уведомления: остаток в процентах от лимита (0 — отключено).
    pub notify_threshold_pct: f64,
    /// Запускать вместе с Windows (зеркалит реестр, источник истины — реестр).
    pub autostart: bool,
    /// Запускаться свёрнутым в трей.
    pub start_minimized: bool,
    /// Суммы на иконке трея (1..=2, по порядку сверху вниз). Недоступные пропускаются.
    pub tray_sources: Vec<TraySource>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            refresh_secs: 60,
            notify_threshold_pct: 10.0,
            autostart: false,
            start_minimized: false,
            tray_sources: vec![TraySource::KeyRemaining],
        }
    }
}

impl Config {
    /// Приводит значения к допустимым диапазонам (NaN → значение по умолчанию).
    pub fn sanitized(mut self) -> Self {
        self.refresh_secs = self.refresh_secs.clamp(MIN_REFRESH_SECS, MAX_REFRESH_SECS);
        self.notify_threshold_pct = if self.notify_threshold_pct.is_nan() {
            Self::default().notify_threshold_pct
        } else {
            self.notify_threshold_pct.clamp(0.0, 100.0)
        };
        let mut seen: Vec<TraySource> = Vec::new();
        for s in self.tray_sources.drain(..) {
            if !seen.contains(&s) && seen.len() < MAX_TRAY_SOURCES {
                seen.push(s);
            }
        }
        if seen.is_empty() {
            seen.push(TraySource::KeyRemaining);
        }
        self.tray_sources = seen;
        self
    }

    /// `%APPDATA%\OpenRouterBalance\config.json`; `None`, если `APPDATA` не задан.
    pub fn config_path() -> Option<std::path::PathBuf> {
        let appdata = std::env::var_os("APPDATA")?;
        if appdata.is_empty() {
            return None;
        }
        Some(
            std::path::PathBuf::from(appdata)
                .join("OpenRouterBalance")
                .join("config.json"),
        )
    }

    /// Загрузка; при отсутствии/порче файла — значения по умолчанию (не паникует).
    pub fn load() -> Self {
        match Self::config_path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    /// Атомарная запись (временный файл + rename).
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "переменная APPDATA не задана")
        })?;
        self.save_to(&path)
    }

    /// Загрузка из произвольного пути (тесты). Любая проблема → `default()`.
    pub fn load_from(path: &std::path::Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str::<Self>(&text)
            .unwrap_or_default()
            .sanitized()
    }

    /// Запись в произвольный путь (тесты), атомарно.
    ///
    /// Данные пишутся во временный файл в том же каталоге и переименовываются:
    /// читатель никогда не увидит частично записанный JSON.
    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = temp_sibling(path);
        std::fs::write(&tmp, json)?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Не оставляем мусор рядом с конфигом, если rename не удался.
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

/// `<path>.<pid>.tmp` рядом с целевым файлом: rename в пределах одного тома.
fn temp_sibling(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Уникальный путь во временной папке (свой на каждый тест).
    fn temp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "openrouter-balance-{tag}-{}-{nanos}",
            std::process::id()
        ));
        dir.join("config.json")
    }

    fn cleanup(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn default_is_in_range() {
        let cfg = Config::default().sanitized();
        assert_eq!(cfg.refresh_secs, 60);
        assert_eq!(cfg.notify_threshold_pct, 10.0);
        assert!(!cfg.autostart);
        assert!(!cfg.start_minimized);
    }

    #[test]
    fn roundtrip_preserves_values() {
        let path = temp_path("roundtrip");
        let cfg = Config {
            refresh_secs: 120,
            notify_threshold_pct: 25.5,
            autostart: true,
            start_minimized: true,
            tray_sources: vec![TraySource::AccountBalance, TraySource::SpentDay],
        };
        cfg.save_to(&path).expect("save");
        assert_eq!(Config::load_from(&path), cfg);
        cleanup(&path);
    }

    #[test]
    fn save_creates_missing_directory() {
        let path = temp_path("mkdir");
        assert!(!path.parent().expect("parent").exists());
        Config::default()
            .save_to(&path)
            .expect("save создаёт каталог");
        assert!(path.exists());
        cleanup(&path);
    }

    #[test]
    fn save_leaves_no_temp_files() {
        let path = temp_path("notmp");
        Config::default().save_to(&path).expect("save");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "остались временные файлы: {leftovers:?}"
        );
        cleanup(&path);
    }

    #[test]
    fn broken_json_falls_back_to_default() {
        let path = temp_path("broken");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "{ это не json ").expect("write");
        assert_eq!(Config::load_from(&path), Config::default());
        cleanup(&path);
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let path = temp_path("missing");
        assert_eq!(Config::load_from(&path), Config::default());
    }

    #[test]
    fn partial_json_uses_serde_defaults() {
        let path = temp_path("partial");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, r#"{"refresh_secs": 30}"#).expect("write");
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.refresh_secs, 30);
        assert_eq!(cfg.notify_threshold_pct, 10.0);
        assert!(!cfg.autostart);
        cleanup(&path);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let path = temp_path("unknown");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, r#"{"refresh_secs": 45, "future_field": {"a": 1}}"#).expect("write");
        assert_eq!(Config::load_from(&path).refresh_secs, 45);
        cleanup(&path);
    }

    #[test]
    fn sanitize_clamps_refresh_secs() {
        let low = Config {
            refresh_secs: 0,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(low.refresh_secs, MIN_REFRESH_SECS);
        let high = Config {
            refresh_secs: u64::MAX,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(high.refresh_secs, MAX_REFRESH_SECS);
        let inside = Config {
            refresh_secs: 900,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(inside.refresh_secs, 900);
    }

    #[test]
    fn sanitize_handles_nan_and_clamps_threshold() {
        let nan = Config {
            notify_threshold_pct: f64::NAN,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(nan.notify_threshold_pct, 10.0);
        let negative = Config {
            notify_threshold_pct: -5.0,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(negative.notify_threshold_pct, 0.0);
        let huge = Config {
            notify_threshold_pct: 1e9,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(huge.notify_threshold_pct, 100.0);
        let inf = Config {
            notify_threshold_pct: f64::INFINITY,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(inf.notify_threshold_pct, 100.0);
    }

    #[test]
    fn sanitize_is_applied_on_load() {
        let path = temp_path("sanitize-load");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &path,
            r#"{"refresh_secs": 1, "notify_threshold_pct": 500.0}"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.refresh_secs, MIN_REFRESH_SECS);
        assert_eq!(cfg.notify_threshold_pct, 100.0);
        cleanup(&path);
    }

    #[test]
    fn config_path_points_to_appdata() {
        // APPDATA на Windows есть всегда; на прочих платформах тест не имеет смысла.
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let path = Config::config_path().expect("APPDATA задан");
            assert!(path.starts_with(std::path::PathBuf::from(appdata)));
            assert!(path.ends_with("config.json"));
            assert!(path.to_string_lossy().contains("OpenRouterBalance"));
        } else {
            assert_eq!(Config::config_path(), None);
        }
    }

    #[test]
    fn saved_file_contains_no_secrets() {
        let path = temp_path("nosecrets");
        Config::default().save_to(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        for forbidden in ["sk-or-", "api_key", "token", "secret", "Bearer"] {
            assert!(
                !text.contains(forbidden),
                "в конфиге найдено «{forbidden}»: {text}"
            );
        }
        cleanup(&path);
    }

    #[test]
    fn sanitize_tray_sources() {
        let cfg = Config {
            tray_sources: vec![
                TraySource::Spent,
                TraySource::Spent,
                TraySource::SpentDay,
                TraySource::SpentWeek,
            ],
            ..Config::default()
        }
        .sanitized();
        assert_eq!(
            cfg.tray_sources,
            vec![TraySource::Spent, TraySource::SpentDay]
        );
        let empty = Config {
            tray_sources: vec![],
            ..Config::default()
        }
        .sanitized();
        assert_eq!(empty.tray_sources, vec![TraySource::KeyRemaining]);
    }
}
