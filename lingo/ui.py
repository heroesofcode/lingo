"""Janela GTK4: transcrição traduzida, sugestões e "como digo isso?"."""

from __future__ import annotations

import logging
import math
import re
from datetime import datetime
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, Gio, GLib, Gtk, Pango  # noqa: E402

from .config import Config, MissingKeyError, load_api_key, load_config  # noqa: E402
from .engine import ME, THEY, Engine, EngineThread  # noqa: E402

log = logging.getLogger(__name__)

APP_ID = "dev.pedro.Lingo"
CSS_PATH = Path(__file__).with_name("style.css")
MAX_ROWS = 300
USAGE = """Uso: lingo [opção]
  (sem opção)  abre a janela ou traz para a frente
  --toggle     mostra/esconde a janela
  --suggest    sugere respostas para a última fala dos outros
  --ask        pergunta ao Claude Code sobre a última fala dos outros
  --pause      pausa/retoma a escuta (pausar libera o microfone)
  --mic        liga/desliga a transcrição do seu microfone
  --mode       troca entre traduzir e call no seu idioma (sem tradução)
  --quit       fecha o Lingo
"""
EMPTY_TEXT = {
    "translate": "Ouvindo a call…\nO que os outros falam aparece aqui enquanto falam,\ntraduzido frase a frase.\n"
                 "Clique numa fala ou use Ctrl+R para ver opções de resposta.",
    "native": "Ouvindo a call…\nO que os outros falam aparece aqui enquanto falam.\n"
              "Clique numa fala ou use Ctrl+R para ver opções de resposta.",
}


def md_to_pango(text: str) -> str:
    """Negrito e `código` do markdown do Claude viram marcação do Pango; o resto vai escapado."""
    markup = GLib.markup_escape_text(text)
    markup = re.sub(r"\*\*(.+?)\*\*", r"<b>\1</b>", markup)
    markup = re.sub(r"`([^`]+)`", r"<tt>\1</tt>", markup)
    try:
        Pango.parse_markup(markup, -1, "\0")
    except GLib.Error:  # marcações cruzadas: melhor texto puro do que nada
        markup = GLib.markup_escape_text(text.replace("**", "").replace("`", ""))
    return markup


def make_label(text: str = "", *css: str, wrap: bool = True, selectable: bool = False) -> Gtk.Label:
    lbl = Gtk.Label(label=text, xalign=0.0, wrap=wrap, selectable=selectable)
    if wrap:
        lbl.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
    for name in css:
        lbl.add_css_class(name)
    return lbl


class Meter(Gtk.DrawingArea):
    """Barrinha de volume desenhada à mão (o GtkLevelBar varia demais com o tema)."""

    TROUGH = (0x36 / 255, 0x3A / 255, 0x4F / 255)

    def __init__(self, rgb: tuple[float, float, float]) -> None:
        super().__init__(content_width=48, content_height=6, valign=Gtk.Align.CENTER)
        self.rgb = rgb
        self.value = 0.0
        self.set_draw_func(self._draw)

    def set_value(self, value: float) -> None:
        value = max(0.0, min(1.0, value))
        if abs(value - self.value) > 0.01:
            self.value = value
            self.queue_draw()

    def _draw(self, _area, cr, width: int, height: int) -> None:
        radius = height / 2

        def pill(w: float) -> None:
            cr.new_sub_path()
            cr.arc(w - radius, radius, radius, -math.pi / 2, math.pi / 2)
            cr.arc(radius, radius, radius, math.pi / 2, 3 * math.pi / 2)
            cr.close_path()

        cr.set_source_rgb(*self.TROUGH)
        pill(width)
        cr.fill()
        if self.value > 0.02:
            cr.set_source_rgb(*self.rgb)
            pill(max(height, width * self.value))
            cr.fill()


class UtteranceRow(Gtk.Box):
    def __init__(self, uid: str, speaker: str, on_click, on_ask, cont: bool) -> None:
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.uid = uid
        self.speaker = speaker
        self.add_css_class("row")
        self.add_css_class("they" if speaker == THEY else "me")
        who = "Eles" if speaker == THEY else "Você"
        self.header = make_label(f"{who} · {datetime.now():%H:%M}", "who")
        self.append(self.header)
        self.set_cont(cont)
        self.text = make_label("", "text-they" if speaker == THEY else "text-me")
        self.append(self.text)
        self.set_text("…", partial=True)
        self.translation = make_label("", "tr", selectable=False)
        self.translation.set_visible(False)
        self.append(self.translation)
        self.note = make_label("", "note")
        self.note.set_visible(False)
        self.append(self.note)
        if speaker == THEY:
            self.set_tooltip_text("Clique: sugerir respostas · botão direito: perguntar ao Claude")
            click = Gtk.GestureClick(button=0)
            click.connect("released", lambda g, *_: (on_ask if g.get_current_button() == 3 else on_click)(self.uid))
            self.add_controller(click)

    def set_cont(self, cont: bool) -> None:
        """Continuação da fala anterior da mesma pessoa: sem cabeçalho, colada na de cima."""
        self.cont = cont
        self.header.set_visible(not cont)
        if cont:
            self.add_css_class("cont")
        else:
            self.remove_css_class("cont")

    def set_text(self, text: str, partial: bool) -> None:
        self.text.set_label(text or "…")
        if partial:
            self.add_css_class("live")
        else:
            self.remove_css_class("live")

    def set_translation(self, text: str) -> None:
        self.translation.set_label(text)
        self.translation.set_visible(bool(text))

    def set_note(self, note: str) -> None:
        self.note.set_label(f"💡 {note}" if note else "")
        self.note.set_visible(bool(note))


class OptionButton(Gtk.Button):
    def __init__(self, index: int, on_pick) -> None:
        super().__init__()
        self.add_css_class("option")
        self.index = index
        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        badge = make_label(str(index + 1), "badge", wrap=False)
        badge.set_valign(Gtk.Align.START)
        box.append(badge)
        texts = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, hexpand=True)
        self.text = make_label("", "opt-text")
        self.gloss = make_label("", "opt-gloss")
        texts.append(self.text)
        texts.append(self.gloss)
        box.append(texts)
        self.set_child(box)
        self.set_tooltip_text(f"Copiar (Ctrl+{index + 1})")
        self.connect("clicked", lambda *_: on_pick(self.text.get_label()))

    def set_option(self, text: str, gloss: str) -> None:
        self.text.set_label(text)
        self.gloss.set_label(gloss)
        self.gloss.set_visible(bool(gloss))
        self.set_visible(bool(text))


class SuggestionPanel(Gtk.Box):
    def __init__(self, on_pick) -> None:
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.add_css_class("panel")
        self.req: str | None = None
        head = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        self.kind = make_label("", "panel-kind", wrap=False)
        head.append(self.kind)
        self.spinner = Gtk.Spinner()
        head.append(self.spinner)
        head.append(Gtk.Box(hexpand=True))
        close = Gtk.Button(icon_name="window-close-symbolic", has_frame=False)
        close.set_tooltip_text("Fechar (Esc)")
        close.connect("clicked", lambda *_: self.set_visible(False))
        head.append(close)
        self.append(head)
        self.title = make_label("", "panel-title")
        self.title.set_ellipsize(Pango.EllipsizeMode.END)
        self.title.set_lines(2)
        self.append(self.title)
        self.options = [OptionButton(i, on_pick) for i in range(3)]
        for opt in self.options:
            self.append(opt)
        self.set_visible(False)

    def start(self, req: str, mode: str, title: str, subtitle: str, reason: str) -> None:
        self.req = req
        if mode == "reply":
            self.kind.set_label("RESPONDER" + (" · automático" if reason == "auto" else ""))
        else:
            self.kind.set_label("COMO DIZER")
        self.title.set_label(f"“{subtitle or title}”")
        # Se o painel já está aberto, as opções anteriores ficam (esmaecidas) até chegar a primeira nova.
        for opt in self.options:
            if self.get_visible():
                opt.add_css_class("stale")
            else:
                opt.set_option("", "")
        self.spinner.start()
        self.set_visible(True)

    def update(self, req: str, options: list[tuple[str, str]]) -> None:
        if req != self.req or not any(text for text, _ in options):
            return
        for opt, (text, gloss) in zip(self.options, options):
            opt.remove_css_class("stale")
            opt.set_option(text, gloss)

    def done(self, req: str) -> None:
        if req == self.req:
            self.spinner.stop()
            for opt in self.options:
                if opt.has_css_class("stale"):  # a nova não chegou (erro): não deixar a antiga com título novo
                    opt.remove_css_class("stale")
                    opt.set_option("", "")

    def option_text(self, index: int) -> str:
        opt = self.options[index]
        return opt.text.get_label() if self.get_visible() and opt.get_visible() else ""


class AskPanel(Gtk.Box):
    """Resposta do Claude Code, com o que ele está consultando enquanto procura."""

    def __init__(self) -> None:
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.add_css_class("panel")
        self.add_css_class("ask")
        self.req: str | None = None
        head = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        head.append(make_label("CLAUDE", "panel-kind", wrap=False))
        self.spinner = Gtk.Spinner()
        head.append(self.spinner)
        head.append(Gtk.Box(hexpand=True))
        close = Gtk.Button(icon_name="window-close-symbolic", has_frame=False)
        close.set_tooltip_text("Fechar (Esc)")
        close.connect("clicked", lambda *_: self.set_visible(False))
        head.append(close)
        self.append(head)
        self.title = make_label("", "panel-title")
        self.title.set_ellipsize(Pango.EllipsizeMode.END)
        self.title.set_lines(2)
        self.append(self.title)
        self.answer = make_label("", "ask-text", selectable=True)
        self.append(self.answer)
        self.progress = make_label("", "ask-progress")
        self.progress.set_ellipsize(Pango.EllipsizeMode.MIDDLE)
        self.append(self.progress)
        self.set_visible(False)

    def start(self, req: str, question: str) -> None:
        self.req = req
        self.title.set_label(f"“{question}”")
        self.set_answer(req, "")
        self.set_progress(req, "Perguntando ao Claude…")
        self.spinner.start()
        self.set_visible(True)

    def set_answer(self, req: str, text: str) -> None:
        if req == self.req:
            self.answer.set_markup(md_to_pango(text))
            self.answer.set_visible(bool(text))

    def set_progress(self, req: str, text: str) -> None:
        if req == self.req:
            self.progress.set_label(text)
            self.progress.set_visible(bool(text))

    def done(self, req: str) -> None:
        if req == self.req:
            self.spinner.stop()
            self.set_progress(req, "" if self.answer.get_label() else "Sem resposta.")


class LingoWindow(Gtk.ApplicationWindow):
    def __init__(self, app: "LingoApp", cfg: Config) -> None:
        super().__init__(application=app, title="Lingo")
        self.app = app
        self.cfg = cfg
        self.add_css_class("lingo")
        self.set_default_size(460, 780)
        self.rows: dict[str, UtteranceRow] = {}
        self.status: dict[str, str] = {}
        self.mode = cfg.mode
        self._stick = True
        self._toast_source = 0
        self._syncing = False

        overlay = Gtk.Overlay()
        self.set_child(overlay)
        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        overlay.set_child(root)
        root.append(self._build_topbar())

        self.scroller = Gtk.ScrolledWindow(vexpand=True, hscrollbar_policy=Gtk.PolicyType.NEVER)
        self.feed = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.feed.add_css_class("feed")
        self.empty = make_label("", "empty")
        self.empty.set_justify(Gtk.Justification.CENTER)
        self.empty.set_xalign(0.5)
        self.feed.append(self.empty)
        self.scroller.set_child(self.feed)
        adj = self.scroller.get_vadjustment()
        adj.connect("value-changed", self._on_scroll)
        adj.connect("notify::upper", self._on_upper)
        adj.connect("notify::page-size", self._on_upper)  # painel de sugestões abriu/fechou
        root.append(self.scroller)

        self.ask_panel = AskPanel()
        root.append(self.ask_panel)
        self.panel = SuggestionPanel(self._copy)
        root.append(self.panel)

        self.composer = Gtk.Box()
        self.composer.add_css_class("composer")
        self.entry = Gtk.Entry(hexpand=True)
        self.entry.connect("activate", self._on_entry)
        self.composer.append(self.entry)
        root.append(self.composer)

        self.toast = make_label("", "toast", wrap=True)
        self.toast.set_halign(Gtk.Align.CENTER)
        self.toast.set_valign(Gtk.Align.END)
        self.toast.set_visible(False)
        self.toast.set_can_target(False)
        overlay.add_overlay(self.toast)

        self._install_shortcuts()
        self._apply_mode(cfg.mode)

    # ---- construção ---------------------------------------------------

    def _build_topbar(self) -> Gtk.Widget:
        bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6, vexpand=False)
        bar.add_css_class("topbar")
        self.dot = make_label("●", "dot", wrap=False)
        bar.append(self.dot)
        self.status_label = make_label("Conectando…", "status", wrap=False)
        bar.append(self.status_label)

        meters = Gtk.Grid(column_spacing=4, row_spacing=1, margin_start=8)
        meters.set_valign(Gtk.Align.CENTER)
        self.level = {}
        colors = {THEY: (0x8A / 255, 0xAD / 255, 0xF4 / 255), ME: (0xC6 / 255, 0xA0 / 255, 0xF6 / 255)}
        for row, (speaker, name) in enumerate(((THEY, "eles"), (ME, "você"))):
            meters.attach(make_label(name, "lvl-label", wrap=False), 0, row, 1, 1)
            meter = Meter(colors[speaker])
            meters.attach(meter, 1, row, 1, 1)
            self.level[speaker] = meter
        bar.append(meters)

        bar.append(Gtk.Box(hexpand=True))
        self.usage = make_label("", "usage", wrap=False)
        self.usage.set_tooltip_text("Minutos de áudio enviados e custo estimado da transcrição")
        bar.append(self.usage)

        self.mode_btn = Gtk.Button(has_frame=False)
        self.mode_btn.add_css_class("mode")
        self.mode_btn.connect("clicked", lambda *_: self.toggle_mode())
        self.mode_btn.set_valign(Gtk.Align.CENTER)
        bar.append(self.mode_btn)

        self.mic_btn = Gtk.ToggleButton(icon_name="audio-input-microphone-symbolic", active=self.cfg.capture_mic,
                                        has_frame=False)
        self.mic_btn.set_tooltip_text("Transcrever meu microfone (Ctrl+M)")
        self.mic_btn.connect("toggled", self._on_mic_toggled)
        self.mic_btn.set_valign(Gtk.Align.CENTER)
        bar.append(self.mic_btn)

        self.pause_btn = Gtk.ToggleButton(icon_name="media-playback-pause-symbolic", has_frame=False)
        self.pause_btn.set_tooltip_text("Pausar escuta e liberar o microfone (Ctrl+P)")
        self.pause_btn.connect("toggled", self._on_pause_toggled)
        self.pause_btn.set_valign(Gtk.Align.CENTER)
        bar.append(self.pause_btn)

        suggest = Gtk.Button(icon_name="mail-reply-sender-symbolic", has_frame=False)
        suggest.set_tooltip_text("Sugerir respostas para a última fala (Ctrl+R)")
        suggest.connect("clicked", lambda *_: self.app.engine_call("suggest"))
        suggest.set_valign(Gtk.Align.CENTER)
        bar.append(suggest)

        clear = Gtk.Button(icon_name="edit-clear-all-symbolic", has_frame=False)
        clear.set_tooltip_text("Limpar a conversa (Ctrl+L)")
        clear.connect("clicked", lambda *_: self.app.engine_call("clear"))
        clear.set_valign(Gtk.Align.CENTER)
        bar.append(clear)
        # Tudo tem atalho; sem foco de teclado o primeiro botão não abre com cara de selecionado.
        for btn in (self.mode_btn, self.mic_btn, self.pause_btn, suggest, clear):
            btn.set_focusable(False)
        return bar

    def _install_shortcuts(self) -> None:
        controller = Gtk.ShortcutController(scope=Gtk.ShortcutScope.GLOBAL)

        def add(trigger: str, callback) -> None:
            action = Gtk.CallbackAction.new(lambda *_: (callback(), True)[1])
            controller.add_shortcut(Gtk.Shortcut.new(Gtk.ShortcutTrigger.parse_string(trigger), action))

        for i in range(3):
            add(f"<Control>{i + 1}", lambda i=i: self._copy(self.panel.option_text(i)))
        add("<Control>r", lambda: self.app.engine_call("suggest"))
        add("<Control>p", lambda: self.pause_btn.set_active(not self.pause_btn.get_active()))
        add("<Control>m", lambda: self.mic_btn.set_active(not self.mic_btn.get_active()))
        add("<Control>l", lambda: self.app.engine_call("clear"))
        add("<Control>t", self.toggle_mode)
        add("<Control>k", lambda: self.app.engine_call("ask"))
        add("Escape", self._close_panels)
        self.add_controller(controller)

    def _close_panels(self) -> None:
        self.panel.set_visible(False)
        self.ask_panel.set_visible(False)

    # ---- modo ---------------------------------------------------------

    def toggle_mode(self) -> None:
        self.app.engine_call("set_mode", "translate" if self.mode == "native" else "native")

    def _apply_mode(self, mode: str) -> None:
        self.mode = mode
        native = mode == "native"
        self.mode_btn.set_label(self.cfg.mode_label(mode))
        self.mode_btn.set_tooltip_text(
            "Call no seu idioma: só transcreve, sem tradução (Ctrl+T troca)" if native
            else "Traduz o que os outros falam (Ctrl+T troca para call no seu idioma)")
        self.entry.set_placeholder_text("Pergunte ao Claude e Enter" if native
                                        else "Como digo…? em português e Enter · ?pergunta vai ao Claude")
        self.empty.set_label(EMPTY_TEXT[mode])
        if native:
            self.add_css_class("native")
        else:
            self.remove_css_class("native")

    # ---- ações --------------------------------------------------------

    def _on_mic_toggled(self, btn: Gtk.ToggleButton) -> None:
        if not self._syncing:
            self.app.engine_call("set_mic", btn.get_active())

    def _on_pause_toggled(self, btn: Gtk.ToggleButton) -> None:
        if not self._syncing:
            self.app.engine_call("set_paused", btn.get_active())

    def _on_entry(self, entry: Gtk.Entry) -> None:
        text = entry.get_text().strip()
        if not text:
            return
        # Na call em português não há "como digo"; o campo serve só para perguntar ao Claude.
        if self.mode == "native" or text.startswith("?"):
            question = text.lstrip("?").strip()
            if question:
                self.app.engine_call("ask", question)
        else:
            self.app.engine_call("phrase", text)
        entry.set_text("")

    def _on_row_click(self, uid: str) -> None:
        self.app.engine_call("suggest", uid)

    def _on_row_ask(self, uid: str) -> None:
        self.app.engine_call("ask", "", uid)

    def _copy(self, text: str) -> None:
        if not text:
            return
        self.get_clipboard().set(text)
        self.show_toast("Copiado")

    def show_toast(self, text: str, seconds: float = 1.6) -> None:
        self.toast.set_label(text)
        self.toast.set_visible(True)
        if self._toast_source:
            GLib.source_remove(self._toast_source)

        def hide() -> bool:
            self.toast.set_visible(False)
            self._toast_source = 0
            return False

        self._toast_source = GLib.timeout_add(int(seconds * 1000), hide)

    # ---- rolagem ------------------------------------------------------

    def _on_scroll(self, adj: Gtk.Adjustment) -> None:
        self._stick = adj.get_value() + adj.get_page_size() >= adj.get_upper() - 40

    def _on_upper(self, adj: Gtk.Adjustment, _pspec) -> None:
        if self._stick:
            adj.set_value(adj.get_upper() - adj.get_page_size())

    # ---- eventos do motor ---------------------------------------------

    def on_event(self, ev: dict) -> bool:
        try:
            self._dispatch(ev)
        except Exception:  # noqa: BLE001 - um evento ruim não pode derrubar a janela
            log.exception("erro tratando evento %s", ev.get("type"))
        return False

    def _dispatch(self, ev: dict) -> None:
        kind = ev["type"]
        if kind == "levels":
            for speaker in (THEY, ME):
                self.level[speaker].set_value(min(1.0, math.sqrt(ev[speaker]) * 2.2))
        elif kind == "status":
            self.status[ev["side"]] = ev["state"]
            if ev["state"] == "error" and ev.get("detail"):
                self.show_toast(ev["detail"], 5)
            self._refresh_status()
        elif kind == "utt_start":
            self._add_row(ev["uid"], ev["speaker"], ev.get("cont", False))
        elif kind == "utt_partial":
            if row := self.rows.get(ev["uid"]):
                row.set_text(ev["text"], partial=True)
        elif kind == "utt_final":
            row = self.rows.get(ev["uid"]) or self._add_row(ev["uid"], ev["speaker"])
            row.set_text(ev["text"], partial=False)
        elif kind == "utt_remove":
            self._remove_row(ev["uid"])
        elif kind == "translation":
            if row := self.rows.get(ev["uid"]):
                row.set_translation(ev["text"])
        elif kind == "note":
            if row := self.rows.get(ev["uid"]):
                row.set_note(ev["note"])
        elif kind == "sugg_start":
            self.panel.start(ev["req"], ev["mode"], ev["title"], ev["subtitle"], ev["reason"])
        elif kind == "sugg_options":
            self.panel.update(ev["req"], ev["options"])
        elif kind == "sugg_done":
            self.panel.done(ev["req"])
        elif kind == "ask_start":
            self.ask_panel.start(ev["req"], ev["question"])
        elif kind == "ask_text":
            self.ask_panel.set_answer(ev["req"], ev["text"])
        elif kind == "ask_progress":
            self.ask_panel.set_progress(ev["req"], ev["text"])
        elif kind == "ask_done":
            self.ask_panel.done(ev["req"])
        elif kind == "usage":
            minutes = ev["minutes"]
            mins = f"{minutes:.1f}" if minutes < 10 else f"{minutes:.0f}"
            usd = f"{ev['usd']:.2f}"
            self.usage.set_label(f"{mins} min · US$ {usd}".replace(".", ","))
        elif kind == "paused":
            self._set_toggle(self.pause_btn, ev["value"])
            self._refresh_status()
        elif kind == "mic":
            self._set_toggle(self.mic_btn, ev["enabled"])
        elif kind == "mode":
            self._apply_mode(ev["mode"])
            label = self.cfg.mode_label(ev["mode"])
            self.show_toast(f"{label}: sem tradução" if ev["mode"] == "native" else f"{label}: com tradução")
        elif kind == "cleared":
            for row in self.rows.values():
                self.feed.remove(row)
            self.rows.clear()
            self.empty.set_visible(True)
            self.panel.set_visible(False)
        elif kind in ("error", "notice"):
            self.show_toast(ev["text"], 4 if kind == "error" else 2.5)

    def _set_toggle(self, btn: Gtk.ToggleButton, value: bool) -> None:
        self._syncing = True
        btn.set_active(value)
        self._syncing = False

    def _add_row(self, uid: str, speaker: str, cont: bool = False) -> UtteranceRow:
        row = UtteranceRow(uid, speaker, self._on_row_click, self._on_row_ask, cont)
        self.rows[uid] = row
        self.feed.append(row)
        self.empty.set_visible(False)
        while len(self.rows) > MAX_ROWS:
            self._remove_row(next(iter(self.rows)))
        return row

    def _remove_row(self, uid: str) -> None:
        uids = list(self.rows)
        row = self.rows.pop(uid, None)
        if row is None:
            return
        self.feed.remove(row)
        i = uids.index(uid)
        if not row.cont and i + 1 < len(uids):
            self.rows[uids[i + 1]].set_cont(False)  # a seguinte vira o começo do bloco

    def _refresh_status(self) -> None:
        states = set(self.status.values()) - {"stopped"}
        if self.pause_btn.get_active():
            state, text = "paused", "Pausado"
        elif "error" in states:
            state, text = "error", "Erro"
        elif states & {"connecting", "reconnecting"}:
            state, text = "connecting", "Conectando…"
        elif "listening" in states:
            state, text = "listening", "Ouvindo"
        else:
            state, text = "paused", "Parado"
        for cls in ("listening", "connecting", "reconnecting", "error", "paused"):
            self.dot.remove_css_class(cls)
        self.dot.add_css_class(state)
        self.status_label.set_label(text)


class LingoApp(Gtk.Application):
    def __init__(self) -> None:
        super().__init__(application_id=APP_ID, flags=Gio.ApplicationFlags.HANDLES_COMMAND_LINE)
        self.window: LingoWindow | None = None
        self.engine: Engine | None = None
        self.thread: EngineThread | None = None

    def do_startup(self) -> None:
        Gtk.Application.do_startup(self)
        provider = Gtk.CssProvider()
        provider.load_from_path(str(CSS_PATH))
        Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), provider,
                                                  Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)

    def do_command_line(self, cmdline: Gio.ApplicationCommandLine) -> int:
        args = cmdline.get_arguments()[1:]
        first = self.window is None
        if first:
            self.activate()
        if not args:
            if not first and self.window:
                self.window.present()
            return 0
        arg = args[0]
        if arg in ("-h", "--help"):
            cmdline.print_literal(USAGE)
        elif arg == "--toggle":
            if not first and self.window:
                if self.window.get_visible():
                    self.window.set_visible(False)
                else:
                    self.window.present()
        elif arg == "--suggest":
            self.engine_call("suggest")
            if self.window and not self.window.get_visible():
                self.window.present()
        elif arg == "--pause" and self.window:
            self.window.pause_btn.set_active(not self.window.pause_btn.get_active())
        elif arg == "--mic" and self.window:
            self.window.mic_btn.set_active(not self.window.mic_btn.get_active())
        elif arg == "--mode" and self.window:
            self.window.toggle_mode()
        elif arg == "--ask":
            self.engine_call("ask")
            if self.window and not self.window.get_visible():
                self.window.present()
        elif arg == "--quit":
            self.quit()
        else:
            cmdline.printerr_literal(f"opção desconhecida: {arg}\n{USAGE}")
            return 2
        return 0

    def do_activate(self) -> None:
        if self.window:
            self.window.present()
            return
        cfg = load_config()
        self.window = LingoWindow(self, cfg)
        self.window.connect("close-request", self._on_close)
        self.window.present()
        try:
            api_key = load_api_key()
        except MissingKeyError as exc:
            self.window.show_toast(str(exc), 3600)
            return
        self.thread = EngineThread()
        window = self.window
        self.engine = Engine(cfg, api_key, lambda ev: GLib.idle_add(window.on_event, ev))
        self.thread.submit(self.engine.start())

    def engine_call(self, method: str, *args) -> None:
        if self.engine and self.thread:
            self.thread.submit(getattr(self.engine, method)(*args))

    def _on_close(self, _win) -> bool:
        self.quit()
        return False

    def do_shutdown(self) -> None:
        if self.engine and self.thread:
            try:
                self.thread.submit(self.engine.shutdown()).result(timeout=4)
            except Exception:  # noqa: BLE001
                log.exception("erro ao parar o motor")
            self.thread.stop()
        Gtk.Application.do_shutdown(self)
