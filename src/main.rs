#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod auth;
mod config;
mod format;
mod state;
mod taskbar;
mod ui;
mod worker;

use std::sync::mpsc;
use std::sync::Arc;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::config::Config;
use crate::taskbar::{InstanceGuard, Shell, ShellEvent};
use crate::worker::Core;

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

/// Паника пишется в файл без содержимого сообщения (оно могло бы включать данные):
/// только место и имя потока.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".into());
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        let line = format!(
            "{} panic in thread '{thread}' at {loc}\n",
            chrono::Local::now().to_rfc3339()
        );
        if let Some(dir) = std::env::var_os("APPDATA") {
            let dir = std::path::Path::new(&dir).join("OpenRouterBalance");
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("panic.log"))
            {
                use std::io::Write;
                let _ = f.write_all(line.as_bytes());
            }
        }
    }));
}

fn load_icon() -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(ICON_PNG).ok()
}

fn main() {
    install_panic_hook();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let minimized_flag = args.iter().any(|a| a == "--minimized");
    let demo = args.iter().find_map(|a| {
        a.strip_prefix("--demo")
            .map(|r| r.trim_start_matches('=').to_string())
    });

    let Some(guard) = InstanceGuard::acquire() else {
        return; // уже запущен: первому экземпляру отправлен сигнал «показать окно»
    };

    let config = Config::load();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("orb-async")
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return,
    };

    let core = match &demo {
        Some(kind) => Core::new_demo(rt.handle().clone(), config.clone(), kind),
        None => Core::new(rt.handle().clone(), config.clone()),
    };
    let signed_in = core.init_keys();
    core.start();

    // Скрытый старт — только если уже есть вход (иначе пользователю нужно окно).
    let start_hidden = signed_in && (minimized_flag || config.start_minimized);

    if start_hidden {
        core.set_start_hidden();
    }
    let (tx, rx) = mpsc::channel::<ShellEvent>();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(ui::WINDOW_TITLE)
        .with_inner_size([470.0, 640.0])
        .with_min_inner_size([400.0, 460.0])
        .with_visible(!start_hidden);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let core_for_app = core.clone();
    let result = eframe::run_native(
        ui::WINDOW_TITLE,
        options,
        Box::new(move |cc| {
            let core = core_for_app;
            let hwnd = cc.window_handle().ok().and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
                _ => None,
            });

            let ctx = cc.egui_ctx.clone();
            let shell = hwnd.and_then(|hwnd| {
                let ctx2 = ctx.clone();
                Shell::spawn(
                    hwnd,
                    tx.clone(),
                    Box::new(move || ctx2.request_repaint()),
                    taskbar::is_autostart(),
                )
                .ok()
            });
            core.attach(ctx, shell.map(Arc::new));

            // Обработка событий трея в отдельном потоке: окно может быть скрыто,
            // и egui в этот момент не перерисовывается.
            let core_ev = core.clone();
            let _ = std::thread::Builder::new()
                .name("orb-events".into())
                .spawn(move || {
                    for ev in rx {
                        match ev {
                            ShellEvent::Refresh => core_ev.refresh_now(),
                            ShellEvent::OpenSettings => core_ev.request_open_settings(),
                            ShellEvent::AutostartToggled(on) => core_ev.autostart_toggled(on),
                            ShellEvent::Quit => core_ev.quit(),
                        }
                    }
                });

            // Повторный запуск приложения → показать окно.
            let core_act = core.clone();
            guard.listen(Box::new(move || core_act.show_window()));
            // guard должен жить до конца процесса.
            std::mem::forget(guard);

            Ok(Box::new(ui::App::new(cc, core)))
        }),
    );
    // Нормальный выход (или ошибка создания окна): убрать иконку трея и завершить процесс.
    let _ = result;
    core.quit();
}
