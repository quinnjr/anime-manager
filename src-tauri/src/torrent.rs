use crate::db::Db;
use crate::error::{AppError, Result};

pub const BASE_URL_KEY: &str = "torrent_base_url";
pub const PASSWORD_KEY: &str = "torrent_password";
pub const TEST_OK_KEY: &str = "torrent_test_ok";

pub fn test_ok(db: &Db) -> bool {
    db.get_setting(TEST_OK_KEY).map(|v| v.as_deref() == Some("true")).unwrap_or(false)
}
pub fn mark_tested(db: &Db, ok: bool) -> Result<()> {
    db.set_setting(TEST_OK_KEY, if ok { "true" } else { "false" })
}
pub fn invalidates_test(key: &str) -> bool {
    matches!(key, k if k == BASE_URL_KEY || k == PASSWORD_KEY)
}
/// Deterministic feed label: re-subscribe resolves to "already subscribed".
pub fn feed_label(parsed_title: &str) -> String {
    format!("animemgr:{parsed_title}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arming_and_label() {
        let db = Db::open_memory().unwrap();
        assert!(!test_ok(&db));
        mark_tested(&db, true).unwrap();
        assert!(test_ok(&db));
        assert!(invalidates_test("torrent_base_url"));
        assert!(invalidates_test("torrent_password"));
        assert!(!invalidates_test("torrent_delay_ms"));
        assert_eq!(feed_label("Sousou no Frieren"), "animemgr:Sousou no Frieren");
    }
}
