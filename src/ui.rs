use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

const SPINNER_TICK_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

pub fn spinner(message: &str) -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap()
            .tick_chars(SPINNER_TICK_CHARS),
    );
    spinner.enable_steady_tick(Duration::from_millis(80));
    spinner.set_message(message.to_string());
    spinner
}

pub fn ok(message: impl AsRef<str>) {
    println!("{} {}", style("✓").green(), message.as_ref());
}

pub fn added(label: impl AsRef<str>, value: impl AsRef<str>) {
    println!(
        "{} {} {}",
        style("+").green(),
        style(label.as_ref()).dim(),
        style(value.as_ref()).magenta()
    );
}

pub fn info(label: impl AsRef<str>, message: impl AsRef<str>) {
    println!(
        "{} {} {}",
        style("→").cyan(),
        style(label.as_ref()).dim(),
        message.as_ref()
    );
}

pub fn section(label: impl AsRef<str>) {
    println!("{}", style(label.as_ref()).bold());
}

pub fn command(command: impl AsRef<str>) {
    println!("  {}", style(command.as_ref()).cyan());
}

pub fn bullet(message: impl AsRef<str>) {
    println!("{} {}", style("-").dim(), message.as_ref());
}

pub fn blank() {
    println!();
}

pub fn change(label: impl AsRef<str>, path: impl AsRef<str>) {
    println!(
        "{} {} {}",
        style("→").cyan(),
        style(label.as_ref()).yellow(),
        style(path.as_ref()).magenta()
    );
}

pub fn summary(label: impl AsRef<str>, count: usize) {
    println!(
        "{} {} {}",
        style("✓").green(),
        style(count).cyan(),
        label.as_ref()
    );
}

pub fn warn(message: impl AsRef<str>) {
    eprintln!("{} {}", style("!").yellow(), message.as_ref());
}
