//! Optional LLM second opinion on folder contents, via OpenCode Zen (OpenAI-compatible).
use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::{InspectChange, InspectReport, ParseOverride};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/v1";
pub const DEFAULT_MODEL: &str = "big-pickle";
pub const MAX_FILES_PER_FOLDER: usize = 200;
pub const MAX_FOLDERS_PER_SCAN: usize = 25;

const SYSTEM_PROMPT: &str = "You are a meticulous anime media librarian. You receive one folder of video files from a fansub/BD release plus a regex parser's current guess for each file. \
Decide the anime title (romaji as listed on AniList, no release-group tags, no quality tags), the season number, and for every file its kind and numbering. \
kind is one of: \"episode\" (a numbered TV episode), \"special\" (OVA/OAD/NCOP/NCED/OP/ED/PV/CM/menu/recap/extra; these go to season 0), \"movie\" (a film or one-shot; season 1, episode 1 unless numbered), \"ignore\" (samples, trailers for other works, junk). \
If a file belongs to a different anime than the folder (e.g. a bundled spin-off), set its own \"title\". \
Respond with JSON only, no prose, exactly this shape: \
{\"title\": string, \"season\": integer|null, \"files\": [{\"name\": string, \"kind\": \"episode\"|\"special\"|\"movie\"|\"ignore\", \"season\": integer, \"episode\": integer, \"title\": string|null}], \"notes\": string}. \
\"name\" must be copied verbatim from the input. Keep episode numbers as printed in the file name (do not renumber to absolute).";

#[derive(Debug, Clone, PartialEq)]
pub struct FileGuess {
    pub name: String,
    pub guess: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FileDecision {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub season: Option<u32>,
    #[serde(default)]
    pub episode: Option<u32>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FolderInspection {
    pub title: String,
    #[serde(default)]
    pub season: Option<u32>,
    #[serde(default)]
    pub files: Vec<FileDecision>,
    #[serde(default)]
    pub notes: String,
}

pub struct Llm {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl Llm {
    pub fn with(base_url: String, api_key: Option<String>, model: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("anime-manager/0.1")
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("client");
        Self { client, base_url: base_url.trim_end_matches('/').to_string(), api_key: api_key.filter(|k| !k.trim().is_empty()), model }
    }

    /// Build from the settings table: `llm_api_key`, `llm_model`, `llm_base_url`.
    pub fn from_db(db: &Db) -> Result<Self> {
        let key = db.get_setting("llm_api_key")?;
        let model = db.get_setting("llm_model")?.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| DEFAULT_MODEL.into());
        let base = db.get_setting("llm_base_url")?.filter(|b| !b.trim().is_empty()).unwrap_or_else(|| DEFAULT_BASE_URL.into());
        Ok(Self::with(base, key, model))
    }

    pub fn configured(&self) -> bool { self.api_key.is_some() }

    pub fn model(&self) -> &str { &self.model }

    pub async fn chat(&self, system: &str, user: &str) -> Result<String> {
        let key = self.api_key.as_ref().ok_or_else(|| AppError::Network("LLM not configured: set an OpenCode Zen API key in Settings".into()))?;
        let body = json!({
            "model": self.model,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ]
        });
        let resp = self.client.post(format!("{}/chat/completions", self.base_url)).bearer_auth(key).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let snippet: String = text.chars().take(300).collect();
            return Err(AppError::Network(format!("LLM returned {status}: {snippet}")));
        }
        let v: Value = serde_json::from_str(&text)?;
        let content = v.pointer("/choices/0/message/content").and_then(|c| c.as_str())
            .ok_or_else(|| AppError::Parse("LLM response had no choices[0].message.content".into()))?;
        Ok(content.to_string())
    }

    /// One-line round trip for the Settings "Test connection" button.
    pub async fn test(&self) -> Result<String> {
        let reply = self.chat("Reply with exactly: ok", "ping").await?;
        Ok(format!("{} replied: {}", self.model, reply.trim()))
    }

    pub async fn inspect_folder(&self, rel_folder: &str, files: &[FileGuess]) -> Result<FolderInspection> {
        let mut user = format!("Folder (relative to the library root): {rel_folder}\n\nFiles ({}):\n", files.len());
        for f in files.iter().take(MAX_FILES_PER_FOLDER) {
            user.push_str(&format!("- {}\n    parser guess: {}\n", f.name, f.guess));
        }
        if files.len() > MAX_FILES_PER_FOLDER {
            user.push_str(&format!("... and {} more files not shown; decide them by the same pattern.\n", files.len() - MAX_FILES_PER_FOLDER));
        }
        let reply = self.chat(SYSTEM_PROMPT, &user).await?;
        parse_inspection(&reply)
    }
}

/// Parse the model's reply, tolerating ```json fences and leading/trailing prose.
pub fn parse_inspection(reply: &str) -> Result<FolderInspection> {
    let t = reply.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    let start = t.find('{').ok_or_else(|| AppError::Parse("LLM reply contained no JSON object".into()))?;
    let end = t.rfind('}').ok_or_else(|| AppError::Parse("LLM reply contained no JSON object".into()))?;
    if end < start { return Err(AppError::Parse("LLM reply contained no JSON object".into())); }
    let v: FolderInspection = serde_json::from_str(&t[start..=end])?;
    if v.title.trim().is_empty() { return Err(AppError::Parse("LLM reply had an empty title".into())); }
    Ok(v)
}

fn rel_to_roots(db: &Db, folder: &str) -> String {
    let roots = db.list_roots().unwrap_or_default();
    let best = roots.iter().map(|r| r.path.trim_end_matches('/').to_string()).filter(|r| folder.starts_with(r.as_str())).max_by_key(|r| r.len());
    match best {
        Some(r) => folder[r.len()..].trim_start_matches('/').to_string(),
        None => folder.to_string(),
    }
}

/// Send each folder to the model and apply its decisions as parse overrides.
/// Failures on one folder are recorded in `report.notes` and do not stop the others.
pub async fn inspect_folders(db: Arc<Db>, llm: Arc<Llm>, folders: Vec<String>, source: &str) -> Result<InspectReport> {
    if !llm.configured() {
        return Err(AppError::Network("LLM not configured: set an OpenCode Zen API key in Settings".into()));
    }
    let mut report = InspectReport::default();
    for folder in folders {
        let rows = db.episodes_in_folder(&folder)?;
        if rows.is_empty() { continue; }
        let files: Vec<FileGuess> = rows.iter().map(|(path, title, season, number)| FileGuess {
            name: Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or(path).to_string(),
            guess: format!("{title} S{season}E{number}"),
        }).collect();
        let rel = rel_to_roots(&db, &folder);
        let inspection = match llm.inspect_folder(&rel, &files).await {
            Ok(i) => i,
            Err(e) => { report.notes.push(format!("{rel}: {e}")); continue; }
        };
        report.folders += 1;
        if !inspection.notes.trim().is_empty() { report.notes.push(format!("{rel}: {}", inspection.notes.trim())); }
        let by_name: BTreeMap<&str, &(String, String, u32, u32)> = rows.iter().map(|r| (Path::new(&r.0).file_name().and_then(|s| s.to_str()).unwrap_or(&r.0), r)).collect();
        for d in &inspection.files {
            let Some((path, cur_title, cur_season, cur_number)) = by_name.get(d.name.as_str()).map(|r| (&r.0, &r.1, r.2, r.3)) else { continue };
            let from = format!("{cur_title} S{cur_season}E{cur_number}");
            if d.kind == "ignore" {
                db.set_override(&ParseOverride { path: path.clone(), title: String::new(), season: 0, number: 0, kind: "ignore".into(), source: source.into() })?;
                db.delete_episode_by_path(path)?;
                report.ignored += 1;
                report.changes.push(InspectChange { path: path.clone(), from, to: "ignored".into() });
                continue;
            }
            let kind = match d.kind.as_str() { "special" | "movie" | "episode" => d.kind.clone(), _ => "episode".into() };
            let title = d.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| inspection.title.clone()).trim().to_string();
            let season = if kind == "special" { 0 } else { d.season.or(inspection.season).unwrap_or(1) };
            let number = d.episode.unwrap_or(if kind == "movie" { 1 } else { cur_number });
            db.set_override(&ParseOverride { path: path.clone(), title: title.clone(), season, number, kind, source: source.into() })?;
            db.reassign_episode(path, &title, season, number)?;
            let to = format!("{title} S{season}E{number}");
            if to != from { report.changes.push(InspectChange { path: path.clone(), from, to }); }
        }
    }
    db.prune_empty()?;
    Ok(report)
}

/// On-demand inspection of every folder that holds an episode of `show_id`.
pub async fn inspect_show(db: Arc<Db>, llm: Arc<Llm>, show_id: i64) -> Result<InspectReport> {
    let paths = db.episode_paths_for_show(show_id)?;
    if paths.is_empty() { return Err(AppError::Db(format!("show {show_id} has no episodes"))); }
    let mut folders: Vec<String> = paths.iter().filter_map(|p| Path::new(p).parent().map(|d| d.to_string_lossy().to_string())).collect();
    folders.sort();
    folders.dedup();
    let mut report = inspect_folders(db.clone(), llm, folders, "llm").await?;
    report.show_id = paths.iter().find_map(|p| db.show_id_for_path(p).ok().flatten());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn reply(content: &str) -> Value {
        json!({"choices": [{"message": {"role": "assistant", "content": content}}]})
    }

    #[test]
    fn parses_fenced_and_plain_json() {
        let plain = r#"{"title":"Sekirei","season":1,"files":[{"name":"01_Sekirei_KDG.mkv","kind":"episode","season":1,"episode":1,"title":null}],"notes":"ok"}"#;
        let i = parse_inspection(plain).unwrap();
        assert_eq!(i.title, "Sekirei");
        assert_eq!(i.files[0].episode, Some(1));
        let fenced = format!("Sure! Here you go:\n```json\n{plain}\n```\nLet me know.");
        assert_eq!(parse_inspection(&fenced).unwrap(), i);
        assert!(matches!(parse_inspection("no json here"), Err(AppError::Parse(_))));
        assert!(matches!(parse_inspection(r#"{"title":"","files":[]}"#), Err(AppError::Parse(_))));
    }

    #[test]
    fn unconfigured_when_key_missing_or_blank() {
        assert!(!Llm::with("http://x".into(), None, "m".into()).configured());
        assert!(!Llm::with("http://x".into(), Some("  ".into()), "m".into()).configured());
        assert!(Llm::with("http://x".into(), Some("k".into()), "m".into()).configured());
        let db = Db::open_memory().unwrap();
        let l = Llm::from_db(&db).unwrap();
        assert!(!l.configured());
        assert_eq!(l.model(), DEFAULT_MODEL);
        db.set_setting("llm_model", "nemotron-3.5-lightning-free").unwrap();
        assert_eq!(Llm::from_db(&db).unwrap().model(), "nemotron-3.5-lightning-free");
    }

    #[tokio::test]
    async fn chat_sends_bearer_and_model_and_maps_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions")).and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply("ok"))).mount(&server).await;
        let llm = Llm::with(server.uri(), Some("sk-test".into()), "big-pickle".into());
        assert_eq!(llm.test().await.unwrap(), "big-pickle replied: ok");
        let none = Llm::with(server.uri(), None, "big-pickle".into());
        assert!(matches!(none.test().await, Err(AppError::Network(_))));
        let bad = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(429).set_body_string("rate limited")).mount(&bad).await;
        let llm = Llm::with(bad.uri(), Some("k".into()), "m".into());
        match llm.test().await { Err(AppError::Network(m)) => assert!(m.contains("429") && m.contains("rate limited")), o => panic!("{o:?}") }
    }

    #[tokio::test]
    async fn inspect_show_applies_overrides_reassigns_and_ignores() {
        let server = MockServer::start().await;
        let content = r#"{"title":"Sekirei","season":1,"files":[
            {"name":"01_Sekirei_KDG.mkv","kind":"episode","season":1,"episode":1,"title":null},
            {"name":"OVA_Kusano.mkv","kind":"special","season":1,"episode":1,"title":null},
            {"name":"sample.mkv","kind":"ignore","season":0,"episode":0,"title":null},
            {"name":"not-in-folder.mkv","kind":"episode","season":1,"episode":9,"title":null}
        ],"notes":"first season"}"#;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(reply(content))).mount(&server).await;
        let db = Arc::new(Db::open_memory().unwrap());
        db.add_root("/lib").unwrap();
        let pn = |t: &str, s: u32, e: u32| ParsedName { title: t.into(), season: s, episode: e, release_group: None, resolution: None, crc: None };
        let rf = |p: &str| RawFile { path: p.into(), size: 1, mtime: 1, stem: "".into(), dirs: vec![] };
        db.upsert_episode(&pn("Season1", 1, 1), &rf("/lib/Sekirei Complete/Season1/01_Sekirei_KDG.mkv")).unwrap();
        db.upsert_episode(&pn("Season1", 1, 2), &rf("/lib/Sekirei Complete/Season1/OVA_Kusano.mkv")).unwrap();
        db.upsert_episode(&pn("Season1", 1, 3), &rf("/lib/Sekirei Complete/Season1/sample.mkv")).unwrap();
        let wrong_id = db.list_shows("").unwrap()[0].id;
        let llm = Arc::new(Llm::with(server.uri(), Some("k".into()), "m".into()));
        let r = inspect_show(db.clone(), llm, wrong_id).await.unwrap();
        assert_eq!(r.folders, 1);
        assert_eq!(r.ignored, 1);
        assert_eq!(r.changes.len(), 3);
        assert_eq!(r.notes, vec!["Sekirei Complete/Season1: first season"]);
        let shows = db.list_shows("").unwrap();
        assert_eq!(shows.len(), 1);
        assert_eq!(shows[0].display_title, "Sekirei");
        assert_eq!(r.show_id, Some(shows[0].id));
        let d = db.get_show(shows[0].id).unwrap();
        let seasons: Vec<(u32, Vec<u32>)> = d.seasons.iter().map(|s| (s.number, s.episodes.iter().map(|e| e.number).collect())).collect();
        assert_eq!(seasons, vec![(0, vec![1]), (1, vec![1])]);
        assert_eq!(db.get_override("/lib/Sekirei Complete/Season1/sample.mkv").unwrap().unwrap().kind, "ignore");
        assert_eq!(db.get_override("/lib/Sekirei Complete/Season1/OVA_Kusano.mkv").unwrap().unwrap().season, 0);
    }

    #[tokio::test]
    async fn folder_failure_is_noted_not_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(reply("I cannot help with that."))).mount(&server).await;
        let db = Arc::new(Db::open_memory().unwrap());
        let pn = ParsedName { title: "X".into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
        db.upsert_episode(&pn, &RawFile { path: "/lib/X/01.mkv".into(), size: 1, mtime: 1, stem: "".into(), dirs: vec![] }).unwrap();
        let llm = Arc::new(Llm::with(server.uri(), Some("k".into()), "m".into()));
        let r = inspect_folders(db.clone(), llm, vec!["/lib/X".into()], "llm").await.unwrap();
        assert_eq!(r.folders, 0);
        assert_eq!(r.notes.len(), 1);
        assert!(r.notes[0].contains("parse"));
        assert_eq!(db.list_shows("").unwrap()[0].display_title, "X");
    }
}
