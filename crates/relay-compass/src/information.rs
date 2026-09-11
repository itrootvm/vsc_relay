use std::collections::{BTreeSet, HashSet};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

const PROFILE_MAGIC: &[u8; 8] = b"VSCRIF02";
const PROFILE_DEPTH: usize = 4;
const PROFILE_WIDTH: usize = 16_384;
const SEEN_WORDS: usize = 16_384;
const MAX_DOCUMENT_CHARS: usize = 32_768;
const CHECKSUM_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InformationProfile {
    documents: u64,
    counts: Vec<u32>,
    seen: Vec<u64>,
}

impl Default for InformationProfile {
    fn default() -> Self {
        Self {
            documents: 0,
            counts: vec![0; PROFILE_DEPTH * PROFILE_WIDTH],
            seen: vec![0; SEEN_WORDS],
        }
    }
}

impl InformationProfile {
    pub fn documents(&self) -> u64 {
        self.documents
    }

    pub fn memory_bytes(&self) -> usize {
        self.counts.len() * std::mem::size_of::<u32>()
            + self.seen.len() * std::mem::size_of::<u64>()
    }

    fn valid_shape(&self) -> bool {
        self.counts.len() == PROFILE_DEPTH * PROFILE_WIDTH && self.seen.len() == SEEN_WORDS
    }

    pub fn observe_document(&mut self, document_id: &str, text: &str, key: &[u8; 32]) -> bool {
        if !self.valid_shape() || text.trim().is_empty() {
            return false;
        }
        let seen_indexes = bloom_indexes(document_id, key);
        if seen_indexes
            .iter()
            .all(|index| self.seen[*index / 64] & (1u64 << (*index % 64)) != 0)
        {
            return false;
        }
        for index in seen_indexes {
            self.seen[index / 64] |= 1u64 << (index % 64);
        }

        for unit in signal_units(text) {
            for (depth, index) in sketch_indexes(&unit, key).into_iter().enumerate() {
                let offset = depth * PROFILE_WIDTH + index;
                self.counts[offset] = self.counts[offset].saturating_add(1);
            }
        }
        self.documents = self.documents.saturating_add(1);
        true
    }

    pub fn document_frequency(&self, unit: &str, key: &[u8; 32]) -> u32 {
        if !self.valid_shape() || self.documents == 0 {
            return 0;
        }
        sketch_indexes(unit, key)
            .into_iter()
            .enumerate()
            .map(|(depth, index)| self.counts[depth * PROFILE_WIDTH + index])
            .min()
            .unwrap_or(0)
            .min(self.documents.min(u32::MAX as u64) as u32)
    }

    pub fn unit_weight(&self, unit: &str, key: &[u8; 32]) -> f32 {
        let n = self.documents as f64;
        let df = self.document_frequency(unit, key) as f64;
        (1.0 + ((n + 1.0) / (df + 1.0)).ln()) as f32
    }

    pub fn ranked_terms(&self, text: &str, key: &[u8; 32]) -> Vec<String> {
        let mut terms = raw_terms(text);
        let mut ranked = terms
            .drain(..)
            .enumerate()
            .map(|(position, term)| {
                let weight = self.unit_weight(&word_unit(&term), key);
                (term, weight, position)
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
        ranked.into_iter().map(|(term, _, _)| term).collect()
    }

    pub fn information_mass(&self, text: &str, key: &[u8; 32]) -> f32 {
        signal_units(text)
            .into_iter()
            .map(|unit| self.unit_weight(&unit, key))
            .sum()
    }

    pub fn weighted_recall(&self, source: &str, candidate: &str, key: &[u8; 32]) -> Option<f32> {
        let source = signal_units(source);
        if source.is_empty() {
            return None;
        }
        let candidate = signal_units(candidate);
        let mut total = 0.0f32;
        let mut shared = 0.0f32;
        for unit in source {
            let weight = self.unit_weight(&unit, key);
            total += weight;
            if candidate.contains(&unit) {
                shared += weight;
            }
        }
        (total > 0.0).then_some((shared / total).clamp(0.0, 1.0))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(
            PROFILE_MAGIC.len() + 8 + self.counts.len() * 4 + self.seen.len() * 8 + CHECKSUM_BYTES,
        );
        bytes.extend_from_slice(PROFILE_MAGIC);
        bytes.extend_from_slice(&self.documents.to_le_bytes());
        for value in &self.counts {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in &self.seen {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let checksum = blake3::hash(&bytes);
        bytes.extend_from_slice(checksum.as_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let payload_len =
            PROFILE_MAGIC.len() + 8 + PROFILE_DEPTH * PROFILE_WIDTH * 4 + SEEN_WORDS * 8;
        if bytes.len() != payload_len + CHECKSUM_BYTES
            || bytes.get(..PROFILE_MAGIC.len())? != PROFILE_MAGIC
        {
            return None;
        }
        let (payload, expected) = bytes.split_at(payload_len);
        if blake3::hash(payload).as_bytes() != expected {
            return None;
        }
        let mut cursor = PROFILE_MAGIC.len();
        let documents = take_u64(payload, &mut cursor)?;
        let mut counts = Vec::with_capacity(PROFILE_DEPTH * PROFILE_WIDTH);
        for _ in 0..PROFILE_DEPTH * PROFILE_WIDTH {
            counts.push(take_u32(payload, &mut cursor)?);
        }
        let mut seen = Vec::with_capacity(SEEN_WORDS);
        for _ in 0..SEEN_WORDS {
            seen.push(take_u64(payload, &mut cursor)?);
        }
        (cursor == payload.len()).then_some(Self {
            documents,
            counts,
            seen,
        })
    }
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(*cursor..*cursor + 4)?.try_into().ok()?;
    *cursor += 4;
    Some(u32::from_le_bytes(raw))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(*cursor..*cursor + 8)?.try_into().ok()?;
    *cursor += 8;
    Some(u64::from_le_bytes(raw))
}

fn keyed_digest(domain: &[u8], value: &str, key: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(key);
    hasher.update(domain);
    hasher.update(&[0]);
    hasher.update(value.as_bytes());
    *hasher.finalize().as_bytes()
}

fn sketch_indexes(unit: &str, key: &[u8; 32]) -> [usize; PROFILE_DEPTH] {
    let digest = keyed_digest(b"vsc-relay-information-unit-v1", unit, key);
    std::array::from_fn(|depth| {
        let start = depth * 8;
        let raw = u64::from_le_bytes(digest[start..start + 8].try_into().unwrap_or([0; 8]));
        raw as usize & (PROFILE_WIDTH - 1)
    })
}

fn bloom_indexes(document_id: &str, key: &[u8; 32]) -> [usize; PROFILE_DEPTH] {
    let digest = keyed_digest(b"vsc-relay-information-document-v1", document_id, key);
    let bits = SEEN_WORDS * 64;
    std::array::from_fn(|depth| {
        let start = depth * 8;
        let raw = u64::from_le_bytes(digest[start..start + 8].try_into().unwrap_or([0; 8]));
        raw as usize & (bits - 1)
    })
}

fn raw_terms(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let normalized = normalized_text(text);
    normalized
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|term| !term.is_empty())
        .filter_map(|term| {
            let term = term.to_string();
            seen.insert(term.clone()).then_some(term)
        })
        .collect()
}

fn word_unit(term: &str) -> String {
    format!("w\u{1f}{term}")
}

fn signal_units(text: &str) -> BTreeSet<String> {
    let normalized = normalized_text(text);
    let mut units = raw_terms(&normalized)
        .into_iter()
        .map(|term| word_unit(&term))
        .collect::<BTreeSet<_>>();

    let mut parts = Vec::new();
    let mut previous_separator = true;
    for grapheme in normalized.graphemes(true) {
        if grapheme
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-'))
        {
            parts.push(grapheme.to_owned());
            previous_separator = false;
        } else if !previous_separator {
            parts.push(" ".to_owned());
            previous_separator = true;
        }
    }
    if parts.last().is_some_and(|value| value == " ") {
        parts.pop();
    }
    for window in parts.windows(3) {
        if window.iter().all(|part| part == " ") {
            continue;
        }
        let gram = window.concat();
        units.insert(format!("c3\u{1f}{gram}"));
    }
    units
}

fn normalized_text(text: &str) -> String {
    text.nfc()
        .flat_map(char::to_lowercase)
        .take(MAX_DOCUMENT_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    #[test]
    fn repeated_source_document_is_not_counted_twice() {
        let mut profile = InformationProfile::default();
        assert!(profile.observe_document("s1:0", "the parser", &KEY));
        assert!(!profile.observe_document("s1:0", "changed text", &KEY));
        assert_eq!(profile.documents(), 1);
    }

    #[test]
    fn frequency_not_a_language_list_controls_weight() {
        let mut profile = InformationProfile::default();
        for (index, text) in [
            "the target0",
            "the target1",
            "the target2",
            "и target3",
            "и target4",
            "и target5",
            "және target6",
            "және target7",
            "және target8",
            "的 target9",
            "的 target10",
            "的 target11",
        ]
        .into_iter()
        .enumerate()
        {
            profile.observe_document(&format!("d{index}"), text, &KEY);
        }
        for common in ["the", "и", "және", "的"] {
            assert!(
                profile.unit_weight(&word_unit("target0"), &KEY)
                    > profile.unit_weight(&word_unit(common), &KEY),
                "{common} should be down-weighted only because it is frequent"
            );
        }
    }

    #[test]
    fn rare_drop_costs_more_than_frequent_drop() {
        let mut profile = InformationProfile::default();
        for index in 0..20 {
            profile.observe_document(&format!("common-{index}"), "please continue", &KEY);
        }
        profile.observe_document("specific", "please deploy pm2", &KEY);
        let drop_common = profile
            .weighted_recall("please deploy pm2", "deploy pm2", &KEY)
            .unwrap();
        let drop_rare = profile
            .weighted_recall("please deploy pm2", "please deploy", &KEY)
            .unwrap();
        assert!(drop_common > drop_rare, "{drop_common} <= {drop_rare}");
    }

    #[test]
    fn cold_start_keeps_every_signal_in_play() {
        let profile = InformationProfile::default();
        assert_eq!(profile.unit_weight(&word_unit("the"), &KEY), 1.0);
        assert_eq!(profile.unit_weight(&word_unit("парсер"), &KEY), 1.0);
        assert_eq!(profile.unit_weight(&word_unit("解析器"), &KEY), 1.0);
    }

    #[test]
    fn compact_profile_round_trips_and_rejects_corruption() {
        let mut profile = InformationProfile::default();
        profile.observe_document("s:1", "проверить parser.rs end to end", &KEY);
        let encoded = profile.encode();
        assert!(profile.memory_bytes() < 400 * 1024);
        assert_eq!(InformationProfile::decode(&encoded), Some(profile));
        let mut corrupt = encoded;
        corrupt[100] ^= 1;
        assert!(InformationProfile::decode(&corrupt).is_none());
    }

    #[test]
    fn canonical_unicode_forms_share_signal_units() {
        let profile = InformationProfile::default();
        let composed = "Проверить café";
        let decomposed = "Проверить cafe\u{301}";
        assert_eq!(signal_units(composed), signal_units(decomposed));
        assert_eq!(
            profile.weighted_recall(composed, decomposed, &KEY),
            Some(1.0)
        );
    }
}
