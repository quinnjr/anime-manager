use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::AniListHit;
use serde_json::{json, Value};
use std::sync::Arc;

const QUERY: &str = r#"
query ($q: String, $id: Int) {
  Page(perPage: 5) {
    media(search: $q, id: $id, type: ANIME) {
      id title { romaji english } coverImage { large } episodes
    }
  }
}"#;

pub struct AniList {
    client: reqwest::Client,
    endpoint: String,
}

impl AniList {
    pub fn new() -> Self {
        Self::with_endpoint("https://graphql.anilist.co".into())
    }

    pub fn with_endpoint(endpoint: String) -> Self {
        Self {
            client: reqwest::Client::builder().user_agent("anime-manager/0.1").build().expect("client"),
            endpoint,
        }
    }

    async fn run(&self, vars: Value) -> Result<Vec<AniListHit>> {
        let resp = self.client.post(&self.endpoint).json(&json!({"query": QUERY, "variables": vars})).send().await?;
        if !resp.status().is_success() {
            return Err(AppError::Network(format!("anilist returned {}", resp.status())));
        }
        let v: Value = resp.json().await?;
        let media = v.pointer("/data/Page/media").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        Ok(media
            .iter()
            .filter_map(|m| {
                Some(AniListHit {
                    id: m.get("id")?.as_i64()?,
                    title_romaji: m.pointer("/title/romaji")?.as_str()?.to_string(),
                    title_english: m.pointer("/title/english").and_then(|t| t.as_str()).map(String::from),
                    cover_url: m.pointer("/coverImage/large").and_then(|t| t.as_str()).map(String::from),
                    episodes: m.get("episodes").and_then(|e| e.as_i64()),
                })
            })
            .collect())
    }

    pub async fn search(&self, q: &str) -> Result<Vec<AniListHit>> {
        self.run(json!({"q": q})).await
    }

    pub async fn by_id(&self, id: i64) -> Result<Option<AniListHit>> {
        Ok(self.run(json!({"id": id})).await?.into_iter().next())
    }
}

impl Default for AniList {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn auto_match_all(db: Arc<Db>, api: Arc<AniList>, notify: impl Fn(i64) + Send + 'static) {
    let pending = match db.shows_needing_match() {
        Ok(p) => p,
        Err(_) => return,
    };
    for (show_id, title) in pending {
        match api.search(&title).await {
            Ok(hits) => {
                if let Some(hit) = hits.first() {
                    if db.set_anilist(show_id, hit).is_ok() {
                        notify(show_id);
                    }
                }
            }
            Err(e) => eprintln!("anilist: {title}: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(700)).await; // stay under AniList's 90 req/min
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn body() -> serde_json::Value {
        serde_json::json!({"data":{"Page":{"media":[
            {"id":154587,"title":{"romaji":"Sousou no Frieren","english":"Frieren: Beyond Journey's End"},
             "coverImage":{"large":"https://img/x.jpg"},"episodes":28},
            {"id":1,"title":{"romaji":"Other","english":null},"coverImage":{"large":null},"episodes":null}
        ]}}})
    }

    #[tokio::test]
    async fn search_parses_hits() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/")).respond_with(ResponseTemplate::new(200).set_body_json(body())).mount(&server).await;
        let api = AniList::with_endpoint(server.uri());
        let hits = api.search("frieren").await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, 154587);
        assert_eq!(hits[0].title_romaji, "Sousou no Frieren");
        assert_eq!(hits[0].episodes, Some(28));
        assert_eq!(hits[1].title_english, None);
    }

    #[tokio::test]
    async fn http_error_is_network_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let api = AniList::with_endpoint(server.uri());
        assert!(matches!(api.search("x").await, Err(crate::error::AppError::Network(_))));
    }

    #[tokio::test]
    async fn auto_match_all_sets_first_hit_and_skips_failures() {
        use crate::parser::ParsedName;
        use crate::scanner::RawFile;
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(body())).mount(&server).await;
        let db = Arc::new(Db::open_memory().unwrap());
        let p = |t: &str| ParsedName { title: t.into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
        let f = |p: &str| RawFile { path: p.into(), size: 1, mtime: 1, stem: "".into(), parent_dir: "".into() };
        db.upsert_episode(&p("frieren"), &f("/a/1.mkv")).unwrap();
        let matched = Arc::new(std::sync::Mutex::new(Vec::new()));
        let m = matched.clone();
        auto_match_all(db.clone(), Arc::new(AniList::with_endpoint(server.uri())), move |id| m.lock().unwrap().push(id)).await;
        assert_eq!(matched.lock().unwrap().len(), 1);
        assert!(db.shows_needing_match().unwrap().is_empty());
        assert_eq!(db.display_title(1).unwrap(), "Sousou no Frieren");
    }
}
