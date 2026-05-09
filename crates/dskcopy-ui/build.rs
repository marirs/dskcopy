fn main() {
    let cfg = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/appwindow.slint", cfg)
        .expect("failed to compile Slint UI");
}