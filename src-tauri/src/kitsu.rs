//! Kitsu metadata lookup, used alongside AniList so a matching outage on one does not leave
//! the library unlabelled. Kitsu needs no API key.
use crate::error::{AppError, Result};
use crate::models::MetadataHit;
use serde_json::Value;

pub const SOURCE: &str = "kitsu";
const DEFAULT_ENDPOINT: &str = "https://kitsu.io/api/edge";

pub struct Kitsu {
    client: reqwest::Client,
    endpoint: String,
}

impl Kitsu {
    pub fn new() -> Self {
        Self::with_endpoint(DEFAULT_ENDPOINT.into())
    }

    pub fn with_endpoint(endpoint: String) -> Self {
        Self {
            // Kitsu routinely takes several seconds; keep well clear of that.
            client: reqwest::Client::builder()
                .user_agent("anime-manager/0.1")
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("client"),
            endpoint: endpoint.trim_end_matches('/').to_string(),
        }
    }

    /// `None` when the resource does not exist, mirroring `AniList::by_id`.
    async fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<Option<Value>> {
        let resp = self
            .client
            .get(url)
            .query(query)
            .header("Accept", "application/vnd.api+json")
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(AppError::Network(format!("kitsu returned {}", resp.status())));
        }
        Ok(Some(resp.json().await?))
    }

    pub async fn search(&self, q: &str) -> Result<Vec<MetadataHit>> {
        let url = format!("{}/anime", self.endpoint);
        let v = self.get(&url, &[("filter[text]", q), ("page[limit]", "5")]).await?;
        Ok(v.as_ref().map(parse_hits).unwrap_or_default())
    }

    pub async fn by_id(&self, id: i64) -> Result<Option<MetadataHit>> {
        let v = self.get(&format!("{}/anime/{id}", self.endpoint), &[]).await?;
        // A single fetch returns one object rather than a list.
        Ok(v.as_ref().and_then(|v| v.get("data")).and_then(parse_hit))
    }
}

impl Default for Kitsu {
    fn default() -> Self { Self::new() }
}

fn parse_hit(a: &Value) -> Option<MetadataHit> {
    let at = a.get("attributes")?;
    // Kitsu ids arrive as JSON:API strings.
    let id: i64 = a.get("id")?.as_str()?.parse().ok()?;
    // Prefer the romaji title. Kitsu's canonicalTitle is often the English one (id 7442 is
    // "Attack on Titan"), and storing that as the romaji field both sinks the similarity score
    // for a romaji-named folder and makes Rename write the English title onto disk.
    let romaji = at
        .pointer("/titles/en_jp")
        .and_then(|t| t.as_str())
        .or_else(|| at.get("canonicalTitle").and_then(|t| t.as_str()))?
        .to_string();
    Some(MetadataHit {
        id,
        source: SOURCE.to_string(),
        title_romaji: romaji,
        title_english: at.pointer("/titles/en").and_then(|t| t.as_str()).map(String::from),
        cover_url: at.pointer("/posterImage/large").and_then(|t| t.as_str()).map(String::from),
        episodes: at.get("episodeCount").and_then(|e| e.as_i64()),
    })
}

fn parse_hits(v: &Value) -> Vec<MetadataHit> {
    v.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| arr.iter().filter_map(parse_hit).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn body() -> Value {
        serde_json::json!({"data": [
            {"id": "46474", "attributes": {
                "canonicalTitle": "Frieren: Beyond Journey's End",
                "titles": {"en": "Frieren: Beyond Journey's End", "en_jp": "Sousou no Frieren"},
                "episodeCount": 28,
                "posterImage": {"large": "https://media.kitsu.app/x/large.jpeg"}}},
            {"id": "49240", "attributes": {
                "canonicalTitle": "Sousou no Frieren 2nd Season",
                "titles": {},
                "episodeCount": null,
                "posterImage": null}},
            {"id": "bogus", "attributes": {"canonicalTitle": "Skipped"}}
        ]})
    }

    #[tokio::test]
    async fn search_parses_hits_and_skips_unusable_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/anime"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body()))
            .mount(&server).await;
        let hits = Kitsu::with_endpoint(server.uri()).search("frieren").await.unwrap();
        assert_eq!(hits.len(), 2, "the non-numeric id is skipped, not fatal");
        assert_eq!(hits[0].id, 46474);
        assert_eq!(hits[0].source, "kitsu");
        assert_eq!(hits[0].title_romaji, "Sousou no Frieren", "romaji wins over an English canonicalTitle");
        assert_eq!(hits[0].title_english.as_deref(), Some("Frieren: Beyond Journey's End"));
        assert_eq!(hits[0].episodes, Some(28));
        assert_eq!(hits[0].cover_url.as_deref(), Some("https://media.kitsu.app/x/large.jpeg"));
        assert_eq!(hits[1].title_english, None);
        assert_eq!(hits[1].cover_url, None);
    }

    #[tokio::test]
    async fn a_missing_entry_is_none_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
        assert!(Kitsu::with_endpoint(server.uri()).by_id(1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn http_error_is_a_network_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(503)).mount(&server).await;
        let r = Kitsu::with_endpoint(server.uri()).search("x").await;
        assert!(matches!(r, Err(AppError::Network(_))), "{r:?}");
    }
}
