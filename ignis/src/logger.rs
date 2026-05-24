use std::path::Path;

/// Initialize the global logger writing to `log_dir/ignis.log`.
pub fn init(log_dir: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(log_dir)?;
    let log_file = log_dir.join("ignis.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)?;

    simplelog::WriteLogger::init(
        simplelog::LevelFilter::Info,
        simplelog::Config::default(),
        file,
    )?;
    Ok(())
}
