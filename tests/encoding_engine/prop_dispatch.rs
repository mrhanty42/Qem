// Property 5: engine_for_encoding stably routes and caches.
//
//
// Property 5 has two halves and the test mirrors that structure:
//
// (a) Caching / pointer stability. For any pair of encoding labels
// `(l1, l2)` such that `DocumentEncoding::from_label(l1).name() ==
// DocumentEncoding::from_label(l2).name()`, the two calls
// `engine_for_encoding(e1)` and `engine_for_encoding(e2)` must
// return the *same* `&'static dyn EncodingEngine` instance — i.e.
// a single shared trait object with a stable address. The dyn
// trait object is two words (data + vtable); the property we
// care about is that the underlying data pointer is the same, so
// we cast both references to `*const ()` (the data half) and
// compare with `==`. That captures `std::ptr::eq` semantics for a
// trait-object reference without having to construct two
// wide-pointer values explicitly.
//
// (b) Self-identification. For any single label `l` drawn from
// Class A ∪ {UTF-8}, the engine returned by `engine_for_encoding`
// must self-report the same encoding through `engine.encoding()`.
// That is the contract `EncodingEngine::encoding` exposes to the
// rest of the document layer.
//
// Class B encodings (UTF-16 LE/BE, Shift_JIS, GB18030, EUC-KR) ship
// their own engines in Phases 6 and 7. Until those land
// `engine_for_encoding` falls back to the UTF-8 engine for those
// labels (`engine.encoding() == UTF-8`, not the requested label), so
// they are excluded from the strategy here. Phases 6/7 will extend
// `CLASS_AB_LABELS` once their engines exist.
//
// The engine is reached through `qem::document::__test_support`, the
// `#[doc(hidden)]` re-export module introduced for the
// integration property tests under `tests/encoding_engine/`. The cases
// count is intentionally pinned at 64 for this spec.

use proptest::prelude::*;
use qem::document::__test_support::{engine_for_encoding, EncodingEngine};
use qem::DocumentEncoding;

/// Class A encodings the spec wires through `SingleByteEngine`
/// plus UTF-8 which routes through `Utf8Engine`. These are the labels
/// for which `engine_for_encoding` is guaranteed (today) to return an
/// engine whose `encoding().name()` round-trips to the requested label.
/// Class B labels (UTF-16 LE/BE, Shift_JIS, GB18030, EUC-KR) join this
/// list once Phases 6/7 ship their engines.
const CLASS_AB_LABELS: &[&str] = &[
    "UTF-8",
    "windows-1250",
    "windows-1251",
    "windows-1252",
    "windows-1253",
    "windows-1254",
    "windows-1255",
    "windows-1256",
    "windows-1257",
    "windows-1258",
    "windows-874",
    "ISO-8859-2",
    "ISO-8859-3",
    "ISO-8859-4",
    "ISO-8859-5",
    "ISO-8859-7",
    "ISO-8859-10",
    "ISO-8859-13",
    "ISO-8859-14",
    "ISO-8859-15",
    "ISO-8859-16",
    "KOI8-R",
    "KOI8-U",
    "IBM866",
    "macintosh",
    "x-mac-cyrillic",
];

fn label_strategy() -> impl Strategy<Value = &'static str> {
    prop::sample::select(CLASS_AB_LABELS.to_vec())
}

fn encoding_strategy() -> impl Strategy<Value = DocumentEncoding> {
    label_strategy().prop_map(|label| {
        DocumentEncoding::from_label(label)
            .unwrap_or_else(|| panic!("encoding_rs should know {label}"))
    })
}

/// Casts a trait-object reference to a raw `*const ()` for identity
/// comparison. Comparing the *data* half of the wide pointer is the
/// equivalent of `std::ptr::eq` for the underlying engine instance:
/// two `&'static dyn EncodingEngine` values returned by the dispatcher
/// for the same encoding name must point at the same backing object.
fn engine_data_ptr(engine: &dyn EncodingEngine) -> *const () {
    engine as *const dyn EncodingEngine as *const ()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 5: `engine_for_encoding` is stable (returns the same
 /// `&'static dyn EncodingEngine` instance for repeated calls with
 /// equivalent encodings) and routes correctly (the returned
 /// engine self-reports the requested encoding).
 ///
 /// The test draws *two* encodings independently — sometimes the
 /// same label, sometimes different — so proptest can shrink toward
 /// minimal counterexamples for both branches:
 ///
 /// * `e1.name() == e2.name()` exercises the caching contract:
 /// two lookups for the same label must hit the same cached
 /// instance.
 /// * `e1.name() != e2.name()` exercises the routing contract:
 /// each lookup is independent and the per-encoding engine must
 /// still self-report correctly.
 ///
 /// Both halves of Property 5 (caching and self-identification) are
 /// asserted on every case, so even when the two labels differ we
 /// still verify `engine.encoding() == e` for both.
    #[test]
    fn property_5_engine_for_encoding_stably_routes_and_caches(
        e1 in encoding_strategy(),
        e2 in encoding_strategy(),
    ) {
        let engine1: &'static dyn EncodingEngine = engine_for_encoding(e1);
        let engine2: &'static dyn EncodingEngine = engine_for_encoding(e2);

 // (b) Self-identification: every engine returned by the
 // dispatcher must self-report the encoding it was looked up
 // for. This holds for both `e1` and `e2` independently.
        prop_assert_eq!(
            engine1.encoding(),
            e1,
            "engine_for_encoding({}) must return an engine that self-reports as {}, \
             got {}",
            e1.name(), e1.name(), engine1.encoding().name(),
        );
        prop_assert_eq!(
            engine2.encoding(),
            e2,
            "engine_for_encoding({}) must return an engine that self-reports as {}, \
             got {}",
            e2.name(), e2.name(), engine2.encoding().name(),
        );

 // (a) Caching / pointer stability: when both lookups target the
 // same encoding name, they must hit the same cached static
 // instance. The data half of the wide pointer is the address
 // of the backing engine; comparing it captures the same
 // identity guarantee as `std::ptr::eq` for trait-object refs.
        if e1.name() == e2.name() {
            let p1 = engine_data_ptr(engine1);
            let p2 = engine_data_ptr(engine2);
            prop_assert_eq!(
                p1, p2,
                "engine_for_encoding({}) must return the same cached static instance \
                 across calls; got distinct data pointers {:p} and {:p}",
                e1.name(), p1, p2,
            );
        }
    }
}
