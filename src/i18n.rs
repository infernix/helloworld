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
pub fn greeting_for(code: &str) -> &'static str {
    match lookup(code) {
        Some(lang) => lang.greeting,
        None => greeting_for(DEFAULT_CODE),
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

/// Parse an `Accept-Language` header and return the best supported code.
///
/// Entries are ordered by descending `q` value (stable, so equal-quality
/// entries keep their header order) and the first matching one wins. The `q`
/// parameter name is matched case-insensitively and its value is parsed after
/// trimming surrounding whitespace; a value that is missing, malformed, not a
/// finite number or outside `0.0..=1.0` yields a quality of `0.0`, disabling
/// the entry. An entry whose tag is exactly `*` matches any language not named
/// by another entry, resolving to the first such catalog language (falling back
/// to [`DEFAULT_CODE`] when every one is named), but only if it wins on
/// quality.
pub fn negotiate(header: &str) -> Option<&'static str> {
    let mut entries: Vec<(f32, &str)> = Vec::new();
    for raw in header.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let mut parts = raw.split(';');
        let tag = parts.next().unwrap_or("").trim();
        let mut quality = 1.0_f32;
        for param in parts {
            let param = param.trim();
            match param.split_once('=') {
                Some((name, value)) if name.trim().eq_ignore_ascii_case("q") => {
                    quality = match value.trim().parse::<f32>() {
                        Ok(parsed) if (0.0..=1.0).contains(&parsed) => parsed,
                        _ => 0.0,
                    };
                }
                // A `q` with no value is malformed and disables the entry.
                None if param.eq_ignore_ascii_case("q") => quality = 0.0,
                _ => {}
            }
        }
        entries.push((quality, tag));
    }

    entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    for &(quality, tag) in &entries {
        if quality <= 0.0 {
            continue;
        }
        if tag == "*" {
            // `*` matches every tag not named by another entry (RFC 7231 5.3.5),
            // so it resolves to the first catalog language the header does not
            // name, keeping `en` as the fallback when every one is named.
            return CATALOG
                .iter()
                .map(|lang| lang.code)
                .find(|code| !entries.iter().any(|(_, tag)| normalize(tag) == *code))
                .or(Some(DEFAULT_CODE));
        }
        if let Some(lang) = lookup(tag) {
            return Some(lang.code);
        }
    }
    None
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

    #[test]
    fn negotiate_picks_best_supported_entry() {
        assert_eq!(negotiate("de-DE,de;q=0.9"), Some("de"));
        assert_eq!(negotiate("xx;q=1.0,ja;q=0.8"), Some("ja"));
        assert_eq!(negotiate("fr-CA,fr;q=0.9,en;q=0.5"), Some("fr"));
        assert_eq!(negotiate(""), None);
        assert_eq!(negotiate("xx,zz"), None);
    }

    #[test]
    fn negotiate_skips_zero_quality_and_honours_wildcards() {
        assert_eq!(negotiate("nl;q=0,de;q=0.5"), Some("de"));
        assert_eq!(negotiate("nl;q=0"), None);
        assert_eq!(negotiate("*"), Some(DEFAULT_CODE));
        assert_eq!(negotiate("nl;q=bogus"), None);
    }

    #[test]
    fn negotiate_orders_wildcard_by_quality() {
        assert_eq!(negotiate("de;q=0.5, *;q=0.9"), Some("en"));
        assert_eq!(negotiate("*;q=0.1, de;q=0.9"), Some("de"));
    }

    #[test]
    fn negotiate_wildcard_skips_languages_named_by_other_entries() {
        assert_eq!(negotiate("*;q=1,en;q=0.5"), Some("nl"));
        assert_eq!(negotiate("en;q=0, *"), Some("nl"));
        let all_named = CATALOG
            .iter()
            .map(|lang| format!("{};q=0", lang.code))
            .collect::<Vec<_>>()
            .join(", ")
            + ", *";
        assert_eq!(negotiate(&all_named), Some(DEFAULT_CODE));
    }

    #[test]
    fn negotiate_treats_a_valueless_quality_as_disabled() {
        assert_eq!(negotiate("nl;q"), None);
        assert_eq!(negotiate("nl;q,de;q=0.5"), Some("de"));
    }

    #[test]
    fn negotiate_rejects_out_of_range_or_unparsable_quality() {
        assert_eq!(negotiate("nl;q=NaN"), None);
        assert_eq!(negotiate("nl;q=1.5"), None);
    }

    #[test]
    fn negotiate_tolerates_whitespace_and_case_in_parameters() {
        assert_eq!(negotiate("nl;q = 0.5"), Some("nl"));
        assert_eq!(negotiate("de;Q=0.9"), Some("de"));
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
