// Property 10: Utf16Engine adheres to its endianness.
//
//
// `Utf16Engine<E>` is parameterised by endianness and must only honour
// the LF / CR byte patterns of its own endianness:
//
// * `Utf16Engine<LittleEndian>` looks for `[0x0A, 0x00]` as LF and
// `[0x0D, 0x00]` as CR. It must NOT interpret the BE-form
// cells `[0x00, 0x0A]` (BE LF) or `[0x00, 0x0D]` (BE CR) as line
// breaks, even when those cells sit at even byte positions.
//
// * `Utf16Engine<BigEndian>` looks for `[0x00, 0x0A]` as LF and
// `[0x00, 0x0D]` as CR. Symmetrically, it must NOT
// interpret the LE-form cells `[0x0A, 0x00]` (LE LF) or
// `[0x0D, 0x00]` (LE CR) at even positions as line breaks.
//
// spells this out as a stand-alone obligation: "WHILE an
// instance of `Utf16Engine` is configured as `LittleEndian`, the engine
// SHALL search only for LE-form line endings and SHALL NOT process
// BE-forms; conversely for `BigEndian`." A regression that cross-wires
// the LF/CR constants between LE and BE — easy to slip into when
// refactoring the `Endian` trait — would be invisible to Properties 3
// (Class A only) and 9 (alignment), so this property pins it down
// directly.
//
// Test strategy. For each case the generator produces a sequence of
// "wrong-endian" cells (BE-form LF / CR for the LE engine and LE-form
// LF / CR for the BE engine). The cells are concatenated into a byte
// buffer with no other content, so every 2-byte aligned position that
// the engine inspects is exactly a wrong-endian cell. The engine's
// `next_line_start(bytes, bytes.len(), 0)` must therefore return
// `bytes.len()` — the "no terminator found" sentinel — proving that
// the wrong-endian cells were treated as ordinary code units rather
// than line breaks (the cell layouts cannot collide with surrogate
// pairs because LE LF / LE CR / BE LF / BE CR all decode as either
// `U+0A00` / `U+0D00` (LE) or `U+000A` / `U+000D` (BE) and are
// outside the surrogate range).
//
// The engine is reached through `qem::document::__test_support`, the
// `#[doc(hidden)]` re-export module introduced for the
// integration property tests under `tests/encoding_engine/`. The cases
// count is intentionally pinned at 64 for this spec.

use proptest::prelude::*;
use qem::document::__test_support::{engine_for_encoding, EncodingEngine};
use qem::DocumentEncoding;

/// Maximum number of 2-byte cells in a generated buffer. 128 cells map
/// to a 256-byte buffer, which is more than enough to exercise long
/// runs of wrong-endian terminators while keeping shrinking quick.
const PROP10_MAX_CELLS: usize = 128;

/// One generator atom. Each atom materialises into exactly one 2-byte
/// cell whose layout is the wrong endianness for the engine under test.
#[derive(Debug, Clone, Copy)]
enum WrongEndianCell {
 /// LF cell laid out for the *opposite* endianness:
 ///
 /// - For the LE engine: BE LF = `[0x00, 0x0A]`.
 /// - For the BE engine: LE LF = `[0x0A, 0x00]`.
    Lf,
 /// CR cell laid out for the *opposite* endianness:
 ///
 /// - For the LE engine: BE CR = `[0x00, 0x0D]`.
 /// - For the BE engine: LE CR = `[0x0D, 0x00]`.
    Cr,
}

fn wrong_endian_cell_strategy() -> impl Strategy<Value = WrongEndianCell> {
    prop_oneof![Just(WrongEndianCell::Lf), Just(WrongEndianCell::Cr)]
}

fn wrong_endian_cells_strategy() -> impl Strategy<Value = Vec<WrongEndianCell>> {
    prop::collection::vec(wrong_endian_cell_strategy(), 0..=PROP10_MAX_CELLS)
}

/// Builds a byte buffer using BE-form LF / CR cells. Fed to the LE
/// engine to verify it does not recognise BE line-ending cells.
fn encode_be_form_cells(cells: &[WrongEndianCell]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(cells.len() * 2);
    for cell in cells {
        let pair: [u8; 2] = match cell {
            WrongEndianCell::Lf => [0x00, 0x0A], // BE LF
            WrongEndianCell::Cr => [0x00, 0x0D], // BE CR
        };
        bytes.extend_from_slice(&pair);
    }
    bytes
}

/// Builds a byte buffer using LE-form LF / CR cells. Fed to the BE
/// engine to verify it does not recognise LE line-ending cells.
fn encode_le_form_cells(cells: &[WrongEndianCell]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(cells.len() * 2);
    for cell in cells {
        let pair: [u8; 2] = match cell {
            WrongEndianCell::Lf => [0x0A, 0x00], // LE LF
            WrongEndianCell::Cr => [0x0D, 0x00], // LE CR
        };
        bytes.extend_from_slice(&pair);
    }
    bytes
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 10: `Utf16Engine<LittleEndian>` ignores BE-form line
 /// endings, and `Utf16Engine<BigEndian>` ignores LE-form line
 /// endings. The two engines are exercised in the same test against
 /// the same generated cell sequence (encoded into the appropriate
 /// wrong-endian byte form for each side), so a regression that
 /// cross-wires LF/CR constants in either direction is caught with
 /// the same proptest seed.
    #[test]
    fn property_10_utf16_engine_adheres_to_its_endianness(
        cells in wrong_endian_cells_strategy(),
    ) {
 // ---- LE engine, BE-form cells ------------------
        let le_encoding = DocumentEncoding::utf16le();
        let le_engine: &dyn EncodingEngine = engine_for_encoding(le_encoding);
        prop_assert_eq!(
            le_engine.encoding(),
            le_encoding,
            "engine_for_encoding must return an engine bound to {}",
            le_encoding.name()
        );

        let le_bytes = encode_be_form_cells(&cells);
        let le_len = le_bytes.len();
        let le_result = le_engine.next_line_start(&le_bytes, le_len, 0);
        prop_assert_eq!(
            le_result,
            le_len,
            "{}: BE-form line-ending cells must NOT be interpreted as line \
             breaks; expected next_line_start == bytes.len()={}, got {}; \
             cells={:?}, bytes={:02X?}",
            le_encoding.name(),
            le_len,
            le_result,
            cells,
            &le_bytes
        );

 // ---- BE engine, LE-form cells ------------------
        let be_encoding = DocumentEncoding::utf16be();
        let be_engine: &dyn EncodingEngine = engine_for_encoding(be_encoding);
        prop_assert_eq!(
            be_engine.encoding(),
            be_encoding,
            "engine_for_encoding must return an engine bound to {}",
            be_encoding.name()
        );

        let be_bytes = encode_le_form_cells(&cells);
        let be_len = be_bytes.len();
        let be_result = be_engine.next_line_start(&be_bytes, be_len, 0);
        prop_assert_eq!(
            be_result,
            be_len,
            "{}: LE-form line-ending cells must NOT be interpreted as line \
             breaks; expected next_line_start == bytes.len()={}, got {}; \
             cells={:?}, bytes={:02X?}",
            be_encoding.name(),
            be_len,
            be_result,
            cells,
            &be_bytes
        );
    }
}

// ---------------------------------------------------------------------
// Deterministic example checks. The proptest above sweeps the random
// surface; these focused cases pin the most obvious cross-wiring
// regressions to specific inputs so a failing build fingerprints the
// exact branch that broke (LE-engine reading BE constants, or
// vice versa).
// ---------------------------------------------------------------------

#[test]
fn property_10_le_engine_ignores_be_lf_cell() {
 // BE LF = [0x00, 0x0A]. An LE engine that mistakenly treated this
 // as LF would return 2 (the byte just past the cell). The correct
 // behaviour is to walk past the cell and return bytes.len()=2 as
 // the "no terminator found" sentinel.
    let engine: &dyn EncodingEngine = engine_for_encoding(DocumentEncoding::utf16le());
    let bytes: [u8; 2] = [0x00, 0x0A];
    let result = engine.next_line_start(&bytes, bytes.len(), 0);
    assert_eq!(
        result,
        bytes.len(),
        "UTF-16LE: BE LF cell [0x00, 0x0A] at byte 0 must NOT be \
         interpreted as LF; expected result={}, got {result}",
        bytes.len()
    );
}

#[test]
fn property_10_le_engine_ignores_be_cr_cell() {
 // BE CR = [0x00, 0x0D]. Same shape as the LF case.
    let engine: &dyn EncodingEngine = engine_for_encoding(DocumentEncoding::utf16le());
    let bytes: [u8; 2] = [0x00, 0x0D];
    let result = engine.next_line_start(&bytes, bytes.len(), 0);
    assert_eq!(
        result,
        bytes.len(),
        "UTF-16LE: BE CR cell [0x00, 0x0D] at byte 0 must NOT be \
         interpreted as CR; expected result={}, got {result}",
        bytes.len()
    );
}

#[test]
fn property_10_be_engine_ignores_le_lf_cell() {
 // LE LF = [0x0A, 0x00]. A BE engine that mistakenly treated this
 // as LF would return 2 instead of bytes.len()=2 as the "no
 // terminator" sentinel.
    let engine: &dyn EncodingEngine = engine_for_encoding(DocumentEncoding::utf16be());
    let bytes: [u8; 2] = [0x0A, 0x00];
    let result = engine.next_line_start(&bytes, bytes.len(), 0);
    assert_eq!(
        result,
        bytes.len(),
        "UTF-16BE: LE LF cell [0x0A, 0x00] at byte 0 must NOT be \
         interpreted as LF; expected result={}, got {result}",
        bytes.len()
    );
}

#[test]
fn property_10_be_engine_ignores_le_cr_cell() {
 // LE CR = [0x0D, 0x00]. Same shape as the LF case.
    let engine: &dyn EncodingEngine = engine_for_encoding(DocumentEncoding::utf16be());
    let bytes: [u8; 2] = [0x0D, 0x00];
    let result = engine.next_line_start(&bytes, bytes.len(), 0);
    assert_eq!(
        result,
        bytes.len(),
        "UTF-16BE: LE CR cell [0x0D, 0x00] at byte 0 must NOT be \
         interpreted as CR; expected result={}, got {result}",
        bytes.len()
    );
}

#[test]
fn property_10_le_engine_walks_past_runs_of_wrong_endian_cells() {
 // A long alternating run of BE-form LF / CR cells. The LE engine
 // must walk past every cell and report no terminator.
    let engine: &dyn EncodingEngine = engine_for_encoding(DocumentEncoding::utf16le());
    let mut bytes = Vec::with_capacity(16);
    for i in 0..8 {
        if i % 2 == 0 {
            bytes.extend_from_slice(&[0x00, 0x0A]); // BE LF
        } else {
            bytes.extend_from_slice(&[0x00, 0x0D]); // BE CR
        }
    }
    let result = engine.next_line_start(&bytes, bytes.len(), 0);
    assert_eq!(
        result,
        bytes.len(),
        "UTF-16LE: a run of BE-form LF/CR cells must NOT contain any \
         line break; expected result={}, got {result}",
        bytes.len()
    );
}

#[test]
fn property_10_be_engine_walks_past_runs_of_wrong_endian_cells() {
 // Symmetric to the LE case: a long alternating run of LE-form
 // LF / CR cells fed to the BE engine must report no terminator.
    let engine: &dyn EncodingEngine = engine_for_encoding(DocumentEncoding::utf16be());
    let mut bytes = Vec::with_capacity(16);
    for i in 0..8 {
        if i % 2 == 0 {
            bytes.extend_from_slice(&[0x0A, 0x00]); // LE LF
        } else {
            bytes.extend_from_slice(&[0x0D, 0x00]); // LE CR
        }
    }
    let result = engine.next_line_start(&bytes, bytes.len(), 0);
    assert_eq!(
        result,
        bytes.len(),
        "UTF-16BE: a run of LE-form LF/CR cells must NOT contain any \
         line break; expected result={}, got {result}",
        bytes.len()
    );
}
