//! Validated shard planning over canonical obligation keys.
//!
//! Membership is exact sorted-index striding: the obligation at sorted index
//! `i` belongs to shard `k` of `n` if and only if `i % n == k`.  Empty catalogs
//! and empty shard matrices are rejected, as is a shard count larger than the
//! catalog.  Valid partitions are pairwise disjoint and their union is the
//! whole catalog.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ErrorCode, Result, VerificationError};

/// Closed execution modes that an obligation may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Interpreter,
    Jit,
    Aot,
}

impl ExecutionMode {
    /// Every closed execution mode, in canonical order.
    pub const ALL: [Self; 3] = [Self::Interpreter, Self::Jit, Self::Aot];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interpreter => "interpreter",
            Self::Jit => "jit",
            Self::Aot => "aot",
        }
    }
}

/// Canonical identity of one logical verification obligation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObligationKey {
    catalog: String,
    case: String,
    configuration: String,
    mode: ExecutionMode,
    platform: String,
}

impl ObligationKey {
    /// Builds a validated obligation key with nonempty, NUL-free fields.
    pub fn new(
        catalog: impl Into<String>,
        case: impl Into<String>,
        configuration: impl Into<String>,
        mode: ExecutionMode,
        platform: impl Into<String>,
    ) -> Result<Self> {
        let key = Self {
            catalog: catalog.into(),
            case: case.into(),
            configuration: configuration.into(),
            mode,
            platform: platform.into(),
        };
        key.validate()?;
        Ok(key)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        require_token("catalog", &self.catalog)?;
        require_token("case", &self.case)?;
        require_token("configuration", &self.configuration)?;
        require_token("platform", &self.platform)?;
        Ok(())
    }

    #[must_use]
    pub fn catalog(&self) -> &str {
        &self.catalog
    }

    #[must_use]
    pub fn case(&self) -> &str {
        &self.case
    }

    #[must_use]
    pub fn configuration(&self) -> &str {
        &self.configuration
    }

    #[must_use]
    pub fn mode(&self) -> ExecutionMode {
        self.mode
    }

    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// Stable encoding used for set digests.  Field separators cannot appear in
    /// validated tokens, so the encoding is unambiguous.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(
            self.catalog.len()
                + self.case.len()
                + self.configuration.len()
                + self.platform.len()
                + 16,
        );
        bytes.extend_from_slice(self.catalog.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(self.case.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(self.configuration.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(self.mode.as_str().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(self.platform.as_bytes());
        bytes
    }
}

impl fmt::Display for ObligationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}/{}#{}/{}",
            self.catalog,
            self.case,
            self.configuration,
            self.mode.as_str(),
            self.platform
        )
    }
}

/// Validated shard coordinates: index `k` of count `n`, with `n >= 1` and `k < n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardSpec {
    index: u32,
    count: u32,
}

impl ShardSpec {
    /// Accepts a single shard of a nonempty matrix.
    pub fn new(index: u32, count: u32) -> Result<Self> {
        if count == 0 {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                "shard matrix must not be empty",
            ));
        }
        if index >= count {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!("shard index {index} is outside matrix of {count}"),
            ));
        }
        Ok(Self { index, count })
    }

    /// The unsharded matrix: a single shard that owns every obligation.
    pub fn unsharded() -> Self {
        Self { index: 0, count: 1 }
    }

    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    #[must_use]
    pub const fn count(self) -> u32 {
        self.count
    }

    #[must_use]
    pub fn owns(self, sorted_index: usize) -> bool {
        sorted_index % self.count as usize == self.index as usize
    }

    /// Sorted catalog indices owned by this shard.
    pub fn member_indices(self, catalog_len: usize) -> impl Iterator<Item = usize> {
        (self.index as usize..catalog_len).step_by(self.count as usize)
    }

    #[must_use]
    pub fn expected_count(self, catalog_len: usize) -> usize {
        if (self.index as usize) >= catalog_len {
            0
        } else {
            (catalog_len - self.index as usize - 1) / self.count as usize + 1
        }
    }
}

/// Digest-bound identity of one planned shard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardIdentity {
    spec: ShardSpec,
    catalog_digest: String,
    catalog_len: usize,
    expected_count: usize,
    obligation_set_digest: String,
}

impl ShardIdentity {
    /// Plans exact strided membership of `keys` for `spec`.
    ///
    /// `keys` must already be the canonical, strictly increasing catalog.  The
    /// identity stores only digests and counts; it does not retain the catalog.
    pub fn plan(spec: ShardSpec, keys: &[ObligationKey]) -> Result<Self> {
        validate_catalog(keys)?;
        if spec.count as usize > keys.len() {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "shard count {} exceeds catalog length {}",
                    spec.count,
                    keys.len()
                ),
            ));
        }
        let catalog_digest = digest_obligation_set(keys.iter());
        let expected_count = spec.expected_count(keys.len());
        if expected_count == 0 {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "shard {}/{} would be empty for catalog length {}",
                    spec.index,
                    spec.count,
                    keys.len()
                ),
            ));
        }
        let obligation_set_digest =
            digest_obligation_set(spec.member_indices(keys.len()).map(|index| &keys[index]));
        Ok(Self {
            spec,
            catalog_digest,
            catalog_len: keys.len(),
            expected_count,
            obligation_set_digest,
        })
    }

    #[must_use]
    pub fn spec(&self) -> ShardSpec {
        self.spec
    }

    #[must_use]
    pub fn catalog_digest(&self) -> &str {
        &self.catalog_digest
    }

    #[must_use]
    pub fn catalog_len(&self) -> usize {
        self.catalog_len
    }

    #[must_use]
    pub fn expected_count(&self) -> usize {
        self.expected_count
    }

    #[must_use]
    pub fn obligation_set_digest(&self) -> &str {
        &self.obligation_set_digest
    }

    /// Reconstructs an identity from already-validated parts (unsharded merge).
    pub fn from_parts(
        spec: ShardSpec,
        catalog_digest: String,
        catalog_len: usize,
        expected_count: usize,
        obligation_set_digest: String,
    ) -> Result<Self> {
        ShardSpec::new(spec.index(), spec.count())?;
        require_sha256("catalog_digest", &catalog_digest)?;
        require_sha256("obligation_set_digest", &obligation_set_digest)?;
        if catalog_len == 0 || expected_count == 0 {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                "shard identity cannot describe an empty catalog or shard",
            ));
        }
        if spec.count() as usize > catalog_len {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "shard count {} exceeds catalog length {catalog_len}",
                    spec.count()
                ),
            ));
        }
        if spec.expected_count(catalog_len) != expected_count {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!(
                    "shard {}/{} expected count {expected_count} does not match catalog formula {}",
                    spec.index(),
                    spec.count(),
                    spec.expected_count(catalog_len)
                ),
            ));
        }
        Ok(Self {
            spec,
            catalog_digest,
            catalog_len,
            expected_count,
            obligation_set_digest,
        })
    }
}

/// SHA-256 (lowercase hex) of canonical obligation encodings in iteration order.
pub fn digest_obligation_set<'a>(keys: impl IntoIterator<Item = &'a ObligationKey>) -> String {
    let mut hasher = Sha256::new();
    for key in keys {
        hasher.update(key.canonical_bytes());
        hasher.update([0x0a]);
    }
    hex_digest(hasher)
}

/// Validates a canonical catalog: nonempty, strictly increasing, each key valid.
pub fn validate_catalog(keys: &[ObligationKey]) -> Result<()> {
    if keys.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "catalog must not be empty",
        ));
    }
    let mut previous: Option<&ObligationKey> = None;
    for key in keys {
        key.validate()?;
        if let Some(prior) = previous
            && key <= prior
        {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!("catalog keys are not strictly increasing around `{key}`"),
            ));
        }
        previous = Some(key);
    }
    Ok(())
}

pub(crate) fn hex_digest(hasher: Sha256) -> String {
    format!("{:x}", hasher.finalize())
}

pub(crate) fn require_token(field: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!("{field} must be nonempty"),
        ));
    }
    if value.as_bytes().contains(&0) {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!("{field} must not contain NUL"),
        ));
    }
    Ok(())
}

pub(crate) fn require_sha256(field: &str, value: &str) -> Result<()> {
    let valid = value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(VerificationError::new(
            ErrorCode::Digest,
            format!("{field} is not a lowercase SHA-256 digest"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn key(index: usize) -> ObligationKey {
        ObligationKey::new(
            "typescript-7.0.2",
            format!("case-{index:04}"),
            "default",
            ExecutionMode::Interpreter,
            "x86_64-unknown-linux-gnu",
        )
        .expect("test key")
    }

    fn catalog(len: usize) -> Vec<ObligationKey> {
        (0..len).map(key).collect()
    }

    #[test]
    fn strided_partition_is_exact() {
        for catalog_len in 1..=24 {
            let keys = catalog(catalog_len);
            validate_catalog(&keys).expect("canonical catalog");
            let catalog_digest = digest_obligation_set(keys.iter());
            for count in 1..=catalog_len {
                let mut union = BTreeSet::new();
                let mut recovered = Vec::new();
                for index in 0..count {
                    let spec = ShardSpec::new(index as u32, count as u32).expect("spec");
                    let identity = ShardIdentity::plan(spec, &keys).expect("plan");
                    assert_eq!(identity.catalog_len(), catalog_len);
                    assert_eq!(identity.catalog_digest(), catalog_digest);
                    assert_eq!(identity.spec(), spec);
                    let members: Vec<usize> = spec.member_indices(catalog_len).collect();
                    assert_eq!(members.len(), spec.expected_count(catalog_len));
                    assert_eq!(members.len(), identity.expected_count());
                    assert!(!members.is_empty());
                    let member_keys: Vec<&ObligationKey> =
                        members.iter().map(|&member| &keys[member]).collect();
                    assert_eq!(
                        identity.obligation_set_digest(),
                        digest_obligation_set(member_keys.iter().copied())
                    );
                    for member in members {
                        assert_eq!(member % count, index);
                        assert!(spec.owns(member));
                        assert!(
                            union.insert(member),
                            "shard {index}/{count} reused index {member}"
                        );
                        recovered.push(keys[member].clone());
                    }
                }
                assert_eq!(union.len(), catalog_len, "union of {count} shards");
                assert!(
                    (0..catalog_len).all(|index| union.contains(&index)),
                    "missing catalog index in {count}-way split"
                );
                recovered.sort();
                assert_eq!(recovered, keys);
            }
            assert_eq!(
                ShardSpec::new(0, (catalog_len as u32) + 1)
                    .ok()
                    .and_then(|spec| ShardIdentity::plan(spec, &keys).ok()),
                None
            );
        }

        assert_eq!(ShardSpec::new(0, 0).unwrap_err().code(), ErrorCode::Schema);
        assert_eq!(ShardSpec::new(1, 1).unwrap_err().code(), ErrorCode::Schema);
        assert_eq!(validate_catalog(&[]).unwrap_err().code(), ErrorCode::Schema);
        let keys = catalog(3);
        let spec = ShardSpec::new(0, 4).expect("index in range of count");
        assert_eq!(
            ShardIdentity::plan(spec, &keys).unwrap_err().code(),
            ErrorCode::Schema
        );
        assert!(ObligationKey::new("", "case", "cfg", ExecutionMode::Jit, "plat").is_err());
        assert!(ObligationKey::new("cat", "case\0x", "cfg", ExecutionMode::Aot, "plat").is_err());
    }
}
