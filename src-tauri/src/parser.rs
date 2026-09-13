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
const SPECIAL_WORDS: &str = r"NCOP|NCED|NCI|Creditless\s?(?:Opening|Ending)|Clean\s?(?:Opening|Ending)|OVA|OAD|OP|ED|SP|Special|Extra|Preview|Recap|Menu|CM|PV|Teaser|Trailer|CharSong|Eyecatch(?:es)?";
static SPECIAL: Lazy<Regex> =
    Lazy::new(|| Regex::new(&format!(r"(?i)(?:\b|\d)({SPECIAL_WORDS})(?:\s|\d|v\d|$)")).unwrap());
static SPECIAL_NUM: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r"(?i)(?:\b|\d)(?:{SPECIAL_WORDS})\s?(\d{{1,3}})(?:v\d)?\b"
    ))
    .unwrap()
});
static SXXEXX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bS(\d{1,2})[ ._]?E(\d{1,4})(?:v\d)?\b").unwrap());
static NXNN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(\d{1,2})x(\d{1,4})(?:v\d)?\b").unwrap());
static DASH_EP: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\s-\s(\d{1,4})(?:v\d)?\b").unwrap());
static EP_PREFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(?:Episode|Ep?)\.?\s?(\d{1,4})(?:v\d)?\b").unwrap());
static JP_EP: Lazy<Regex> = Lazy::new(|| Regex::new(r"第(\d{1,4})[話话]").unwrap());
static TRAILING_NUM: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)[\s_.-](\d{1,4})(?:v\d)?\s*$").unwrap());
static LEADING_NUM: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\s*(\d{1,4})(?:v\d)?\s*(?:[-_.]|\s|$)").unwrap());
static MID_NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\s(\d{2,3})(?:v\d)?\s").unwrap());
static SEASON_SUFFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\s+(?:(\d{1,2})(?:st|nd|rd|th)\s+Season|Season\s+(\d{1,2})|Part\s+(\d{1,2})|S(\d{1,2}))\s*$")
        .unwrap()
});
static ROMAN_SUFFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\s+(II|III|IV|V|VI|VII|VIII|IX)\s*$").unwrap());
static DIR_SEASON: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(?:Season\s*|S)(\d{1,2})(?:[^0-9]|$)").unwrap());
static GENERIC_DIR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:Extras?|SPs?|Specials?|NCs?|Bonus|OVAs?|OADs?|Movies?|The Movie|\..*)$|^(?:Season\s*\d+|S\d+)(?:[^0-9]|$)").unwrap()
});
/// Release-metadata words that commonly trail a title in folder or file names.
static NOISE_TAIL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:[\s\-]+(?:Complete|Batch|BD-?Rip|BD|Blu-?ray|WEB-?DL|WEB-?Rip|WEB|Dual[\s-]?Audio|x264|x265|HEVC|AVC|H\.?26[45]|AAC|FLAC|OPUS|AC3|10-?bits?|Hi10P?|DVD-?Rip|DVD|Uncensored|Subbed|Dubbed|Eng(?:lish)?\s*Subs?|Multi-?Subs?|Remux|\d{3,4}p|\(?(?:19|20)\d\d\)?))+\s*$").unwrap()
});
static JUNK_STEM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^\s*(?:sample|test)\s*$").unwrap());
static WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

/// Bare 4-digit numbers in the 1900–2099 range are release years, not episode numbers.
fn not_year(n: u32) -> Option<u32> {
    if (1900..=2099).contains(&n) {
        None
    } else {
        Some(n)
    }
}

fn roman(s: &str) -> u32 {
    match s {
        "II" => 2,
        "III" => 3,
        "IV" => 4,
        "V" => 5,
        "VI" => 6,
        "VII" => 7,
        "VIII" => 8,
        "IX" => 9,
        _ => 1,
    }
}

fn clean_title(s: &str) -> String {
    let s = s.replace(['_', '.'], " ");
    let s = WS.replace_all(&s, " ");
    let s = s.trim().trim_matches(|c| c == '-' || c == ' ').trim();
    let s = NOISE_TAIL.replace(s, "");
    s.trim()
        .trim_matches(|c| c == '-' || c == ' ')
        .trim()
        .to_string()
}

/// Strip a trailing season suffix ("2nd Season", "Season 2", "Part 2", "S2", roman numeral)
/// from an already-cleaned title. Returns the season number if one was removed.
fn strip_season_suffix(title: &mut String) -> Option<u32> {
    if let Some(c) = SEASON_SUFFIX.captures(title) {
        let n = (1..=4)
            .find_map(|i| c.get(i))
            .and_then(|m| m.as_str().parse().ok());
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

/// Derive a show title from the nearest ancestor directory that is not a generic folder
/// ("Extras", "Season 1", ".unwanted", ...). Used when the file stem carries only an episode
/// marker such as "S01E07-Title" or "01 - Title".
fn title_from_dirs(dirs: &[String]) -> Option<(String, Option<u32>)> {
    for d in dirs {
        let d = d.trim();
        if d.is_empty() || GENERIC_DIR.is_match(d) {
            continue;
        }
        let mut work = BRACKET.replace_all(d, " ").to_string();
        if let Some(m) = RES.find(&work.clone()) {
            work.truncate(m.start());
        }
        let mut title = clean_title(&work);
        let season = strip_season_suffix(&mut title);
        if title.is_empty() || GENERIC_DIR.is_match(&title) {
            continue;
        }
        return Some((title, season));
    }
    None
}

fn season_from_dirs(dirs: &[String]) -> Option<u32> {
    dirs.iter().find_map(|d| {
        DIR_SEASON
            .captures(d.trim())
            .and_then(|c| c[1].parse().ok())
    })
}

/// A parse result plus whether it came from a weak heuristic worth a second opinion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub name: ParsedName,
    /// True when the title came from an ancestor directory, the stem had no episode marker
    /// (movie / one-shot), or the episode came from the leading- or mid-number fallback.
    pub low_confidence: bool,
}

/// Parse a video file stem. `dirs` are the ancestor directory names, nearest first, up to and
/// including the scan root; they supply the season ("Season 2") and, when the stem has no title
/// of its own, the show name.
pub fn parse(stem: &str, dirs: &[String]) -> Option<ParsedName> {
    parse_with_confidence(stem, dirs).map(|p| p.name)
}

pub fn parse_with_confidence(stem: &str, dirs: &[String]) -> Option<Parsed> {
    if JUNK_STEM.is_match(stem) {
        return None;
    }
    let mut low_confidence = false;
    // Pass 1: bracket tokens → group / crc / resolution / special marker
    let mut release_group = None;
    let mut crc = None;
    let mut resolution = None;
    let mut bracket_special: Option<Option<u32>> = None;
    for cap in BRACKET.captures_iter(stem) {
        let inner = cap
            .get(1)
            .or_else(|| cap.get(2))
            .map(|m| m.as_str().trim())
            .unwrap_or("");
        if inner.is_empty() {
            continue;
        }
        if CRC.is_match(inner) {
            crc.get_or_insert(inner.to_string());
            continue;
        }
        let spaced = inner.replace('_', " ");
        if let Some(m) = RES.find(&spaced) {
            resolution.get_or_insert(m.as_str().to_string());
            continue;
        }
        if SPECIAL.find(&spaced).is_some_and(|m| m.start() == 0) {
            let n = SPECIAL_NUM
                .captures(&spaced)
                .and_then(|c| c[1].parse().ok());
            bracket_special.get_or_insert(n);
            continue;
        }
        if release_group.is_none() && cap.get(1).is_some() {
            release_group = Some(inner.to_string());
        }
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
    if let Some(m) = RES.find(&work.clone()) {
        work.truncate(m.start());
    }

    // Pass 2: season/episode markers. `marker` records the matched span so a special keyword
    // can be recognised only where a marker may legitimately sit — anywhere else it belongs to
    // the episode's own subtitle ("Grand Blue - 03 - The Extra Class" is season 1, not a special).
    let mut season: Option<u32> = None;
    let mut episode: Option<u32> = None;
    let mut title_end = work.len();
    let mut marker: Option<(usize, usize)> = None;
    // A 4-digit number in the 1900-2099 range is a release year in every branch, not an episode.
    let ep_ok =
        |c: &regex::Captures, i: usize| c[i].parse::<u32>().ok().and_then(not_year).is_some();

    // Ordered, first match wins. What separated the branches was always data, not logic: which
    // group holds a season, whether the year filter applies (a special's number is never a year),
    // whether the marker leads the name rather than ending the title, and whether the guess is
    // weak enough to send the folder to the LLM assist.
    let markers: [(&Lazy<Regex>, Option<usize>, usize, bool, bool, bool); 9] = [
        (&SXXEXX, Some(1), 2, true, false, false),
        (&NXNN, Some(1), 2, true, false, false),
        (&JP_EP, None, 1, true, false, false),
        (&DASH_EP, None, 1, true, false, false),
        (&EP_PREFIX, None, 1, true, false, false),
        (&SPECIAL_NUM, None, 1, false, false, false),
        (&TRAILING_NUM, None, 1, true, false, false),
        (&LEADING_NUM, None, 1, true, true, true),
        (&MID_NUM, None, 1, true, false, true),
    ];
    for (re, season_group, ep_group, year_filter, marker_leads, weak) in markers {
        let Some(c) = re
            .captures(&work)
            .filter(|c| !year_filter || ep_ok(c, ep_group))
        else {
            continue;
        };
        if let Some(i) = season_group {
            season = Some(c[i].parse().ok()?);
        }
        episode = Some(c[ep_group].parse().ok()?);
        let m = c.get(0).unwrap();
        title_end = if marker_leads { 0 } else { m.start() };
        marker = Some((m.start(), m.end()));
        low_confidence |= weak;
        break;
    }

    // Where a special keyword is allowed to count: the marker itself plus anything glued to it
    // up to the next " - " (which introduces an episode subtitle). With no marker, the whole stem.
    let special_region: &str = match marker {
        Some((_, mend)) => {
            let tail = &work[title_end..];
            let rel = mend - title_end;
            let cut = tail[rel..]
                .find(" - ")
                .map(|i| rel + i)
                .unwrap_or(tail.len());
            &tail[..cut]
        }
        None => &work,
    };
    // A title that is nothing but a marker ("NCED - 03") is a special; one that merely starts
    // with the word ("Special A - 01", "Extra Olympia Kyklos") is a real show title.
    let title_text = &work[..title_end];
    let bare_marker = SPECIAL.find(title_text).is_some_and(|m| {
        let rest = format!("{}{}", &title_text[..m.start()], &title_text[m.end()..]);
        clean_title(&rest).is_empty()
    });
    let is_special = bracket_special.is_some() || SPECIAL.is_match(special_region) || bare_marker;

    // Specials: episode number is whatever digits trail the special marker, else 1
    if is_special && episode.is_none() {
        episode = bracket_special.flatten().or(Some(1));
    }

    let mut title = title_text.to_string();
    if is_special
        && (marker.is_none() || bare_marker)
        && let Some(m) = SPECIAL.find(&title)
    {
        title.truncate(m.start());
    }

    // Pass 3: season suffix in title
    let mut title = clean_title(&title);
    let mut season_from_bare_s = false;
    if season.is_none() {
        let had_s = SEASON_SUFFIX
            .captures(&title)
            .map(|c| c.get(4).is_some())
            .unwrap_or(false);
        season = strip_season_suffix(&mut title);
        season_from_bare_s = had_s && season.is_some();
    }

    // A bare "Show - S3" with no episode anywhere is a numbered special, not season 3.
    let (is_special, episode) = match episode {
        Some(e) => (is_special, e),
        None if season_from_bare_s => (true, season.take().unwrap()),
        // Title-only file (movie, one-shot): a single-episode show.
        None => {
            if !is_special {
                low_confidence = true;
            }
            (is_special, 1)
        }
    };

    // Pass 4: stem carried no title → fall back to the ancestor directories
    if title.is_empty() {
        let (t, s) = title_from_dirs(dirs)?;
        title = t;
        if season.is_none() {
            season = s;
        }
        low_confidence = true;
    }

    // Pass 5: fallbacks
    let season = if is_special {
        0
    } else {
        season.or_else(|| season_from_dirs(dirs)).unwrap_or(1)
    };

    if title.is_empty() {
        return None;
    }

    Some(Parsed {
        name: ParsedName {
            title,
            season,
            episode,
            release_group,
            resolution,
            crc,
        },
        low_confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(stem: &str) -> ParsedName {
        parse(stem, &[]).expect(stem)
    }
    fn d(dir: &str) -> Vec<String> {
        vec![dir.to_string()]
    }

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
        let r = parse("Show - 03", &d("Season 3")).unwrap();
        assert_eq!(r.season, 3);
        let r = parse("Show - 03", &d("S4")).unwrap();
        assert_eq!(r.season, 4);
        let r = parse("Show - 03", &d("Show Name")).unwrap();
        assert_eq!(r.season, 1);
    }

    #[test]
    fn explicit_season_beats_parent_dir() {
        let r = parse("Show S02E03", &d("Season 5")).unwrap();
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
    fn title_only_file_is_a_single_episode_show() {
        let r = p("K-ON! The Movie");
        assert_eq!(r.title, "K-ON! The Movie");
        assert_eq!((r.season, r.episode), (1, 1));
        let r = p("[B-Bass] Seitokai Yakuindomo The Movie [2EC03FB6]");
        assert_eq!(r.title, "Seitokai Yakuindomo The Movie");
        assert_eq!(r.crc.as_deref(), Some("2EC03FB6"));
        assert!(parse("sample", &[]).is_none());
        assert!(parse("random_video", &d("")).is_some());
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
        let r =
            p("[bonkai77].Space.Dandy.Episode.01.Live.with.the.Flow,.Baby.1080p.Dual.Audio.Bluray");
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
        let r = parse(
            "S01E07-Thus, the Sisters Trade Places [F29C75F9]",
            &d("Makina-san's a Love Bot S01 1080p Dual Audio WEBRip AAC x265-EMBER"),
        )
        .unwrap();
        assert_eq!(r.title, "Makina-san's a Love Bot");
        assert_eq!((r.season, r.episode), (1, 7));
        let r = parse(
            "01- He woke up as a Bagel Girl [darkflux]",
            &d("Bagel Girl"),
        )
        .unwrap();
        assert_eq!(r.title, "Bagel Girl");
        assert_eq!(r.episode, 1);
        let r = parse(
            "06 - Operation Seduce Sang Woo [darkflux]",
            &d("Bagel Girl"),
        )
        .unwrap();
        assert_eq!(r.episode, 6);
        let r = parse("05_The Strange Tale of Maison Izumo_KDG", &d("Sekirei")).unwrap();
        assert_eq!(r.title, "Sekirei");
        assert_eq!(r.episode, 5);
        let r = parse(
            "[WBDP] 02 - Fever - Campaigning for Love [BD][1080p-FLAC][HEVC] [3B622F30]",
            &d("[WBDP] Yagate Kimi ni Naru [BD][1080p-FLAC][HEVC]"),
        )
        .unwrap();
        assert_eq!(r.title, "Yagate Kimi ni Naru");
        assert_eq!(r.episode, 2);
        assert_eq!(r.release_group.as_deref(), Some("WBDP"));
    }

    #[test]
    fn empty_title_and_generic_parent_is_unparseable() {
        assert!(parse("NCED1", &d("Extra")).is_none());
        assert!(parse("NCED - 01", &d("NC")).is_none());
        assert!(parse("S01E02", &d("Season 1")).is_none());
        assert!(parse("Episode 4", &d("Extras")).is_none());
        assert!(parse("00_OVA_Two-Topic Gossip_KDG", &d("Season2_Pure Engagement")).is_none());
        assert!(parse("K-ON! The Movie", &d("The Movie")).is_some());
    }

    #[test]
    fn mid_title_standalone_number() {
        let r = p(
            "Tamako Market 01 That Girl is the Cute Daughter of a Mochi Shop Owner (BD1080p AC3 10bit)",
        );
        assert_eq!(r.title, "Tamako Market");
        assert_eq!(r.episode, 1);
        let r = p(
            "[Underwater] Panty and Stocking with Garterbelt 13 - Bitch Girls - Bitch Girls 2 Bitch (BD 720p) [3A1C8DA9]",
        );
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
    fn special_word_in_an_episode_subtitle_is_not_a_special() {
        // The keyword sits in the episode's own subtitle, after the marker, so it must not
        // drag a real season-1 episode into the Specials tab.
        let r = p("Azumanga Daioh - Episode 08 - New Years Dream Special");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("Azumanga Daioh", 1, 8)
        );
        let r = p("Grand Blue Dreaming - 03 - The Extra Class");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("Grand Blue Dreaming", 1, 3)
        );
        let r = p("Bocchi the Rock - 07 - Live Special");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("Bocchi the Rock", 1, 7)
        );
        let r = p("Show - 12 - The Recap Job");
        assert_eq!((r.title.as_str(), r.season, r.episode), ("Show", 1, 12));
    }

    #[test]
    fn titles_that_begin_with_a_special_keyword_survive() {
        // Truncating at the keyword used to empty these titles and drop the files entirely.
        let r = p("Special A - 01");
        assert_eq!((r.title.as_str(), r.season, r.episode), ("Special A", 1, 1));
        let r = p("Extra Olympia Kyklos - 03");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("Extra Olympia Kyklos", 1, 3)
        );
        let r = p("Preview Girls - 05");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("Preview Girls", 1, 5)
        );
        let r = p("OP-ED Collection - 02");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("OP-ED Collection", 1, 2)
        );
        // ...but a title that is nothing except the keyword really is a special.
        let dirs = vec!["NC".to_string(), "Show Name".to_string()];
        let r = parse("NCED - 03", &dirs).unwrap();
        assert_eq!((r.title.as_str(), r.season, r.episode), ("Show Name", 0, 3));
    }

    #[test]
    fn years_are_never_episodes_in_any_branch() {
        for stem in [
            "Kimi no Na wa - 2016 - 1080p",
            "Akira - 1988",
            "Ghost in the Shell - 1995",
            "Show Ep 2016",
        ] {
            let r = parse_with_confidence(stem, &[]).expect(stem);
            assert!(
                r.name.episode < 1900,
                "{stem} parsed episode {}",
                r.name.episode
            );
            assert!(
                r.low_confidence,
                "{stem} must be flagged so the LLM safety net revisits it"
            );
        }
        // A genuine 4-digit episode number is still accepted.
        assert_eq!(p("Long Show - 1234").episode, 1234);
    }

    #[test]
    fn year_is_not_an_episode_and_is_dropped_from_title() {
        let r =
            p("Akira (1988) [30th Anniversary Blu-ray] [1080p x265 HEVC 10bit 5.1 AAC][RecMan]");
        assert_eq!(r.title, "Akira");
        assert_eq!((r.season, r.episode), (1, 1));
        let r = p("Cosmic.Princess.Kaguya.2026.1080p.NF.WEB-DL.DUAL.DDP5.1.H.264-VARYG");
        assert_eq!(r.title, "Cosmic Princess Kaguya");
        assert_eq!((r.season, r.episode), (1, 1));
    }

    #[test]
    fn title_from_grandparent_when_parent_is_a_season_folder() {
        let dirs = vec![
            "Season1".to_string(),
            "Sekirei Complete BDrip 1080p Dual-Audio x265".to_string(),
        ];
        let r = parse("05_The Strange Tale of Maison Izumo_KDG", &dirs).unwrap();
        assert_eq!(r.title, "Sekirei");
        assert_eq!((r.season, r.episode), (1, 5));
        let dirs = vec![
            "Season2_Pure Engagement".to_string(),
            "Sekirei Complete BDrip 1080p Dual-Audio x265".to_string(),
        ];
        let r = parse("01_Silent Omen_KDG", &dirs).unwrap();
        assert_eq!(r.title, "Sekirei");
        assert_eq!((r.season, r.episode), (2, 1));
        let r = parse("00_OVA_Two-Topic Gossip_KDG", &dirs).unwrap();
        assert_eq!((r.title.as_str(), r.season, r.episode), ("Sekirei", 0, 0));
        let dirs = vec![
            "NC".into(),
            "Extras".into(),
            "[KH] Why the Hell are You Here Teacher (BD 1080p) [Dual-Audio]".into(),
        ];
        let r = parse("NCED - 03", &dirs).unwrap();
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("Why the Hell are You Here Teacher", 0, 3)
        );
        let dirs = vec!["Extra".into(), "S2".into(), "K-On!".into()];
        let r = parse("NCED2", &dirs).unwrap();
        assert_eq!((r.title.as_str(), r.season, r.episode), ("K-On!", 0, 2));
    }

    #[test]
    fn op_ed_and_bracketed_special_words() {
        let r = p("(Hi10)_High_School_DxD_New_-_OP1_(BD_1080p)_(FFF)");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("High School DxD New", 0, 1)
        );
        let r = p("[Exiled-Destiny]_UFO_Ultramaiden_Valkyrie_2_Clean_Ending_(FFAE0F4B)");
        assert_eq!(
            (r.title.as_str(), r.season),
            ("UFO Ultramaiden Valkyrie 2", 0)
        );
        let r = p("[grimf] Ichigo Mashimaro CharSong3 Matsuri");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("Ichigo Mashimaro", 0, 3)
        );
        let r = p("[Airota&VCB-Studio] Asagao to Kase-san. [Teaser][Ma10p_1080p][x265_flac]");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("Asagao to Kase-san", 0, 1)
        );
        let r = p("[Doki] Mayo Chiki! - NCEDv2 (1920x1080 Hi10P BD FLAC) [DB77308F]");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("Mayo Chiki!", 0, 1)
        );
        let r = p("[Scum] Girlish Number - NCI [BD][4BE57A27]");
        assert_eq!((r.title.as_str(), r.season), ("Girlish Number", 0));
    }

    #[test]
    fn a_version_suffix_does_not_split_a_special_from_its_siblings() {
        // "Special 01v2" and "Special 02" sit in one folder and must land in the same season.
        let a = p("[Commie] Yuyushiki - Special 01v2 [BD 720p AAC] [0BA75EEF]");
        let b = p("[Commie] Yuyushiki - Special 02 [BD 720p AAC] [1AD2AC62]");
        assert_eq!((a.title.as_str(), a.season, a.episode), ("Yuyushiki", 0, 1));
        assert_eq!((b.title.as_str(), b.season, b.episode), ("Yuyushiki", 0, 2));
        assert_eq!(a.title, b.title);
    }

    #[test]
    fn bare_s_number_without_episode_is_a_special() {
        let r = p("(Hi10)_High_School_DxD_BorN_-_S5_(BD_1080p)_(CBM)");
        assert_eq!(
            (r.title.as_str(), r.season, r.episode),
            ("High School DxD BorN", 0, 5)
        );
    }

    #[test]
    fn noise_tail_stripped_from_title() {
        let r = p("[Exiled-Destiny]_Aria_Season_1_The_Animation_Ep01_Subbed_(9A11D5BB)");
        assert_eq!(r.title, "Aria Season 1 The Animation");
        assert_eq!(r.episode, 1);
        let r = p("Sekirei Complete BDrip 1080p Dual-Audio x265 - 03");
        assert_eq!(r.title, "Sekirei");
    }

    #[test]
    fn confidence_flags_weak_heuristics() {
        let lc = |stem: &str, dirs: &[&str]| {
            let d: Vec<String> = dirs.iter().map(|s| s.to_string()).collect();
            parse_with_confidence(stem, &d).expect(stem).low_confidence
        };
        assert!(!lc("[SubsPlease] Frieren - 05 (1080p) [A1B2C3D4]", &[]));
        assert!(!lc("Mob.Psycho.100.S02E07.1080p.WEB.x264", &[]));
        assert!(!lc("[Group] Show - NCOP1 [1080p]", &[]));
        assert!(lc("K-ON! The Movie", &[]));
        assert!(lc(
            "S01E07-Thus, the Sisters Trade Places",
            &["Makina-san's a Love Bot S01"]
        ));
        assert!(lc(
            "01- He woke up as a Bagel Girl [darkflux]",
            &["Bagel Girl"]
        ));
        assert!(lc(
            "Tamako Market 01 That Girl is the Cute Daughter of a Mochi Shop Owner",
            &[]
        ));
    }
}
