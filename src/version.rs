// SPDX-License-Identifier: Apache-2.0
//! Adobe version strings, compared the way Adobe's own records spell them.
//!
//! Builds are dotted numbers with a varying number of parts: "27.11.0.26", "26.3.1",
//! "15.6.0.202609251514", and some feeds zero-pad ("25.3.0.084" names the same build an exe
//! reports as 25.3.0.84). Comparison is numeric per part with missing parts read as 0, so
//! "27.10" > "27.9" and "26.3.1" == "26.3.1.0".

use std::cmp::Ordering;

/// Numeric parts; a non-numeric part reads as 0 (never seen in Adobe's data, but never fatal).
pub fn key(v: &str) -> Vec<u64> {
    v.trim().split('.').map(|c| c.trim().parse::<u64>().unwrap_or(0)).collect()
}

pub fn cmp(a: &str, b: &str) -> Ordering {
    let (ka, kb) = (key(a), key(b));
    let n = ka.len().max(kb.len());
    for i in 0..n {
        let (x, y) = (ka.get(i).copied().unwrap_or(0), kb.get(i).copied().unwrap_or(0));
        match x.cmp(&y) {
            Ordering::Equal => continue,
            o => return o,
        }
    }
    Ordering::Equal
}

pub fn eq(a: &str, b: &str) -> bool {
    cmp(a, b) == Ordering::Equal
}

/// The major number ("27" of "27.11.0.26").
pub fn major(v: &str) -> u64 {
    key(v).first().copied().unwrap_or(0)
}

/// The short label people use for a build: "27.11" for 27.11.0.26, "26.5.2" for 26.5.2.5,
/// "15.6" for 15.6.0.202609251514. Trailing ".0" parts and the build number (4th part) go.
pub fn label(v: &str) -> String {
    let parts: Vec<&str> = v.trim().split('.').collect();
    let mut keep: Vec<&str> = parts.iter().take(3).copied().collect();
    while keep.len() > 2 && keep.last().map(|p| p.trim_start_matches('0').is_empty()).unwrap_or(false) {
        keep.pop();
    }
    keep.join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_not_lexical() {
        assert_eq!(cmp("27.10.0.3", "27.9.0.21"), Ordering::Greater);
        assert_eq!(cmp("26.3.1", "26.3.1.0"), Ordering::Equal);
        assert!(eq("25.3.0.084", "25.3.0.84"));
        assert_eq!(cmp("15.6.0.202609251514", "15.5.1.202608131348"), Ordering::Greater);
        assert_eq!(major("27.11.0.26"), 27);
    }

    #[test]
    fn labels_read_like_adobe_writes_them() {
        assert_eq!(label("27.11.0.26"), "27.11");
        assert_eq!(label("26.5.2.5"), "26.5.2");
        assert_eq!(label("15.6.0.202609251514"), "15.6");
        assert_eq!(label("26.3.1.1"), "26.3.1");
        assert_eq!(label("27.0.0.25"), "27.0");
    }
}
