// GUI entry point. Launches the Slint UI directly with no CLI parsing.
fn main() {
    if let Err(e) = dskcopy_ui::launch() {
        eprintln!("Failed to launch GUI: {e}");
        std::process::exit(1);
    }
}
