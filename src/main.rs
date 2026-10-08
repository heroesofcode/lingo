fn main() -> gtk::glib::ExitCode {
    lingo::config::isolate_font_cache();
    lingo::logging::init();
    lingo::ui::run()
}
