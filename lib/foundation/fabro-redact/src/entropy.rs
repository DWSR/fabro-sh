use std::sync::LazyLock;

use regex::Regex;
use ulid::Ulid;

use super::Region;

/// Matches high-entropy alphanumeric strings (10+ chars).
/// Excludes `/` to avoid matching file paths as single tokens.
static SECRET_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9+_=-]{10,}").expect("hardcoded regex should compile"));

const ENTROPY_THRESHOLD: f64 = 4.5;

/// The length of a ULID's canonical text form.
const ULID_LEN: usize = 26;

/// The timestamp window a token must fall in to be recognized as a ULID:
/// 2020-01-01T00:00:00Z through 2100-01-01T00:00:00Z.
///
/// Fabro's run, session, turn, message and pair IDs are ULIDs, and a
/// ULID occasionally has enough distinct characters to cross the entropy
/// threshold. Exempting the format keeps those IDs intact everywhere they
/// appear (URLs, branch names, commit messages, free text) instead of
/// protecting one field at a time. The window bounds what the exemption
/// can miss: a secret would have to be exactly 26 uppercase Crockford
/// base32 characters whose leading 48 bits decode to a date in this
/// range, which is roughly one in a hundred of the already rare tokens
/// that take that shape. The gitleaks patterns still run on every token.
const ULID_MIN_TIMESTAMP_MS: u64 = 1_577_836_800_000;
const ULID_MAX_TIMESTAMP_MS: u64 = 4_102_444_800_000;

/// Compute Shannon entropy (bits per byte) of a string.
pub(super) fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut freq = [0u32; 256];
    for &b in s.as_bytes() {
        freq[b as usize] += 1;
    }
    let len = s.len() as f64;
    let mut entropy = 0.0;
    for &count in &freq {
        if count > 0 {
            let p = f64::from(count) / len;
            entropy -= p * p.log2();
        }
    }
    entropy
}

/// Find high-entropy alphanumeric tokens in `s`.
///
/// Returns regions where tokens match `[A-Za-z0-9+_=-]{10,}` and have
/// Shannon entropy above the threshold (4.5 bits). Protects against
/// consuming characters from JSON escape sequences.
///
/// An assignment token (`NAME=value`) is measured and redacted by its
/// value alone. Measuring the pair merges the name's charset into the
/// value's and pushes innocuous values (a pure-hex git SHA can never
/// exceed 4.0 bits by itself) over the threshold, and redacting the
/// pair destroys the name that says what was redacted.
pub(super) fn find_entropy_regions(s: &str) -> Vec<Region> {
    let mut regions = Vec::new();
    for m in SECRET_PATTERN.find_iter(s) {
        let mut start = m.start();
        let end = m.end();

        // Protect against consuming characters from JSON escape sequences.
        // E.g. in "controller.go\nmodel.go", regex could match "nmodel"
        // (consuming 'n' from '\n'). Skip the escape character to avoid
        // creating invalid escape sequences after replacement.
        if start > 0 && s.as_bytes()[start - 1] == b'\\' {
            match s.as_bytes()[start] {
                b'n' | b't' | b'r' | b'b' | b'f' | b'u' | b'"' | b'\\' | b'/' => {
                    start += 1;
                    if end - start < 10 {
                        continue;
                    }
                }
                _ => {}
            }
        }

        if let Some(offset) = assignment_value_offset(&s[start..end]) {
            start += offset;
        }

        // Identifiers are not secrets: a token that is a ULID, or ends in
        // `-ULID`, `_ULID` or `=ULID` segments, is measured without them.
        let end = start + ulid_free_len(&s[start..end]);
        if end - start < 10 {
            continue;
        }

        if shannon_entropy(&s[start..end]) > ENTROPY_THRESHOLD {
            regions.push(Region { start, end });
        }
    }
    regions
}

/// For an assignment token (`NAME=value` with an identifier-shaped name),
/// return the byte offset where the value begins. Entropy above the 4.5-bit
/// threshold needs at least 23 distinct characters, so a value too short to
/// qualify simply measures under the threshold; no length guard is needed.
fn assignment_value_offset(token: &str) -> Option<usize> {
    let eq = token.find('=')?;
    let value = &token[eq + 1..];
    if value.is_empty() || value.starts_with('=') {
        return None;
    }
    let name = &token[..eq];
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(eq + 1)
}

/// Whether `token` is a ULID in canonical form (26 uppercase Crockford
/// base32 characters) with a timestamp inside the recognized window.
///
/// The alphabet is checked by hand because the `ulid` crate's decoder is
/// lenient about case; the exemption is meant to be strict.
fn is_ulid(token: &str) -> bool {
    token.len() == ULID_LEN
        && token.bytes().all(|b| {
            matches!(
                b,
                b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
            )
        })
        && Ulid::from_string(token).is_ok_and(|ulid| {
            (ULID_MIN_TIMESTAMP_MS..ULID_MAX_TIMESTAMP_MS).contains(&ulid.timestamp_ms())
        })
}

/// The length of `token` once every trailing ULID segment is removed.
///
/// A token that is itself a ULID shrinks to nothing. A trailing ULID
/// preceded by `-`, `_` or `=` is stripped together with its separator,
/// repeatedly, so `20261020-<ULID>` leaves the date and
/// `<secret>-<ULID>` leaves the secret to be measured on its own. Twenty-six
/// trailing characters that are not set off by a separator stay part of the
/// token: they could be the tail of a longer secret.
fn ulid_free_len(token: &str) -> usize {
    let mut len = token.len();
    loop {
        let Some(id_start) = len.checked_sub(ULID_LEN) else {
            return len;
        };
        if !is_ulid(&token[id_start..len]) {
            return len;
        }
        if id_start == 0 {
            return 0;
        }
        if !matches!(token.as_bytes()[id_start - 1], b'-' | b'_' | b'=') {
            return len;
        }
        len = id_start - 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run IDs that cross the entropy threshold: each has at least 23
    /// distinct characters out of 26.
    const HIGH_ENTROPY_ULIDS: [&str; 2] =
        ["01M5228EHCT0P73Z4BYGDRWJNS", "01M5228EHCT0P73Z4BYGDRWJNV"];

    const SECRET: &str = "xK9mZ2vL8nQ5rT1wY4bC7dF0gH3jE6p";

    #[test]
    fn high_entropy_ulids_cross_the_threshold_on_their_own() {
        for id in HIGH_ENTROPY_ULIDS {
            let e = shannon_entropy(id);
            assert!(e > ENTROPY_THRESHOLD, "{id}: expected > 4.5, got {e}");
        }
    }

    #[test]
    fn ulids_are_not_secrets() {
        for id in HIGH_ENTROPY_ULIDS {
            for text in [
                id.to_string(),
                format!("fabro/run/{id}"),
                format!("http://localhost:3000/runs/{id}"),
                format!("run_id={id}"),
                format!("20261020-{id}"),
                format!("fabro-run-{id}"),
                format!("checkpoint_{id}_{id}"),
            ] {
                let regions = find_entropy_regions(&text);
                assert!(regions.is_empty(), "{text}: got regions {regions:?}");
            }
        }
    }

    #[test]
    fn secrets_beside_a_ulid_are_still_found() {
        let id = HIGH_ENTROPY_ULIDS[0];
        let text = format!("{SECRET}-{id}");
        assert_eq!(find_entropy_regions(&text), vec![Region {
            start: 0,
            end:   SECRET.len(),
        }]);
        let text = format!("https://{SECRET}@example.com/runs/{id}");
        assert_eq!(find_entropy_regions(&text), vec![Region {
            start: "https://".len(),
            end:   "https://".len() + SECRET.len(),
        }]);
    }

    #[test]
    fn a_ulid_glued_to_other_characters_is_scanned_as_a_whole() {
        // No separator sets the ID off, so the token is measured intact.
        let text = format!("x{}", HIGH_ENTROPY_ULIDS[0]);
        assert!(shannon_entropy(&text) > ENTROPY_THRESHOLD);
        assert_eq!(find_entropy_regions(&text), vec![Region {
            start: 0,
            end:   text.len(),
        }]);
    }

    #[test]
    fn ulid_recognition_is_strict() {
        let id = HIGH_ENTROPY_ULIDS[0];
        assert!(is_ulid(id));
        // Lowercase is not canonical, even though the crate would decode it.
        assert!(!is_ulid(&id.to_ascii_lowercase()));
        assert!(!find_entropy_regions(&id.to_ascii_lowercase()).is_empty());
        // Wrong length or alphabet.
        assert!(!is_ulid(&id[1..]));
        assert!(!is_ulid(&format!("{id}0")));
        assert!(!is_ulid(&format!("I{}", &id[1..])));
        // A random secret that happens to be 26 base32 characters but whose
        // timestamp falls outside the window is still a secret.
        let ulid = Ulid::from_string(id).unwrap();
        let year_2010 = Ulid::from_parts(1_262_304_000_000, ulid.random()).to_string();
        let year_2150 = Ulid::from_parts(5_680_281_600_000, ulid.random()).to_string();
        assert!(!is_ulid(&year_2010), "{year_2010}");
        assert!(!is_ulid(&year_2150), "{year_2150}");
        // The window edges.
        assert!(is_ulid(
            &Ulid::from_parts(ULID_MIN_TIMESTAMP_MS, ulid.random()).to_string()
        ));
        assert!(!is_ulid(
            &Ulid::from_parts(ULID_MAX_TIMESTAMP_MS, ulid.random()).to_string()
        ));
    }

    #[test]
    fn ulid_free_len_strips_trailing_segments_only() {
        let id = HIGH_ENTROPY_ULIDS[0];
        assert_eq!(ulid_free_len(id), 0);
        assert_eq!(ulid_free_len(&format!("abc-{id}")), 3);
        assert_eq!(ulid_free_len(&format!("abc_{id}-{id}")), 3);
        assert_eq!(ulid_free_len(&format!("{id}-abc")), id.len() + 4);
        assert_eq!(ulid_free_len(&format!("x{id}")), id.len() + 1);
        assert_eq!(ulid_free_len("short"), 5);
    }

    #[test]
    fn entropy_empty_string() {
        assert!(shannon_entropy("").abs() < f64::EPSILON);
    }

    #[test]
    fn entropy_single_char_repeated() {
        assert!(shannon_entropy("aaaa").abs() < f64::EPSILON);
    }

    #[test]
    fn entropy_two_equal_chars() {
        let e = shannon_entropy("ab");
        assert!((e - 1.0).abs() < 0.001, "expected ~1.0, got {e}");
    }

    #[test]
    fn entropy_aws_key_above_3() {
        let e = shannon_entropy("AKIAIOSFODNN7EXAMPLE");
        assert!(e > 3.0, "expected > 3.0, got {e}");
    }

    #[test]
    fn regions_empty_for_normal_text() {
        assert!(find_entropy_regions("hello world").is_empty());
    }

    #[test]
    fn regions_finds_high_entropy_token() {
        // "key=xK9..." matches as one token, but only the value is
        // measured and flagged; the name survives redaction.
        let input = "key=xK9mZ2vL8nQ5rT1wY4bC7dF0gH3jE6p";
        let regions = find_entropy_regions(input);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start, "key=".len());
        assert_eq!(regions[0].end, input.len());
    }

    #[test]
    fn regions_find_padded_base64_tokens() {
        for input in [
            "WxFhjC5EAnh30M0JIe0Wa58Xb1BYf8kedTTdKUbbd9Y=",
            "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789ABCDEF==",
        ] {
            assert_eq!(find_entropy_regions(input), vec![Region {
                start: 0,
                end:   input.len(),
            }]);
        }
    }

    #[test]
    fn regions_empty_for_json_escape_sequence() {
        // "controller.go\nmodel.go" — the regex could match across the \n boundary
        let regions = find_entropy_regions(r"controller.go\nmodel.go");
        assert!(regions.is_empty(), "got regions: {regions:?}");
    }

    #[test]
    fn regions_empty_for_file_path() {
        // / is excluded from the pattern, so path segments are short
        let regions = find_entropy_regions("/tmp/test/controller.go");
        assert!(regions.is_empty(), "got regions: {regions:?}");
    }
}
