//! Settings in %APPDATA%\KKCUT\config.ini, one `key=value` per line.

use std::path::PathBuf;

/// How many skipped songs the Skipped tab can keep.
pub const HISTORY_CHOICES: [usize; 4] = [5, 15, 25, 50];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The main switch: skip songs while KKBOX plays.
    pub enabled: bool,
    /// Skip songs by singers and bands on the China list.
    pub artists: bool,
    /// Skip songs with simplified Chinese song or album titles.
    pub titles: bool,
    /// Skip songs by artists on the black list.
    pub black_enabled: bool,
    /// Skip songs by artists on the author's black list (`kkbox::AUTHOR_BLACK`).
    pub author_enabled: bool,
    /// Skip songs by AI and virtual singers (`kkbox::Lists::ai`).
    pub ai: bool,
    /// Never skip Japanese virtual singers (初音ミク and other Vocaloid).
    pub ai_keep_japan: bool,
    /// Artists (as KKBOX names them) never skipped.
    pub white: Vec<String>,
    /// Artists always skipped while `black_enabled` is on.
    pub black: Vec<String>,
    /// Skipped songs kept: one of `HISTORY_CHOICES`.
    pub history: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            artists: true,
            titles: true,
            black_enabled: true,
            author_enabled: true,
            ai: true,
            ai_keep_japan: true,
            white: Vec::new(),
            black: Vec::new(),
            history: 15,
        }
    }
}

/// %APPDATA%\KKCUT, created when missing.
pub fn data_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("KKCUT");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("config.ini"))
}

fn split_list(v: &str) -> Vec<String> {
    v.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

impl Config {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| Self::parse(&t))
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = path().ok_or_else(|| std::io::Error::other("APPDATA is not set"))?;
        // Written beside and renamed, so a crash never leaves half a file.
        let temp = path.with_extension("ini.tmp");
        std::fs::write(&temp, self.to_text())?;
        std::fs::rename(temp, path)
    }

    pub fn parse(text: &str) -> Self {
        let mut c = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "enabled" => c.enabled = v.parse().unwrap_or(c.enabled),
                "artists" => c.artists = v.parse().unwrap_or(c.artists),
                "titles" => c.titles = v.parse().unwrap_or(c.titles),
                "black.enabled" => c.black_enabled = v.parse().unwrap_or(c.black_enabled),
                "author.enabled" => c.author_enabled = v.parse().unwrap_or(c.author_enabled),
                "ai.enabled" => c.ai = v.parse().unwrap_or(c.ai),
                "ai.keep_japan" => c.ai_keep_japan = v.parse().unwrap_or(c.ai_keep_japan),
                "white" => c.white = split_list(v),
                "black" => c.black = split_list(v),
                "history" => {
                    if let Ok(n) = v.parse()
                        && HISTORY_CHOICES.contains(&n)
                    {
                        c.history = n;
                    }
                }
                _ => {}
            }
        }
        c
    }

    pub fn to_text(&self) -> String {
        format!(
            "enabled={}\nartists={}\ntitles={}\nblack.enabled={}\nauthor.enabled={}\nai.enabled={}\nai.keep_japan={}\nhistory={}\nwhite={}\nblack={}\n",
            self.enabled,
            self.artists,
            self.titles,
            self.black_enabled,
            self.author_enabled,
            self.ai,
            self.ai_keep_japan,
            self.history,
            self.white.join("|"),
            self.black.join("|"),
        )
    }

    /// Adds `artist` to the black list (`black`) or the white list, and takes
    /// it off the other one.
    pub fn list_add(&mut self, black: bool, artist: &str) -> bool {
        let artist = artist.replace('|', " ").trim().to_string();
        if artist.is_empty() {
            return false;
        }
        let (to, from) = if black {
            (&mut self.black, &mut self.white)
        } else {
            (&mut self.white, &mut self.black)
        };
        from.retain(|a| *a != artist);
        if !to.contains(&artist) {
            to.push(artist);
        }
        true
    }

    pub fn list_remove(&mut self, black: bool, artist: &str) {
        let list = if black { &mut self.black } else { &mut self.white };
        list.retain(|a| a != artist);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let c = Config {
            enabled: false,
            artists: false,
            titles: true,
            black_enabled: false,
            author_enabled: false,
            ai: false,
            ai_keep_japan: false,
            white: vec!["李健 (Li Jian)".into(), "GAI".into()],
            black: vec!["某网络歌手".into()],
            history: 50,
        };
        assert_eq!(Config::parse(&c.to_text()), c);
        assert_eq!(Config::parse(""), Config::default());
        // A config.ini from before v1.4.0 has both virtual singer switches on.
        let old = Config::parse("enabled=true\nauthor.enabled=false\n");
        assert!(old.ai && old.ai_keep_japan);
        // Unknown history sizes fall back to the default.
        assert_eq!(Config::parse("history=7").history, 15);
    }

    #[test]
    fn a_name_moves_between_the_lists() {
        let mut c = Config::default();
        assert!(c.list_add(true, " 五月天 "));
        assert_eq!(c.black, ["五月天"]);
        assert!(c.list_add(false, "五月天"));
        assert!(c.black.is_empty());
        assert_eq!(c.white, ["五月天"]);
        assert!(!c.list_add(false, "  "));
        assert!(c.list_add(false, "A|B"));
        assert_eq!(c.white, ["五月天", "A B"]);
        c.list_remove(false, "五月天");
        assert_eq!(c.white, ["A B"]);
    }
}
