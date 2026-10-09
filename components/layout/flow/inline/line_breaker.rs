/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ops::Range;

use icu_locale_core::LanguageIdentifier;
use icu_segmenter::options::{LineBreakOptions, WordBreakInvariantOptions};
use icu_segmenter::{LineSegmenter, WordSegmenter};
use servo_base::text::Utf8CodeUnits;
use style::computed_values::hyphens::T as Hyphens;

use crate::flow::inline::hyphenation::hyphenate_word;

/// A single line break opportunity found by the [`LineBreaker`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LineBreakOpportunity {
    /// The UTF-8 offset in the text at which a line may break.
    pub(crate) offset: Utf8CodeUnits,
    /// Whether breaking here requires rendering a hyphen. This is true for soft
    /// hyphen (U+00AD) breaks and for dictionary (`hyphens: auto`) breaks.
    pub(crate) hyphenate: bool,
}

pub(crate) struct LineBreaker {
    linebreaks: Vec<LineBreakOpportunity>,
    current_linebreak_offset: usize,
}

impl LineBreaker {
    pub(crate) fn new(
        string: &str,
        options: LineBreakOptions<'_>,
        hyphens: Hyphens,
        language: Option<&LanguageIdentifier>,
    ) -> Self {
        let line_segmenter = LineSegmenter::new_auto(options);
        let mut linebreaks: Vec<LineBreakOpportunity> = line_segmenter
            .segment_str(string)
            .skip(1)
            .map(|offset| {
                // From https://docs.rs/icu_segmenter/1.5.0/icu_segmenter/struct.LineSegmenter.html
                // > For consistency with the grapheme, word, and sentence segmenters, there is
                // > always a breakpoint returned at index 0, but this breakpoint is not a
                // > meaningful line break opportunity.
                //
                // Skip this first line break opportunity, as it isn't interesting to us.
                let offset = Utf8CodeUnits(offset as u32);
                LineBreakOpportunity {
                    offset,
                    // ICU treats U+00AD SOFT HYPHEN as a break-after opportunity regardless of the
                    // value of the CSS `hyphens` property, so identify those breaks here.
                    hyphenate: string[..usize::from(offset)].ends_with('\u{00AD}'),
                }
            })
            .collect();

        match hyphens {
            Hyphens::None => linebreaks.retain(|opportunity| !opportunity.hyphenate),
            Hyphens::Manual => {},
            Hyphens::Auto => {
                if let Some(language) = language {
                    add_dictionary_hyphenation_breaks(string, language, &mut linebreaks);
                }
            },
        }

        Self {
            linebreaks,
            current_linebreak_offset: 0,
        }
    }

    /// Whether there is a hyphenation break opportunity exactly at `offset`. This is used
    /// when a segment ends at a break opportunity: the break itself is handled by the next
    /// segment (as a break at its start), but the hyphen still needs to be attached to the
    /// last run of this segment.
    pub(crate) fn is_hyphenation_break_at(&self, offset: Utf8CodeUnits) -> bool {
        match self
            .linebreaks
            .binary_search_by_key(&offset, |opportunity| opportunity.offset)
        {
            Ok(index) => self.linebreaks[index].hyphenate,
            Err(_) => false,
        }
    }

    pub(crate) fn advance_to_linebreaks_in_range(
        &mut self,
        text_range: Range<Utf8CodeUnits>,
    ) -> &[LineBreakOpportunity] {
        let linebreaks_in_range = self.linebreaks_in_range_after_current_offset(text_range);
        self.current_linebreak_offset = linebreaks_in_range.end;
        &self.linebreaks[linebreaks_in_range]
    }

    fn linebreaks_in_range_after_current_offset(
        &self,
        text_range: Range<Utf8CodeUnits>,
    ) -> Range<usize> {
        assert!(text_range.start <= text_range.end);

        let mut linebreaks_range = self.current_linebreak_offset..self.linebreaks.len();

        while self.linebreaks[linebreaks_range.start].offset < text_range.start &&
            linebreaks_range.len() > 1
        {
            linebreaks_range.start += 1;
        }

        let mut ending_linebreak_index = linebreaks_range.start;
        while self.linebreaks[ending_linebreak_index].offset < text_range.end &&
            ending_linebreak_index < self.linebreaks.len() - 1
        {
            ending_linebreak_index += 1;
        }
        linebreaks_range.end = ending_linebreak_index;
        linebreaks_range
    }
}

/// Add the hyphenation opportunities found by the dictionary engine for every word
/// in `string`, merging them into `linebreaks`.
fn add_dictionary_hyphenation_breaks(
    string: &str,
    language: &LanguageIdentifier,
    linebreaks: &mut Vec<LineBreakOpportunity>,
) {
    let word_segmenter = WordSegmenter::new_auto(WordBreakInvariantOptions::default());
    let mut dictionary_breaks = Vec::new();

    let mut word_start = 0;
    for word_end in word_segmenter.segment_str(string) {
        let word = &string[word_start..word_end];
        if word.chars().any(char::is_alphabetic) {
            for break_offset in hyphenate_word(word, language) {
                dictionary_breaks.push(Utf8CodeUnits((word_start + break_offset) as u32));
            }
        }
        word_start = word_end;
    }

    for offset in dictionary_breaks {
        if !linebreaks
            .iter()
            .any(|opportunity| opportunity.offset == offset)
        {
            linebreaks.push(LineBreakOpportunity {
                offset,
                hyphenate: true,
            });
        }
    }
    linebreaks.sort_by_key(|opportunity| opportunity.offset);
}

#[cfg(test)]
mod test {
    use super::*;

    fn linebreak_offsets(linebreaker: &LineBreaker) -> Vec<u32> {
        linebreaker
            .linebreaks
            .iter()
            .map(|opportunity| opportunity.offset.0)
            .collect()
    }

    fn linebreaks_in_range_after_current_offset(
        linebreaker: &LineBreaker,
        range: Range<u32>,
    ) -> Range<usize> {
        linebreaker.linebreaks_in_range_after_current_offset(
            Utf8CodeUnits(range.start)..Utf8CodeUnits(range.end),
        )
    }

    #[test]
    fn test_linebreaker_ranges() {
        let linebreaker = LineBreaker::new(
            "abc def",
            LineBreakOptions::default(),
            Hyphens::Manual,
            None,
        );
        assert_eq!(linebreak_offsets(&linebreaker), [4, 7]);
        assert_eq!(
            linebreaks_in_range_after_current_offset(&linebreaker, 0..5),
            0..1
        );
        // The last linebreak should not be included for the text range we are interested in.
        assert_eq!(
            linebreaks_in_range_after_current_offset(&linebreaker, 0..7),
            0..1
        );

        let linebreaker = LineBreaker::new(
            "abc d def",
            LineBreakOptions::default(),
            Hyphens::Manual,
            None,
        );
        assert_eq!(linebreak_offsets(&linebreaker), [4, 6, 9]);
        assert_eq!(
            linebreaks_in_range_after_current_offset(&linebreaker, 0..5),
            0..1
        );
        assert_eq!(
            linebreaks_in_range_after_current_offset(&linebreaker, 0..7),
            0..2
        );
        assert_eq!(
            linebreaks_in_range_after_current_offset(&linebreaker, 0..9),
            0..2
        );

        assert_eq!(
            linebreaks_in_range_after_current_offset(&linebreaker, 4..9),
            0..2
        );

        std::panic::catch_unwind(|| {
            let linebreaker = LineBreaker::new(
                "abc def",
                LineBreakOptions::default(),
                Hyphens::Manual,
                None,
            );
            linebreaks_in_range_after_current_offset(&linebreaker, 5..2);
        })
        .expect_err("Reversed range should cause an assertion failure.");
    }

    fn advance_to_linebreaks_in_range(
        linebreaker: &mut LineBreaker,
        range: Range<u32>,
    ) -> &[LineBreakOpportunity] {
        linebreaker
            .advance_to_linebreaks_in_range(Utf8CodeUnits(range.start)..Utf8CodeUnits(range.end))
    }

    #[test]
    fn test_linebreaker_stateful_advance() {
        let mut linebreaker = LineBreaker::new(
            "abc d def",
            LineBreakOptions::default(),
            Hyphens::Manual,
            None,
        );
        assert_eq!(linebreak_offsets(&linebreaker), [4, 6, 9]);
        assert!(
            advance_to_linebreaks_in_range(&mut linebreaker, 0..7)
                .iter()
                .map(|o| o.offset.0)
                .collect::<Vec<_>>() ==
                [4, 6]
        );
        assert!(advance_to_linebreaks_in_range(&mut linebreaker, 8..9).is_empty());

        // We've already advanced, so a range from the beginning shouldn't affect things.
        assert!(advance_to_linebreaks_in_range(&mut linebreaker, 0..9).is_empty());

        linebreaker.current_linebreak_offset = 0;

        // Sending a value out of range shouldn't break things.
        assert!(
            advance_to_linebreaks_in_range(&mut linebreaker, 0..999)
                .iter()
                .map(|o| o.offset.0)
                .collect::<Vec<_>>() ==
                [4, 6]
        );

        linebreaker.current_linebreak_offset = 0;

        std::panic::catch_unwind(|| {
            let mut linebreaker = LineBreaker::new(
                "abc d def",
                LineBreakOptions::default(),
                Hyphens::Manual,
                None,
            );
            advance_to_linebreaks_in_range(&mut linebreaker, 2..0);
        })
        .expect_err("Reversed range should cause an assertion failure.");
    }

    #[test]
    fn test_soft_hyphen_opportunities() {
        let text = "Deoxy\u{00AD}ribo\u{00AD}nucleic acid";
        let manual = LineBreaker::new(text, LineBreakOptions::default(), Hyphens::Manual, None);
        assert_eq!(
            manual
                .linebreaks
                .iter()
                .filter(|opportunity| opportunity.hyphenate)
                .count(),
            2
        );

        let none = LineBreaker::new(text, LineBreakOptions::default(), Hyphens::None, None);
        assert!(
            !none
                .linebreaks
                .iter()
                .any(|opportunity| opportunity.hyphenate)
        );
    }
}
