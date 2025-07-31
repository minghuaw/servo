/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::ptr::{self, NonNull};
use std::rc::Rc;

use cipher::consts::U804;
use dom_struct::dom_struct;
use encoding_rs::Decoder;
use js::conversions::{latin1_to_string, ToJSValConvertible};
use js::jsapi::{JS_DeprecatedStringHasLatin1Chars, JS_GetLatin1StringCharsAndLength, JS_GetTwoByteStringCharsAndLength, Uint8Array};
use js::jsval::UndefinedValue;
use js::rust::{HandleObject as SafeHandleObject, HandleValue as SafeHandleValue, ToString};

use crate::dom::bindings::codegen::Bindings::TextEncoderStreamBinding::TextEncoderStreamMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::reflector::{Reflector, reflect_dom_object_with_proto};
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::textencodercommon::TextEncoderCommon;
use crate::dom::transformstreamdefaultcontroller::TransformerType;
use crate::dom::types::{GlobalScope, TransformStream, TransformStreamDefaultController};
use crate::script_runtime::{CanGc, JSContext as SafeJSContext};
use crate::{DomTypeHolder, DomTypes};

/// <https://encoding.spec.whatwg.org/#encode-and-enqueue-a-chunk>
#[allow(unsafe_code)]
pub(crate) fn encode_and_enqueue_a_chunk(
    cx: SafeJSContext,
    global: &GlobalScope,
    chunk: SafeHandleValue,
    encoder: &TextEncoderCommon,
    controller: &TransformStreamDefaultController,
    can_gc: CanGc,
) -> Fallible<()> {
    log::debug!("encode_and_enqueue_a_chunk");
    let output = unsafe {
        let js_str = NonNull::new(ToString(*cx, chunk))
            .ok_or_else(|| Error::Type("Converting to DOMString failed".to_owned()))?;
        if JS_DeprecatedStringHasLatin1Chars(js_str.as_ptr()) {
            let s = latin1_to_string(*cx, js_str.as_ptr());
            log::debug!("latin1 s: {:?}", s);
            s.into_bytes()
        } else {
            let mut length = 0;
            let chars =
                JS_GetTwoByteStringCharsAndLength(*cx, std::ptr::null(), js_str.as_ptr(), &mut length);
            assert!(!chars.is_null());
            let input: &[u16] = std::slice::from_raw_parts(chars, length);
            log::debug!("utf16 input: {:?}", input);
            encoder.encode_code_units(input, false)?
        }
    };

    log::debug!("output: {:?}", output);

    if output.is_empty() {
        return Ok(())
    }

    rooted!(in(*cx) let mut chunk = UndefinedValue());
    unsafe {
        output.to_jsval(*cx, chunk.handle_mut());
    }
    controller.enqueue(cx, global, chunk.handle(), can_gc)
}

/// <https://encoding.spec.whatwg.org/#encode-and-flush>
#[allow(unsafe_code)]
pub(crate) fn encode_and_flush(
    cx: SafeJSContext,
    global: &GlobalScope,
    encoder: &TextEncoderCommon,
    controller: &TransformStreamDefaultController,
    can_gc: CanGc,
) -> Fallible<()> {
    let output = encoder.encode_code_units(&[], true)?;

    if output.is_empty() {
        return Ok(())
    }

    rooted!(in(*cx) let mut chunk = UndefinedValue());
    unsafe { output.to_jsval(*cx, chunk.handle_mut()); }
    controller.enqueue(cx, global, chunk.handle(), can_gc)
}

/// <https://encoding.spec.whatwg.org/#textencoderstream>
#[dom_struct]
pub(crate) struct TextEncoderStream {
    reflector_: Reflector,

    /// This uses an `encoding_rs::Decoder` as opposed to `encoding_rs::Encoder`
    /// because `encoding_rs::Encoder` does NOT store pending high surrogate internally
    /// by design (see <https://github.com/hsivonen/encoding_rs/issues/82>) but
    /// the `encoding_rs::Decoder` does.
    ///
    /// <https://encoding.spec.whatwg.org/#textencoderstream-encoder>
    #[ignore_malloc_size_of = "Rc is hard"]
    encoder: Rc<TextEncoderCommon>,

    /// <https://streams.spec.whatwg.org/#generictransformstream>
    transform: Dom<TransformStream>,
}

impl TextEncoderStream {
    /// See documentation on [`TextEncoderStream::encoder`] for why a
    /// [`encoding_rs::Decoder`] is used.
    fn new_inherited(
        encoder: Rc<TextEncoderCommon>,
        transform: &TransformStream,
    ) -> TextEncoderStream {
        Self {
            reflector_: Reflector::new(),
            encoder,
            transform: Dom::from_ref(transform),
        }
    }

    fn new_with_proto(
        cx: SafeJSContext,
        global: &GlobalScope,
        proto: Option<SafeHandleObject>,
        can_gc: CanGc,
    ) -> Fallible<DomRoot<TextEncoderStream>> {
        let encoder = Rc::new(TextEncoderCommon::new_inherited());
        let transformer_type = TransformerType::Encoder(encoder.clone());

        let transform_stream = TransformStream::new_with_proto(global, None, can_gc);
        transform_stream.set_up(cx, global, transformer_type, can_gc)?;

        Ok(reflect_dom_object_with_proto(
            Box::new(TextEncoderStream::new_inherited(encoder, &transform_stream)),
            global,
            proto,
            can_gc,
        ))
    }
}

#[allow(non_snake_case)]
impl TextEncoderStreamMethods<DomTypeHolder> for TextEncoderStream {
    /// <https://encoding.spec.whatwg.org/#dom-textencoderstream>
    fn Constructor(
        global: &<DomTypeHolder as DomTypes>::GlobalScope,
        proto: Option<SafeHandleObject>,
        can_gc: CanGc,
    ) -> Fallible<DomRoot<<DomTypeHolder as DomTypes>::TextEncoderStream>> {
        Self::new_with_proto(GlobalScope::get_cx(), global, proto, can_gc)
    }

    /// <https://encoding.spec.whatwg.org/#dom-textencoder-encoding>
    fn Encoding(&self) -> DOMString {
        DOMString::from("utf-8")
    }

    /// <https://streams.spec.whatwg.org/#dom-generictransformstream-readable>
    fn Readable(&self) -> DomRoot<<DomTypeHolder as script_bindings::DomTypes>::ReadableStream> {
        self.transform.get_readable()
    }

    /// <https://streams.spec.whatwg.org/#dom-generictransformstream-writable>
    fn Writable(&self) -> DomRoot<<DomTypeHolder as DomTypes>::WritableStream> {
        self.transform.get_writable()
    }
}
