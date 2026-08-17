// Точка входа десктоп-приложения: при старте спавнит Python sidecar-процесс
// (backend/app/sidecar/main.py, собранный PyInstaller-ом, см.
// backend/build_sidecar.ps1), читает его порт из stdout, поллит /health до
// готовности и убивает процесс при закрытии окна. Всё остальное общение
// (транскрибация, генерация отчётов, скачивание моделей) идёт напрямую из
// Vue-webview через fetch() на http://127.0.0.1:<port> — Rust здесь только
// управляет жизненным циклом sidecar-процесса.

// Без этого xstt-desktop.exe компилируется с PE-subsystem CONSOLE (дефолт
// для Rust-бинарников) — Windows Terminal привязывает к нему консольное
// окно при каждом запуске, отдельное от самого GUI-окна приложения.
// Debug-сборка сохраняет консоль для удобства (видно println!/panic!).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use tauri::{path::BaseDirectory, Manager, State};
use tauri_plugin_shell::process::CommandChild;
use tauri_plugin_shell::ShellExt;

struct SidecarState {
    port: Mutex<Option<u16>>,
    child: Mutex<Option<CommandChild>>,
}

#[tauri::command]
fn read_sidecar_log(app: tauri::AppHandle) -> Result<String, String> {
    let path = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("sidecar.log");
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_devtools(window: tauri::WebviewWindow) {
    // AreDevToolsEnabled=FALSE (см. setup(), блокирует случайные F12/Ctrl+Shift+I)
    // блокирует и программный OpenDevToolsWindow() — временно включаем перед
    // вызовом и возвращаем обратно, иначе только эта команда и открывала бы их.
    #[cfg(windows)]
    let _ = window.with_webview(|webview| {
        unsafe {
            if let Ok(core) = webview.controller().CoreWebView2() {
                if let Ok(settings) = core.Settings() {
                    let _ = settings.SetAreDevToolsEnabled(true);
                }
            }
        }
    });
    window.open_devtools();
}

#[tauri::command]
async fn get_sidecar_port(state: State<'_, SidecarState>) -> Result<u16, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(port) = *state.port.lock().unwrap() {
            return Ok(port);
        }
        if std::time::Instant::now() > deadline {
            return Err("Локальный движок не запустился за 30 секунд".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_health(port: u16) -> bool {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/health");
    for _ in 0..100 {
        if let Ok(resp) = client.get(&url).timeout(Duration::from_millis(500)).send().await {
            if resp.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    false
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(SidecarState {
            port: Mutex::new(None),
            child: Mutex::new(None),
        })
        .setup(|app| {
            let handle = app.handle().clone();

            // WebView2 по умолчанию (AreDevToolsEnabled=TRUE) сам разрешает
            // правый клик "Inspect" и клавиатурные шорткаты для DevTools —
            // это НЕ то же самое, что Tauri's features=["devtools"] (та
            // включает только программный window.open_devtools(), см. ниже
            // read_sidecar_log/open_devtools команды). Отключаем нативные
            // триггеры полностью — DevTools доступны только через нашу
            // скрытую комбинацию Ctrl+Shift+Z+X+C.
            #[cfg(windows)]
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.with_webview(|webview| {
                    #[cfg(windows)]
                    unsafe {
                        if let Ok(core) = webview.controller().CoreWebView2() {
                            if let Ok(settings) = core.Settings() {
                                let _ = settings.SetAreDevToolsEnabled(false);
                            }
                        }
                    }
                });
            }

            // PyInstaller-бандл — это папка (exe + _internal/), не единый файл,
            // поэтому используем прямой Command::new() по разрешённому пути
            // ресурса вместо строгого именования Command::sidecar()
            // (<name>-<target-triple>.exe), которое ожидает один файл.
            // В dev-режиме bundle.resources ещё не скопированы (это шаг
            // упаковки релиза) — берём файл прямо из src-tauri/binaries/.
            let sidecar_path = if cfg!(debug_assertions) {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries/sidecar/sidecar.exe")
            } else {
                app.path()
                    .resolve("binaries/sidecar/sidecar.exe", BaseDirectory::Resource)
                    .expect("не удалось разрешить путь к sidecar-бинарнику")
            };

            // Куда десктоп-клиент складывает скачанные модели — по умолчанию
            // рядом с исполняемым файлом приложения, а не в дефолт из
            // серверного app/config.py (/models-cache, не существует на Windows).
            let models_dir = app
                .path()
                .app_data_dir()
                .expect("не удалось определить директорию данных приложения")
                .join("models-cache");
            std::fs::create_dir_all(&models_dir).ok();

            let mut command = handle
                .shell()
                .command(sidecar_path)
                .env("MODELS_CACHE_DIR", models_dir.to_string_lossy().to_string());

            // ponytail: заглушка GPU для локальной отладки UI на машине без
            // видеокарты — только в debug-сборке, никогда не попадает в релиз.
            // Убрать, когда появится тестовая машина с реальным GPU.
            if cfg!(debug_assertions) {
                command = command.env("FAKE_GPU_NAME", "NVIDIA GeForce RTX 4060 Laptop GPU");
            }

            let (mut rx, child) = command
                .spawn()
                .expect("не удалось запустить sidecar-процесс");

            let state = handle.state::<SidecarState>();
            *state.child.lock().unwrap() = Some(child);

            // sidecar.exe собран с console=False (не мелькает окно консоли
            // при старте приложения) — весь его stdout/stderr пишем в файл
            // вместо консоли, перезаписывая при каждом запуске (только
            // последняя сессия, не накапливается бесконечно). Открывается
            // скрытой комбинацией Ctrl+Shift+Z+X+R в UI (см. useSidecarLog.ts).
            let log_path = app
                .path()
                .app_data_dir()
                .expect("не удалось определить директорию данных приложения")
                .join("sidecar.log");
            let log_file = std::fs::File::create(&log_path).ok();
            let log_file = std::sync::Arc::new(Mutex::new(log_file));

            let handle_for_task = handle.clone();
            let log_file_for_task = log_file.clone();
            tauri::async_runtime::spawn(async move {
                use tauri_plugin_shell::process::CommandEvent;

                let mut port_found = false;

                while let Some(event) = rx.recv().await {
                    let line = match &event {
                        CommandEvent::Stdout(line) | CommandEvent::Stderr(line) => Some(line),
                        _ => None,
                    };
                    let Some(line) = line else { continue };

                    if let Some(f) = log_file_for_task.lock().unwrap().as_mut() {
                        let _ = f.write_all(line);
                        let _ = f.write_all(b"\n");
                        let _ = f.flush();
                    }

                    if port_found {
                        continue;
                    }
                    if let CommandEvent::Stdout(_) = event {
                        let text = String::from_utf8_lossy(line);
                        if let Some(port_str) = text.trim().strip_prefix("SIDECAR_PORT=") {
                            if let Ok(port) = port_str.trim().parse::<u16>() {
                                port_found = true;
                                let ready = wait_for_health(port).await;
                                if ready {
                                    let state = handle_for_task.state::<SidecarState>();
                                    *state.port.lock().unwrap() = Some(port);
                                }
                            }
                        }
                    }
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let state = window.state::<SidecarState>();
                let child = state.child.lock().unwrap().take();
                if let Some(child) = child {
                    // child.kill() убивает только сам sidecar.exe, но не его
                    // дочерний GPU-подпроцесс (python-cuda/python.exe,
                    // спавленный sidecar'ом через subprocess.Popen для CUDA-
                    // инференса) — тот остаётся осиротевшим и продолжает
                    // есть память. taskkill /T убивает всё дерево по PID.
                    let pid = child.pid();
                    #[cfg(target_os = "windows")]
                    {
                        drop(child);
                        let _ = std::process::Command::new("taskkill")
                            .args(["/T", "/F", "/PID", &pid.to_string()])
                            .output();
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        let _ = child.kill();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![get_sidecar_port, read_sidecar_log, open_devtools])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
