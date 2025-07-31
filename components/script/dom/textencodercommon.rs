/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;

use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::error::{Fallible, Error};

#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct TextEncoderCommon {
    #[ignore_malloc_size_of = "defined in encoding_rs"]
    #[no_trace]
    utf16_encoder: RefCell<encoding_rs::Decoder>,

    remaining: RefCell<Vec<u8>>,
}

impl TextEncoderCommon {
    pub(crate) fn new_inherited() -> Self {
        #[cfg(target_endian = "big")]
        let utf16_encoder = encoding_rs::UTF_16BE.new_decoder_with_bom_removal();
        #[cfg(not(target_endian = "big"))]
        let utf16_encoder = encoding_rs::UTF_16LE.new_decoder_with_bom_removal();

        Self {
            utf16_encoder: RefCell::new(utf16_encoder),
            remaining: RefCell::new(Vec::new())
        }
    }

    pub(crate) fn encoding(&self) -> DOMString {
        DOMString::from("utf-8")
    }

    #[allow(unsafe_code)]
    pub(crate) fn encode_code_units(&self, code_units: &[u16], last: bool) -> Fallible<Vec<u8>> {        
        let code_unit_bytes: &[u8] = unsafe { std::mem::transmute(code_units) };
        let mut input = code_unit_bytes;
        let mut remaining = self.remaining.borrow_mut();
        if !remaining.is_empty() {
            remaining.extend_from_slice(code_unit_bytes);
            input = &remaining[..];
        }
        let mut encoder = self.utf16_encoder.borrow_mut();
        let mut output = String::with_capacity(encoder.max_utf8_buffer_length(input.len())
            .ok_or_else(|| Error::Type("Expected utf-8 encoding would overflow".to_owned()))?);

        let (result, read, _replaced) = encoder.decode_to_string(&input, &mut output, last);

        let (_consumed, new_remaining) = input.split_at(read);
        *remaining = new_remaining.to_vec();

        match result {
            encoding_rs::CoderResult::InputEmpty => Ok(output.into_bytes()),
            encoding_rs::CoderResult::OutputFull => unreachable!(),
        }
    }
}