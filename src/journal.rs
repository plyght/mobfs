use crate::config::{AppConfig, STATE_DIR};
use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const JOURNAL_FILE: &str = "journal.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum JournalOp {
    Upload { rel: String, upload_id: String },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Journal {
    pub pending: Vec<JournalOp>,
}

pub fn load(config: &AppConfig) -> Result<Journal> {
    let path = path(config);
    if !path.exists() {
        return Ok(Journal::default());
    }
    Ok(toml::from_str(&fs::read_to_string(path)?)?)
}

pub fn record(config: &AppConfig, op: JournalOp) -> Result<()> {
    let mut journal = load(config)?;
    if !journal.pending.contains(&op) {
        journal.pending.push(op);
    }
    save(config, &journal)
}

pub fn complete(config: &AppConfig, op: &JournalOp) -> Result<()> {
    let mut journal = load(config)?;
    journal.pending.retain(|pending| pending != op);
    save(config, &journal)
}

fn save(config: &AppConfig, journal: &Journal) -> Result<()> {
    let dir = config.local.root.join(STATE_DIR);
    fs::create_dir_all(&dir)?;
    fs::write(path(config), toml::to_string_pretty(journal)?)?;
    Ok(())
}

fn path(config: &AppConfig) -> PathBuf {
    config.local.root.join(STATE_DIR).join(JOURNAL_FILE)
}
