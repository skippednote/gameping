use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

const BUILTIN: &str = include_str!("../assets/games.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct GameFile {
    #[serde(default, rename = "game")]
    pub games: Vec<Game>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Game {
    pub id: String,
    pub name: String,
    pub targets: Vec<TargetSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TargetSpec {
    /// Display label; derived from the source when omitted.
    pub region: Option<String>,
    pub src: String,
    #[serde(default)]
    pub direct: bool,
}

pub fn user_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/gameping/games.toml"))
}

/// Built-in games, with the user file (if any) replacing same-id games and appending new ones.
pub fn load(extra: Option<PathBuf>) -> Result<Vec<Game>> {
    let mut games = toml::from_str::<GameFile>(BUILTIN)
        .context("parsing built-in games.toml")?
        .games;

    let path = extra.or_else(user_config_path);
    if let Some(path) = path.filter(|p| p.exists()) {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let user: GameFile =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        for g in user.games {
            match games.iter_mut().find(|x| x.id == g.id) {
                Some(slot) => *slot = g,
                None => games.push(g),
            }
        }
    }
    Ok(games)
}
