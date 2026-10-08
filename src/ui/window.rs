//! The window: top bar, list of utterances, panels and the text field.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use indexmap::IndexMap;

use super::widgets::{AskPanel, Meter, Row, SuggestionPanel, label};
use crate::config::{Config, Mode};
use crate::engine::{Command, EngineHandle, Event, Origin, Speaker};
use crate::realtime::Status;

const MAX_ROWS: usize = 300;
/// below this width the top bar gets compact
const COMPACT_BELOW: i32 = 460;
/// from this width on, the panels and the text field move to a column on the right
const WIDE_FROM: i32 = 900;
/// widest the conversation text gets, so lines stay readable on very wide windows
const MAX_FEED: i32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Compact,
    Normal,
    Wide,
}

fn layout_for(width: i32) -> Layout {
    if width < COMPACT_BELOW {
        Layout::Compact
    } else if width < WIDE_FROM {
        Layout::Normal
    } else {
        Layout::Wide
    }
}

fn side_width(width: i32) -> i32 {
    ((width as f64 * 0.38).round() as i32).clamp(380, 560)
}

fn empty_text(mode: Mode) -> &'static str {
    match mode {
        Mode::Translate => {
            "Ouvindo a call…\nO que os outros falam aparece aqui enquanto falam,\ntraduzido frase a frase.\n\
             Clique numa fala ou use Ctrl+R para ver opções de resposta."
        }
        Mode::Native => {
            "Ouvindo a call…\nO que os outros falam aparece aqui enquanto falam.\n\
             Clique numa fala ou use Ctrl+R para ver opções de resposta."
        }
    }
}

pub struct Window {
    /// weak reference to itself, so the signal closures do not keep the window alive
    me: Weak<Window>,
    win: gtk::ApplicationWindow,
    cfg: Rc<Config>,
    engine: EngineHandle,
    rows: RefCell<IndexMap<String, Row>>,
    status: RefCell<HashMap<Origin, Status>>,
    mode: Cell<Mode>,
    stick: Cell<bool>,
    toast_source: RefCell<Option<glib::SourceId>>,
    syncing: Cell<bool>,
    feed: gtk::Box,
    body: gtk::Box,
    side: gtk::Box,
    side_scroller: gtk::ScrolledWindow,
    side_hint: gtk::Label,
    layout: Cell<Option<Layout>>,
    size: Cell<(i32, i32)>,
    empty: gtk::Label,
    ask_panel: AskPanel,
    panel: SuggestionPanel,
    entry: gtk::Entry,
    toast: gtk::Label,
    dot: gtk::Label,
    status_label: gtk::Label,
    they_meter: Meter,
    me_meter: Meter,
    usage: gtk::Label,
    mode_btn: gtk::Button,
    mic_btn: gtk::ToggleButton,
    pause_btn: gtk::ToggleButton,
}

/// Calls `f` with the window, if it still exists.
fn with_window(me: &Weak<Window>, f: impl Fn(&Window) + 'static) -> impl Fn() + 'static {
    let me = me.clone();
    move || {
        if let Some(window) = me.upgrade() {
            f(&window);
        }
    }
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).has_frame(false).valign(gtk::Align::Center).build()
}

fn icon_toggle(icon: &str, tooltip: &str, active: bool) -> gtk::ToggleButton {
    gtk::ToggleButton::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .active(active)
        .has_frame(false)
        .valign(gtk::Align::Center)
        .build()
}

fn rgb(hex: u32) -> (f64, f64, f64) {
    let channel = |shift: u32| ((hex >> shift) & 0xFF) as f64 / 255.0;
    (channel(16), channel(8), channel(0))
}

impl Window {
    pub fn new(app: &gtk::Application, cfg: Rc<Config>, engine: EngineHandle) -> Rc<Window> {
        let window = Rc::new_cyclic(|me: &Weak<Window>| {
            let win = gtk::ApplicationWindow::builder()
                .application(app)
                .title("Lingo")
                .default_width(960)
                .default_height(780)
                .build();
            win.add_css_class("lingo");
            win.connect_close_request(|win| {
                if let Some(app) = win.application() {
                    app.quit();
                }
                glib::Propagation::Proceed
            });
            let overlay = gtk::Overlay::new();
            win.set_child(Some(&overlay));
            let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
            overlay.set_child(Some(&root));

            // Texts that change length shrink with an ellipsis instead of widening the window: Hyprland grows a
            // floating window to its minimum width (past the screen edge, if need be) and never narrows it back.
            let bar = gtk::Box::new(gtk::Orientation::Horizontal, 3);
            bar.add_css_class("topbar");
            let dot = label("●", &["dot"], false);
            let status_label = label("Conectando…", &["status"], false);
            status_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            bar.append(&dot);
            bar.append(&status_label);
            let meters = gtk::Grid::builder()
                .column_spacing(4)
                .row_spacing(1)
                .margin_start(8)
                .valign(gtk::Align::Center)
                .build();
            let they_meter = Meter::new(rgb(0x8AADF4));
            let me_meter = Meter::new(rgb(0xC6A0F6));
            meters.attach(&label("eles", &["lvl-label"], false), 0, 0, 1, 1);
            meters.attach(&they_meter.area, 1, 0, 1, 1);
            meters.attach(&label("você", &["lvl-label"], false), 0, 1, 1, 1);
            meters.attach(&me_meter.area, 1, 1, 1, 1);
            bar.append(&meters);
            bar.append(&gtk::Box::builder().hexpand(true).build());
            let usage = label("", &["usage"], false);
            usage.set_ellipsize(gtk::pango::EllipsizeMode::End);
            usage.set_tooltip_text(Some("Custo estimado da transcrição e da tradução"));
            bar.append(&usage);
            let mode_btn = gtk::Button::builder().has_frame(false).valign(gtk::Align::Center).build();
            mode_btn.add_css_class("mode");
            let mic_btn = icon_toggle(
                "audio-input-microphone-symbolic",
                "Transcrever meu microfone (Ctrl+M)",
                cfg.audio.capture_mic,
            );
            let pause_btn =
                icon_toggle("media-playback-pause-symbolic", "Pausar escuta e liberar o microfone (Ctrl+P)", false);
            let suggest_btn =
                icon_button("mail-reply-sender-symbolic", "Sugerir respostas para a última fala (Ctrl+R)");
            let clear_btn = icon_button("edit-clear-all-symbolic", "Limpar a conversa (Ctrl+L)");
            let buttons: [&gtk::Widget; 5] = [
                mode_btn.upcast_ref(),
                mic_btn.upcast_ref(),
                pause_btn.upcast_ref(),
                suggest_btn.upcast_ref(),
                clear_btn.upcast_ref(),
            ];
            for button in buttons {
                // Everything has a shortcut; without keyboard focus the first button does not open looking selected.
                button.set_focusable(false);
                bar.append(button);
            }
            root.append(&bar);

            let scroller = gtk::ScrolledWindow::builder()
                .hexpand(true)
                .vexpand(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build();
            let feed = gtk::Box::new(gtk::Orientation::Vertical, 0);
            feed.add_css_class("feed");
            let empty = label("", &["empty"], true);
            empty.set_justify(gtk::Justification::Center);
            empty.set_xalign(0.5);
            feed.append(&empty);
            scroller.set_child(Some(&feed));
            // Below the conversation or, on wide windows, in a column to its right: the panels and the text field.
            let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).build();
            body.append(&scroller);
            root.append(&body);
            let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
            side.add_css_class("side");
            // the text field expands, and GTK would pass that on to the column, which would then split the extra
            // width with the conversation instead of keeping the width apply_size gives it
            side.set_hexpand(false);
            // Automatic, not Never: with Never the column would take on the natural width of the panels' long
            // texts and squeeze the conversation; this way it keeps exactly the width apply_size gives it.
            let side_scroller = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Automatic)
                .propagate_natural_height(true)
                .build();
            let panels = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let side_hint = label(
                "As respostas sugeridas e as do Claude aparecem aqui.\nClique numa fala ou use Ctrl+R.",
                &["side-hint"],
                true,
            );
            side_hint.set_justify(gtk::Justification::Center);
            side_hint.set_xalign(0.5);
            side_hint.set_visible(false);
            panels.append(&side_hint);
            side_scroller.set_child(Some(&panels));
            side.append(&side_scroller);
            body.append(&side);

            let ask_panel = AskPanel::new();
            panels.append(&ask_panel.root);
            let copy = {
                let me = me.clone();
                move |text: String| {
                    if let Some(window) = me.upgrade() {
                        window.copy(&text);
                    }
                }
            };
            let panel = SuggestionPanel::new(Rc::new(copy));
            panels.append(&panel.root);
            for root in [&panel.root, &ask_panel.root] {
                let refresh = with_window(me, Window::refresh_side_hint);
                root.connect_visible_notify(move |_| refresh());
            }

            let composer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            composer.add_css_class("composer");
            let entry = gtk::Entry::builder().hexpand(true).build();
            composer.append(&entry);
            side.append(&composer);

            let toast = label("", &["toast"], true);
            toast.set_halign(gtk::Align::Center);
            toast.set_valign(gtk::Align::End);
            toast.set_visible(false);
            toast.set_can_target(false);
            overlay.add_overlay(&toast);
            // An invisible layer over the whole window, only to learn its size.
            let probe = gtk::DrawingArea::builder().can_target(false).build();
            probe.connect_resize({
                let me = me.clone();
                move |_, width, height| {
                    // changing the layout during the size allocation itself would make GTK warn
                    let me = me.clone();
                    glib::idle_add_local_once(move || {
                        if let Some(window) = me.upgrade() {
                            window.apply_size(width, height);
                        }
                    });
                }
            });
            overlay.add_overlay(&probe);

            let adj = scroller.vadjustment();
            adj.connect_value_changed({
                let me = me.clone();
                move |adj| {
                    if let Some(window) = me.upgrade() {
                        window.stick.set(adj.value() + adj.page_size() >= adj.upper() - 40.0);
                    }
                }
            });
            // "page-size" changes when a panel opens or closes
            for property in ["upper", "page-size"] {
                let me = me.clone();
                adj.connect_notify_local(Some(property), move |adj, _| {
                    if let Some(window) = me.upgrade()
                        && window.stick.get()
                    {
                        adj.set_value(adj.upper() - adj.page_size());
                    }
                });
            }
            let on_entry = with_window(me, Window::on_entry);
            entry.connect_activate(move |_| on_entry());
            let toggle_mode = with_window(me, Window::toggle_mode);
            mode_btn.connect_clicked(move |_| toggle_mode());
            let suggest = with_window(me, |w| w.engine.send(Command::Suggest(None)));
            suggest_btn.connect_clicked(move |_| suggest());
            let clear = with_window(me, |w| w.engine.send(Command::Clear));
            clear_btn.connect_clicked(move |_| clear());
            mic_btn.connect_toggled({
                let me = me.clone();
                move |btn| {
                    if let Some(window) = me.upgrade()
                        && !window.syncing.get()
                    {
                        window.engine.send(Command::SetMic(btn.is_active()));
                    }
                }
            });
            pause_btn.connect_toggled({
                let me = me.clone();
                move |btn| {
                    if let Some(window) = me.upgrade()
                        && !window.syncing.get()
                    {
                        window.engine.send(Command::SetPaused(btn.is_active()));
                    }
                }
            });

            Window {
                me: me.clone(),
                win,
                mode: Cell::new(cfg.mode()),
                cfg,
                engine,
                rows: RefCell::new(IndexMap::new()),
                status: RefCell::new(HashMap::new()),
                stick: Cell::new(true),
                toast_source: RefCell::new(None),
                syncing: Cell::new(false),
                feed,
                body,
                side,
                side_scroller,
                side_hint,
                layout: Cell::new(None),
                size: Cell::new((0, 0)),
                empty,
                ask_panel,
                panel,
                entry,
                toast,
                dot,
                status_label,
                they_meter,
                me_meter,
                usage,
                mode_btn,
                mic_btn,
                pause_btn,
            }
        });
        window.install_shortcuts();
        window.apply_mode(window.mode.get());
        window
    }

    fn install_shortcuts(&self) {
        let controller = gtk::ShortcutController::new();
        controller.set_scope(gtk::ShortcutScope::Global);
        let add = |trigger: &str, f: Box<dyn Fn(&Window)>| {
            let me = self.me.clone();
            let action = gtk::CallbackAction::new(move |_, _| {
                if let Some(window) = me.upgrade() {
                    f(&window);
                }
                glib::Propagation::Stop
            });
            controller.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(trigger), Some(action)));
        };
        for i in 0..3 {
            add(&format!("<Control>{}", i + 1), Box::new(move |w| w.copy(&w.panel.option_text(i))));
        }
        add("<Control>r", Box::new(|w| w.engine.send(Command::Suggest(None))));
        add("<Control>p", Box::new(Window::toggle_pause));
        add("<Control>m", Box::new(Window::toggle_mic));
        add("<Control>l", Box::new(|w| w.engine.send(Command::Clear)));
        add("<Control>t", Box::new(Window::toggle_mode));
        add("<Control>k", Box::new(|w| w.engine.send(Command::Ask { question: String::new(), uid: None })));
        add("Escape", Box::new(Window::close_panels));
        self.win.add_controller(controller);
    }

    pub fn engine(&self) -> &EngineHandle {
        &self.engine
    }

    pub fn present(&self) {
        self.win.present();
    }

    pub fn is_visible(&self) -> bool {
        self.win.is_visible()
    }

    pub fn toggle_visible(&self) {
        if self.win.is_visible() {
            self.win.set_visible(false);
        } else {
            self.win.present();
        }
    }

    pub fn toggle_pause(&self) {
        self.pause_btn.set_active(!self.pause_btn.is_active());
    }

    pub fn toggle_mic(&self) {
        self.mic_btn.set_active(!self.mic_btn.is_active());
    }

    pub fn toggle_mode(&self) {
        self.engine.send(Command::SetMode(self.mode.get().toggled()));
    }

    fn apply_size(&self, width: i32, height: i32) {
        if (width, height) == self.size.get() {
            return;
        }
        self.size.set((width, height));
        let layout = layout_for(width);
        let wide = layout == Layout::Wide;
        let side = if wide { side_width(width) } else { -1 };
        self.side.set_size_request(side, -1);
        // stacked under the conversation, the panels take at most half the height and scroll beyond that
        self.side_scroller.set_max_content_height(if wide { -1 } else { height / 2 });
        let feed = if wide { width - side } else { width };
        let margin = ((feed - MAX_FEED) / 2).max(0);
        self.feed.set_margin_start(margin);
        self.feed.set_margin_end(margin);
        if self.layout.get() == Some(layout) {
            return;
        }
        self.layout.set(Some(layout));
        let compact = layout == Layout::Compact;
        self.status_label.set_visible(!compact);
        self.usage.set_visible(!compact);
        self.body.set_orientation(if wide { gtk::Orientation::Horizontal } else { gtk::Orientation::Vertical });
        self.side_scroller.set_vexpand(wide);
        if wide {
            self.side.add_css_class("wide");
        } else {
            self.side.remove_css_class("wide");
        }
        self.refresh_side_hint();
    }

    fn refresh_side_hint(&self) {
        let empty = !self.panel.root.is_visible() && !self.ask_panel.root.is_visible();
        self.side_hint.set_visible(self.layout.get() == Some(Layout::Wide) && empty);
    }

    /// In the compact layout the status text and the cost are hidden; the dot's tooltip still has them.
    fn refresh_dot_tooltip(&self) {
        let (status, usage) = (self.status_label.label(), self.usage.label());
        let text = if usage.is_empty() { status.to_string() } else { format!("{status} · {usage}") };
        self.dot.set_tooltip_text(Some(&text));
    }

    fn close_panels(&self) {
        self.panel.root.set_visible(false);
        self.ask_panel.root.set_visible(false);
    }

    fn apply_mode(&self, mode: Mode) {
        self.mode.set(mode);
        let native = mode == Mode::Native;
        self.mode_btn.set_label(&self.cfg.mode_label(mode));
        self.mode_btn.set_tooltip_text(Some(if native {
            "Call no seu idioma: só transcreve, sem tradução (Ctrl+T troca)"
        } else {
            "Traduz o que os outros falam (Ctrl+T troca para call no seu idioma)"
        }));
        self.entry.set_placeholder_text(Some(if native {
            "Pergunte ao Claude e Enter"
        } else {
            "Como digo…? em português · ?pergunta ao Claude"
        }));
        self.empty.set_label(empty_text(mode));
        if native {
            self.win.add_css_class("native");
        } else {
            self.win.remove_css_class("native");
        }
    }

    fn on_entry(&self) {
        let text = self.entry.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        // In a call in your own language there is no "how do I say"; the field is only for asking Claude.
        if self.mode.get() == Mode::Native || text.starts_with('?') {
            let question = text.trim_start_matches('?').trim().to_string();
            if !question.is_empty() {
                self.engine.send(Command::Ask { question, uid: None });
            }
        } else {
            self.engine.send(Command::Phrase(text));
        }
        self.entry.set_text("");
    }

    fn copy(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.win.clipboard().set_text(text);
        self.show_toast("Copiado", 1.6);
    }

    pub fn show_toast(&self, text: &str, seconds: f64) {
        self.toast.set_label(text);
        self.toast.set_visible(true);
        if let Some(source) = self.toast_source.borrow_mut().take() {
            source.remove();
        }
        let me = self.me.clone();
        let source = glib::timeout_add_local_once(Duration::from_secs_f64(seconds), move || {
            if let Some(window) = me.upgrade() {
                window.toast_source.borrow_mut().take(); // already fired: do not remove it again
                window.toast.set_visible(false);
            }
        });
        *self.toast_source.borrow_mut() = Some(source);
    }

    fn set_toggle(&self, button: &gtk::ToggleButton, value: bool) {
        self.syncing.set(true);
        button.set_active(value);
        self.syncing.set(false);
    }

    fn row(&self, uid: &str) -> Option<Row> {
        self.rows.borrow().get(uid).cloned()
    }

    fn add_row(&self, uid: &str, speaker: Speaker, cont: bool) -> Row {
        let row = Row::new(speaker, cont);
        if speaker == Speaker::They {
            let click = gtk::GestureClick::builder().button(0).build();
            let (me, uid_owned) = (self.me.clone(), uid.to_string());
            click.connect_released(move |gesture, _, _, _| {
                let Some(window) = me.upgrade() else { return };
                let uid = Some(uid_owned.clone());
                if gesture.current_button() == 3 {
                    window.engine.send(Command::Ask { question: String::new(), uid });
                } else {
                    window.engine.send(Command::Suggest(uid));
                }
            });
            row.root.add_controller(click);
        }
        self.feed.append(&row.root);
        self.rows.borrow_mut().insert(uid.to_string(), row.clone());
        self.empty.set_visible(false);
        while self.rows.borrow().len() > MAX_ROWS {
            let first = self.rows.borrow().get_index(0).map(|(uid, _)| uid.clone());
            match first {
                Some(first) => self.remove_row(&first),
                None => break,
            }
        }
        row
    }

    fn remove_row(&self, uid: &str) {
        let mut rows = self.rows.borrow_mut();
        let Some(index) = rows.get_index_of(uid) else { return };
        let Some((_, row)) = rows.shift_remove_index(index) else { return };
        self.feed.remove(&row.root);
        if !row.is_cont()
            && let Some((_, next)) = rows.get_index(index)
        {
            next.set_cont(false); // the next one becomes the start of the block
        }
    }

    fn refresh_status(&self) {
        let states: HashSet<Status> =
            self.status.borrow().values().copied().filter(|s| *s != Status::Stopped).collect();
        let (class, text) = if self.pause_btn.is_active() {
            ("paused", "Pausado")
        } else if states.contains(&Status::Error) {
            ("error", "Erro")
        } else if states.contains(&Status::Connecting) || states.contains(&Status::Reconnecting) {
            ("connecting", "Conectando…")
        } else if states.contains(&Status::Listening) {
            ("listening", "Ouvindo")
        } else {
            ("paused", "Parado")
        };
        for name in ["listening", "connecting", "reconnecting", "error", "paused"] {
            self.dot.remove_css_class(name);
        }
        self.dot.add_css_class(class);
        self.status_label.set_label(text);
        self.refresh_dot_tooltip();
    }

    pub fn dispatch(&self, ev: Event) {
        match ev {
            Event::Levels { they, me } => {
                self.they_meter.set_value((they as f64).sqrt() * 2.2);
                self.me_meter.set_value((me as f64).sqrt() * 2.2);
            }
            Event::Status { origin, status, detail } => {
                self.status.borrow_mut().insert(origin, status);
                if status == Status::Error && !detail.is_empty() {
                    self.show_toast(&detail, 5.0);
                }
                self.refresh_status();
            }
            Event::UttStart { uid, speaker, cont } => {
                self.add_row(&uid, speaker, cont);
            }
            Event::UttPartial { uid, text } => {
                if let Some(row) = self.row(&uid) {
                    row.set_text(&text, true);
                }
            }
            Event::UttFinal { uid, speaker, text } => {
                let row = self.row(&uid).unwrap_or_else(|| self.add_row(&uid, speaker, false));
                row.set_text(&text, false);
            }
            Event::UttRemove { uid } => self.remove_row(&uid),
            Event::Translation { uid, text } => {
                if let Some(row) = self.row(&uid) {
                    row.set_translation(&text);
                }
            }
            Event::Note { uid, note } => {
                if let Some(row) = self.row(&uid) {
                    row.set_note(&note);
                }
            }
            Event::SuggStart { req, kind, title, subtitle, auto } => {
                self.panel.start(req, kind, &title, &subtitle, auto)
            }
            Event::SuggOptions { req, options } => self.panel.update(req, &options),
            Event::SuggDone { req } => self.panel.done(req),
            Event::AskStart { req, question } => self.ask_panel.start(req, &question),
            Event::AskText { req, text } => self.ask_panel.set_answer(req, &text),
            Event::AskProgress { req, text } => self.ask_panel.set_progress(req, &text),
            Event::AskDone { req } => self.ask_panel.done(req),
            Event::Usage { minutes, usd } => {
                let minutes = if minutes < 10.0 { format!("{minutes:.1}") } else { format!("{minutes:.0}") };
                self.usage.set_label(&format!("US$ {usd:.2}").replace('.', ","));
                let tooltip = format!("{minutes} min de áudio enviados · custo estimado da transcrição e da tradução");
                self.usage.set_tooltip_text(Some(&tooltip.replace('.', ",")));
                self.refresh_dot_tooltip();
            }
            Event::Paused(paused) => {
                self.set_toggle(&self.pause_btn, paused);
                self.refresh_status();
            }
            Event::Mic(enabled) => self.set_toggle(&self.mic_btn, enabled),
            Event::Mode(mode) => {
                self.apply_mode(mode);
                let what = if mode == Mode::Native { "sem tradução" } else { "com tradução" };
                self.show_toast(&format!("{}: {what}", self.cfg.mode_label(mode)), 1.6);
            }
            Event::Cleared => {
                for (_, row) in self.rows.borrow_mut().drain(..) {
                    self.feed.remove(&row.root);
                }
                self.empty.set_visible(true);
                self.panel.root.set_visible(false);
            }
            Event::Error(text) => self.show_toast(&text, 4.0),
            Event::Notice(text) => self.show_toast(&text, 2.5),
        }
    }
}
