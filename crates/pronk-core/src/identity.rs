//! Deterministic manufacturer-name to EDID PNP-ID resolution.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use pronk_userns::is_host_root_owner;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

pub const SYSTEM_PNP_IDS_PATH: &str = "/usr/share/hwdata/pnp.ids";
pub const DEFAULT_SYNTHESIZER_PNP_ID: PnpId = PnpId(*b"CKM");

const MAX_DATABASE_BYTES: u64 = 1024 * 1024;
const MAX_DATABASE_ENTRIES: usize = 8192;
const MAX_VENDOR_NAME_BYTES: usize = 256;

/// A syntactically valid three-letter EDID manufacturer identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PnpId([u8; 3]);

impl PnpId {
    pub fn parse(value: &str) -> Result<Self, PnpIdError> {
        let bytes: [u8; 3] = value
            .as_bytes()
            .try_into()
            .map_err(|_| PnpIdError::InvalidPnpId(value.into()))?;
        if !bytes.iter().all(u8::is_ascii_uppercase) {
            return Err(PnpIdError::InvalidPnpId(value.into()));
        }
        Ok(Self(bytes))
    }

    pub fn as_str(&self) -> &str {
        // Construction permits only ASCII uppercase bytes.
        std::str::from_utf8(&self.0).expect("validated PNP IDs are UTF-8")
    }

    pub fn bytes(self) -> [u8; 3] {
        self.0
    }
}

impl std::fmt::Display for PnpId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PnpResolutionSource {
    AuthenticatedPnpId,
    ExactName,
    LegalSuffixName,
    ReviewedAlias,
    SynthesizerFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPnpId {
    pub pnp_id: PnpId,
    pub database_name: Option<String>,
    pub source: PnpResolutionSource,
    pub candidate_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PnpAlias {
    pub manufacturer_name: String,
    pub pnp_id: PnpId,
}

/// An immutable resolver built from one trusted `pnp.ids` snapshot.
#[derive(Debug, Clone)]
pub struct PnpIdResolver {
    names_by_id: BTreeMap<PnpId, String>,
    exact_ids_by_name: BTreeMap<String, BTreeSet<PnpId>>,
    legal_ids_by_name: BTreeMap<String, BTreeSet<PnpId>>,
    aliases: BTreeMap<String, PnpId>,
    fallback: PnpId,
}

impl PnpIdResolver {
    /// Load a root-owned database that is not writable by group or others.
    pub fn load_system(
        path: impl AsRef<Path>,
        aliases: &[PnpAlias],
        fallback: PnpId,
    ) -> Result<Self, PnpIdError> {
        let path = path.as_ref();
        let mut file = File::open(path).map_err(|source| PnpIdError::Open {
            path: path.to_owned(),
            source,
        })?;
        let metadata = file.metadata().map_err(|source| PnpIdError::Inspect {
            path: path.to_owned(),
            source,
        })?;
        if !metadata.is_file()
            || !is_host_root_owner(metadata.uid())
            || metadata.mode() & 0o022 != 0
        {
            return Err(PnpIdError::UntrustedFile {
                path: path.to_owned(),
                uid: metadata.uid(),
                mode: metadata.mode() & 0o7777,
            });
        }
        if metadata.len() > MAX_DATABASE_BYTES {
            return Err(PnpIdError::DatabaseTooLarge(metadata.len()));
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.by_ref()
            .take(MAX_DATABASE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| PnpIdError::Read {
                path: path.to_owned(),
                source,
            })?;
        if bytes.len() as u64 > MAX_DATABASE_BYTES {
            return Err(PnpIdError::DatabaseTooLarge(bytes.len() as u64));
        }
        let text = std::str::from_utf8(&bytes).map_err(PnpIdError::Encoding)?;
        Self::from_database(text, aliases, fallback)
    }

    /// Parse a bounded database already obtained from a trusted source.
    pub fn from_database(
        database: &str,
        aliases: &[PnpAlias],
        fallback: PnpId,
    ) -> Result<Self, PnpIdError> {
        if database.len() as u64 > MAX_DATABASE_BYTES {
            return Err(PnpIdError::DatabaseTooLarge(database.len() as u64));
        }

        let mut names_by_id: BTreeMap<PnpId, String> = BTreeMap::new();
        for (line_index, raw_line) in database.lines().enumerate() {
            let line_number = line_index + 1;
            let line = raw_line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (raw_id, raw_name) = line
                .split_once(|character: char| character.is_ascii_whitespace())
                .ok_or(PnpIdError::MalformedLine(line_number))?;
            let name = raw_name.trim();
            // hwdata retains a handful of historical non-PNP keys (for
            // example lowercase or punctuation-containing identifiers). They
            // cannot be encoded in EDID and are deliberately excluded from
            // this resolver rather than poisoning the otherwise trusted file.
            let Ok(pnp_id) = PnpId::parse(raw_id) else {
                continue;
            };
            if name.is_empty()
                || name.len() > MAX_VENDOR_NAME_BYTES
                || name.chars().any(char::is_control)
            {
                return Err(PnpIdError::InvalidDatabaseName(line_number));
            }
            if names_by_id.insert(pnp_id, name.to_owned()).is_some() {
                return Err(PnpIdError::DuplicateDatabaseId(pnp_id));
            }
            if names_by_id.len() > MAX_DATABASE_ENTRIES {
                return Err(PnpIdError::TooManyEntries(names_by_id.len()));
            }
        }
        if names_by_id.is_empty() {
            return Err(PnpIdError::EmptyDatabase);
        }

        let mut exact_ids_by_name: BTreeMap<String, BTreeSet<PnpId>> = BTreeMap::new();
        let mut legal_ids_by_name: BTreeMap<String, BTreeSet<PnpId>> = BTreeMap::new();
        for (pnp_id, name) in &names_by_id {
            let exact = normalize_manufacturer_name(name);
            if exact.is_empty() {
                return Err(PnpIdError::InvalidNormalizedDatabaseName(*pnp_id));
            }
            exact_ids_by_name
                .entry(exact.clone())
                .or_default()
                .insert(*pnp_id);
            legal_ids_by_name
                .entry(strip_legal_suffixes(&exact))
                .or_default()
                .insert(*pnp_id);
        }

        let mut reviewed_aliases = BTreeMap::new();
        for alias in aliases {
            if !names_by_id.contains_key(&alias.pnp_id) {
                return Err(PnpIdError::UnknownAliasTarget(alias.pnp_id));
            }
            let key = strip_legal_suffixes(&normalize_manufacturer_name(&alias.manufacturer_name));
            if key.is_empty() {
                return Err(PnpIdError::InvalidAlias(alias.manufacturer_name.clone()));
            }
            if reviewed_aliases.insert(key.clone(), alias.pnp_id).is_some() {
                return Err(PnpIdError::DuplicateAlias(key));
            }
        }

        Ok(Self {
            names_by_id,
            exact_ids_by_name,
            legal_ids_by_name,
            aliases: reviewed_aliases,
            fallback,
        })
    }

    pub fn resolve(
        &self,
        authenticated_pnp_id: Option<&str>,
        manufacturer_name: Option<&str>,
    ) -> Result<ResolvedPnpId, PnpIdError> {
        if let Some(value) = authenticated_pnp_id {
            let pnp_id = PnpId::parse(value)?;
            return Ok(self.resolution(pnp_id, PnpResolutionSource::AuthenticatedPnpId, 1));
        }

        let Some(manufacturer_name) = manufacturer_name else {
            return Ok(self.fallback());
        };
        let exact = normalize_manufacturer_name(manufacturer_name);
        if exact.is_empty() {
            return Ok(self.fallback());
        }
        if let Some(candidates) = self.exact_ids_by_name.get(&exact) {
            return Ok(self.resolution_from_candidates(candidates, PnpResolutionSource::ExactName));
        }

        let legal = strip_legal_suffixes(&exact);
        if let Some(candidates) = self.legal_ids_by_name.get(&legal) {
            return Ok(
                self.resolution_from_candidates(candidates, PnpResolutionSource::LegalSuffixName)
            );
        }
        if let Some(pnp_id) = self.aliases.get(&legal) {
            return Ok(self.resolution(*pnp_id, PnpResolutionSource::ReviewedAlias, 1));
        }
        Ok(self.fallback())
    }

    pub fn database_name(&self, pnp_id: PnpId) -> Option<&str> {
        self.names_by_id.get(&pnp_id).map(String::as_str)
    }

    fn resolution_from_candidates(
        &self,
        candidates: &BTreeSet<PnpId>,
        source: PnpResolutionSource,
    ) -> ResolvedPnpId {
        let pnp_id = *candidates
            .first()
            .expect("name indexes contain at least one PNP ID");
        self.resolution(pnp_id, source, candidates.len())
    }

    fn resolution(
        &self,
        pnp_id: PnpId,
        source: PnpResolutionSource,
        candidate_count: usize,
    ) -> ResolvedPnpId {
        ResolvedPnpId {
            pnp_id,
            database_name: self.names_by_id.get(&pnp_id).cloned(),
            source,
            candidate_count,
        }
    }

    fn fallback(&self) -> ResolvedPnpId {
        self.resolution(self.fallback, PnpResolutionSource::SynthesizerFallback, 0)
    }
}

/// Normalize only the harmless variations accepted by the resolution policy.
pub fn normalize_manufacturer_name(value: &str) -> String {
    let mut normalized = String::new();
    let mut pending_space = false;
    for character in value.nfkc().flat_map(char::to_lowercase) {
        match character {
            '.' => {}
            ',' => pending_space = !normalized.is_empty(),
            character if character.is_whitespace() => pending_space = !normalized.is_empty(),
            character => {
                if pending_space {
                    normalized.push(' ');
                    pending_space = false;
                }
                normalized.push(character);
            }
        }
    }
    normalized
}

/// Strip the version-1 terminal legal-suffix set, repeatedly.
pub fn strip_legal_suffixes(normalized_name: &str) -> String {
    const LEGAL_SUFFIXES_V1: &[&str] = &[
        "corporation",
        "corp",
        "incorporated",
        "inc",
        "company",
        "co",
        "limited",
        "ltd",
        "llc",
        "plc",
        "gmbh",
        "ag",
        "sa",
        "bv",
    ];

    let mut words: Vec<_> = normalized_name.split_whitespace().collect();
    while words.len() > 1
        && words
            .last()
            .is_some_and(|word| LEGAL_SUFFIXES_V1.contains(word))
    {
        words.pop();
    }
    words.join(" ")
}

#[derive(Debug, Error)]
pub enum PnpIdError {
    #[error("invalid PNP ID {0:?}; expected exactly three ASCII uppercase letters")]
    InvalidPnpId(String),
    #[error("open PNP database {}: {source}", path.display())]
    Open {
        path: std::path::PathBuf,
        source: io::Error,
    },
    #[error("inspect PNP database {}: {source}", path.display())]
    Inspect {
        path: std::path::PathBuf,
        source: io::Error,
    },
    #[error("read PNP database {}: {source}", path.display())]
    Read {
        path: std::path::PathBuf,
        source: io::Error,
    },
    #[error("PNP database {} is not a root-owned regular file without group/other writes (uid={uid}, mode={mode:#o})", path.display())]
    UntrustedFile {
        path: std::path::PathBuf,
        uid: u32,
        mode: u32,
    },
    #[error("PNP database is {0} bytes; limit is {MAX_DATABASE_BYTES}")]
    DatabaseTooLarge(u64),
    #[error("PNP database is not UTF-8: {0}")]
    Encoding(#[from] std::str::Utf8Error),
    #[error("PNP database line {0} has no identifier/name separator")]
    MalformedLine(usize),
    #[error("PNP database line {0} has an empty, overlong, or control-containing name")]
    InvalidDatabaseName(usize),
    #[error("PNP database contains duplicate identifier {0}")]
    DuplicateDatabaseId(PnpId),
    #[error("PNP database contains {0} entries; limit is {MAX_DATABASE_ENTRIES}")]
    TooManyEntries(usize),
    #[error("PNP database is empty")]
    EmptyDatabase,
    #[error("PNP database name for {0} is empty after normalization")]
    InvalidNormalizedDatabaseName(PnpId),
    #[error("reviewed PNP alias targets unknown identifier {0}")]
    UnknownAliasTarget(PnpId),
    #[error("reviewed PNP alias {0:?} is empty after normalization")]
    InvalidAlias(String),
    #[error("reviewed PNP alias key {0:?} is duplicated")]
    DuplicateAlias(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
GGL\tGoogle Inc.\n\
SON\tSony\n\
SNY\tSony\n\
TCL\tTechnical Concepts Ltd\n\
TOL\tTCL Corporation\n";

    fn resolver() -> PnpIdResolver {
        PnpIdResolver::from_database(
            FIXTURE,
            &[PnpAlias {
                manufacturer_name: "Alphabet Devices".into(),
                pnp_id: PnpId::parse("GGL").unwrap(),
            }],
            DEFAULT_SYNTHESIZER_PNP_ID,
        )
        .unwrap()
    }

    #[test]
    fn normalizes_unicode_spacing_and_harmless_punctuation() {
        assert_eq!(
            normalize_manufacturer_name("  Ｇｏｏｇｌｅ,  Inc.  "),
            "google inc"
        );
        assert_eq!(strip_legal_suffixes("example company limited"), "example");
        assert_eq!(strip_legal_suffixes("ag"), "ag");
    }

    #[test]
    fn authenticated_id_wins_without_reinterpreting_three_letter_names() {
        let resolver = resolver();
        let direct = resolver.resolve(Some("SNY"), Some("unrelated")).unwrap();
        assert_eq!(direct.pnp_id.as_str(), "SNY");
        assert_eq!(direct.source, PnpResolutionSource::AuthenticatedPnpId);

        let unresolved = resolver.resolve(None, Some("GGL")).unwrap();
        assert_eq!(unresolved.pnp_id, DEFAULT_SYNTHESIZER_PNP_ID);
        assert_eq!(unresolved.source, PnpResolutionSource::SynthesizerFallback);
        assert!(resolver.resolve(Some("ggl"), None).is_err());
    }

    #[test]
    fn exact_and_legal_matches_are_deterministic() {
        let resolver = resolver();
        let sony = resolver.resolve(None, Some("Sony")).unwrap();
        assert_eq!(sony.pnp_id.as_str(), "SNY");
        assert_eq!(sony.candidate_count, 2);
        assert_eq!(sony.source, PnpResolutionSource::ExactName);

        let google = resolver.resolve(None, Some("Google")).unwrap();
        assert_eq!(google.pnp_id.as_str(), "GGL");
        assert_eq!(google.source, PnpResolutionSource::LegalSuffixName);

        let tcl = resolver.resolve(None, Some("TCL")).unwrap();
        assert_eq!(tcl.pnp_id.as_str(), "TOL");
        assert_eq!(tcl.database_name.as_deref(), Some("TCL Corporation"));
    }

    #[test]
    fn aliases_run_only_after_database_matching() {
        let resolver = resolver();
        let alias = resolver
            .resolve(None, Some("Alphabet Devices, LLC"))
            .unwrap();
        assert_eq!(alias.pnp_id.as_str(), "GGL");
        assert_eq!(alias.source, PnpResolutionSource::ReviewedAlias);
    }

    #[test]
    fn validates_database_and_aliases() {
        assert!(matches!(
            PnpIdResolver::from_database("Bad LINE\n", &[], DEFAULT_SYNTHESIZER_PNP_ID),
            Err(PnpIdError::EmptyDatabase)
        ));
        assert!(matches!(
            PnpIdResolver::from_database(
                "AAA\tFirst\nAAA\tSecond\n",
                &[],
                DEFAULT_SYNTHESIZER_PNP_ID,
            ),
            Err(PnpIdError::DuplicateDatabaseId(_))
        ));
        assert!(matches!(
            PnpIdResolver::from_database(
                FIXTURE,
                &[PnpAlias {
                    manufacturer_name: "Unknown".into(),
                    pnp_id: PnpId::parse("ZZZ").unwrap(),
                }],
                DEFAULT_SYNTHESIZER_PNP_ID
            ),
            Err(PnpIdError::UnknownAliasTarget(_))
        ));
    }

    #[test]
    fn loads_the_packaged_root_owned_hwdata_database() {
        let resolver =
            PnpIdResolver::load_system(SYSTEM_PNP_IDS_PATH, &[], DEFAULT_SYNTHESIZER_PNP_ID)
                .unwrap();
        let sony = resolver.resolve(None, Some("Sony")).unwrap();
        assert_eq!(sony.database_name.as_deref(), Some("Sony"));
        assert_eq!(sony.source, PnpResolutionSource::ExactName);
        assert!(sony.candidate_count >= 1);

        let google = resolver.resolve(None, Some("Google")).unwrap();
        assert_eq!(google.database_name.as_deref(), Some("Google Inc."));
        assert_eq!(google.source, PnpResolutionSource::LegalSuffixName);
    }
}
