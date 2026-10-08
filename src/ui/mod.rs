//! GTK4 interface and the command line (`lingo --toggle` etc. talk to the window that is already open).

mod widgets;
mod window;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use crate::config::{self, Config};
use crate::engine::{Command, Engine, EngineHandle};
use window::Window;

const APP_ID: &str = "dev.pedro.Lingo";
const USAGE: &str = "Uso: lingo [opção]
  (sem opção)  abre a janela ou traz para a frente
  --toggle     mostra/esconde a janela
  --suggest    sugere respostas para a última fala dos outros
  --ask        pergunta ao Claude Code sobre a última fala dos outros
  --pause      pausa/retoma a escuta (pausar libera o microfone)
  --mic        liga/desliga a transcrição do seu microfone
  --mode       troca entre traduzir e call no seu idioma (sem tradução)
  --quit       fecha o Lingo
";
const OPTIONS: [&str; 7] = ["--toggle", "--suggest", "--ask", "--pause", "--mic", "--mode", "--quit"];

/// The engine runs on tokio, in its own threads; the window stays on the main thread, with GTK.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("lingo-engine")
            .enable_all()
            .build()
            .expect("runtime do tokio")
    })
}

#[derive(Default)]
struct State {
    window: RefCell<Option<Rc<Window>>>,
    engine: RefCell<Option<EngineHandle>>,
}

pub fn run() -> glib::ExitCode {
    // LINGO_APP_ID lets you open a second instance (for tests) without touching the one that is open.
    let app_id = std::env::var("LINGO_APP_ID").unwrap_or_else(|_| APP_ID.to_string());
    let app =
        gtk::Application::builder().application_id(app_id).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    let state = Rc::new(State::default());
    app.connect_startup(|_| load_css());
    app.connect_activate({
        let state = state.clone();
        move |app| activate(app, &state)
    });
    app.connect_command_line({
        let state = state.clone();
        move |app, cmdline| command_line(app, cmdline, &state)
    });
    app.connect_shutdown(move |_| shutdown(&state));
    app.run()
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

fn activate(app: &gtk::Application, state: &Rc<State>) {
    if let Some(window) = state.window.borrow().as_ref() {
        window.present();
        return;
    }
    let (cfg, cfg_error) = match Config::load(&config::config_file()) {
        Ok(cfg) => (cfg, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let (events, inbox_ui) = async_channel::unbounded();
    let (handle, key_error) = match config::load_api_key() {
        Ok(key) => {
            let (engine, handle, inbox) = Engine::new(cfg.clone(), key, events);
            runtime().spawn(engine.run(inbox));
            (Some(handle), None)
        }
        Err(e) => (None, Some(e)),
    };
    let window = Window::new(app, Rc::new(cfg), handle.clone().unwrap_or_else(EngineHandle::detached));
    let weak = Rc::downgrade(&window);
    glib::spawn_future_local(async move {
        while let Ok(ev) = inbox_ui.recv().await {
            let Some(window) = weak.upgrade() else { break };
            window.dispatch(ev);
        }
    });
    window.present();
    if let Some(e) = cfg_error {
        log::warn!("config.toml inválido: {e}");
        window.show_toast(&format!("config.toml inválido, usando o padrão: {e}"), 10.0);
    }
    if let Some(e) = key_error {
        window.show_toast(&e, 3600.0);
    }
    *state.window.borrow_mut() = Some(window);
    *state.engine.borrow_mut() = handle;
}

fn command_line(app: &gtk::Application, cmdline: &gio::ApplicationCommandLine, state: &Rc<State>) -> glib::ExitCode {
    let args: Vec<String> = cmdline.arguments().iter().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let first = state.window.borrow().is_none();
    // With no window open, --help and --quit answer without opening Lingo (and the microphone).
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            cmdline.print_literal(USAGE);
            return glib::ExitCode::SUCCESS;
        }
        Some("--quit") if first => return glib::ExitCode::SUCCESS,
        Some(other) if !OPTIONS.contains(&other) => {
            cmdline.printerr_literal(&format!("opção desconhecida: {other}\n{USAGE}"));
            return glib::ExitCode::from(2);
        }
        _ => {}
    }
    if first {
        app.activate();
    }
    let Some(window) = state.window.borrow().clone() else { return glib::ExitCode::FAILURE };
    let show = || {
        if !window.is_visible() {
            window.present();
        }
    };
    match args.first().map(String::as_str) {
        None => {
            if !first {
                window.present();
            }
        }
        Some("--toggle") => {
            if !first {
                window.toggle_visible();
            }
        }
        Some("--suggest") => {
            window.engine().send(Command::Suggest(None));
            show();
        }
        Some("--ask") => {
            window.engine().send(Command::Ask { question: String::new(), uid: None });
            show();
        }
        Some("--pause") => window.toggle_pause(),
        Some("--mic") => window.toggle_mic(),
        Some("--mode") => window.toggle_mode(),
        Some("--quit") => app.quit(),
        Some(_) => {}
    }
    glib::ExitCode::SUCCESS
}

fn shutdown(state: &State) {
    let Some(engine) = state.engine.borrow_mut().take() else { return };
    let stopped = runtime().block_on(async { tokio::time::timeout(Duration::from_secs(4), engine.shutdown()).await });
    if stopped.is_err() {
        log::warn!("o motor não parou em 4 s");
    }
}
