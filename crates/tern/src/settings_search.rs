//! Searching Settings: one field over every section. A row matches when each word of the query
//! is in its section's name, its title or its description, ignoring case. What the person
//! types is only ever text: no pattern syntax, no markup, so `*`, `<b>` or `\` match themselves.
//! Nothing here draws; the Settings page asks it which rows show and which spans to highlight.

use std::ops::Range;

/// The words typed into the search field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    words: Vec<String>,
}

impl Query {
    pub fn new(text: &str) -> Self {
        Self {
            words: text
                .split_whitespace()
                .map(|w| w.chars().map(lower).collect())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

/// Where a row's title and description match, as byte ranges to highlight.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hits {
    pub title: Vec<Range<usize>>,
    pub description: Vec<Range<usize>>,
}

/// The row's highlights when every word is in its section, title or description; `None` when
/// a word is missing.
pub fn row_matches(query: &Query, section: &str, title: &str, description: &str) -> Option<Hits> {
    let mut hits = Hits::default();
    for word in &query.words {
        let in_title = find(title, word);
        let in_description = find(description, word);
        if in_title.is_empty() && in_description.is_empty() && find(section, word).is_empty() {
            return None;
        }
        hits.title.extend(in_title);
        hits.description.extend(in_description);
    }
    hits.title = merged(hits.title);
    hits.description = merged(hits.description);
    Some(hits)
}

/// Ranges sorted and joined where two words' matches touch or overlap, so a span is never
/// highlighted twice.
fn merged(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// A character in lower case, when that is one character (as for every setting's words);
/// otherwise as it is, so byte offsets stay those of the text shown.
fn lower(c: char) -> char {
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(one), None) => one,
        _ => c,
    }
}

/// Byte ranges of every place `word` (lowercase) occurs in `text`, ignoring case, never
/// overlapping.
pub fn find(text: &str, word: &str) -> Vec<Range<usize>> {
    let word: Vec<char> = word.chars().collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut found = Vec::new();
    if word.is_empty() {
        return found;
    }
    let mut at = 0;
    while at + word.len() <= chars.len() {
        let here = (0..word.len()).all(|k| lower(chars[at + k].1) == word[k]);
        if here {
            let end = chars.get(at + word.len()).map_or(text.len(), |(i, _)| *i);
            found.push(chars[at].0..end);
            at += word.len();
        } else {
            at += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::single_range_in_vec_init)]
    use super::*;

    fn hits(q: &str, section: &str, title: &str, description: &str) -> Option<Hits> {
        row_matches(&Query::new(q), section, title, description)
    }

    #[test]
    fn every_word_must_be_somewhere_in_the_row_or_its_section() {
        let row = (
            "Terminal",
            "Copy on select",
            "Finishing a selection copies it",
        );
        let m = |q| hits(q, row.0, row.1, row.2);
        assert!(m("copy").is_some());
        assert!(m("COPY select").is_some(), "case is ignored");
        assert!(m("terminal copy").is_some(), "the section's name counts");
        assert!(m("copy paste").is_none(), "every word must be there");
        assert!(m("vault").is_none());
    }

    #[test]
    fn the_matched_words_are_highlighted_in_title_and_description() {
        let h = hits(
            "sel",
            "Terminal",
            "Copy on select",
            "Finishing a selection copies it",
        )
        .unwrap();
        assert_eq!(h.title, [8..11]);
        assert_eq!(h.description, [12..15]);
    }

    #[test]
    fn a_word_found_only_in_the_section_highlights_nothing() {
        let h = hits("terminal", "Terminal", "Copy on select", "").unwrap();
        assert_eq!(h, Hits::default());
    }

    #[test]
    fn symbols_and_markup_are_plain_text() {
        let row = (
            "Terminal",
            "Programs can copy (OSC 52)",
            "<b>bold</b> a.*b \\n",
        );
        let m = |q| hits(q, row.0, row.1, row.2);
        assert!(m("(osc").is_some());
        assert!(m("<b>").is_some());
        assert_eq!(m("<b>").unwrap().description, [0..3]);
        assert!(m("a.*b").is_some());
        assert!(m("\\n").is_some());
        assert!(
            m(".*").is_some(),
            "a dot-star is two characters, not a pattern"
        );
        assert!(m("a.b").is_none(), "a dot is a dot");
    }

    #[test]
    fn an_empty_or_blank_query_is_no_search() {
        assert!(Query::new("").is_empty());
        assert!(Query::new("   \t ").is_empty());
        assert!(!Query::new(" copy ").is_empty());
    }

    #[test]
    fn found_places_do_not_overlap_and_stay_on_char_boundaries() {
        assert_eq!(find("aaaa", "aa"), [0..2, 2..4]);
        assert_eq!(find("Café CAFÉ", "café"), [0..5, 6..11]);
        assert_eq!(find("abc", ""), Vec::<Range<usize>>::new());
    }
}
