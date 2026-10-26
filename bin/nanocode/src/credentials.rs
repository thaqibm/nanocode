use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{config::Config, host::Result};

fn stty(tty: &std::fs::File, setting: &str) -> std::io::Result<()> {
    let status = Command::new("stty")
        .arg(setting)
        .stdin(Stdio::from(tty.try_clone()?))
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "stty {setting} failed with {status}"
        )))
    }
}

pub fn prompt_secret(label: &str) -> std::io::Result<String> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    write!(tty, "{label}")?;
    tty.flush()?;
    stty(&tty, "-echo")?;
    let mut line = String::new();
    let read = std::io::BufReader::new(tty.try_clone()?).read_line(&mut line);
    let restored = stty(&tty, "echo");
    writeln!(tty)?;
    read?;
    restored?;
    Ok(line.trim().to_owned())
}

pub fn login(config: &Config, home: &Path, name: &str) -> Result<PathBuf> {
    let Some(provider) = config.providers.get(name) else {
        let known = config.providers.keys().cloned().collect::<Vec<_>>();
        return Err(if known.is_empty() {
            format!(
                "no providers are configured; add [providers.{name}] to {}",
                Config::path(home).display()
            )
        } else {
            format!("no provider named {name}; configured: {}", known.join(", "))
        }
        .into());
    };
    let key = prompt_secret(&format!(
        "Paste your {name} API key ({}; input hidden): ",
        provider.base_url
    ))?;
    provider.save_key(name, home, &key)
}

pub fn ensure_key(config: &Config, home: &Path, model: Option<&str>) -> Result<()> {
    let Some((name, provider)) = model.and_then(|model| config.provider_for(model)) else {
        return Ok(());
    };
    if provider.key(name, home).is_ok() || !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Ok(());
    }
    eprintln!("nanocode: {name} has no API key yet");
    let path = login(config, home, name)?;
    eprintln!("nanocode: saved the {name} key to {}", path.display());
    Ok(())
}
