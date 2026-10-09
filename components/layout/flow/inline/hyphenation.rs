/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Support for CSS `hyphens: auto`, using the same `mapped_hyph` engine and
//! compiled hyphenation dictionaries that Gecko uses.

use icu_locale_core::LanguageIdentifier;
use mapped_hyph::Hyphenator;

/// Compiled hyphenation dictionaries, copied from Gecko's
/// `intl/locales/<locale>/hyphenation/`. See `hyphenation_dicts/README*`.
static HYPH_EN_US: &[u8] = include_bytes!("hyphenation_dicts/hyph_en_US.hyf");

/// Return the compiled hyphenation dictionary to use for the given language, if
/// one is available. Only the primary language subtag is considered.
fn dictionary_for_language(language: &LanguageIdentifier) -> Option<&'static [u8]> {
    match language.language.as_str() {
        "en" => Some(HYPH_EN_US),
        _ => None,
    }
}

/// Hyphenate a single word, returning the byte offsets within `word` after
/// which a hyphen may be inserted. The returned offsets are always at character
/// boundaries.
pub(crate) fn hyphenate_word(word: &str, language: &LanguageIdentifier) -> Vec<usize> {
    let Some(dictionary) = dictionary_for_language(language) else {
        return Vec::new();
    };

    let mut values = vec![0u8; word.len()];
    let hyphenator = Hyphenator::new(dictionary);
    if hyphenator.find_hyphen_values(word, &mut values) <= 0 {
        return Vec::new();
    }

    values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| ((value & 1) == 1).then_some(index + 1))
        .collect()
}
