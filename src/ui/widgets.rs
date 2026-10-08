//! Window parts: volume meter, utterance row, and the suggestion and Claude panels.

use std::cell::Cell;
use std::f64::consts::PI;
use std::rc::Rc;
use std::sync::LazyLock;

use gtk::prelude::*;
use gtk::{cairo, glib, pango};
use regex::Regex;

use crate::engine::{Speaker, SuggKind};
use crate::text::ReplyOption;

/// Bold and `code` in Claude's markdown become Pango markup; everything else is escaped.
pub fn md_to_pango(text: &str) -> String {
    static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
    static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());
    let escaped = glib::markup_escape_text(text);
    let markup = BOLD.replace_all(&escaped, "<b>$1</b>");
    let markup = CODE.replace_all(&markup, "<tt>$1</tt>").into_owned();
    if pango::parse_markup(&markup, '\0').is_ok() {
        markup
    } else {
        // crossed marks: plain text is better than nothing
        glib::markup_escape_text(&text.replace("**", "").replace('`', "")).to_string()
    }
}

pub fn label(text: &str, classes: &[&str], wrap: bool) -> gtk::Label {
    let label = gtk::Label::builder().label(text).xalign(0.0).wrap(wrap).build();
    if wrap {
        label.set_wrap_mode(pango::WrapMode::WordChar);
    }
    for class in classes {
        label.add_css_class(class);
    }
    label
}

fn close_button(panel: &gtk::Box) -> gtk::Button {
    let close =
        gtk::Button::builder().icon_name("window-close-symbolic").has_frame(false).tooltip_text("Fechar (Esc)").build();
    let panel = panel.downgrade();
    close.connect_clicked(move |_| {
        if let Some(panel) = panel.upgrade() {
            panel.set_visible(false);
        }
    });
    close
}

/// Small volume bar drawn by hand (GtkLevelBar varies too much between themes).
pub struct Meter {
    pub area: gtk::DrawingArea,
    value: Rc<Cell<f64>>,
}

impl Meter {
    pub fn new(rgb: (f64, f64, f64)) -> Meter {
        let area = gtk::DrawingArea::builder().content_width(48).content_height(6).valign(gtk::Align::Center).build();
        let value = Rc::new(Cell::new(0.0));
        let shown = value.clone();
        area.set_draw_func(move |_, cr, width, height| draw_pill(cr, width as f64, height as f64, shown.get(), rgb));
        Meter { area, value }
    }

    pub fn set_value(&self, value: f64) {
        let value = value.clamp(0.0, 1.0);
        if (value - self.value.get()).abs() > 0.01 {
            self.value.set(value);
            self.area.queue_draw();
        }
    }
}

fn draw_pill(cr: &cairo::Context, width: f64, height: f64, value: f64, (r, g, b): (f64, f64, f64)) {
    let radius = height / 2.0;
    let pill = |w: f64| {
        cr.new_sub_path();
        cr.arc(w - radius, radius, radius, -PI / 2.0, PI / 2.0);
        cr.arc(radius, radius, radius, PI / 2.0, 3.0 * PI / 2.0);
        cr.close_path();
    };
    cr.set_source_rgb(0x36 as f64 / 255.0, 0x3A as f64 / 255.0, 0x4F as f64 / 255.0);
    pill(width);
    let _ = cr.fill();
    if value > 0.02 {
        cr.set_source_rgb(r, g, b);
        pill(height.max(width * value));
        let _ = cr.fill();
    }
}

/// One utterance in the list. Those from "them" also show the translation and the notes.
#[derive(Clone)]
pub struct Row {
    pub root: gtk::Box,
    header: gtk::Label,
    text: gtk::Label,
    translation: gtk::Label,
    note: gtk::Label,
    cont: Rc<Cell<bool>>,
}

impl Row {
    pub fn new(speaker: Speaker, cont: bool) -> Row {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("row");
        let they = speaker == Speaker::They;
        root.add_css_class(if they { "they" } else { "me" });
        let who = if they { "Eles" } else { "Você" };
        let header = label(&format!("{who} · {}", chrono::Local::now().format("%H:%M")), &["who"], true);
        let text = label("", &[if they { "text-they" } else { "text-me" }], true);
        let translation = label("", &["tr"], true);
        let note = label("", &["note"], true);
        translation.set_visible(false);
        note.set_visible(false);
        for widget in [&header, &text, &translation, &note] {
            root.append(widget);
        }
        let row = Row { root, header, text, translation, note, cont: Rc::new(Cell::new(false)) };
        row.set_cont(cont);
        row.set_text("…", true);
        if they {
            row.root.set_tooltip_text(Some("Clique: sugerir respostas · botão direito: perguntar ao Claude"));
        }
        row
    }

    pub fn is_cont(&self) -> bool {
        self.cont.get()
    }

    /// Continuation of the same person's previous utterance: no header, attached to the one above.
    pub fn set_cont(&self, cont: bool) {
        self.cont.set(cont);
        self.header.set_visible(!cont);
        if cont {
            self.root.add_css_class("cont");
        } else {
            self.root.remove_css_class("cont");
        }
    }

    pub fn set_text(&self, text: &str, partial: bool) {
        self.text.set_label(if text.is_empty() { "…" } else { text });
        if partial {
            self.root.add_css_class("live");
        } else {
            self.root.remove_css_class("live");
        }
    }

    pub fn set_translation(&self, text: &str) {
        self.translation.set_label(text);
        self.translation.set_visible(!text.is_empty());
    }

    pub fn set_note(&self, note: &str) {
        self.note.set_label(&if note.is_empty() { String::new() } else { format!("💡 {note}") });
        self.note.set_visible(!note.is_empty());
    }
}

#[derive(Clone)]
struct OptionButton {
    button: gtk::Button,
    text: gtk::Label,
    gloss: gtk::Label,
}

impl OptionButton {
    fn new(index: usize, on_pick: Rc<dyn Fn(String)>) -> OptionButton {
        let button = gtk::Button::new();
        button.add_css_class("option");
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let badge = label(&(index + 1).to_string(), &["badge"], false);
        badge.set_valign(gtk::Align::Start);
        row.append(&badge);
        let texts = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
        let text = label("", &["opt-text"], true);
        let gloss = label("", &["opt-gloss"], true);
        texts.append(&text);
        texts.append(&gloss);
        row.append(&texts);
        button.set_child(Some(&row));
        button.set_tooltip_text(Some(&format!("Copiar (Ctrl+{})", index + 1)));
        let picked = text.clone();
        button.connect_clicked(move |_| on_pick(picked.label().to_string()));
        OptionButton { button, text, gloss }
    }

    fn set_option(&self, text: &str, gloss: &str) {
        self.text.set_label(text);
        self.gloss.set_label(gloss);
        self.gloss.set_visible(!gloss.is_empty());
        self.button.set_visible(!text.is_empty());
    }
}

pub struct SuggestionPanel {
    pub root: gtk::Box,
    kind: gtk::Label,
    spinner: gtk::Spinner,
    title: gtk::Label,
    options: Vec<OptionButton>,
    req: Cell<Option<u64>>,
}

impl SuggestionPanel {
    pub fn new(on_pick: Rc<dyn Fn(String)>) -> SuggestionPanel {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("panel");
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let kind = label("", &["panel-kind"], false);
        let spinner = gtk::Spinner::new();
        head.append(&kind);
        head.append(&spinner);
        head.append(&gtk::Box::builder().hexpand(true).build());
        head.append(&close_button(&root));
        root.append(&head);
        let title = label("", &["panel-title"], true);
        title.set_ellipsize(pango::EllipsizeMode::End);
        title.set_lines(2);
        root.append(&title);
        let options: Vec<OptionButton> = (0..3).map(|i| OptionButton::new(i, on_pick.clone())).collect();
        for option in &options {
            root.append(&option.button);
        }
        root.set_visible(false);
        SuggestionPanel { root, kind, spinner, title, options, req: Cell::new(None) }
    }

    pub fn start(&self, req: u64, kind: SuggKind, title: &str, subtitle: &str, auto: bool) {
        self.req.set(Some(req));
        self.kind.set_label(match (kind, auto) {
            (SuggKind::Reply, true) => "RESPONDER · automático",
            (SuggKind::Reply, false) => "RESPONDER",
            (SuggKind::Phrase, _) => "COMO DIZER",
        });
        self.title.set_label(&format!("“{}”", if subtitle.is_empty() { title } else { subtitle }));
        // If the panel is already open, the previous options stay (dimmed) until the first new one arrives.
        for option in &self.options {
            if self.root.is_visible() {
                option.button.add_css_class("stale");
            } else {
                option.set_option("", "");
            }
        }
        self.spinner.start();
        self.root.set_visible(true);
    }

    pub fn update(&self, req: u64, options: &[ReplyOption]) {
        if self.req.get() != Some(req) || options.iter().all(|o| o.text.is_empty()) {
            return;
        }
        for (button, option) in self.options.iter().zip(options) {
            button.button.remove_css_class("stale");
            button.set_option(&option.text, &option.gloss);
        }
    }

    pub fn done(&self, req: u64) {
        if self.req.get() != Some(req) {
            return;
        }
        self.spinner.stop();
        for option in &self.options {
            // the new one did not arrive (error): do not leave the old one under a new title
            if option.button.has_css_class("stale") {
                option.button.remove_css_class("stale");
                option.set_option("", "");
            }
        }
    }

    pub fn option_text(&self, index: usize) -> String {
        match self.options.get(index) {
            Some(option) if self.root.is_visible() && option.button.is_visible() => option.text.label().to_string(),
            _ => String::new(),
        }
    }
}

/// Claude Code's answer, with what it is looking at while it searches.
pub struct AskPanel {
    pub root: gtk::Box,
    spinner: gtk::Spinner,
    title: gtk::Label,
    answer: gtk::Label,
    progress: gtk::Label,
    req: Cell<Option<u64>>,
}

impl AskPanel {
    pub fn new() -> AskPanel {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("panel");
        root.add_css_class("ask");
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let spinner = gtk::Spinner::new();
        head.append(&label("CLAUDE", &["panel-kind"], false));
        head.append(&spinner);
        head.append(&gtk::Box::builder().hexpand(true).build());
        head.append(&close_button(&root));
        root.append(&head);
        let title = label("", &["panel-title"], true);
        title.set_ellipsize(pango::EllipsizeMode::End);
        title.set_lines(2);
        let answer = label("", &["ask-text"], true);
        answer.set_selectable(true);
        let progress = label("", &["ask-progress"], true);
        progress.set_ellipsize(pango::EllipsizeMode::Middle);
        for widget in [&title, &answer, &progress] {
            root.append(widget);
        }
        root.set_visible(false);
        AskPanel { root, spinner, title, answer, progress, req: Cell::new(None) }
    }

    pub fn start(&self, req: u64, question: &str) {
        self.req.set(Some(req));
        self.title.set_label(&format!("“{question}”"));
        self.set_answer(req, "");
        self.set_progress(req, "Perguntando ao Claude…");
        self.spinner.start();
        self.root.set_visible(true);
    }

    pub fn set_answer(&self, req: u64, text: &str) {
        if self.req.get() == Some(req) {
            self.answer.set_markup(&md_to_pango(text));
            self.answer.set_visible(!text.is_empty());
        }
    }

    pub fn set_progress(&self, req: u64, text: &str) {
        if self.req.get() == Some(req) {
            self.progress.set_label(text);
            self.progress.set_visible(!text.is_empty());
        }
    }

    pub fn done(&self, req: u64) {
        if self.req.get() == Some(req) {
            self.spinner.stop();
            let empty = self.answer.label().is_empty();
            self.set_progress(req, if empty { "Sem resposta." } else { "" });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::md_to_pango;

    #[test]
    fn bold_and_code_become_pango_markup() {
        assert_eq!(
            md_to_pango("Está na **3.2.0** da `payments-lib`."),
            "Está na <b>3.2.0</b> da <tt>payments-lib</tt>."
        );
    }

    #[test]
    fn escapes_and_survives_crossed_marks() {
        assert_eq!(md_to_pango("a < b & c"), "a &lt; b &amp; c");
        assert_eq!(md_to_pango("**a `b** c`"), "a b c");
    }
}
