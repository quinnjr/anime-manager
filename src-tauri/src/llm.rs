//! Optional LLM second opinion on folder contents, via OpenCode Zen (OpenAI-compatible).
use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::{AssistProgress, InspectChange, InspectReport, ParseOverride};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/v1";
pub const DEFAULT_MODEL: &str = "big-pickle";
pub const MAX_FILES_PER_FOLDER: usize = 200;
/// Attempts per request; waits grow as retry_base * 2^n and honour Retry-After.
pub const MAX_ATTEMPTS: u32 = 6;
/// Default pause between folders so free-tier rate limits are not hammered.
pub const DEFAULT_DELAY_MS: u64 = 500;

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
    retry_base: Duration,
    delay_between_folders: Duration,
}

impl Llm {
    pub fn with(base_url: String, api_key: Option<String>, model: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("anime-manager/0.1")
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("client");
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.filter(|k| !k.trim().is_empty()),
            model,
            retry_base: Duration::from_secs(1),
            delay_between_folders: Duration::from_millis(DEFAULT_DELAY_MS),
        }
    }

    /// Shorten the retry and inter-folder waits (tests).
    pub fn with_timing(mut self, retry_base: Duration, delay_between_folders: Duration) -> Self {
        self.retry_base = retry_base;
        self.delay_between_folders = delay_between_folders;
        self
    }

    /// Build from the settings table: `llm_api_key`, `llm_model`, `llm_base_url`, `llm_delay_ms`.
    pub fn from_db(db: &Db) -> Result<Self> {
        let key = db.get_setting("llm_api_key")?;
        let model = db.get_setting("llm_model")?.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| DEFAULT_MODEL.into());
        let base = db.get_setting("llm_base_url")?.filter(|b| !b.trim().is_empty()).unwrap_or_else(|| DEFAULT_BASE_URL.into());
        let delay = db.get_setting("llm_delay_ms")?.and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_DELAY_MS);
        Ok(Self::with(base, key, model).with_timing(Duration::from_secs(1), Duration::from_millis(delay)))
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
        let mut attempt = 0u32;
        let text = loop {
            attempt += 1;
            let sent = self.client.post(format!("{}/chat/completions", self.base_url)).bearer_auth(key).json(&body).send().await;
            // Transient failures (429, 5xx, transport errors) back off and retry; anything else is final.
            let (retryable, wait, err) = match sent {
                Ok(resp) => {
                    let status = resp.status();
                    let retry_after = resp.headers().get("retry-after").and_then(|h| h.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).map(Duration::from_secs);
                    let text = resp.text().await?;
                    if status.is_success() { break text; }
                    let snippet: String = text.chars().take(300).collect();
                    let e = AppError::Network(format!("LLM returned {status}: {snippet}"));
                    (status.as_u16() == 429 || status.is_server_error(), retry_after, e)
                }
                Err(e) => (true, None, AppError::Network(e.to_string())),
            };
            if !retryable || attempt >= MAX_ATTEMPTS { return Err(err); }
            let backoff = self.retry_base * 2u32.pow(attempt - 1);
            tokio::time::sleep(wait.unwrap_or(backoff).max(backoff)).await;
        };
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

/// Inspect one folder and apply the model's decisions as parse overrides.
/// Returns Ok(false) when the folder held no known episodes.
async fn inspect_one(db: &Db, llm: &Llm, folder: &str, source: &str, report: &mut InspectReport) -> Result<bool> {
    let rows = db.episodes_in_folder(folder)?;
    if rows.is_empty() { return Ok(false); }
    let files: Vec<FileGuess> = rows.iter().map(|(path, title, season, number)| FileGuess {
        name: Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or(path).to_string(),
        guess: format!("{title} S{season}E{number}"),
    }).collect();
    let rel = rel_to_roots(db, folder);
    let inspection = match llm.inspect_folder(&rel, &files).await {
        Ok(i) => i,
        Err(e) => { report.notes.push(format!("{rel}: {e}")); return Ok(true); }
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
    db.prune_empty()?;
    Ok(true)
}

/// Send each folder to the model and apply its decisions as parse overrides.
/// Failures on one folder are recorded in `report.notes` and do not stop the others.
pub async fn inspect_folders(db: Arc<Db>, llm: Arc<Llm>, folders: Vec<String>, source: &str) -> Result<InspectReport> {
    if !llm.configured() {
        return Err(AppError::Network("LLM not configured: set an OpenCode Zen API key in Settings".into()));
    }
    let mut report = InspectReport::default();
    let mut first = true;
    for folder in folders {
        if !first { tokio::time::sleep(llm.delay_between_folders).await; }
        first = false;
        inspect_one(&db, &llm, &folder, source, &mut report).await?;
    }
    Ok(report)
}

/// Background work list of folders awaiting an LLM opinion. Scans enqueue; one worker drains.
/// Enqueuing while a worker runs simply extends that run, so no folder is ever dropped.
#[derive(Default)]
pub struct AssistQueue {
    pending: Mutex<VecDeque<String>>,
    running: AtomicBool,
    done: AtomicUsize,
    total: AtomicUsize,
}

impl AssistQueue {
    /// Add folders not already queued. Returns how many were new.
    pub fn enqueue(&self, folders: impl IntoIterator<Item = String>) -> usize {
        let mut q = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let mut added = 0;
        for f in folders {
            if !q.contains(&f) { q.push_back(f); added += 1; }
        }
        self.total.fetch_add(added, Ordering::SeqCst);
        added
    }

    pub fn is_running(&self) -> bool { self.running.load(Ordering::SeqCst) }

    pub fn progress(&self) -> AssistProgress {
        AssistProgress { done: self.done.load(Ordering::SeqCst), total: self.total.load(Ordering::SeqCst), folder: String::new(), running: self.is_running() }
    }

    fn pop(&self) -> Option<String> { self.pending.lock().unwrap_or_else(|e| e.into_inner()).pop_front() }

    /// Try to become the worker. Returns false if one is already draining the queue.
    pub fn try_start(&self) -> bool {
        self.running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    }

    /// Drain the queue one folder at a time, reporting progress after each. Must follow a
    /// successful `try_start`. Clears the counters and the running flag when the queue is empty.
    pub async fn run(&self, db: Arc<Db>, llm: Arc<Llm>, source: &str, on_progress: &(dyn Fn(AssistProgress) + Send + Sync)) -> InspectReport {
        let mut report = InspectReport::default();
        let mut first = true;
        while let Some(folder) = self.pop() {
            if !first { tokio::time::sleep(llm.delay_between_folders).await; }
            first = false;
            if let Err(e) = inspect_one(&db, &llm, &folder, source, &mut report).await {
                report.notes.push(format!("{folder}: {e}"));
            }
            let done = self.done.fetch_add(1, Ordering::SeqCst) + 1;
            on_progress(AssistProgress { done, total: self.total.load(Ordering::SeqCst), folder, running: true });
        }
        self.done.store(0, Ordering::SeqCst);
        self.total.store(0, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        report
    }
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
        let llm = Llm::with(bad.uri(), Some("k".into()), "m".into()).with_timing(Duration::from_millis(1), Duration::ZERO);
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
    async fn retries_on_429_then_succeeds_and_gives_up_after_max_attempts() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0")).up_to_n_times(2).mount(&server).await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(reply("ok"))).mount(&server).await;
        let llm = Llm::with(server.uri(), Some("k".into()), "m".into()).with_timing(Duration::from_millis(1), Duration::ZERO);
        assert_eq!(llm.test().await.unwrap(), "m replied: ok");
        assert_eq!(server.received_requests().await.unwrap().len(), 3);

        let always = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(503)).mount(&always).await;
        let llm = Llm::with(always.uri(), Some("k".into()), "m".into()).with_timing(Duration::from_millis(1), Duration::ZERO);
        assert!(matches!(llm.test().await, Err(AppError::Network(_))));
        assert_eq!(always.received_requests().await.unwrap().len(), MAX_ATTEMPTS as usize);

        let auth = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(401)).mount(&auth).await;
        let llm = Llm::with(auth.uri(), Some("k".into()), "m".into()).with_timing(Duration::from_millis(1), Duration::ZERO);
        assert!(llm.test().await.is_err());
        assert_eq!(auth.received_requests().await.unwrap().len(), 1, "401 must not be retried");
    }

    #[tokio::test]
    async fn queue_drains_everything_dedupes_and_reports_progress() {
        let server = MockServer::start().await;
        let content = r#"{"title":"T","season":1,"files":[{"name":"01.mkv","kind":"episode","season":1,"episode":1,"title":null}],"notes":""}"#;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(reply(content))).mount(&server).await;
        let db = Arc::new(Db::open_memory().unwrap());
        let pn = |t: &str| ParsedName { title: t.into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
        let n = 40;
        for i in 0..n {
            db.upsert_episode(&pn(&format!("Show{i}")), &RawFile { path: format!("/lib/Show{i}/01.mkv").into(), size: 1, mtime: 1, stem: "".into(), dirs: vec![] }).unwrap();
        }
        let q = Arc::new(AssistQueue::default());
        let folders: Vec<String> = (0..n).map(|i| format!("/lib/Show{i}")).collect();
        assert_eq!(q.enqueue(folders.clone()), n);
        assert_eq!(q.enqueue(folders.clone()), 0, "duplicates are not re-queued");
        assert_eq!(q.enqueue(vec!["/lib/nothing-here".to_string()]), 1);
        assert!(q.try_start());
        assert!(!q.try_start(), "second worker refused while running");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let llm = Arc::new(Llm::with(server.uri(), Some("k".into()), "m".into()).with_timing(Duration::from_millis(1), Duration::ZERO));
        let report = q.run(db.clone(), llm, "llm", &move |p| s2.lock().unwrap().push(p)).await;
        assert_eq!(report.folders, n, "every folder inspected, none capped");
        assert_eq!(report.changes.len(), n);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), n + 1);
        assert_eq!((seen[0].done, seen[0].total), (1, n + 1));
        assert_eq!((seen[n].done, seen[n].total), (n + 1, n + 1));
        assert!(!q.is_running());
        assert_eq!(q.progress().total, 0);
        assert_eq!(db.list_shows("").unwrap().len(), 1, "all merged into T");
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
