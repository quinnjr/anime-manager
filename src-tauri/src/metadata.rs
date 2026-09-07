//! Cross-search across metadata providers.
//!
//! AniList and Kitsu both go down independently — AniList disabled its API outright in
//! September 2026 — so neither is trusted alone. A search asks both and merges what comes back;
//! auto-matching takes whichever provider offers the most convincing title.
use crate::anilist::{self, AniList, best_match};
use crate::error::Result;
use crate::kitsu::Kitsu;
use crate::models::MetadataHit;

pub struct Providers {
    pub anilist: AniList,
    pub kitsu: Kitsu,
}

impl Default for Providers {
    fn default() -> Self { Self::new() }
}

impl Providers {
    pub fn new() -> Self {
        Self { anilist: AniList::new(), kitsu: Kitsu::new() }
    }

    /// Hits from every provider that answered, AniList first. A provider that errors is
    /// skipped rather than failing the search, so one outage does not block matching.
    pub async fn search(&self, q: &str) -> (Vec<MetadataHit>, Vec<String>) {
        let mut hits = Vec::new();
        let mut errors = Vec::new();
        match self.anilist.search(q).await {
            Ok(h) => hits.extend(h),
            Err(e) => errors.push(format!("anilist: {e}")),
        }
        match self.kitsu.search(q).await {
            Ok(h) => hits.extend(h),
            Err(e) => errors.push(format!("kitsu: {e}")),
        }
        (hits, errors)
    }

    pub async fn by_id(&self, source: &str, id: i64) -> Result<Option<MetadataHit>> {
        if source == crate::kitsu::SOURCE {
            self.kitsu.by_id(id).await
        } else {
            self.anilist.by_id(id).await
        }
    }

    /// The best hit across providers for `title`, or None if nothing is close enough to apply
    /// unattended. Ranking is the same similarity used for a single provider, so a strong Kitsu
    /// hit beats a weak AniList one rather than losing on provider order.
    pub async fn best(&self, title: &str) -> Option<MetadataHit> {
        let (hits, errors) = self.search(title).await;
        for e in &errors {
            eprintln!("metadata {title}: {e}");
        }
        best_match(title, &hits).cloned()
    }
}

/// Re-export so callers do not need to know which provider defines the threshold.
pub use anilist::MIN_AUTO_MATCH_SIMILARITY;

/// Match every unmatched show against whichever provider answers, then fetch its cover art.
pub async fn auto_match_all(
    db: std::sync::Arc<crate::db::Db>,
    providers: std::sync::Arc<Providers>,
    notify: impl Fn(i64) + Send + 'static,
) {
    let pending = match db.shows_needing_match() {
        Ok(p) => p,
        Err(_) => return,
    };
    for (show_id, title) in pending {
        if let Some(hit) = providers.best(&title).await
            && db.set_anilist(show_id, &hit).is_ok()
        {
            notify(show_id);
        }
        // Stay well under AniList's 90 requests/minute; Kitsu is slower than that anyway.
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn kitsu_body() -> serde_json::Value {
        serde_json::json!({"data": [{"id": "46474", "attributes": {
            "canonicalTitle": "Sousou no Frieren", "titles": {},
            "episodeCount": 28, "posterImage": {"large": "https://k/large.jpeg"}}}]})
    }

    /// AniList down (403, as it actually was), Kitsu healthy.
    async fn one_provider_down() -> (MockServer, MockServer) {
        let dead = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(403)).mount(&dead).await;
        let live = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(kitsu_body())).mount(&live).await;
        (dead, live)
    }

    #[tokio::test]
    async fn a_dead_provider_does_not_block_the_other() {
        let (dead, live) = one_provider_down().await;
        let p = Providers {
            anilist: AniList::with_endpoint(dead.uri()).with_retry_base(std::time::Duration::from_millis(1)),
            kitsu: Kitsu::with_endpoint(live.uri()),
        };
        let (hits, errors) = p.search("Frieren").await;
        assert_eq!(hits.len(), 1, "kitsu still answers");
        assert_eq!(hits[0].source, "kitsu");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("anilist:"), "{errors:?}");

        let best = p.best("Frieren").await.expect("matched through the surviving provider");
        assert_eq!((best.source.as_str(), best.id), ("kitsu", 46474));
    }

    #[tokio::test]
    async fn both_down_yields_no_match_rather_than_a_wrong_one() {
        let dead = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(403)).mount(&dead).await;
        let dead2 = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(503)).mount(&dead2).await;
        let p = Providers {
            anilist: AniList::with_endpoint(dead.uri()).with_retry_base(std::time::Duration::from_millis(1)),
            kitsu: Kitsu::with_endpoint(dead2.uri()),
        };
        let (hits, errors) = p.search("Frieren").await;
        assert!(hits.is_empty());
        assert_eq!(errors.len(), 2);
        assert!(p.best("Frieren").await.is_none());
    }

    #[tokio::test]
    async fn an_unconvincing_hit_is_still_rejected_across_providers() {
        let (dead, live) = one_provider_down().await;
        let p = Providers {
            anilist: AniList::with_endpoint(dead.uri()).with_retry_base(std::time::Duration::from_millis(1)),
            kitsu: Kitsu::with_endpoint(live.uri()),
        };
        assert!(p.best("Bagel Girl").await.is_none(), "a bad hit must not be applied unattended");
    }
}
