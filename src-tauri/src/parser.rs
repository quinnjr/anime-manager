use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedName {
    pub title: String,
    pub season: u32,
    pub episode: u32,
    pub release_group: Option<String>,
    pub resolution: Option<String>,
    pub crc: Option<String>,
}

static BRACKET: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[([^\]]*)\]|\(([^)]*)\)").unwrap());
static CRC: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9A-Fa-f]{8}$").unwrap());
static RES: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(\d{3,4}p|\d{3,4}x\d{3,4})\b").unwrap());
static SPECIAL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:\b|\d)(NCOP|NCED|OVA|OAD|SP|Special|Extra|Preview)(?:\s|\d|$)").unwrap());
static SPECIAL_NUM: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:\b|\d)(?:NCOP|NCED|OVA|OAD|SP|Special|Extra|Preview)\s?(\d{1,3})\b").unwrap());
static SXXEXX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bS(\d{1,2})[ ._]?E(\d{1,4})(?:v\d)?\b").unwrap());
static NXNN: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(\d{1,2})x(\d{1,4})(?:v\d)?\b").unwrap());
static DASH_EP: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\s-\s(\d{1,4})(?:v\d)?\b").unwrap());
static EP_PREFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(?:Episode|Ep?)\.?\s?(\d{1,4})(?:v\d)?\b").unwrap());
static JP_EP: Lazy<Regex> = Lazy::new(|| Regex::new(r"第(\d{1,4})[話话]").unwrap());
static TRAILING_NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)[\s_.-](\d{1,4})(?:v\d)?\s*$").unwrap());
static LEADING_NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^\s*(\d{1,4})(?:v\d)?\s*(?:[-_.]|\s|$)").unwrap());
static MID_NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\s(\d{2,3})(?:v\d)?\s").unwrap());
static SEASON_SUFFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\s+(?:(\d{1,2})(?:st|nd|rd|th)\s+Season|Season\s+(\d{1,2})|Part\s+(\d{1,2})|S(\d{1,2}))\s*$")
        .unwrap()
});
static ROMAN_SUFFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+(II|III|IV|V|VI|VII|VIII|IX)\s*$").unwrap());
static DIR_SEASON: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(?:Season\s*|S)(\d{1,2})$").unwrap());
static GENERIC_DIR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:Extras?|SPs?|Specials?|NCs?|Bonus|OVAs?|OADs?|Movies?|\..*)$|^(?:Season\s*\d+|S\d+)\b").unwrap()
});
static WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

/// Bare 4-digit numbers in the 1900–2099 range are release years, not episode numbers.
fn not_year(n: u32) -> Option<u32> {
    if (1900..=2099).contains(&n) { None } else { Some(n) }
}

fn roman(s: &str) -> u32 {
    match s {
        "II" => 2, "III" => 3, "IV" => 4, "V" => 5, "VI" => 6, "VII" => 7, "VIII" => 8, "IX" => 9,
        _ => 1,
    }
}

fn clean_title(s: &str) -> String {
    let s = s.replace(['_', '.'], " ");
    let s = WS.replace_all(&s, " ");
    s.trim().trim_matches(|c| c == '-' || c == ' ').trim().to_string()
}

/// Strip a trailing season suffix ("2nd Season", "Season 2", "Part 2", "S2", roman numeral)
/// from an already-cleaned title. Returns the season number if one was removed.
fn strip_season_suffix(title: &mut String) -> Option<u32> {
    if let Some(c) = SEASON_SUFFIX.captures(title) {
        let n = (1..=4).find_map(|i| c.get(i)).and_then(|m| m.as_str().parse().ok());
        let start = c.get(0).unwrap().start();
        title.truncate(start);
        *title = clean_title(title);
        n
    } else if let Some(c) = ROMAN_SUFFIX.captures(title) {
        let n = roman(&c[1]);
        let start = c.get(0).unwrap().start();
        title.truncate(start);
        *title = clean_title(title);
        Some(n)
    } else {
        None
    }
}

/// Derive a show title from the parent directory name when the file stem carries only an
/// episode marker (e.g. "S01E07-Title" or "01 - Title"). Generic folders such as "Extras",
/// "Season 1" or hidden ".unwanted" yield None.
fn title_from_parent(parent_dir: &str) -> Option<(String, Option<u32>)> {
    let parent = parent_dir.trim();
    if parent.is_empty() || GENERIC_DIR.is_match(parent) { return None; }
    let mut work = BRACKET.replace_all(parent, " ").to_string();
    if let Some(m) = RES.find(&work.clone()) { work.truncate(m.start()); }
    let mut title = clean_title(&work);
    let season = strip_season_suffix(&mut title);
    if title.is_empty() || GENERIC_DIR.is_match(&title) { return None; }
    Some((title, season))
}

pub fn parse(stem: &str, parent_dir: &str) -> Option<ParsedName> {
    // Pass 1: bracket tokens → group / crc / resolution
    let mut release_group = None;
    let mut crc = None;
    let mut resolution = None;
    for cap in BRACKET.captures_iter(stem) {
        let inner = cap.get(1).or_else(|| cap.get(2)).map(|m| m.as_str().trim()).unwrap_or("");
        if inner.is_empty() { continue; }
        if CRC.is_match(inner) { crc.get_or_insert(inner.to_string()); continue; }
        let spaced = inner.replace('_', " ");
        if let Some(m) = RES.find(&spaced) { resolution.get_or_insert(m.as_str().to_string()); continue; }
        if release_group.is_none() && cap.get(1).is_some() { release_group = Some(inner.to_string()); }
    }
    let work = BRACKET.replace_all(stem, " ").to_string();
    // Normalise `_` and `.` separators to spaces so every marker regex sees word boundaries.
    let mut work = work.replace(['_', '.'], " ");
    if resolution.is_none()
        && let Some(m) = RES.find(&work)
    {
        resolution = Some(m.as_str().to_string());
    }
    // Strip a trailing "1080p WEB x264"-style tail: everything from the resolution token onward.
    if let Some(m) = RES.find(&work.clone()) { work.truncate(m.start()); }

    let is_special = SPECIAL.is_match(&work);

    // Pass 2: season/episode markers
    let mut season: Option<u32> = None;
    let mut episode: Option<u32> = None;
    let mut title_end = work.len();

    if let Some(c) = SXXEXX.captures(&work) {
        season = Some(c[1].parse().ok()?);
        episode = Some(c[2].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = NXNN.captures(&work) {
        season = Some(c[1].parse().ok()?);
        episode = Some(c[2].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = JP_EP.captures(&work) {
        episode = Some(c[1].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = DASH_EP.captures(&work) {
        episode = Some(c[1].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = EP_PREFIX.captures(&work) {
        episode = Some(c[1].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = SPECIAL_NUM.captures(&work) {
        episode = Some(c[1].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = TRAILING_NUM.captures(&work).filter(|c| c[1].parse().ok().and_then(not_year).is_some()) {
        episode = Some(c[1].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = LEADING_NUM.captures(&work).filter(|c| c[1].parse().ok().and_then(not_year).is_some()) {
        episode = Some(c[1].parse().ok()?);
        title_end = 0;
    } else if let Some(c) = MID_NUM.captures(&work).filter(|c| c[1].parse().ok().and_then(not_year).is_some()) {
        episode = Some(c[1].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    }

    // Specials: episode number is whatever digits trail the special marker, else 1
    if is_special && episode.is_none() {
        episode = Some(1);
    }
    let episode = episode?;

    let mut title = work[..title_end].to_string();
    if is_special && let Some(m) = SPECIAL.find(&title) {
        title.truncate(m.start());
    }

    // Pass 3: season suffix in title
    let mut title = clean_title(&title);
    if season.is_none() {
        season = strip_season_suffix(&mut title);
    }

    // Pass 4: stem carried no title → fall back to the parent directory name
    if title.is_empty() {
        let (t, s) = title_from_parent(parent_dir)?;
        title = t;
        if season.is_none() { season = s; }
    }

    // Pass 5: fallbacks
    let season = if is_special {
        0
    } else {
        season.or_else(|| DIR_SEASON.captures(parent_dir.trim()).and_then(|c| c[1].parse().ok())).unwrap_or(1)
    };

    if title.is_empty() { return None; }

    Some(ParsedName { title, season, episode, release_group, resolution, crc })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(stem: &str) -> ParsedName { parse(stem, "").expect(stem) }

    #[test]
    fn subsplease_style() {
        let r = p("[SubsPlease] Frieren - Beyond Journey's End - 05 (1080p) [A1B2C3D4]");
        assert_eq!(r.title, "Frieren - Beyond Journey's End");
        assert_eq!((r.season, r.episode), (1, 5));
        assert_eq!(r.release_group.as_deref(), Some("SubsPlease"));
        assert_eq!(r.resolution.as_deref(), Some("1080p"));
        assert_eq!(r.crc.as_deref(), Some("A1B2C3D4"));
    }

    #[test]
    fn sxxexx() {
        let r = p("Mob.Psycho.100.S02E07.1080p.WEB.x264");
        assert_eq!(r.title, "Mob Psycho 100");
        assert_eq!((r.season, r.episode), (2, 7));
        assert_eq!(r.resolution.as_deref(), Some("1080p"));
    }

    #[test]
    fn nxnn() {
        let r = p("Spy x Family 2x03");
        assert_eq!(r.title, "Spy x Family");
        assert_eq!((r.season, r.episode), (2, 3));
    }

    #[test]
    fn second_season_suffix() {
        let r = p("[Erai-raws] Jujutsu Kaisen 2nd Season - 12 [1080p]");
        assert_eq!(r.title, "Jujutsu Kaisen");
        assert_eq!((r.season, r.episode), (2, 12));
    }

    #[test]
    fn season_word_suffix() {
        let r = p("Vinland Saga Season 2 - 04");
        assert_eq!(r.title, "Vinland Saga");
        assert_eq!((r.season, r.episode), (2, 4));
    }

    #[test]
    fn part_suffix() {
        let r = p("Attack on Titan Final Season Part 2 - 03");
        assert_eq!(r.title, "Attack on Titan Final Season");
        assert_eq!((r.season, r.episode), (2, 3));
    }

    #[test]
    fn roman_numeral_suffix() {
        let r = p("Oregairu III - 02");
        assert_eq!(r.title, "Oregairu");
        assert_eq!((r.season, r.episode), (3, 2));
    }

    #[test]
    fn version_suffix_dropped() {
        let r = p("[Group] Show - 08v2 [720p]");
        assert_eq!((r.season, r.episode), (1, 8));
        assert_eq!(r.resolution.as_deref(), Some("720p"));
    }

    #[test]
    fn ep_prefix() {
        assert_eq!(p("Cowboy Bebop Ep 11").episode, 11);
        assert_eq!(p("Cowboy Bebop Ep.11").episode, 11);
        assert_eq!(p("Cowboy Bebop E11").episode, 11);
    }

    #[test]
    fn japanese_episode_marker() {
        let r = p("葬送のフリーレン 第05話");
        assert_eq!(r.episode, 5);
        assert_eq!(r.title, "葬送のフリーレン");
    }

    #[test]
    fn season_from_parent_dir() {
        let r = parse("Show - 03", "Season 3").unwrap();
        assert_eq!(r.season, 3);
        let r = parse("Show - 03", "S4").unwrap();
        assert_eq!(r.season, 4);
        let r = parse("Show - 03", "Show Name").unwrap();
        assert_eq!(r.season, 1);
    }

    #[test]
    fn explicit_season_beats_parent_dir() {
        let r = parse("Show S02E03", "Season 5").unwrap();
        assert_eq!(r.season, 2);
    }

    #[test]
    fn trailing_number_fallback() {
        let r = p("Some_Show_07");
        assert_eq!(r.title, "Some Show");
        assert_eq!(r.episode, 7);
    }

    #[test]
    fn specials_go_to_season_zero() {
        assert_eq!(p("[Group] Show - NCOP1 [1080p]").season, 0);
        assert_eq!(p("[Group] Show - OVA 02").season, 0);
        assert_eq!(p("Show Special 1").season, 0);
    }

    #[test]
    fn resolution_wxh() {
        let r = p("Show - 01 (1920x1080)");
        assert_eq!(r.resolution.as_deref(), Some("1920x1080"));
    }

    #[test]
    fn no_episode_returns_none() {
        assert!(parse("random_video", "").is_none());
    }

    #[test]
    fn underscores_and_dots_to_spaces() {
        assert_eq!(p("Made_in_Abyss_-_04").title, "Made in Abyss");
        assert_eq!(p("Made.in.Abyss.-.04").title, "Made in Abyss");
    }

    #[test]
    fn title_with_number_not_treated_as_episode() {
        let r = p("[Group] Steins;Gate 0 - 04 [1080p]");
        assert_eq!(r.title, "Steins;Gate 0");
        assert_eq!(r.episode, 4);
    }

    #[test]
    fn hyphen_separated_with_group_crc_lowercase() {
        let r = p("[hc] Bocchi the Rock! - 01 [abcdef12]");
        assert_eq!(r.crc.as_deref(), Some("abcdef12"));
        assert_eq!(r.title, "Bocchi the Rock!");
    }

    #[test]
    fn three_digit_episode() {
        assert_eq!(p("One Piece - 1071").episode, 1071);
    }

    #[test]
    fn special_marker_must_be_a_whole_word() {
        let r = p("Extraordinary You - 04");
        assert_eq!(r.title, "Extraordinary You");
        assert_eq!((r.season, r.episode), (1, 4));
    }

    // --- real-world shapes from a 3,600-file collection ---

    #[test]
    fn underscore_separated_dash_episode() {
        let r = p("[Hatsuyuki]_Kotoura-san_-_06_[BD][1920x1080][D2B10246]");
        assert_eq!(r.title, "Kotoura-san");
        assert_eq!((r.season, r.episode), (1, 6));
        assert_eq!(r.release_group.as_deref(), Some("Hatsuyuki"));
        let r = p("[DB]Gekkan Shoujo Nozaki-kun_-_01_(Dual Audio_10bit_BD1080p_x265)");
        assert_eq!(r.title, "Gekkan Shoujo Nozaki-kun");
        assert_eq!(r.episode, 1);
        let r = p("(Hi10)_High_School_DxD_BorN_-_09_(BD_1080p)_(FFF)");
        assert_eq!(r.title, "High School DxD BorN");
        assert_eq!(r.episode, 9);
    }

    #[test]
    fn episode_word() {
        let r = p("Azumanga Daioh - Episode 01 - Miss Yukari");
        assert_eq!(r.title, "Azumanga Daioh");
        assert_eq!((r.season, r.episode), (1, 1));
        let r = p("[AniDL] Manyuu Hikenchou - Episode 03 [720p BD][English Subbed]");
        assert_eq!(r.title, "Manyuu Hikenchou");
        assert_eq!(r.episode, 3);
        let r = p("[bonkai77].Space.Dandy.Episode.01.Live.with.the.Flow,.Baby.1080p.Dual.Audio.Bluray");
        assert_eq!(r.title, "Space Dandy");
        assert_eq!(r.episode, 1);
    }

    #[test]
    fn ep_prefix_with_underscores() {
        let r = p("[Exiled-Destiny]_Negima_Ep07_(D568C690)");
        assert_eq!(r.title, "Negima");
        assert_eq!(r.episode, 7);
        assert_eq!(r.crc.as_deref(), Some("D568C690"));
        let r = p("[~AA~]_Kashimashi_-_Girl_Meets_Girl_ep08_[ABC5136F]");
        assert_eq!(r.title, "Kashimashi - Girl Meets Girl");
        assert_eq!(r.episode, 8);
    }

    #[test]
    fn trailing_number_with_underscore_after() {
        let r = p("[Coalgirls]_Aikatsu_001_(1280x720_Blu-ray_FLAC)_[556A4483]");
        assert_eq!(r.title, "Aikatsu");
        assert_eq!(r.episode, 1);
        assert_eq!(r.resolution.as_deref(), Some("1280x720"));
    }

    #[test]
    fn stem_is_only_episode_uses_parent_dir_for_title() {
        let r = parse("S01E07-Thus, the Sisters Trade Places [F29C75F9]", "Makina-san's a Love Bot S01 1080p Dual Audio WEBRip AAC x265-EMBER").unwrap();
        assert_eq!(r.title, "Makina-san's a Love Bot");
        assert_eq!((r.season, r.episode), (1, 7));
        let r = parse("01- He woke up as a Bagel Girl [darkflux]", "Bagel Girl").unwrap();
        assert_eq!(r.title, "Bagel Girl");
        assert_eq!(r.episode, 1);
        let r = parse("06 - Operation Seduce Sang Woo [darkflux]", "Bagel Girl").unwrap();
        assert_eq!(r.episode, 6);
        let r = parse("05_The Strange Tale of Maison Izumo_KDG", "Sekirei").unwrap();
        assert_eq!(r.title, "Sekirei");
        assert_eq!(r.episode, 5);
        let r = parse("[WBDP] 02 - Fever - Campaigning for Love [BD][1080p-FLAC][HEVC] [3B622F30]", "[WBDP] Yagate Kimi ni Naru [BD][1080p-FLAC][HEVC]").unwrap();
        assert_eq!(r.title, "Yagate Kimi ni Naru");
        assert_eq!(r.episode, 2);
        assert_eq!(r.release_group.as_deref(), Some("WBDP"));
    }

    #[test]
    fn empty_title_and_generic_parent_is_unparseable() {
        assert!(parse("NCED1", "Extra").is_none());
        assert!(parse("NCED - 01", "NC").is_none());
        assert!(parse("S01E02", "Season 1").is_none());
        assert!(parse("Episode 4", "Extras").is_none());
        assert!(parse("00_OVA_Two-Topic Gossip_KDG", "Season2_Pure Engagement").is_none());
    }

    #[test]
    fn mid_title_standalone_number() {
        let r = p("Tamako Market 01 That Girl is the Cute Daughter of a Mochi Shop Owner (BD1080p AC3 10bit)");
        assert_eq!(r.title, "Tamako Market");
        assert_eq!(r.episode, 1);
        let r = p("[Underwater] Panty and Stocking with Garterbelt 13 - Bitch Girls - Bitch Girls 2 Bitch (BD 720p) [3A1C8DA9]");
        assert_eq!(r.title, "Panty and Stocking with Garterbelt");
        assert_eq!(r.episode, 13);
    }

    #[test]
    fn special_glued_to_season_marker() {
        let r = p("[Reza] Oreimo - S01OVA01");
        assert_eq!(r.title, "Oreimo");
        assert_eq!((r.season, r.episode), (0, 1));
        let r = p("[-__-'] Shakunetsu no Takkyuu Musume - SP1 [BD 720p] [D9848E2E]");
        assert_eq!(r.title, "Shakunetsu no Takkyuu Musume");
        assert_eq!((r.season, r.episode), (0, 1));
        let r = p("[DB]Gekkan Shoujo Nozaki-kun_-_NCED_(10bit_BD1080p_x265)");
        assert_eq!(r.title, "Gekkan Shoujo Nozaki-kun");
        assert_eq!(r.season, 0);
    }

    #[test]
    fn special_word_after_episode_title_still_season_zero() {
        let r = p("Azumanga Daioh - Episode 08 - New Years Dream Special");
        assert_eq!(r.title, "Azumanga Daioh");
        assert_eq!((r.season, r.episode), (0, 8));
    }

    #[test]
    fn year_is_not_an_episode() {
        assert!(parse("Akira (1988) [30th Anniversary Blu-ray] [1080p x265 HEVC 10bit 5.1 AAC][RecMan]", "").is_none());
        assert!(parse("Cosmic.Princess.Kaguya.2026.1080p.NF.WEB-DL.DUAL.DDP5.1.H.264-VARYG", "").is_none());
    }
}
