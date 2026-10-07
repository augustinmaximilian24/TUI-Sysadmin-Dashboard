//! Offline-Extraktion aus einer Markdown-Datei: Titel, explizite Links
//! (`[[Wikilinks]]`, `[Text](pfad.md)`) und Begriffe (Tags, Frontmatter-
//! Tags, Überschriftenwörter).
//!
//! Reine Textverarbeitung ohne Dateisystemzugriff, damit sie mit festen
//! Fixtures testbar ist (Regel 24). Code-Blöcke und Inline-Code werden
//! ignoriert, damit Beispiel-Syntax keine Scheinverknüpfungen erzeugt.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Höchstzahl gespeicherter Links pro Datei (Regel 18).
pub const MAX_LINKS_PER_FILE: usize = 200;
/// Höchstzahl gespeicherter Begriffe pro Datei (Regel 18).
pub const MAX_TERMS_PER_FILE: usize = 100;
/// Längere Tags werden verworfen (meist Fehlerkennungen).
const MAX_TAG_LEN: usize = 64;

/// Ein noch nicht aufgelöster Verweis auf eine andere Datei.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "target", rename_all = "snake_case")]
pub enum LinkRef {
    /// `[[Ziel]]` -- wird über den Dateinamen aufgelöst.
    Wiki(String),
    /// `[Text](pfad.md)` -- wird relativ zur verweisenden Datei aufgelöst.
    Path(String),
}

/// Ergebnis der Extraktion einer Datei.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extracted {
    /// Titel aus dem Frontmatter oder der ersten H1-Überschrift.
    pub title: Option<String>,
    /// Explizite Verweise in Dokumentreihenfolge, ohne Duplikate.
    pub links: Vec<LinkRef>,
    /// Begriffe: Tags mit Präfix `#`, Überschriftenwörter ohne Präfix.
    pub terms: Vec<String>,
}

/// Wörter, die in Überschriften zu allgemein sind, um eine Verbindung zu
/// begründen (Deutsch und Englisch, nur Wörter ab 4 Zeichen relevant).
const STOPWORDS: &[&str] = &[
    "aber",
    "alle",
    "allem",
    "allen",
    "alles",
    "auch",
    "beim",
    "dass",
    "dein",
    "denn",
    "diese",
    "diesem",
    "diesen",
    "dieser",
    "dieses",
    "doch",
    "eine",
    "einem",
    "einen",
    "einer",
    "eines",
    "etwa",
    "haben",
    "hier",
    "ihre",
    "immer",
    "kann",
    "keine",
    "mein",
    "meine",
    "mehr",
    "nach",
    "noch",
    "nicht",
    "oder",
    "ohne",
    "sehr",
    "sein",
    "sind",
    "über",
    "unter",
    "viel",
    "wann",
    "warum",
    "weil",
    "wenn",
    "werden",
    "wird",
    "wurde",
    "neue",
    "neuer",
    "neues",
    "teil",
    "einleitung",
    "zusammenfassung",
    "fazit",
    "übersicht",
    "notizen",
    "notiz",
    "quellen",
    "offene",
    "fragen",
    "nächste",
    "schritte",
    "about",
    "after",
    "also",
    "from",
    "have",
    "into",
    "more",
    "only",
    "over",
    "some",
    "than",
    "that",
    "them",
    "then",
    "there",
    "these",
    "this",
    "what",
    "when",
    "where",
    "which",
    "with",
    "your",
    "note",
    "notes",
    "todo",
    "part",
    "summary",
    "overview",
    "introduction",
    "links",
    "sources",
    "references",
    "next",
    "steps",
];

fn wiki_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"!?\[\[([^\[\]|#\n]+)(?:#[^\[\]|\n]*)?(?:\|[^\[\]\n]*)?\]\]")
            .expect("statisches Regex ist gültig")
    })
}

fn md_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\[[^\]\n]*\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"\n]*")?\s*\)"#)
            .expect("statisches Regex ist gültig")
    })
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:^|[\s(,])#([\p{L}\p{N}_][\p{L}\p{N}_/\-]*)")
            .expect("statisches Regex ist gültig")
    })
}

fn inline_code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`\n]*`").expect("statisches Regex ist gültig"))
}

/// Extrahiert Titel, Links und Begriffe aus einem Markdown-Text.
///
/// `min_term_len` ist die Mindestlänge (in Zeichen) eines
/// Überschriftenworts; Tags sind davon ausgenommen.
pub fn extract(content: &str, min_term_len: usize) -> Extracted {
    let (frontmatter, body) = split_frontmatter(content);
    let front = frontmatter.map(parse_frontmatter).unwrap_or_default();

    let mut title = front.title;
    let mut links = Vec::new();
    let mut seen_links = HashSet::new();
    let mut terms = Vec::new();
    let mut seen_terms = HashSet::new();

    let mut push_term = |term: String, terms: &mut Vec<String>| {
        if terms.len() < MAX_TERMS_PER_FILE && seen_terms.insert(term.clone()) {
            terms.push(term);
        }
    };
    for tag in front.tags {
        push_term(format!("#{tag}"), &mut terms);
    }

    let mut in_fence = false;
    for raw_line in body.lines() {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let line = inline_code_re().replace_all(raw_line, " ");

        for link in line_links(&line) {
            if links.len() < MAX_LINKS_PER_FILE && seen_links.insert(link.clone()) {
                links.push(link);
            }
        }

        if let Some(heading) = heading_text(&line) {
            if title.is_none() && line.trim_start().starts_with("# ") {
                title = Some(clean_heading(heading));
            }
            for word in heading_words(heading, min_term_len) {
                push_term(word, &mut terms);
            }
            continue;
        }
        for cap in tag_re().captures_iter(&line) {
            if let Some(tag) = cap.get(1).and_then(|m| normalize_tag(m.as_str())) {
                push_term(format!("#{tag}"), &mut terms);
            }
        }
    }

    Extracted {
        title,
        links,
        terms,
    }
}

/// Alle Wiki- und Markdown-Links einer (bereits von Inline-Code befreiten) Zeile.
fn line_links(line: &str) -> Vec<LinkRef> {
    let mut out = Vec::new();
    for cap in wiki_re().captures_iter(line) {
        if let Some(target) = cap.get(1).map(|m| m.as_str().trim()) {
            if !target.is_empty() {
                out.push(LinkRef::Wiki(target.to_string()));
            }
        }
    }
    for cap in md_link_re().captures_iter(line) {
        let Some(href) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        if href.contains("://") || href.starts_with("mailto:") || href.starts_with('#') {
            continue;
        }
        let without_anchor = href.split(['#', '?']).next().unwrap_or("");
        let decoded = percent_decode(without_anchor);
        if !decoded.is_empty() {
            out.push(LinkRef::Path(decoded));
        }
    }
    out
}

/// Text einer ATX-Überschrift (`#` bis `######` gefolgt von Leerzeichen).
fn heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    rest.starts_with([' ', '\t']).then(|| rest.trim())
}

fn clean_heading(text: &str) -> String {
    text.trim_end_matches('#').trim().to_string()
}

fn heading_words(text: &str, min_len: usize) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= min_len)
        .filter(|w| !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

fn normalize_tag(raw: &str) -> Option<String> {
    let tag = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim_start_matches('#')
        .trim_end_matches(['/', '-'])
        .to_lowercase();
    let valid = !tag.is_empty()
        && tag.chars().count() <= MAX_TAG_LEN
        && !tag.chars().all(|c| c.is_ascii_digit())
        && !tag.contains(char::is_whitespace);
    valid.then_some(tag)
}

/// Dekodiert `%XX`-Sequenzen; ungültige Sequenzen bleiben unverändert.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Default)]
struct Frontmatter {
    title: Option<String>,
    tags: Vec<String>,
}

/// Trennt einen YAML-Frontmatter-Block (`---` ... `---`) vom Rest.
fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let text = content.strip_prefix('\u{feff}').unwrap_or(content);
    let Some(after) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (None, text);
    };
    let mut offset = 0;
    for line in after.split_inclusive('\n') {
        let t = line.trim_end();
        if t == "---" || t == "..." {
            return (Some(&after[..offset]), &after[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

/// Minimaler Frontmatter-Leser für `title`, `tags`/`tag`/`keywords`
/// (Inline-Liste, Komma-Liste oder Blockliste). Bewusst kein YAML-Parser:
/// mehr wird nicht gebraucht, und eine kaputte Kopfzeile darf nie zum
/// Abbruch führen (Regel 16).
fn parse_frontmatter(block: &str) -> Frontmatter {
    let mut front = Frontmatter::default();
    let mut in_tag_list = false;
    for line in block.lines() {
        let trimmed = line.trim_start();
        if in_tag_list {
            if let Some(item) = trimmed.strip_prefix("- ") {
                if let Some(tag) = normalize_tag(item) {
                    front.tags.push(tag);
                }
                continue;
            }
        }
        in_tag_list = false;
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_lowercase().as_str() {
            "title" => {
                let title = value.trim_matches(|c| c == '"' || c == '\'').trim();
                if !title.is_empty() {
                    front.title = Some(title.to_string());
                }
            }
            "tags" | "tag" | "keywords" => {
                if value.is_empty() {
                    in_tag_list = true;
                    continue;
                }
                let inner = value.trim_start_matches('[').trim_end_matches(']');
                let parts: Vec<&str> = if inner.contains(',') {
                    inner.split(',').collect()
                } else {
                    inner.split_whitespace().collect()
                };
                front
                    .tags
                    .extend(parts.into_iter().filter_map(normalize_tag));
            }
            _ => {}
        }
    }
    front
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wikilinks_mit_alias_und_anker() {
        let ex = extract(
            "Siehe [[Projekt A]], [[ordner/Notiz B|B]] und ![[Bild C#oben]].",
            4,
        );
        assert_eq!(
            ex.links,
            vec![
                LinkRef::Wiki("Projekt A".into()),
                LinkRef::Wiki("ordner/Notiz B".into()),
                LinkRef::Wiki("Bild C".into()),
            ]
        );
    }

    #[test]
    fn markdown_links_ohne_urls_und_anker() {
        let ex = extract(
            "[a](../x/a.md) [b](<b%20c.md>) [web](https://example.org) [anker](#oben) [c](c.md#teil \"Titel\")",
            4,
        );
        assert_eq!(
            ex.links,
            vec![
                LinkRef::Path("../x/a.md".into()),
                LinkRef::Path("b c.md".into()),
                LinkRef::Path("c.md".into()),
            ]
        );
    }

    #[test]
    fn code_bloecke_und_inline_code_werden_ignoriert() {
        let ex = extract("```\n[[nicht]] #nein\n```\n`[[auch nicht]]` [[ja]] #tag", 4);
        assert_eq!(ex.links, vec![LinkRef::Wiki("ja".into())]);
        assert_eq!(ex.terms, vec!["#tag".to_string()]);
    }

    #[test]
    fn tags_aber_keine_ueberschriften_zahlen_oder_url_anker() {
        let ex = extract(
            "Text #Rust und #home-lab/server, Issue #123, https://x.org/seite#anker",
            4,
        );
        assert_eq!(
            ex.terms,
            vec!["#rust".to_string(), "#home-lab/server".to_string()]
        );
    }

    #[test]
    fn frontmatter_titel_und_tag_varianten() {
        let inline = extract(
            "---\ntitle: \"Mein Titel\"\ntags: [Rust, linux]\n---\n# Andere H1\n",
            4,
        );
        assert_eq!(inline.title.as_deref(), Some("Mein Titel"));
        assert!(inline
            .terms
            .starts_with(&["#rust".to_string(), "#linux".to_string()]));

        let block = extract("---\ntags:\n  - backup\n  - \"#nas\"\n---\nText\n", 4);
        assert_eq!(block.terms, vec!["#backup".to_string(), "#nas".to_string()]);
    }

    #[test]
    fn titel_aus_erster_h1_und_ueberschriftenwoerter_ohne_stopwoerter() {
        let ex = extract(
            "## Vorab\n# Backup-Strategie für den Heimserver\n## Offene Fragen\n",
            4,
        );
        assert_eq!(
            ex.title.as_deref(),
            Some("Backup-Strategie für den Heimserver")
        );
        assert_eq!(
            ex.terms,
            vec![
                "vorab".to_string(),
                "backup".into(),
                "strategie".into(),
                "heimserver".into()
            ]
        );
    }

    #[test]
    fn kaputtes_frontmatter_ohne_ende_bricht_nicht_ab() {
        let ex = extract("---\ntitle: X\nkein ende\n[[Ziel]]", 4);
        assert_eq!(ex.links, vec![LinkRef::Wiki("Ziel".into())]);
        assert_eq!(ex.title, None);
    }

    #[test]
    fn links_und_begriffe_sind_dedupliziert_und_begrenzt() {
        let many: String = (0..500)
            .map(|i| format!("[[n{i}]] [[n{i}]] #t{i}x "))
            .collect();
        let ex = extract(&many, 4);
        assert_eq!(ex.links.len(), MAX_LINKS_PER_FILE);
        assert_eq!(ex.terms.len(), MAX_TERMS_PER_FILE);
    }

    #[test]
    fn percent_decode_laesst_ungueltiges_stehen() {
        assert_eq!(percent_decode("a%20b%zz%2"), "a b%zz%2");
    }
}
