//! Embedded language catalog, language lookup and `Accept-Language` negotiation.
//!
//! No external crates are used: the catalog is a plain static table and the
//! negotiation code is a small hand-rolled parser.

/// Fallback language code used when nothing else matches.
pub const DEFAULT_CODE: &str = "en";

/// A single catalog entry: BCP-47-ish code, native display name, greeting.
#[derive(Debug, PartialEq, Eq)]
pub struct Lang {
    /// Lowercase primary language subtag, e.g. `nl`, `zh`.
    pub code: &'static str,
    /// Name of the language written in that language.
    pub name: &'static str,
    /// The hello-world greeting in that language.
    pub greeting: &'static str,
}

/// The embedded catalog. Order is preserved for `--list-langs`.
pub static CATALOG: &[Lang] = &[
    Lang {
        code: "en",
        name: "English",
        greeting: "Hello, World!",
    },
    Lang {
        code: "nl",
        name: "Nederlands",
        greeting: "Hallo, wereld!",
    },
    Lang {
        code: "de",
        name: "Deutsch",
        greeting: "Hallo, Welt!",
    },
    Lang {
        code: "fr",
        name: "Français",
        greeting: "Bonjour le monde !",
    },
    Lang {
        code: "es",
        name: "Español",
        greeting: "¡Hola, Mundo!",
    },
    Lang {
        code: "it",
        name: "Italiano",
        greeting: "Ciao, mondo!",
    },
    Lang {
        code: "pt",
        name: "Português",
        greeting: "Olá, mundo!",
    },
    Lang {
        code: "pl",
        name: "Polski",
        greeting: "Witaj, świecie!",
    },
    Lang {
        code: "sv",
        name: "Svenska",
        greeting: "Hej, världen!",
    },
    Lang {
        code: "da",
        name: "Dansk",
        greeting: "Hej, verden!",
    },
    Lang {
        code: "fi",
        name: "Suomi",
        greeting: "Hei, maailma!",
    },
    Lang {
        code: "nb",
        name: "Norsk bokmål",
        greeting: "Hei, verden!",
    },
    Lang {
        code: "ru",
        name: "Русский",
        greeting: "Привет, мир!",
    },
    Lang {
        code: "uk",
        name: "Українська",
        greeting: "Привіт, світе!",
    },
    Lang {
        code: "ja",
        name: "日本語",
        greeting: "こんにちは世界！",
    },
    Lang {
        code: "zh",
        name: "中文",
        greeting: "你好，世界！",
    },
    Lang {
        code: "ko",
        name: "한국어",
        greeting: "안녕하세요, 세계!",
    },
    Lang {
        code: "ar",
        name: "العربية",
        greeting: "مرحبا بالعالم!",
    },
    Lang {
        code: "he",
        name: "עברית",
        greeting: "שלום, עולם!",
    },
    Lang {
        code: "tr",
        name: "Türkçe",
        greeting: "Merhaba, Dünya!",
    },
    Lang {
        code: "el",
        name: "Ελληνικά",
        greeting: "Γεια σου, κόσμε!",
    },
    Lang {
        code: "cs",
        name: "Čeština",
        greeting: "Ahoj, světe!",
    },
];

/// Reduce a language tag to its lowercase primary subtag.
///
/// `nl-NL` -> `nl`, `pt_BR` -> `pt`, `EN` -> `en`.
pub fn normalize(tag: &str) -> String {
    let tag = tag.trim();
    let primary = tag.split(['-', '_']).next().unwrap_or("");
    primary.to_ascii_lowercase()
}

/// Case-insensitive lookup by primary subtag.
///
/// Matches the primary subtag only, so `nl-NL` resolves to `nl`.
pub fn lookup(code: &str) -> Option<&'static Lang> {
    let primary = normalize(code);
    if primary.is_empty() {
        return None;
    }
    CATALOG.iter().find(|entry| entry.code == primary)
}

/// Greeting for a code, falling back to the default language.
///
/// `code` is looked up first; if it is not in the catalog the greeting of
/// [`DEFAULT_CODE`] is returned. When neither is in the catalog the result is
/// an empty string.
pub fn greeting_for(code: &str) -> &'static str {
    if let Some(lang) = lookup(code) {
        return lang.greeting;
    }
    match lookup(DEFAULT_CODE) {
        Some(lang) => lang.greeting,
        None => "",
    }
}

/// Render the `--list-langs` table: `code<TAB>native name`, one per line.
pub fn list_langs() -> String {
    let mut out = String::new();
    for lang in CATALOG {
        out.push_str(lang.code);
        out.push('\t');
        out.push_str(lang.name);
        out.push('\n');
    }
    out
}

/// Comma separated list of supported codes, for error messages.
pub fn supported_codes() -> String {
    let mut codes: Vec<&str> = CATALOG.iter().map(|lang| lang.code).collect();
    codes.sort_unstable();
    codes.join(", ")
}

/// Parse an RFC 9110 `qvalue`.
///
/// The grammar is `qvalue = ( "0" [ "." 0*3DIGIT ] ) / ( "1" [ "." 0*3("0") ] )`,
/// so `0`, `0.5`, `0.125`, `1` and `1.000` are valid while `.5`, `00.5`,
/// `+0.5`, `1e-1`, `0.0001`, `0.1234`, `1.5` and `NaN` are not.
fn parse_qvalue(value: &str) -> Option<f32> {
    let (integer, fraction) = match value.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (value, ""),
    };
    match integer {
        "0" => {
            if fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
        }
        "1" => {
            if fraction.len() > 3 || !fraction.bytes().all(|b| b == b'0') {
                return None;
            }
        }
        _ => return None,
    }
    format!("{integer}.{fraction}").parse::<f32>().ok()
}

/// Whether `tag` is a `language-range` (RFC 4647 2.1).
///
/// A range is `*` or 1..=8 ASCII letters followed by any number of
/// `-`-separated 1..=8 character ASCII alphanumeric subtags.
fn language_range_is_valid(tag: &str) -> bool {
    if tag == "*" {
        return true;
    }
    let mut subtags = tag.split('-');
    let primary = subtags.next().unwrap_or("");
    if primary.is_empty() || primary.len() > 8 || !primary.bytes().all(|b| b.is_ascii_alphabetic())
    {
        return false;
    }
    subtags.all(|subtag| {
        !subtag.is_empty() && subtag.len() <= 8 && subtag.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// Parse a weighted list into `(quality, tag)` pairs.
///
/// Entries are split on `,`; the tag is everything before the first `;`. The
/// weight defaults to `1.0` and is set by a `q=value` parameter (matched
/// case-insensitively) whose value is validated by `parse_qvalue`; a malformed or
/// missing weight drops the whole entry. Later `q` parameters override earlier
/// ones, and other parameters are ignored. The tag itself is not validated, so the
/// `Accept-Language` range list and the `Accept-Encoding` coding list are parsed
/// by this one function.
pub fn parse_weighted_list(header: &str) -> Vec<(f32, &str)> {
    let mut entries: Vec<(f32, &str)> = Vec::new();
    for raw in header.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let mut parts = raw.split(';');
        let tag = parts.next().unwrap_or("").trim();
        if tag.is_empty() {
            continue;
        }
        let mut quality = Some(1.0_f32);
        for param in parts {
            let param = param.trim();
            match param.split_once('=') {
                Some((name, value)) if name.trim().eq_ignore_ascii_case("q") => {
                    quality = parse_qvalue(value.trim());
                }
                // A `q` with no value is malformed and drops the entry.
                None if param.eq_ignore_ascii_case("q") => quality = None,
                _ => {}
            }
        }
        if let Some(weight) = quality {
            entries.push((weight, tag));
        }
    }
    entries
}

/// Parse an `Accept-Language` header into `(quality, tag)` pairs.
///
/// Entries are parsed by [`parse_weighted_list`]; an entry whose tag is not a
/// `language-range` per [`language_range_is_valid`] is dropped as well.
fn parse_entries(header: &str) -> Vec<(f32, &str)> {
    parse_weighted_list(header)
        .into_iter()
        .filter(|(_, tag)| language_range_is_valid(tag))
        .collect()
}

/// Weight of `code` under `entries`, using the most specific matching range.
///
/// An exact range (any tag that is not `*` and whose [`normalize`]d form equals
/// `code`) beats a `*` wildcard; within a specificity the highest weight wins.
fn quality(entries: &[(f32, &str)], code: &str) -> Option<f32> {
    let mut best: Option<f32> = None;
    for &(weight, tag) in entries {
        if tag != "*" && normalize(tag) == code {
            best = Some(best.map_or(weight, |current| current.max(weight)));
        }
    }
    if best.is_some() {
        return best;
    }
    for &(weight, tag) in entries {
        if tag == "*" {
            best = Some(best.map_or(weight, |current| current.max(weight)));
        }
    }
    best
}

/// Outcome of `Accept-Language` negotiation.
#[derive(Debug, PartialEq, Eq)]
pub enum Negotiated {
    /// Serve this catalog language.
    Language(&'static str),
    /// The header made every catalog language unacceptable.
    NotAcceptable,
}

/// Parse an `Accept-Language` header and decide which catalog language to serve.
///
/// Every entry's weight is parsed (default `1.0`); an entry whose weight is
/// missing or malformed, or whose tag is not a `language-range` (RFC 4647 2.1),
/// is ignored, and a valid weight of `0` makes that language not acceptable
/// (RFC 9110 12.4.2). The most specific matching range decides,
/// so an exact range beats `*`. The highest weight wins, with ties resolved in
/// catalog order, and a header with no usable entry leaves the default acceptable.
pub fn negotiate(header: &str) -> Negotiated {
    let entries = parse_entries(header);
    let mut best: Option<(&'static str, f32)> = None;
    for lang in CATALOG {
        if let Some(weight) = quality(&entries, lang.code) {
            if weight > 0.0 && best.map_or(true, |(_, current)| weight > current) {
                best = Some((lang.code, weight));
            }
        }
    }
    if let Some((code, _)) = best {
        return Negotiated::Language(code);
    }
    if entries.is_empty() {
        Negotiated::Language(DEFAULT_CODE)
    } else {
        Negotiated::NotAcceptable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_reduces_to_primary_subtag() {
        assert_eq!(normalize("nl-NL"), "nl");
        assert_eq!(normalize("PT_br"), "pt");
        assert_eq!(normalize("  EN  "), "en");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn lookup_is_case_insensitive_and_subtag_aware() {
        assert_eq!(lookup("NL").map(|lang| lang.code), Some("nl"));
        assert_eq!(lookup("pt-BR").map(|lang| lang.code), Some("pt"));
        assert_eq!(lookup("zh-CN").map(|lang| lang.code), Some("zh"));
        assert_eq!(lookup("klingon"), None);
        assert_eq!(lookup(""), None);
    }

    #[test]
    fn greeting_for_falls_back_to_default() {
        assert_eq!(greeting_for("en"), "Hello, World!");
        assert_eq!(greeting_for("xx"), greeting_for(DEFAULT_CODE));
    }

    /// Shorthand for a catalog language outcome.
    fn lang(code: &'static str) -> Negotiated {
        Negotiated::Language(code)
    }

    #[test]
    fn parse_qvalue_matches_the_rfc_grammar() {
        for accepted in ["0", "0.", "0.5", "0.125", "1", "1.", "1.0", "1.00", "1.000"] {
            assert!(
                parse_qvalue(accepted).is_some(),
                "should accept {accepted:?}"
            );
        }
        for rejected in [
            ".5", "00.5", "+0.5", "-0.5", "1e-1", "0.0001", "0.1234", "1.5", "NaN", "",
        ] {
            assert_eq!(parse_qvalue(rejected), None, "should reject {rejected:?}");
        }
        assert_eq!(parse_qvalue("0.125"), Some(0.125));
        assert_eq!(parse_qvalue("1.000"), Some(1.0));
    }

    #[test]
    fn negotiate_picks_best_supported_entry() {
        assert_eq!(negotiate("de-DE,de;q=0.9"), lang("de"));
        assert_eq!(negotiate("xx;q=1.0,ja;q=0.8"), lang("ja"));
        assert_eq!(negotiate("fr-CA,fr;q=0.9,en;q=0.5"), lang("fr"));
        assert_eq!(negotiate(""), lang(DEFAULT_CODE));
        assert_eq!(negotiate("xx,zz"), Negotiated::NotAcceptable);
    }

    #[test]
    fn negotiate_skips_zero_quality_and_honours_wildcards() {
        assert_eq!(negotiate("nl;q=0,de;q=0.5"), lang("de"));
        assert_eq!(negotiate("nl;q=0"), Negotiated::NotAcceptable);
        assert_eq!(negotiate("*"), lang(DEFAULT_CODE));
        assert_eq!(negotiate("nl;q=bogus"), lang(DEFAULT_CODE));
    }

    #[test]
    fn negotiate_orders_wildcard_by_quality() {
        assert_eq!(negotiate("de;q=0.5, *;q=0.9"), lang("en"));
        assert_eq!(negotiate("*;q=0.1, de;q=0.9"), lang("de"));
    }

    #[test]
    fn negotiate_wildcard_skips_languages_named_by_other_entries() {
        assert_eq!(negotiate("*;q=1,en;q=0.5"), lang("nl"));
        assert_eq!(negotiate("en;q=0, *"), lang("nl"));
        let all_named = CATALOG
            .iter()
            .map(|lang| format!("{};q=0", lang.code))
            .collect::<Vec<_>>()
            .join(", ")
            + ", *";
        assert_eq!(negotiate(&all_named), Negotiated::NotAcceptable);
    }

    #[test]
    fn negotiate_treats_a_valueless_quality_as_disabled() {
        assert_eq!(negotiate("nl;q"), lang(DEFAULT_CODE));
        assert_eq!(negotiate("nl;q,de;q=0.5"), lang("de"));
    }

    #[test]
    fn negotiate_rejects_out_of_range_or_unparsable_quality() {
        assert_eq!(negotiate("nl;q=NaN"), lang(DEFAULT_CODE));
        assert_eq!(negotiate("nl;q=1.5"), lang(DEFAULT_CODE));
    }

    #[test]
    fn negotiate_tolerates_whitespace_and_case_in_parameters() {
        assert_eq!(negotiate("nl;q = 0.5"), lang("nl"));
        assert_eq!(negotiate("de;Q=0.9"), lang("de"));
    }

    #[test]
    fn negotiate_rejects_malformed_qvalues() {
        assert_eq!(negotiate("ja;q=.5, de;q=0.4"), lang("de"));
        for header in ["nl;q=1e-1", "nl;q=+0.5", "nl;q=0.0001", "nl;q=0.1234"] {
            assert_eq!(negotiate(header), lang(DEFAULT_CODE), "header {header:?}");
        }
        assert_eq!(negotiate("nl;q=1.000"), lang("nl"));
    }

    #[test]
    fn parse_entries_drops_ill_formed_ranges() {
        for header in [
            "-;q=1",
            "123;q=1",
            "!;q=1",
            "abcdefghi;q=1",
            "en-;q=1",
            "en-abcdefghi;q=1",
            "en_US;q=1",
        ] {
            assert!(parse_entries(header).is_empty(), "should drop {header:?}");
        }
    }

    #[test]
    fn negotiate_treats_an_ill_formed_range_as_malformed() {
        assert_eq!(negotiate("-;q=1"), lang("en"));
        assert_eq!(negotiate("123;q=1"), lang("en"));
        assert_eq!(negotiate("abcdefghi;q=1"), lang("en"));
        assert_eq!(negotiate("-;q=1, de;q=0.5"), lang("de"));
        assert_eq!(negotiate("en-US;q=0.9"), lang("en"));
        assert_eq!(negotiate("en-abcdefgh;q=0.9"), lang("en"));
        assert_eq!(negotiate("xx;q=1"), Negotiated::NotAcceptable);
    }

    #[test]
    fn negotiate_never_serves_a_language_marked_unacceptable() {
        assert_eq!(negotiate("en;q=0"), Negotiated::NotAcceptable);
        assert_eq!(negotiate("en;q=0, nl"), lang("nl"));
        assert_eq!(negotiate("en;q=0, nl;q=0"), Negotiated::NotAcceptable);
    }

    #[test]
    fn catalog_is_well_formed() {
        assert!(lookup(DEFAULT_CODE).is_some(), "default code missing");
        for lang in CATALOG {
            assert_eq!(lang.code, normalize(lang.code), "code not normalized");
            assert!(!lang.name.is_empty(), "empty name for {}", lang.code);
            assert!(
                !lang.greeting.is_empty(),
                "empty greeting for {}",
                lang.code
            );
        }
        let mut codes: Vec<&str> = CATALOG.iter().map(|lang| lang.code).collect();
        codes.sort_unstable();
        let count = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), count, "duplicate codes in CATALOG");
    }
}
