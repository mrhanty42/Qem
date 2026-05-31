// Property 19: reverse-DFA size-limit overflow surfaces as typed RegexCompileError
//
//
// When the reverse-DFA build for a regex pattern overflows its
// `dfa_size_limit` / `determinize_size_limit` ceiling, the build
// MUST surface that as a typed `RegexCompileError` carrying a
// non-empty diagnostic message — never as a `panic`, never as
// arithmetic overflow, never as OOM..
//
// # Test design
//
// The internal builder
// `qem::document::regex_search::build_reverse_dfa_with_limit` is
// `pub(crate)` and not visible from this integration crate. It is
// the natural surface to drive Property 19 because it accepts a
// caller-chosen size limit, so a tight 64 KiB cap reliably trips
// the typed-error path on any non-trivial bounded alternation —
// no need to reach the production 32 MiB ceiling through the
// forward-search API (which has its own ~10 MiB regex-crate
// limit and would reject the pattern up front instead of
// exercising the reverse-DFA path).
//
// The test reaches the same builder through a thin wrapper in the
// hidden `qem::document::__test_support` module, mirroring the
// pattern introduced for `align_byte_offset_floor` and
// `bytes_for_alignment` (and for `engine_for_encoding`).
// The wrapper drops the compiled DFA before returning so this
// integration test does not need to depend on `regex_automata`
// types; only the success-vs-typed-error outcome is observable.
//
// # Strategy
//
// Each case generates a wide bounded-alternation pattern of the
// shape
//
// `(t1|t2|...|tk){0,N}`
//
// for `N ∈ [128, 4096]` and `k ∈ [4, 12]`, drawn from a fixed pool
// of multi-byte tokens. This shape blows up determinization size
// super-linearly in both `N` and `k`: each alternation branch
// contributes its own bounded-repetition NFA and the cross-product
// determinizes into a wide DFA. Even the smallest case
// (`k=4`, `N=128`) overflows a 64 KiB ceiling reliably across
// `regex_automata` 0.4.x versions; the wider cases overflow with
// large headroom.
//
// For each pattern the test asserts:
//
// * If the helper returns `Err(_)`, the message is non-empty
// (the contract: typed diagnostic, not a panic).
// * If it returns `Ok(_)`, that is also legal — `prop_assume!`
// skips the case rather than failing it. is a contract
// on the FAILURE path: when overflow occurs it must be
// well-typed; it does not require every input to overflow.
// * Anything else (panic, stack overflow, OOM) fails the test
// structurally because proptest treats panics as case
// failures.
//
// `ProptestConfig::with_cases(64)` per spec.

use proptest::prelude::*;
use qem::document::__test_support::build_reverse_dfa_with_limit;

/// Tight reverse-DFA size limit used by Property 19. 64 KiB is small
/// enough that any non-trivial bounded alternation overflows
/// determinization, which lets the property reliably exercise the
/// typed-error path without needing a 32 MiB pattern. Production
/// `RegexSearchQuery::ensure_reverse` is unaffected — it still goes
/// through the project-wide 32 MiB ceiling.
const TEST_REVERSE_DFA_LIMIT_BYTES: usize = 64 * 1024;

/// Multi-character tokens used as alternation branches. Mixing
/// lengths and shared prefixes (`"alpha"`/`"alphabet"`
/// `"foo"`/`"foobar"`) widens the determinized state set faster
/// than equal-width tokens would, so even small `(k, N)` budgets
/// reliably overflow the 64 KiB ceiling.
const TOKENS: &[&str] = &[
    "foo", "bar", "baz", "quux", "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "alphabet",
    "foobar",
];

/// Builds `(t1|t2|...|tk){0,n}` from the first `k` tokens.
///
/// `k ∈ [4, TOKENS.len()]` and `n ∈ [128, 4096]`. The bounded
/// repetition is what drives the super-linear blow-up: the NFA for
/// `(alt){0,n}` unrolls `n` copies of `alt` joined by an optional
/// transition, and determinizing that with `MatchKind::LeftmostFirst`
/// produces `Θ(k·n)` states before any state-merging — far past
/// 64 KiB even on the small end of the parameter space.
fn build_wide_alternation(k: usize, n: usize) -> String {
    let mut alt = String::with_capacity(64);
    alt.push('(');
    for (i, token) in TOKENS.iter().take(k).enumerate() {
        if i > 0 {
            alt.push('|');
        }
        alt.push_str(token);
    }
    alt.push(')');
    format!("{alt}{{0,{n}}}")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 19: a reverse-DFA build that exceeds its
 /// `dfa_size_limit` / `determinize_size_limit` MUST return
 /// `Err(RegexCompileError)` with a non-empty message — never a
 /// panic, stack overflow, or OOM.
 ///
 /// The test runs `build_reverse_dfa_with_limit(pattern, 64 KiB)`
 /// with patterns generated to overflow the ceiling. A success
 /// (`Ok`) is `prop_assume!`-skipped — the property is one-sided:
 /// it pins the FAILURE path's shape, not the universality of
 /// failure.
    #[test]
    fn property_19_reverse_dfa_overflow_is_a_typed_error(
        k in 4usize..=TOKENS.len(),
        n in 128usize..=4_096,
    ) {
        let pattern = build_wide_alternation(k, n);

        match build_reverse_dfa_with_limit(&pattern, TEST_REVERSE_DFA_LIMIT_BYTES) {
            Ok(()) => {
 // The 64 KiB ceiling held. Skip — Property 19's
 // contract only governs the failure path.
                let reason = format!(
                    "reverse-DFA build fit under 64 KiB for k={k}, n={n}; nothing to check",
                );
                prop_assume!(false, "{}", reason);
            }
            Err(err) => {
                prop_assert!(
                    !err.message().is_empty(),
                    "RegexCompileError from reverse-DFA size-limit overflow must carry a \
                     non-empty diagnostic message; pattern length = {} bytes, k = {}, \
                     n = {}",
                    pattern.len(),
                    k,
                    n,
                );
            }
        }
    }
}
