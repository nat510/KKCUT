//! Skips songs by singers and bands from China, by AI and virtual singers,
//! songs with simplified Chinese song or album titles, and songs by artists
//! on the black list, while KKBOX plays.
//!
//! KKBOX for Windows reports the song it plays to Windows' media controls,
//! which also skip to the next song. The guard thread waits on their
//! notices, plus one look every 5 s while KKBOX plays in case a notice is
//! missed; nothing while KKBOX is paused or closed, and nothing at all while
//! the main switch is off. Every call to KKBOX gives up after a few seconds.

use crate::config::Config;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows_future::{AsyncStatus, IAsyncOperation};

/// KKBOX's media session names (its app ID) contain this, ignoring case:
/// `electron.app.KKBOX`, or `com.kkbox.electron-app`, depending on how
/// KKBOX was started. KKBOX makes its session once it starts playing.
const KKBOX: &str = "kkbox";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Song {
    pub title: String,
    pub artist: String,
    pub album: String,
}

// ------------------------------------------------------------------ rules

const GB2312: u32 = 20936;
const BIG5: u32 = 950;
const SHIFT_JIS: u32 = 932;

fn in_code_page(page: u32, c: char) -> bool {
    use windows_sys::Win32::Globalization::{WC_NO_BEST_FIT_CHARS, WideCharToMultiByte};
    let mut wide = [0u16; 2];
    let wide = c.encode_utf16(&mut wide);
    let (mut out, mut defaulted) = ([0u8; 8], 0);
    let n = unsafe {
        WideCharToMultiByte(
            page,
            WC_NO_BEST_FIT_CHARS,
            wide.as_ptr(),
            wide.len() as i32,
            out.as_mut_ptr(),
            out.len() as i32,
            std::ptr::null(),
            &mut defaulted,
        )
    };
    n > 0 && defaulted == 0
}

fn is_han(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}')
}

fn is_kana(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' | '\u{FF66}'..='\u{FF9F}')
}

/// Text in simplified Chinese: it has a character only simplified Chinese
/// uses (in GB2312, not in Big5 and not in Japanese Shift-JIS, e.g. 们 说 爱),
/// and no kana and no character only traditional Chinese uses (響 倫 華).
/// Characters both scripts use (台 后 范 杰) and Japanese forms shared with
/// simplified Chinese (国 学 残) prove nothing.
pub fn simplified(text: &str) -> bool {
    let mut found = false;
    for c in text.chars() {
        if is_kana(c) {
            return false;
        }
        if !is_han(c) {
            continue;
        }
        let (gb, big5) = (in_code_page(GB2312, c), in_code_page(BIG5, c));
        if big5 && !gb {
            return false;
        }
        if gb && !big5 && !in_code_page(SHIFT_JIS, c) {
            found = true;
        }
    }
    found
}

/// Windows' own simplified/traditional character mapping.
fn convert(text: &str, traditional: bool) -> String {
    use windows_sys::Win32::Globalization::{
        LCMAP_SIMPLIFIED_CHINESE, LCMAP_TRADITIONAL_CHINESE, LCMapStringEx,
    };
    let flag = if traditional {
        LCMAP_TRADITIONAL_CHINESE
    } else {
        LCMAP_SIMPLIFIED_CHINESE
    };
    let source: Vec<u16> = text.encode_utf16().collect();
    let mut out = vec![0u16; source.len() * 2 + 8];
    let locale = crate::wide("zh-CN");
    let n = unsafe {
        LCMapStringEx(
            locale.as_ptr(),
            flag,
            source.as_ptr(),
            source.len() as i32,
            out.as_mut_ptr(),
            out.len() as i32,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if n <= 0 {
        return text.to_string();
    }
    String::from_utf16_lossy(&out[..n as usize])
}

/// A name without its disambiguation: "李健 (歌手)" and "李健（歌手）" are "李健".
fn strip(name: &str) -> &str {
    let name = name.trim();
    match name.find(['(', '（']) {
        Some(i) if i > 0 => name[..i].trim(),
        _ => name,
    }
}

/// A name as compared: stripped, in simplified form, Latin names ignoring
/// case, without the middle dot (希林娜依·高 is 希林娜依高). Simplified is
/// the one form both scripts fold into: 林俊傑 and 林俊杰 are one key, though
/// Windows keeps 杰 when converting to traditional. Names are never compared
/// in traditional form for the same reason (头发 becomes 頭發).
fn name_key(name: &str) -> String {
    convert(strip(name), false)
        .to_lowercase()
        .replace(['·', '・', '•', '‧'], "")
        .chars()
        .map(|c| {
            FOLD.iter()
                .find(|(from, _)| *from == c)
                .map_or(c, |(_, to)| *to)
        })
        .collect()
}

/// Characters Windows' conversion leaves unsimplified, or turns into a rarer
/// form (鍾 becomes 锺 where China writes 钟). Only variants of one character:
/// 祢, 吒, 鍊, 鉅 and 囉 mean something else and stay, and 肖 and 蕭 are two
/// surnames.
const FOLD: &[(char, char)] = &[
    ('後', '后'),
    ('乾', '干'),
    ('锺', '钟'),
    ('於', '于'),
    ('蹟', '迹'),
    ('綫', '线'),
    ('羣', '群'),
    ('峯', '峰'),
    ('牀', '床'),
    ('衆', '众'),
    ('爲', '为'),
    ('僞', '伪'),
    ('綉', '绣'),
    ('慾', '欲'),
    ('昇', '升'),
    ('粧', '妆'),
];

/// KKBOX often writes a Chinese name and its Latin alias with no bracket
/// ("攬佬SKAI ISYOURGOD"); the Chinese part, two characters or more, is a
/// name too.
fn han_prefix(name: &str) -> Option<&str> {
    let end = name.find(|c: char| !is_han(c) && c != '·')?;
    let (head, rest) = name.split_at(end);
    (head.chars().count() >= 2
        && rest
            .trim_start()
            .starts_with(|c: char| c.is_ascii_alphanumeric()))
    .then_some(head)
}

/// Each artist of a KKBOX artist line, e.g. "周杰倫 (Jay Chou)、費玉清", as
/// (name key, the name as KKBOX shows it).
fn parts(artist: &str) -> Vec<(String, String)> {
    let mut line = artist.to_string();
    for joint in [" feat. ", " Feat. ", " ft. ", " featuring ", " x ", " X "] {
        line = line.replace(joint, "|");
    }
    line.split(['|', '、', ',', '，', '&', '＆', '/', '／', ';', '；', '×'])
        .flat_map(|part| {
            let shown = strip(part);
            let alias = han_prefix(shown).map(|h| (name_key(h), h.to_string()));
            // Chinese names have no spaces: "五月天 阿信" is two of them.
            let words: Vec<&str> = shown.split_whitespace().collect();
            let chinese =
                |w: &&str| w.chars().count() >= 2 && w.chars().all(|c| is_han(c) || c == '·');
            let several: Vec<(String, String)> = if words.len() > 1 && words.iter().all(chinese) {
                words.iter().map(|w| (name_key(w), w.to_string())).collect()
            } else {
                Vec::new()
            };
            std::iter::once((name_key(part), shown.to_string()))
                .chain(alias)
                .chain(several)
        })
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

/// The name keys of each artist of a KKBOX artist line.
pub fn names(artist: &str) -> Vec<String> {
    parts(artist).into_iter().map(|(key, _)| key).collect()
}

/// The singer lists, as name keys (see `name_key`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lists {
    /// Singers and bands from China.
    pub china: HashSet<String>,
    /// Singers and bands from Taiwan, Hong Kong, Macau, Malaysia and Singapore.
    pub elsewhere: HashSet<String>,
    /// AI and virtual singers (Vocaloid characters, virtual idols), by their
    /// Chinese, Japanese and English names.
    pub ai: HashSet<String>,
    /// The Japanese ones among them (初音ミク, 初音未來, Hatsune Miku).
    pub ai_japan: HashSet<String>,
    /// When the lists were fetched (Unix time); 0 = never.
    pub fetched: u64,
    pub categories: usize,
}

/// The author's black list, switched by `author_enabled`; the white list beats it.
pub const AUTHOR_BLACK: &[&str] = &[
    "DJ鐵柱",
    "YUQI",
    "黃詩扶",
    "張遠",
    "黃霄雲",
    "郁可唯 (Yisa Yu)",
    "晟欣音樂",
    "孟慧圓",
    "吳卓源 (Julia Wu)",
    "阿耶奈",
    "Cheeze, Trazzy, 雾明",
    "田園",
    "小藍背心",
    "林晚星",
    "一顆星島",
    "晚秋夢",
    "希林娜依高",
    "張弛",
    "王天放",
    "張星特",
    "格雷乐团-Grey",
    "薇薇",
    "陳敏萱",
    "Jansley Lee",
    "章昊 (ZHANG HAO)",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rules {
    pub artists: bool,
    pub titles: bool,
    pub black_enabled: bool,
    pub author_enabled: bool,
    pub ai: bool,
    /// Japanese virtual singers are not skipped.
    pub ai_keep_japan: bool,
    /// Name keys of the white list (never skipped), the black list and the author's.
    pub white: HashSet<String>,
    pub black: HashSet<String>,
    pub author: HashSet<String>,
}

impl Rules {
    pub fn of(config: &Config) -> Self {
        Self {
            artists: config.artists,
            titles: config.titles,
            black_enabled: config.black_enabled,
            author_enabled: config.author_enabled,
            ai: config.ai,
            ai_keep_japan: config.ai_keep_japan,
            white: config.white.iter().flat_map(|a| names(a)).collect(),
            black: config.black.iter().flat_map(|a| names(a)).collect(),
            author: AUTHOR_BLACK.iter().flat_map(|a| names(a)).collect(),
        }
    }

    /// Whether an artist of this line is on the white list.
    pub fn allowed(&self, artist: &str) -> bool {
        names(artist).iter().any(|n| self.white.contains(n))
    }
}

/// Why a song is skipped. Names are as KKBOX shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    BlackList(String),
    AuthorList(String),
    Artist(String),
    Virtual(String),
    SimplifiedTitle,
    SimplifiedAlbum,
}

impl Reason {
    /// The artist that matched, if a name did.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::BlackList(n) | Self::AuthorList(n) | Self::Artist(n) | Self::Virtual(n) => {
                Some(n)
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::BlackList(name) => write!(f, "黑名單：{name}"),
            Self::AuthorList(name) => write!(f, "作者黑名單：{name}"),
            Self::Artist(name) => write!(f, "中國歌手：{name}"),
            Self::Virtual(name) => write!(f, "虛擬歌手：{name}"),
            Self::SimplifiedTitle => f.write_str("簡體中文歌名"),
            Self::SimplifiedAlbum => f.write_str("簡體中文專輯名"),
        }
    }
}

/// Why `song` is skipped, if it is. The white list beats every rule;
/// artists on the Taiwan, Hong Kong, Macau, Malaysia and Singapore list, and
/// songs with kana in the artist line, are never skipped for a title.
/// Japanese virtual singers can be kept: by the Japanese list, or kana in the
/// artist line.
pub fn judge(song: &Song, lists: &Lists, rules: &Rules) -> Option<Reason> {
    let artists = parts(&song.artist);
    if artists.iter().any(|(k, _)| rules.white.contains(k)) {
        return None;
    }
    if rules.black_enabled
        && let Some((_, name)) = artists.iter().find(|(k, _)| rules.black.contains(k))
    {
        return Some(Reason::BlackList(name.clone()));
    }
    if rules.author_enabled
        && let Some((_, name)) = artists.iter().find(|(k, _)| rules.author.contains(k))
    {
        return Some(Reason::AuthorList(name.clone()));
    }
    if rules.artists
        && let Some((_, name)) = artists.iter().find(|(k, _)| lists.china.contains(k))
    {
        return Some(Reason::Artist(name.clone()));
    }
    if rules.ai
        && let Some((key, name)) = artists.iter().find(|(k, _)| lists.ai.contains(k))
        && !(rules.ai_keep_japan
            && (lists.ai_japan.contains(key) || song.artist.chars().any(is_kana)))
    {
        return Some(Reason::Virtual(name.clone()));
    }
    if !rules.titles
        || artists.iter().any(|(k, _)| lists.elsewhere.contains(k))
        || song.artist.chars().any(is_kana)
    {
        return None;
    }
    if simplified(&song.title) {
        Some(Reason::SimplifiedTitle)
    } else if simplified(&song.album) && !song.title.chars().any(is_kana) {
        Some(Reason::SimplifiedAlbum)
    } else {
        None
    }
}

// ------------------------------------------------------------------ files

fn data_file(name: &str) -> Option<PathBuf> {
    crate::config::data_dir().map(|d| d.join(name))
}

const LISTS_FILE: &str = "china-artists.json";
const SKIPPED_FILE: &str = "skipped.txt";

/// AI and virtual singers built into KKCUT, so their switches work before
/// any fetch: one name a line, "ja" after a tab for the Japanese ones.
const BUILT_IN_VIRTUAL: &str = include_str!("kkbox-virtual.txt");

/// Chinese singers Wikipedia also files under another place (海鸣威 is in
/// 香港男歌手), so a Refresh leaves them off the China list.
const CHINA_TOO: &[&str] = &["海鸣威", "张杰"];

fn add_built_in(lists: &mut Lists) {
    for name in CHINA_TOO {
        let key = name_key(name);
        lists.elsewhere.remove(&key);
        lists.china.insert(key);
    }
    for line in BUILT_IN_VIRTUAL.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, mark) = line.split_once('\t').unwrap_or((line, ""));
        let key = name_key(name);
        if mark == "ja" {
            lists.ai_japan.insert(key.clone());
        }
        lists.ai.insert(key);
    }
}

/// The lists in use: the last fetch's, if any, with the built-in virtual
/// singers. Names are keyed as they load, so a change to `name_key` needs no
/// fetch.
pub fn current() -> Lists {
    let mut lists = load().unwrap_or_default();
    add_built_in(&mut lists);
    lists
}

/// The lists saved by the last fetch, if any.
fn load() -> Option<Lists> {
    let text = std::fs::read_to_string(data_file(LISTS_FILE)?).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let set = |key: &str| -> HashSet<String> {
        json[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .map(name_key)
            .collect()
    };
    Some(Lists {
        china: set("china"),
        elsewhere: set("elsewhere"),
        ai: set("ai"),
        ai_japan: set("ai_japan"),
        fetched: json["fetched"].as_u64().unwrap_or(0),
        categories: json["categories"].as_u64().unwrap_or(0) as usize,
    })
}

/// Saves the names in both script forms.
fn save(
    china: &HashSet<String>,
    elsewhere: &HashSet<String>,
    both: &[String],
    ai: &HashSet<String>,
    ai_japan: &HashSet<String>,
    categories: usize,
) -> Result<(), String> {
    fn sorted(set: &HashSet<String>) -> Vec<&String> {
        let mut v: Vec<&String> = set.iter().collect();
        v.sort();
        v
    }
    let json = serde_json::json!({
        "fetched": now(),
        "categories": categories,
        "china": sorted(china),
        "elsewhere": sorted(elsewhere),
        // Also in an other-place category, and kept on the China list: for review.
        "both": both,
        "ai": sorted(ai),
        "ai_japan": sorted(ai_japan),
    });
    let path = data_file(LISTS_FILE).ok_or("找不到 APPDATA 資料夾")?;
    std::fs::write(path, json.to_string()).map_err(|e| e.to_string())
}

/// A song KKCUT skipped.
#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    pub when: u64,
    pub song: Song,
    /// Why, and what KKBOX did, for people.
    pub reason: String,
    /// The artist that matched, as KKBOX shows it; empty for a title.
    pub name: String,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn clean(text: &str) -> String {
    text.replace(['\t', '\r', '\n'], " ")
}

fn parse_skipped(text: &str) -> Vec<Skipped> {
    text.lines()
        .rev()
        .filter_map(|line| {
            let mut f = line.split('\t');
            Some(Skipped {
                when: f.next()?.parse().ok()?,
                song: Song {
                    title: f.next()?.to_string(),
                    artist: f.next()?.to_string(),
                    album: f.next()?.to_string(),
                },
                reason: f.next()?.to_string(),
                name: f.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

fn skipped_line(s: &Skipped) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}",
        s.when,
        clean(&s.song.title),
        clean(&s.song.artist),
        clean(&s.song.album),
        clean(&s.reason),
        clean(&s.name)
    )
}

/// Serializes changes to the skipped file (the guard adds, the window deletes).
static SKIPPED_LOCK: Mutex<()> = Mutex::new(());

/// The songs skipped last, newest first.
pub fn skipped() -> Vec<Skipped> {
    let _lock = SKIPPED_LOCK.lock().unwrap();
    data_file(SKIPPED_FILE)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| parse_skipped(&t))
        .unwrap_or_default()
}

/// Rewrites the file with `list` (newest first), at most `keep` songs.
fn write_skipped(list: &[Skipped], keep: usize) {
    let Some(path) = data_file(SKIPPED_FILE) else {
        return;
    };
    let lines: Vec<String> = list.iter().take(keep).rev().map(skipped_line).collect();
    let text = if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    };
    let _ = std::fs::write(path, text);
}

/// Adds a record. With `amend`, a pause right after a skip replaces that
/// skip's record of the same song instead of adding a second one.
fn record(entry: Skipped, keep: usize, amend: bool) {
    let _lock = SKIPPED_LOCK.lock().unwrap();
    let path = data_file(SKIPPED_FILE);
    let old = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let mut list = parse_skipped(&old);
    if amend && list.first().is_some_and(|s| s.song == entry.song) {
        list[0] = entry;
    } else {
        list.insert(0, entry);
    }
    write_skipped(&list, keep);
}

/// Deletes one record, found by its time and title.
pub fn delete_skipped(when: u64, title: &str) {
    let _lock = SKIPPED_LOCK.lock().unwrap();
    let Some(text) = data_file(SKIPPED_FILE).and_then(|p| std::fs::read_to_string(p).ok()) else {
        return;
    };
    let mut list = parse_skipped(&text);
    let before = list.len();
    let mut gone = false;
    list.retain(|s| {
        let hit = !gone && s.when == when && s.song.title == title;
        gone |= hit;
        !hit
    });
    if list.len() != before {
        write_skipped(&list, usize::MAX);
    }
}

// ------------------------------------------------------------------ Wikipedia

/// Chinese Wikipedia categories of singers and bands from China. Their
/// subcategories are followed, except the ones about other places.
const CHINA: &[&str] = &[
    "中国歌手",
    "中国男歌手",
    "中國女歌手",
    "中国摇滚乐队",
    "中国重金屬樂團",
    "中國演唱團體",
    "中国女子演唱团体",
    "中国说唱团体",
    "中國樂團",
    "中國創作歌手",
    "中国嘻哈音乐家",
    "中国民谣歌手",
];

/// Categories of the places left out. Their singers are never skipped for a
/// title; one also in a China category stays on the China list.
const ELSEWHERE: &[&str] = &[
    "台灣歌手",
    "台灣男歌手",
    "台灣女歌手",
    "台灣樂團",
    "香港歌手",
    "香港男歌手",
    "香港女歌手",
    "香港樂團",
    "香港演唱團體",
    "澳門歌手",
    "馬來西亞歌手",
    "新加坡歌手",
    "新加坡男歌手",
    "新加坡女歌手",
];

/// Subcategories about the places left out, by name.
const PLACES: &[&str] = &[
    "台灣",
    "台湾",
    "臺灣",
    "臺湾",
    "香港",
    "澳門",
    "澳门",
    "馬來西亞",
    "马来西亚",
    "新加坡",
];

/// Subcategories that hold songs, albums, shows or templates, not people, and
/// record labels' artists (金牌大風藝人), who come from anywhere: a Taiwan
/// label made 许巍 look Taiwanese.
const NOT_PEOPLE: &[&str] = &[
    "模板", "節目", "节目", "命名", "作品", "歌曲", "專輯", "专辑", "電影", "电影", "選秀", "选秀",
    "歌妓", "藝人", "艺人",
];

/// AI and virtual singers: Vocaloid characters and virtual idols of China,
/// Taiwan, Japan and Korea (subcategories included).
const VIRTUAL: &[&str] = &["虚拟歌手"];
const VIRTUAL_DEPTH: usize = 2;
/// Pages in those categories that are about the idea, not a singer.
const NOT_SINGERS: &[&str] = &["虛擬歌手", "虚拟歌手", "AR performers", "VOCALOID衍生角色"];

/// How deep subcategories are followed. The places left out have large
/// trees (by decade, by genre), and their main categories hold most names.
const CHINA_DEPTH: usize = 4;
const ELSEWHERE_DEPTH: usize = 2;
/// A stop for a category tree gone wrong.
const MAX_CATEGORIES: usize = 1200;

fn percent(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out += &format!("%{b:02X}");
        }
    }
    out
}

/// Wikimedia asks for one request at a time, and a pause when told to slow
/// down. About one request a second keeps a fetch to a few minutes.
fn api(query: &str) -> Result<serde_json::Value, String> {
    let url = format!("https://zh.wikipedia.org/w/api.php?{query}");
    let mut wait = Duration::from_secs(15);
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(900));
        match crate::net::get(
            &url,
            crate::net::Get {
                limit: 4 << 20,
                timeout: Duration::from_secs(30),
                redirects: true,
            },
        ) {
            Ok(body) => return serde_json::from_slice(&body).map_err(|e| e.to_string()),
            Err(e) if e.contains("HTTP 429") || e.contains("HTTP 503") => {
                std::thread::sleep(wait);
                wait *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    Err("維基百科要求放慢速度，請稍後再試".into())
}

/// Page titles and subcategories of one category.
fn members(category: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let (mut pages, mut subcats) = (Vec::new(), Vec::new());
    let mut next = String::new();
    loop {
        let mut query = format!(
            "action=query&format=json&formatversion=2&list=categorymembers&cmlimit=500\
             &cmnamespace=0%7C14&cmprop=title%7Cns&cmtitle={}",
            percent(&format!("Category:{category}"))
        );
        if !next.is_empty() {
            query += &format!("&cmcontinue={}", percent(&next));
        }
        let json = api(&query)?;
        for m in json["query"]["categorymembers"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(title) = m["title"].as_str() else {
                continue;
            };
            match m["ns"].as_u64() {
                Some(0) => pages.push(title.to_string()),
                Some(14) => {
                    let name = title.split_once(':').map_or(title, |(_, n)| n);
                    subcats.push(name.to_string());
                }
                _ => {}
            }
        }
        match json["continue"]["cmcontinue"].as_str() {
            Some(c) => next = c.to_string(),
            None => return Ok((pages, subcats)),
        }
    }
}

/// Every page under `roots`. Subcategories about the places left out go to
/// `other` instead when given.
fn crawl(
    roots: &[&str],
    mut other: Option<&mut Vec<String>>,
    max_depth: usize,
    seen: &mut HashSet<String>,
    progress: &dyn Fn(usize),
) -> Result<Vec<String>, String> {
    let mut queue: Vec<(String, usize)> = roots.iter().map(|r| (r.to_string(), 0)).collect();
    let mut pages = Vec::new();
    while let Some((category, depth)) = queue.pop() {
        if !seen.insert(category.clone()) {
            continue;
        }
        if seen.len() > MAX_CATEGORIES {
            return Err("維基百科的分類比預期多，已停止".into());
        }
        progress(seen.len());
        let (found, subcats) = members(&category)?;
        pages.extend(found);
        for sub in subcats {
            if NOT_PEOPLE.iter().any(|w| sub.contains(w)) {
                continue;
            }
            match other.as_deref_mut() {
                Some(other) if PLACES.iter().any(|p| sub.contains(p)) => other.push(sub),
                _ if depth < max_depth => queue.push((sub, depth + 1)),
                _ => {}
            }
        }
    }
    Ok(pages)
}

/// A page's titles on the Japanese and English Wikipedias.
#[derive(Default)]
struct Titles {
    ja: Option<String>,
    en: Option<String>,
}

/// The Japanese and English titles of Chinese Wikipedia pages, by page:
/// KKBOX shows Vocaloid singers by their Japanese names (初音ミク, not 初音未來).
fn other_titles(titles: &[String]) -> Result<HashMap<String, Titles>, String> {
    let mut out: HashMap<String, Titles> = HashMap::new();
    for chunk in titles.chunks(50) {
        let list = percent(&chunk.join("|"));
        for lang in ["ja", "en"] {
            let json = api(&format!(
                "action=query&format=json&formatversion=2&prop=langlinks&lllimit=max\
                 &lllang={lang}&titles={list}"
            ))?;
            for page in json["query"]["pages"].as_array().into_iter().flatten() {
                let Some(title) = page["title"].as_str() else {
                    continue;
                };
                for link in page["langlinks"].as_array().into_iter().flatten() {
                    if let Some(t) = link["title"].as_str() {
                        let entry = out.entry(title.to_string()).or_default();
                        match lang {
                            "ja" => entry.ja = Some(t.to_string()),
                            _ => entry.en = Some(t.to_string()),
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// A virtual singer from Japan: the Japanese Wikipedia writes the name in
/// kana or Latin letters (初音ミク, Megpoid, キズナアイ). Chinese ones keep
/// their Chinese characters there (洛天依, 楽正綾).
fn japanese(ja_title: &str) -> bool {
    let name = strip(ja_title);
    name.chars().any(is_kana) || name.is_ascii()
}

/// A virtual singer's name worth matching. One Latin word (Lily, MAYU, MEIKO,
/// KAITO, Oliver) is also a real artist's name, so Latin names need two words
/// or a digit (Hatsune Miku, 22/7).
fn distinct(name: &str) -> bool {
    let name = strip(name);
    !name.is_ascii()
        || name.split_whitespace().count() > 1
        || name.chars().any(|c| c.is_ascii_digit())
}

/// The virtual singer list, and the Japanese ones among them, in both scripts.
fn virtual_singers(
    seen: &mut HashSet<String>,
    progress: &dyn Fn(usize),
) -> Result<(HashSet<String>, HashSet<String>), String> {
    let mut pages = crawl(VIRTUAL, None, VIRTUAL_DEPTH, seen, progress)?;
    pages.retain(|t| !NOT_SINGERS.contains(&t.as_str()));
    pages.sort();
    pages.dedup();
    let others = other_titles(&pages)?;
    let (mut all, mut japan) = (Vec::new(), Vec::new());
    for page in pages {
        let other = others.get(&page);
        let mut names = vec![page];
        names.extend(other.and_then(|o| o.ja.clone()));
        names.extend(other.and_then(|o| o.en.clone()));
        names.retain(|n| distinct(n));
        if other.and_then(|o| o.ja.as_deref()).is_some_and(japanese) {
            japan.extend(names.iter().cloned());
        }
        all.extend(names);
    }
    Ok((both_forms(&all), both_forms(&japan)))
}

/// Both script forms of each name.
fn both_forms(titles: &[String]) -> HashSet<String> {
    let mut out = HashSet::new();
    for title in titles {
        let name = strip(title);
        out.insert(convert(name, false));
        out.insert(convert(name, true));
    }
    out
}

/// A name in both a China and an other-place category stays on `elsewhere`
/// and comes off the China list: most of them are Taiwan and Hong Kong
/// singers who also work in China (任贤齐, 林忆莲, 李玟). Returns those names,
/// simplified and sorted, saved as "both" for review; the Chinese ones go in
/// `CHINA_TOO`.
fn elsewhere_wins(china: &mut HashSet<String>, elsewhere: &HashSet<String>) -> Vec<String> {
    let in_elsewhere: HashSet<String> = elsewhere.iter().map(|n| name_key(n)).collect();
    let mut both: Vec<String> = china
        .iter()
        .filter(|n| in_elsewhere.contains(&name_key(n)))
        .map(|n| convert(n, false))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    both.sort();
    china.retain(|n| !in_elsewhere.contains(&name_key(n)));
    both
}

/// Fetches the lists from Chinese Wikipedia and saves them. `progress` gets
/// the number of categories read so far.
pub fn refresh(progress: &dyn Fn(usize)) -> Result<Lists, String> {
    let mut seen = HashSet::new();
    let mut places = Vec::new();
    let china = crawl(CHINA, Some(&mut places), CHINA_DEPTH, &mut seen, progress)?;
    let roots: Vec<&str> = ELSEWHERE
        .iter()
        .copied()
        .chain(places.iter().map(String::as_str))
        .collect();
    let elsewhere = crawl(&roots, None, ELSEWHERE_DEPTH, &mut seen, progress)?;
    let mut china = both_forms(&china);
    let elsewhere = both_forms(&elsewhere);
    let both = elsewhere_wins(&mut china, &elsewhere);
    if china.is_empty() {
        return Err("維基百科沒有傳回任何歌手".into());
    }
    // Its own crawl: the China one may have read some of these categories
    // already. The count goes on from it.
    let read = seen.len();
    let mut seen_virtual = HashSet::new();
    let (ai, ai_japan) = virtual_singers(&mut seen_virtual, &|n| progress(read + n))?;
    let categories = read + seen_virtual.len();
    save(&china, &elsewhere, &both, &ai, &ai_japan, categories)?;
    let mut lists = load().ok_or_else(|| "名單未能儲存".to_string())?;
    add_built_in(&mut lists);
    Ok(lists)
}

// ------------------------------------------------------------------ media session

/// One COM apartment per thread, joined for the thread's lifetime.
struct Apartment(bool);
impl Apartment {
    fn initialize() -> windows::core::Result<Self> {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            // The caller owns an STA apartment. Use it without uninitializing it.
            return Ok(Self(false));
        }
        result.ok()?; // S_OK and S_FALSE each own one initialization reference.
        Ok(Self(true))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}
thread_local! {
    static APARTMENT: windows::core::Result<Apartment> = Apartment::initialize();
}

/// KKBOX answers through Windows' media service, and a call can go unanswered
/// (KKBOX busy, closing or restarting); no call may hold up the guard or the window.
const ANSWER: Duration = Duration::from_secs(3);
/// The session manager, asked for once per process, can be slow to come.
const MANAGER_ANSWER: Duration = Duration::from_secs(15);

/// Waits for `op` (named `what` in the log) for at most `limit`.
fn wait_for<T: windows::core::RuntimeType + 'static>(
    op: IAsyncOperation<T>,
    what: &str,
    limit: Duration,
) -> windows::core::Result<T> {
    let start = Instant::now();
    while op.Status()? == AsyncStatus::Started {
        if start.elapsed() > limit {
            let _ = op.Cancel();
            crate::log(&format!(
                "KKBOX: no answer to {what} in {} s, cancelled",
                limit.as_secs()
            ));
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_ABORT,
                format!("Windows 媒體控制 {} 秒內沒有回應", limit.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    op.GetResults()
}

/// Waits for a call to KKBOX for at most [`ANSWER`].
fn wait<T: windows::core::RuntimeType + 'static>(
    op: IAsyncOperation<T>,
    what: &str,
) -> windows::core::Result<T> {
    wait_for(op, what, ANSWER)
}

/// One session manager for the process: each new one leaves a thread behind
/// in Windows' media runtime. Asked for every 2 s by the window, that came to
/// thousands of threads, and Windows' media controls stopped answering.
static MANAGER: Mutex<Option<Manager>> = Mutex::new(None);

fn manager() -> windows::core::Result<Manager> {
    APARTMENT.with(|a| a.as_ref().map(|_| ()).map_err(Clone::clone))?;
    let mut cached = MANAGER.lock().unwrap();
    if let Some(m) = cached.as_ref() {
        return Ok(m.clone());
    }
    // The manager outlives the thread that asked for it (the window reads on
    // short-lived threads): the multithreaded apartment is kept for the process.
    unsafe { windows::Win32::System::Com::CoIncrementMTAUsage()? };
    let m = wait_for(Manager::RequestAsync()?, "RequestAsync", MANAGER_ANSWER)?;
    *cached = Some(m.clone());
    Ok(m)
}

fn kkbox_session(manager: &Manager) -> windows::core::Result<Option<Session>> {
    for session in manager.GetSessions()? {
        if session
            .SourceAppUserModelId()?
            .to_string()
            .to_ascii_lowercase()
            .contains(KKBOX)
        {
            return Ok(Some(session));
        }
    }
    Ok(None)
}

fn song(session: &Session) -> windows::core::Result<Song> {
    let p = wait(
        session.TryGetMediaPropertiesAsync()?,
        "TryGetMediaPropertiesAsync",
    )?;
    Ok(Song {
        title: p.Title()?.to_string(),
        artist: p.Artist()?.to_string(),
        album: p.AlbumTitle()?.to_string(),
    })
}

/// What KKBOX plays now: `None` while KKBOX has no session.
pub fn now_playing() -> windows::core::Result<Option<(Song, bool)>> {
    let Some(session) = kkbox_session(&manager()?)? else {
        return Ok(None);
    };
    let playing = session.GetPlaybackInfo()?.PlaybackStatus()? == Status::Playing;
    Ok(Some((song(&session)?, playing)))
}

// ------------------------------------------------------------------ guard

/// KKBOX reports a new song in several notices (title, artist, artwork).
const SETTLE: Duration = Duration::from_millis(400);

struct Setup {
    rules: Rules,
    lists: Arc<Lists>,
    keep: usize,
}

/// The main switch (None = off), and a wake-up for the guard thread.
static GUARD: (Mutex<Option<Setup>>, Condvar) = (Mutex::new(None), Condvar::new());
/// Set by media notices and settings changes; the guard looks at KKBOX again.
static DIRTY: AtomicBool = AtomicBool::new(false);
/// Set when the rules change (a name added to the black list, a switch, a
/// new China list): the playing song is judged under them, and the skips in
/// a row start again from none.
static REJUDGE: AtomicBool = AtomicBool::new(false);

fn poke() {
    DIRTY.store(true, Ordering::Release);
    GUARD.1.notify_all();
}

/// Called on start, on every settings change and after a list fetch.
pub fn configure(config: &Config, lists: Arc<Lists>) {
    let setup = config.enabled.then(|| Setup {
        rules: Rules::of(config),
        lists,
        keep: config.history,
    });
    let mut current = GUARD.0.lock().unwrap();
    let changed = matches!((&*current, &setup), (Some(old), Some(new))
        if old.rules != new.rules || !Arc::ptr_eq(&old.lists, &new.lists));
    *current = setup;
    drop(current);
    if changed {
        REJUDGE.store(true, Ordering::Release);
    }
    poke();
}

/// Starts the guard thread. It sleeps with no timer while the switch is off.
pub fn spawn() {
    std::thread::spawn(guard);
}

/// The registered notices; dropping unregisters them.
struct Watch {
    manager: Manager,
    sessions: i64,
    kkbox: Option<(Session, i64, i64)>,
}

impl Watch {
    fn new() -> windows::core::Result<Self> {
        let manager = manager()?;
        let sessions = manager.SessionsChanged(&TypedEventHandler::new(|_, _| {
            poke();
            Ok(())
        }))?;
        Ok(Self {
            manager,
            sessions,
            kkbox: None,
        })
    }

    /// KKBOX's session, watched; it comes and goes with KKBOX.
    fn follow(&mut self) -> windows::core::Result<Option<Session>> {
        let found = kkbox_session(&self.manager)?;
        if self.kkbox.as_ref().map(|k| &k.0) != found.as_ref() {
            self.unfollow();
            if let Some(s) = &found {
                let properties = s.MediaPropertiesChanged(&TypedEventHandler::new(|_, _| {
                    poke();
                    Ok(())
                }))?;
                let playback = s.PlaybackInfoChanged(&TypedEventHandler::new(|_, _| {
                    poke();
                    Ok(())
                }))?;
                self.kkbox = Some((s.clone(), properties, playback));
            }
        }
        Ok(found)
    }

    fn unfollow(&mut self) {
        if let Some((s, properties, playback)) = self.kkbox.take() {
            let _ = s.RemoveMediaPropertiesChanged(properties);
            let _ = s.RemovePlaybackInfoChanged(playback);
        }
    }

    /// Skips the playing song if a rule says so, or pauses KKBOX when
    /// skipping does not get rid of it. Returns when to look again without
    /// a notice.
    fn check(&mut self, memory: &mut Memory) -> windows::core::Result<Option<Duration>> {
        // Soon after a skip, look every second: KKBOX may bring the same song
        // back with no notice, and shows no song for a second or two between songs.
        let soon = memory
            .skipped
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() < AFTER_SKIP)
            .then_some(Duration::from_secs(1));
        // KKBOX drops its session for a second or so at every song change, so
        // a missing session keeps what was skipped: with Repeat one, the same
        // song comes back in a new session.
        let Some(session) = self.follow()? else {
            return Ok(soon);
        };
        if session.GetPlaybackInfo()?.PlaybackStatus()? != Status::Playing {
            return Ok(soon);
        }
        // While KKBOX plays, a light look now and then in case a notice is missed.
        let soon = soon.or(Some(PLAYING_LOOK));
        let song = song(&session)?;
        if song.title.is_empty() {
            return Ok(soon);
        }
        let (reason, keep) = {
            let setup = GUARD.0.lock().unwrap();
            let Some(setup) = setup.as_ref() else {
                return Ok(None);
            };
            (judge(&song, &setup.lists, &setup.rules), setup.keep)
        };
        let Some(reason) = reason else {
            *memory = Memory::default();
            return Ok(soon);
        };
        let back = match &memory.skipped {
            Some((s, at)) if *s == song && at.elapsed() < REMEMBER => {
                // KKBOX may still show the skipped song while the next one loads.
                if at.elapsed() < GRACE {
                    return Ok(Some(GRACE - at.elapsed()));
                }
                true
            }
            _ => false,
        };
        let why = if back {
            Some("切歌後又回到這首（只剩這首可播，或單曲循環）".to_string())
        } else if memory.run >= MAX_RUN {
            Some(format!("連續切了 {MAX_RUN} 首，剩下的歌都會被切"))
        } else if !wait(session.TrySkipNextAsync()?, "TrySkipNextAsync")? {
            Some("KKBOX 沒有切歌".to_string())
        } else {
            None
        };
        let name = reason.name().unwrap_or_default().to_string();
        let entry = |text: String| Skipped {
            when: now(),
            song: song.clone(),
            reason: text,
            name: name.clone(),
        };
        match why {
            None => {
                record(entry(reason.to_string()), keep, false);
                memory.skipped = Some((song, Instant::now()));
                memory.run += 1;
                Ok(Some(Duration::from_secs(1)))
            }
            Some(why) => {
                let paused = match wait(session.TryPauseAsync()?, "TryPauseAsync") {
                    Ok(true) => "已暫停 KKBOX",
                    _ => "KKBOX 也沒有暫停",
                };
                // A song skipping brought back replaces its own skip record.
                record(entry(format!("{reason}；{why}，{paused}")), keep, back);
                *memory = Memory::default();
                Ok(None)
            }
        }
    }
}

/// What the guard remembers between looks at KKBOX.
#[derive(Default)]
struct Memory {
    /// The song skipped last, and when. Still playing after [`GRACE`], it is
    /// paused instead.
    skipped: Option<(Song, Instant)>,
    /// Songs skipped in a row with nothing allowed in between.
    run: u32,
}

/// KKBOX's time to start the next song after a skip.
const GRACE: Duration = Duration::from_secs(3);
/// How long after a skip the guard keeps looking every second.
const AFTER_SKIP: Duration = Duration::from_secs(8);
/// A skipped song seen again later than this (KKBOX closed and opened
/// again, say) is skipped as a new one, not paused.
const REMEMBER: Duration = Duration::from_secs(30);
/// Skips in a row before KKCUT pauses KKBOX: every song left is skipped.
const MAX_RUN: u32 = 8;
/// While KKBOX plays: one look at its song this often, without a notice.
const PLAYING_LOOK: Duration = Duration::from_secs(5);
/// When Windows' media controls could not be watched: try again this often.
const WATCH_RETRY: Duration = Duration::from_secs(30);

impl Drop for Watch {
    fn drop(&mut self) {
        self.unfollow();
        let _ = self.manager.RemoveSessionsChanged(self.sessions);
    }
}

fn guard() {
    let mut watch: Option<Watch> = None;
    let mut memory = Memory::default();
    // When to look at KKBOX again without a notice.
    let mut look: Option<Instant> = None;
    loop {
        let on = {
            let mut setup = GUARD.0.lock().unwrap();
            while !DIRTY.load(Ordering::Acquire) {
                match look {
                    None => setup = GUARD.1.wait(setup).unwrap(),
                    Some(at) => {
                        let now = Instant::now();
                        if now >= at {
                            break;
                        }
                        setup = GUARD.1.wait_timeout(setup, at - now).unwrap().0;
                    }
                }
            }
            setup.is_some()
        };
        look = None;
        if !on {
            watch = None;
            memory = Memory::default();
            DIRTY.store(false, Ordering::Release);
            continue;
        }
        if watch.is_none() {
            watch = match Watch::new() {
                Ok(w) => Some(w),
                Err(e) => {
                    crate::log(&format!("KKBOX not watched: {e}"));
                    None
                }
            };
        }
        while DIRTY.swap(false, Ordering::AcqRel) {
            std::thread::sleep(SETTLE);
        }
        if REJUDGE.swap(false, Ordering::AcqRel) {
            memory.run = 0;
        }
        let Some(w) = watch.as_mut() else {
            look = Some(Instant::now() + WATCH_RETRY);
            continue;
        };
        match w.check(&mut memory) {
            Ok(next) => look = next.map(|d| Instant::now() + d),
            Err(e) => {
                crate::log(&format!("KKBOX song not checked: {e}"));
                // A call KKBOX left unanswered: try again shortly.
                look = Some(Instant::now() + PLAYING_LOOK);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(title: &str, artist: &str, album: &str) -> Song {
        Song {
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
        }
    }

    fn keys(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| name_key(n)).collect()
    }

    fn lists() -> Lists {
        Lists {
            china: keys(&["华晨宇", "李健 (歌手)", "新裤子"]),
            // As Wikipedia titles them: 林俊杰 in simplified, 王菲 in either.
            elsewhere: keys(&["林俊杰", "王菲"]),
            ..Default::default()
        }
    }

    fn rules() -> Rules {
        Rules {
            artists: true,
            titles: true,
            black_enabled: true,
            author_enabled: true,
            ai: true,
            ai_keep_japan: false,
            white: HashSet::new(),
            black: HashSet::new(),
            author: AUTHOR_BLACK.iter().flat_map(|a| names(a)).collect(),
        }
    }

    #[test]
    fn name_keys_fold_what_windows_misses() {
        assert_eq!(name_key("鍾漢良"), name_key("钟汉良"));
        assert_eq!(name_key("林俊傑"), name_key("林俊杰"));
        for (a, b) in [
            ("張遠", "张远"),
            ("以後", "以后"),
            ("峯", "峰"),
            ("爲你", "为你"),
            ("昇", "升"),
        ] {
            assert_eq!(name_key(a), name_key(b), "{a} {b}");
        }
        // Different names stay apart: 肖 and 蕭 are two surnames, and 祢 is not 你.
        assert_ne!(name_key("肖战"), name_key("蕭戰"));
        assert_ne!(name_key("祢知道"), name_key("你知道"));
        for c in ['祢', '吒', '鍊', '鉅', '囉', '肖', '蕭'] {
            assert!(FOLD.iter().all(|(from, to)| *from != c && *to != c), "{c}");
        }
    }

    #[test]
    fn virtual_singers_are_skipped_by_their_switch() {
        let l = Lists {
            ai: keys(&["初音ミク", "洛天依", "Hatsune Miku"]),
            ..lists()
        };
        let r = rules();
        assert_eq!(
            judge(&song("千本桜", "黒うさP feat. 初音ミク", ""), &l, &r),
            Some(Reason::Virtual("初音ミク".into()))
        );
        assert_eq!(
            Reason::Virtual("洛天依".into()).to_string(),
            "虛擬歌手：洛天依"
        );
        // 洛天依 in traditional matches too.
        assert_eq!(
            judge(&song("普通DISCO", "洛天依, 言和", ""), &l, &r),
            Some(Reason::Virtual("洛天依".into()))
        );
        assert_eq!(
            judge(&song("World is Mine", "HATSUNE MIKU", ""), &l, &r),
            Some(Reason::Virtual("HATSUNE MIKU".into()))
        );
        let off = Rules {
            ai: false,
            ..rules()
        };
        assert_eq!(judge(&song("千本桜", "初音ミク", ""), &l, &off), None);
        // The China list is named first.
        assert_eq!(
            judge(&song("x", "华晨宇, 洛天依", ""), &l, &r),
            Some(Reason::Artist("华晨宇".into()))
        );
        // Japanese virtual singers can be kept: by the Japanese list, or kana
        // in the artist line.
        let l = Lists {
            ai_japan: keys(&["初音ミク", "Hatsune Miku"]),
            ..l
        };
        let keep = Rules {
            ai_keep_japan: true,
            ..rules()
        };
        assert_eq!(
            judge(&song("World is Mine", "Hatsune Miku", ""), &l, &keep),
            None
        );
        assert_eq!(
            judge(&song("千本桜", "黒うさP feat. 初音ミク", ""), &l, &keep),
            None
        );
        assert_eq!(
            judge(&song("普通DISCO", "洛天依", ""), &l, &keep),
            Some(Reason::Virtual("洛天依".into()))
        );
        assert_eq!(
            judge(&song("x", "洛天依 feat. ヨルシカ", ""), &l, &keep),
            None
        );
        assert!(japanese("初音ミク") && japanese("Megpoid") && japanese("キズナアイ"));
        assert!(!japanese("洛天依") && !japanese("楽正綾"));
        // The white list and the black list still come first.
        let allowed = Rules {
            white: names("初音ミク").into_iter().collect(),
            ..rules()
        };
        assert_eq!(judge(&song("千本桜", "初音ミク", ""), &l, &allowed), None);
        let blocked = Rules {
            black: names("初音ミク").into_iter().collect(),
            ..keep
        };
        assert_eq!(
            judge(&song("千本桜", "初音ミク", ""), &l, &blocked),
            Some(Reason::BlackList("初音ミク".into()))
        );
    }

    #[test]
    fn built_in_virtual_singers_need_no_refresh() {
        let names_in_file = BUILT_IN_VIRTUAL
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .count();
        assert_eq!(names_in_file, 117 + 24);
        let mut l = Lists::default();
        add_built_in(&mut l);
        for (name, japanese) in [
            ("洛天依", false),
            ("樂正綾", false),
            ("初音ミク", true),
            ("初音未來", true),
        ] {
            assert!(l.ai.contains(&name_key(name)), "{name}");
            assert_eq!(l.ai_japan.contains(&name_key(name)), japanese, "{name}");
        }
        for name in ["lily", "mayu", "meiko", "kaito", "oliver"] {
            assert!(!l.ai.contains(name), "{name}");
        }
        // Added by hand, matched as KKBOX shows them, in either script; none Japanese.
        for name in [
            "Breaking Rust",
            "Cain Walker",
            "蘇娜",
            "石川小町",
            "大頭針Official",
            "Yuri尤栗",
            "登叔",
            "六六",
            "幽嵐",
            "Meomuru머무르",
            "머무르",
            "Sienna Rose",
            "滄桑小傑",
            "荼鳶",
            "徐愛顏",
            "陸思川",
            "方念",
            "摯彬同學",
            "海莉",
            "雲宇光",
            "幻神·米絲",
            "嚴清語",
            "薔蕪",
            "輝宇·星",
        ] {
            assert!(names(name).iter().any(|k| l.ai.contains(k)), "{name}");
            assert!(
                !names(name).iter().any(|k| l.ai_japan.contains(k)),
                "{name}"
            );
        }
        // The built-in names join a fetched list.
        let mut fetched = Lists {
            ai: keys(&["新虚拟歌手"]),
            fetched: 1,
            ..Default::default()
        };
        add_built_in(&mut fetched);
        assert!(fetched.ai.contains(&name_key("新虚拟歌手")));
        assert!(fetched.ai.contains(&name_key("洛天依")));
    }

    #[test]
    fn only_distinct_virtual_names_are_kept() {
        assert!(distinct("初音ミク"));
        assert!(distinct("洛天依"));
        assert!(distinct("Hatsune Miku"));
        assert!(distinct("22/7 (企劃)"));
        // One Latin word is also a real artist's name.
        assert!(!distinct("Lily (角色)"));
        assert!(!distinct("MAYU"));
        assert!(!distinct("MEIKO"));
        assert!(!distinct("KAITO"));
        assert!(!distinct("Oliver (软件)"));
    }

    #[test]
    fn a_singer_in_china_and_elsewhere_stays_on_the_other_list() {
        let mut china = both_forms(&["海鸣威".into(), "任贤齐".into(), "华晨宇".into()]);
        let elsewhere = both_forms(&["海鳴威".into(), "任賢齊".into(), "周杰倫".into()]);
        let both = elsewhere_wins(&mut china, &elsewhere);
        assert_eq!(both, ["任贤齐", "海鸣威"]);
        let l = Lists {
            china: china.iter().map(|n| name_key(n)).collect(),
            elsewhere: elsewhere.iter().map(|n| name_key(n)).collect(),
            ..Default::default()
        };
        assert!(!l.china.contains(&name_key("任賢齊")));
        assert!(l.elsewhere.contains(&name_key("任賢齊")));
        assert!(l.china.contains(&name_key("華晨宇")));
        assert_eq!(judge(&song("心太软", "任贤齐", ""), &l, &rules()), None);
    }

    #[test]
    fn built_in_chinese_singers_stay_on_the_china_list() {
        let mut l = Lists {
            elsewhere: keys(&["海鸣威", "张杰", "任贤齐"]),
            fetched: 1,
            ..Default::default()
        };
        add_built_in(&mut l);
        for name in ["海鳴威", "張杰", "張傑"] {
            assert!(l.china.contains(&name_key(name)), "{name}");
            assert!(!l.elsewhere.contains(&name_key(name)), "{name}");
        }
        assert!(l.elsewhere.contains(&name_key("任賢齊")));
        assert_eq!(
            judge(&song("x", "海鸣威 (Haiming Wei)", ""), &l, &rules()),
            Some(Reason::Artist("海鸣威".into()))
        );
    }

    #[test]
    fn label_categories_are_not_followed() {
        for label in ["金牌大風藝人", "华研国际音乐艺人"] {
            assert!(NOT_PEOPLE.iter().any(|w| label.contains(w)), "{label}");
        }
        assert!(!NOT_PEOPLE.iter().any(|w| "香港男歌手".contains(w)));
    }

    #[test]
    fn simplified_chinese_is_told_from_traditional_and_japanese() {
        assert!(simplified("说爱你"));
        assert!(simplified("我们的歌"));
        assert!(simplified("Love 这首歌"));
        // Traditional, or characters both scripts use.
        assert!(!simplified("世界末日"));
        assert!(!simplified("周杰倫"));
        assert!(!simplified("范特西"));
        assert!(!simplified("台北的天空"));
        assert!(!simplified("你的答案"));
        // A traditional-only character wins over a simplified one.
        assert!(!simplified("說爱你"));
        // Japanese: kana, or kanji shared with simplified Chinese.
        assert!(!simplified("国境の南"));
        assert!(!simplified("残響散歌"));
        assert!(!simplified("学園"));
        assert!(simplified("学园"));
        assert!(!simplified("Lemon"));
    }

    #[test]
    fn names_are_split_and_stripped_like_kkbox_shows_them() {
        assert_eq!(names("周杰倫 (Jay Chou)"), ["周杰伦"]);
        assert_eq!(names("李健（歌手）"), ["李健"]);
        assert_eq!(
            names("華晨宇 (Hua Chenyu)、GAI & Higher Brothers feat. Tizzy T"),
            ["华晨宇", "gai", "higher brothers", "tizzy t"]
        );
        assert_eq!(names("(G)I-DLE"), ["(g)i-dle"]);
        // A long line with aliases: the last artist is still a name of its own.
        assert!(
            names(
                "言承旭 (Jerry Yan), 吳建豪 (Van Ness Wu), 周渝民 (Vic Chou), 五月天 阿信 (Ashin)"
            )
            .contains(&"五月天".to_string())
        );
        // Chinese names have no spaces: space-separated Chinese words are names too.
        assert_eq!(
            names("周渝民 (Vic Chou), 五月天 阿信 (Ashin)"),
            ["周渝民", "五月天 阿信", "五月天", "阿信"]
        );
        // ... but not the words of a Latin name, or a word of one character.
        assert_eq!(names("Jansley Lee"), ["jansley lee"]);
        assert_eq!(names("林 宥嘉"), ["林 宥嘉"]);
        // A Latin alias with no bracket; the Chinese part is matched too.
        assert_eq!(
            names("Gareth.T, 攬佬SKAI ISYOURGOD"),
            ["gareth.t", "揽佬skai isyourgod", "揽佬"]
        );
        assert_eq!(
            names("宇宙人 Cosmos People"),
            ["宇宙人 cosmos people", "宇宙人"]
        );
        // One character, or no Latin after it, is not split.
        assert_eq!(names("林A"), ["林a"]);
        assert_eq!(names("五月天阿信"), ["五月天阿信"]);
        // The middle dot is left out, written or not.
        assert_eq!(names("希林娜依·高 (Curley G)"), ["希林娜依高"]);
        assert_eq!(names("希林娜依高"), ["希林娜依高"]);
        assert!(names("").is_empty());
    }

    #[test]
    fn every_joint_kkbox_uses_splits_the_artist_line() {
        for joint in [
            " feat. ",
            " Feat. ",
            " ft. ",
            " featuring ",
            " x ",
            " X ",
            "|",
            "、",
            ",",
            "，",
            "&",
            "＆",
            "/",
            "／",
            ";",
            "；",
            "×",
        ] {
            assert_eq!(names(&format!("A{joint}B")), ["a", "b"], "{joint:?}");
        }
        // A trailing alias in either bracket is dropped.
        assert_eq!(names("周杰倫（Jay Chou）"), ["周杰伦"]);
        assert_eq!(names("章昊 (ZHANG HAO)"), ["章昊"]);
        // Both scripts compare as one name.
        assert_eq!(names("林俊傑"), names("林俊杰"));
        assert_eq!(names("周杰倫"), names("周杰伦"));
        // A word that only looks like a joint is not one.
        assert_eq!(names("Xavier"), ["xavier"]);
    }

    #[test]
    fn both_script_forms_are_listed() {
        let forms = both_forms(&["华晨宇".into(), "逃跑计划".into()]);
        for name in ["华晨宇", "華晨宇", "逃跑计划", "逃跑計劃"] {
            assert!(forms.contains(name), "{name}");
        }
    }

    #[test]
    fn songs_are_skipped_for_the_artist_or_a_simplified_title() {
        let (l, r) = (lists(), rules());
        // The reason names the artist as KKBOX shows it.
        assert_eq!(
            judge(
                &song("煙火裡的塵埃", "華晨宇 (Hua Chenyu)", "卡西莫多的禮物"),
                &l,
                &r
            ),
            Some(Reason::Artist("華晨宇".into()))
        );
        assert_eq!(
            judge(&song("Lemon", "Mike、李健", ""), &l, &r),
            Some(Reason::Artist("李健".into()))
        );
        assert_eq!(
            judge(&song("说爱你", "某网络歌手", ""), &l, &r),
            Some(Reason::SimplifiedTitle)
        );
        assert_eq!(
            judge(&song("你的答案", "阿冗", "你的答案 (热门版)"), &l, &r),
            Some(Reason::SimplifiedAlbum)
        );
        assert_eq!(judge(&song("世界末日", "周杰倫", "范特西"), &l, &r), None);
        let l = Lists {
            china: keys(&["攬佬"]),
            elsewhere: keys(&["Gareth.T"]),
            ..Default::default()
        };
        assert_eq!(
            judge(
                &song(
                    "跟悲傷結了帳",
                    "Gareth.T, 攬佬SKAI ISYOURGOD",
                    "跟悲傷結了帳"
                ),
                &l,
                &r
            ),
            Some(Reason::Artist("攬佬".into()))
        );
    }

    #[test]
    fn allowed_singers_and_other_places_are_spared() {
        let l = lists();
        // Singapore and Hong Kong singers keep simplified titles.
        assert_eq!(
            judge(&song("修炼爱情", "林俊杰 (JJ Lin)", ""), &l, &rules()),
            None
        );
        // KKBOX writes 林俊傑; Windows keeps 杰 when converting to traditional.
        assert_eq!(
            judge(&song("修炼爱情", "林俊傑 (JJ Lin)", ""), &l, &rules()),
            None
        );
        assert_eq!(judge(&song("红豆", "王菲", ""), &l, &rules()), None);
        // Japanese artists keep titles with shared kanji.
        assert_eq!(judge(&song("说爱你", "ヨルシカ", ""), &l, &rules()), None);
        // The white list beats every rule.
        let allowed = Rules {
            white: names("李健 (Li Jian)").into_iter().collect(),
            ..rules()
        };
        assert_eq!(judge(&song("贝加尔湖畔", "李健", ""), &l, &allowed), None);
        assert!(allowed.allowed("Mike、李健"));
        // The black list skips whatever the other switches say, in either script.
        let blocked = Rules {
            artists: false,
            titles: false,
            black: names("宇宙人").into_iter().collect(),
            ..rules()
        };
        assert_eq!(
            judge(
                &song("沒有人像我一樣", "宇宙人 (Cosmos People)", ""),
                &l,
                &blocked
            ),
            Some(Reason::BlackList("宇宙人".into()))
        );
        // ... unless its own switch is off.
        let black_off = Rules {
            black_enabled: false,
            ..blocked.clone()
        };
        assert_eq!(
            judge(&song("沒有人像我一樣", "宇宙人", ""), &l, &black_off),
            None
        );
        let both = Rules {
            white: blocked.black.clone(),
            ..blocked
        };
        assert_eq!(
            judge(&song("沒有人像我一樣", "宇宙人", ""), &l, &both),
            None
        );
        // Each rule can be switched off.
        let none = Rules {
            artists: false,
            titles: false,
            ..rules()
        };
        assert_eq!(judge(&song("说爱你", "华晨宇", ""), &l, &none), None);
    }

    #[test]
    fn the_author_list_has_its_own_switch_and_yields_to_the_white_list() {
        let l = Lists::default();
        // KKBOX writes 張遠 or 张远; the list holds 張遠.
        assert_eq!(
            judge(&song("嘉賓", "张远", ""), &l, &rules()),
            Some(Reason::AuthorList("张远".into()))
        );
        assert_eq!(
            judge(&song("x", "郁可唯", ""), &l, &rules()),
            Some(Reason::AuthorList("郁可唯".into()))
        );
        assert_eq!(
            judge(&song("x", "雾明", ""), &l, &rules()),
            Some(Reason::AuthorList("雾明".into()))
        );
        assert_eq!(
            judge(
                &song("我不想念", "希林娜依·高 (Curley G)", ""),
                &l,
                &rules()
            ),
            Some(Reason::AuthorList("希林娜依·高".into()))
        );
        assert_eq!(
            judge(&song("x", "格雷樂團-Grey", ""), &l, &rules()),
            Some(Reason::AuthorList("格雷樂團-Grey".into()))
        );
        assert_eq!(
            judge(&song("x", "陈敏萱", ""), &l, &rules()),
            Some(Reason::AuthorList("陈敏萱".into()))
        );
        assert_eq!(
            judge(&song("x", "JANSLEY LEE", ""), &l, &rules()),
            Some(Reason::AuthorList("JANSLEY LEE".into()))
        );
        assert_eq!(
            judge(&song("x", "章昊", ""), &l, &rules()),
            Some(Reason::AuthorList("章昊".into()))
        );
        // A name of the list inside a longer one is not that name.
        assert_eq!(judge(&song("x", "薇薇安", ""), &l, &rules()), None);
        let off = Rules {
            author_enabled: false,
            ..rules()
        };
        assert_eq!(judge(&song("x", "張遠", ""), &l, &off), None);
        let allowed = Rules {
            white: names("張遠").into_iter().collect(),
            ..rules()
        };
        assert_eq!(judge(&song("x", "張遠", ""), &l, &allowed), None);
        // The user's own black list is named first.
        let both = Rules {
            black: names("張遠").into_iter().collect(),
            ..rules()
        };
        assert_eq!(
            judge(&song("x", "張遠", ""), &l, &both),
            Some(Reason::BlackList("張遠".into()))
        );
    }

    #[test]
    fn skipped_lines_round_trip() {
        let line = clean("a\tb\nc");
        assert_eq!(line, "a b c");
        assert_eq!(
            percent("Category:中国 A"),
            "Category%3A%E4%B8%AD%E5%9B%BD%20A"
        );
        let s = Skipped {
            when: 7,
            song: song("t", "a", "b"),
            reason: "中國歌手：周深".into(),
            name: "周深".into(),
        };
        assert_eq!(parse_skipped(&skipped_line(&s)), [s.clone()]);
        // Gear L's five-field lines read with no name.
        let old = parse_skipped("1\tt\ta\tb\tr");
        assert_eq!(old[0].name, "");
        // Newest first.
        let two = parse_skipped("1\ta\tb\tc\td\te\n2\tf\tg\th\ti\tj\n");
        assert_eq!(two.iter().map(|s| s.when).collect::<Vec<_>>(), [2, 1]);
    }

    // Reads Windows' media sessions; changes nothing.
    #[test]
    #[ignore = "reads what KKBOX plays on this PC"]
    fn hw_now_playing_reads_kkbox() {
        println!("{:?}", now_playing().unwrap());
    }

    // Writes down what KKBOX reports every 250 ms, for KKCUT_RECORD_SECS
    // seconds (30 by default); reads only, changes nothing. Run with
    // `cargo test hw_record -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads what KKBOX plays on this PC"]
    fn hw_record_what_kkbox_reports() {
        let secs: u64 = std::env::var("KKCUT_RECORD_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let manager = manager().unwrap();
        for s in manager.GetSessions().unwrap() {
            println!("session {}", s.SourceAppUserModelId().unwrap());
        }
        let start = Instant::now();
        let before = thread_count();
        while start.elapsed() < Duration::from_secs(secs) {
            let t = start.elapsed().as_millis();
            // Asked for each time, as the window does: it must be the cached one.
            match kkbox_session(&super::manager().unwrap()) {
                Ok(Some(session)) => {
                    let status = session
                        .GetPlaybackInfo()
                        .and_then(|p| p.PlaybackStatus())
                        .map(|s| format!("{s:?}"))
                        .unwrap_or_else(|e| e.to_string());
                    let at = session
                        .GetTimelineProperties()
                        .and_then(|p| p.Position())
                        .map_or(-1, |p| p.Duration / 10_000);
                    match super::song(&session) {
                        Ok(s) => println!(
                            "{t:>6} {status} {at:>7}ms | {} | {} | {}",
                            s.title, s.artist, s.album
                        ),
                        Err(e) => println!("{t:>6} {status} {at:>7}ms | error {e}"),
                    }
                }
                Ok(None) => println!("{t:>6} no KKBOX session"),
                Err(e) => println!("{t:>6} error {e}"),
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        // One manager for the process: the thread count stays flat.
        println!("threads before {before} after {}", thread_count());
    }

    /// Threads of this process.
    fn thread_count() -> usize {
        let script = format!("(Get-Process -Id {}).Threads.Count", std::process::id());
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0)
    }

    // About 10 requests to zh.wikipedia.org; writes nothing.
    #[test]
    #[ignore = "fetches the virtual singer list from Wikipedia"]
    fn net_virtual_singers_are_fetched() {
        let (ai, japan) = virtual_singers(&mut HashSet::new(), &|_| {}).unwrap();
        let mut sorted: Vec<&String> = ai.iter().collect();
        sorted.sort();
        println!("{} names: {sorted:?}", ai.len());
        let mut sorted: Vec<&String> = japan.iter().collect();
        sorted.sort();
        println!("{} Japanese: {sorted:?}", japan.len());
        let japan: HashSet<String> = japan.iter().map(|n| name_key(n)).collect();
        assert!(japan.contains(&name_key("初音ミク")) && japan.contains(&name_key("初音未來")));
        assert!(!japan.contains(&name_key("洛天依")));
        let keys: HashSet<String> = ai.iter().map(|n| name_key(n)).collect();
        for name in ["初音未來", "初音ミク", "洛天依", "樂正綾", "言和"] {
            assert!(keys.contains(&name_key(name)), "{name}");
        }
        for name in ["lily", "mayu", "oliver", "虛擬歌手"] {
            assert!(!keys.contains(&name_key(name)), "{name}");
        }
    }

    // About 400 requests to zh.wikipedia.org, about 9 minutes; writes
    // china-artists.json.
    #[test]
    #[ignore = "fetches the singer lists from Wikipedia"]
    fn net_refresh_fetches_the_lists() {
        let lists = refresh(&|n| eprint!("\r{n} categories")).unwrap();
        eprintln!();
        println!(
            "china {} elsewhere {} ai {} ai_japan {} categories {}",
            lists.china.len(),
            lists.elsewhere.len(),
            lists.ai.len(),
            lists.ai_japan.len(),
            lists.categories
        );
        for name in ["华晨宇", "華晨宇", "周深", "毛不易", "薛之謙"] {
            assert!(lists.china.contains(&name_key(name)), "{name}");
        }
        // Only under a Taiwan label's artists: on the China list.
        for name in ["许巍", "郁可唯"] {
            assert!(lists.china.contains(&name_key(name)), "{name}");
        }
        // Chinese, though also filed under another place: built in.
        for name in ["海鸣威", "张杰"] {
            let key = name_key(name);
            assert!(lists.china.contains(&key), "{name}");
            assert!(!lists.elsewhere.contains(&key), "{name}");
        }
        // In a China and an other-place category: on the other list.
        for name in ["周杰倫", "林俊傑", "陳奕迅", "五月天", "任賢齊", "王菲"] {
            let key = name_key(name);
            assert!(!lists.china.contains(&key), "{name}");
            assert!(lists.elsewhere.contains(&key), "{name}");
        }
    }
}
