//! Cross-search across metadata providers.
//!
//! AniList and Kitsu both go down independently — AniList disabled its API outright in
//! September 2026 — so neither is trusted alone. A search asks both and merges what comes back;
//! auto-matching takes whichever provider offers the most convincing title.
use crate::anilist::{AniList, best_match};
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
        // Concurrent, not sequential: AniList's retry ladder can run to minutes on a 429, and
        // awaiting it first would gate the healthy provider behind the sick one — the opposite
        // of why a second provider exists. join! (not try_join!) keeps one side's Ok on error.
        let (a, k) = tokio::join!(self.anilist.search(q), self.kitsu.search(q));
        let mut hits = Vec::new();
        let mut errors = Vec::new();
        // AniList first, so it wins an exact similarity tie in best_match.
        match a {
            Ok(h) => hits.extend(h),
            Err(e) => errors.push(format!("anilist: {e}")),
        }
        match k {
            Ok(h) => hits.extend(h),
            Err(e) => errors.push(format!("kitsu: {e}")),
        }
        (hits, errors)
    }

    /// Look one id up on a named provider. An unrecognised name is an error, never a silent
    /// fallback: AniList and Kitsu ids overlap heavily, so guessing resolves a real but
    /// unrelated show and Rename would then write its title onto disk.
    pub async fn by_id(&self, source: &str, id: i64) -> Result<Option<MetadataHit>> {
        match source {
            crate::anilist::SOURCE => self.anilist.by_id(id).await,
            crate::kitsu::SOURCE => self.kitsu.by_id(id).await,
            other => Err(crate::error::AppError::Network(format!("unknown metadata source {other:?}"))),
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

/// Match every unmatched show against whichever provider answers, then fetch its cover art.
pub async fn auto_match_all(
    db: std::sync::Arc<crate::db::Db>,
    providers: std::sync::Arc<Providers>,
    on_progress: impl Fn(crate::models::MatchProgress) + Send + 'static,
) -> usize {
    let pending = match db.shows_needing_match() {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let total = pending.len();
    let mut matched = 0;
    for (i, (show_id, title)) in pending.into_iter().enumerate() {
        let changed = match providers.best(&title).await {
            Some(hit) if db.set_anilist(show_id, &hit).is_ok() => {
                matched += 1;
                Some(show_id)
            }
            _ => None,
        };
        on_progress(crate::models::MatchProgress {
            done: i + 1,
            total,
            title,
            phase: "matching".into(),
            changed,
            running: true,
        });
        // Stay well under AniList's 90 requests/minute; Kitsu is slower than that anyway.
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
    // Matching is what reveals that two rows are the same series, so fold them now rather than
    // leaving the library showing one show twice with its watched state split between them.
    if let Ok(n) = db.merge_duplicate_shows()
        && n > 0
    {
        eprintln!("merged {n} duplicate show rows");
    }
    matched
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
    async fn matching_reports_every_step_and_counts_what_it_applied() {
        use crate::parser::ParsedName;
        use crate::scanner::RawFile;
        let (dead, live) = one_provider_down().await;
        let db = std::sync::Arc::new(crate::db::Db::open_memory().unwrap());
        for t in ["Sousou no Frieren", "Definitely Not A Real Anime XYZQ"] {
            let pn = ParsedName { title: t.into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
            db.upsert_episode(&pn, &RawFile { path: format!("/a/{t}.mkv").into(), size: 1, mtime: 1, stem: "".into(), dirs: vec![] }).unwrap();
        }
        let p = std::sync::Arc::new(Providers {
            anilist: AniList::with_endpoint(dead.uri()).with_retry_base(std::time::Duration::from_millis(1)),
            kitsu: Kitsu::with_endpoint(live.uri()),
        });
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let matched = auto_match_all(db.clone(), p, move |pr| s2.lock().unwrap().push(pr)).await;
        assert_eq!(matched, 1, "only the real title matches");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "every show reports, matched or not");
        assert_eq!((seen[0].done, seen[0].total), (1, 2));
        assert_eq!(seen[1].done, 2);
        assert_eq!(seen.iter().filter(|p| p.changed.is_some()).count(), 1);
        assert_eq!(db.shows_needing_match().unwrap().len(), 1, "the unmatched one stays pending");
        assert!(seen.iter().all(|p| p.running && p.phase == "matching"));
    }

    #[tokio::test]
    async fn an_unknown_source_is_rejected_rather_than_guessed() {
        let p = Providers::new();
        let e = p.by_id("Kitsu", 46474).await.unwrap_err();
        assert!(format!("{e}").contains("unknown metadata source"), "{e}");
        assert!(p.by_id("", 1).await.is_err());
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
