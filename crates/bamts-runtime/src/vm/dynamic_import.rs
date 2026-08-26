#![cfg(test)]
//! Dynamic-import classification and promise-capability settlement policy.
//!
//! This leaf describes decisions only. Module storage, provider invocation,
//! compilation, evaluation, and Promise mutation remain owned by `Machine`.

use crate::ImportTarget;
use bamts_bytecode::{EcmaString, ModuleId};

pub(crate) const RESOLVE_TYPE_ERROR: &str = "resolve dynamic module specifier";

/// One canonical import attribute retained for the provider call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ForwardedAttribute {
    key: String,
    value: String,
}

impl ForwardedAttribute {
    pub(crate) fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    pub(crate) fn as_pair(&self) -> (&str, &str) {
        (&self.key, &self.value)
    }
}

/// Owned arguments which survive from option evaluation until host resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostResolveRequest {
    pub(crate) specifier: EcmaString,
    pub(crate) attributes: Vec<ForwardedAttribute>,
    pub(crate) referrer: EcmaString,
}

/// Existing targets visible without consulting a host provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DynamicImportLookup {
    /// A declared edge carrying both static and dynamic kinds.
    pub(crate) static_coalesced: Option<ImportTarget>,
    /// A bundled local found by declared-edge or relative-name resolution.
    pub(crate) local: Option<ModuleId>,
    /// Canonical specifier already present in the external registry.
    pub(crate) external: Option<EcmaString>,
    /// Whether this machine was constructed with a module provider.
    pub(crate) provider_available: bool,
}

/// The first action selected after specifier and option evaluation completes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DynamicImportPlan {
    StaticCoalesced(ImportTarget),
    Local(ModuleId),
    External(EcmaString),
    HostResolve(HostResolveRequest),
    RejectTypeError(&'static str),
}

/// Classifies a dynamic import without invoking the provider or mutating a
/// module registry. Existing targets always precede host resolution.
pub(crate) fn plan_dynamic_import(
    request: HostResolveRequest,
    lookup: DynamicImportLookup,
) -> DynamicImportPlan {
    if let Some(target) = lookup.static_coalesced {
        return DynamicImportPlan::StaticCoalesced(target);
    }
    if let Some(module) = lookup.local {
        return DynamicImportPlan::Local(module);
    }
    if let Some(specifier) = lookup.external {
        return DynamicImportPlan::External(specifier);
    }
    if lookup.provider_available {
        return DynamicImportPlan::HostResolve(request);
    }
    DynamicImportPlan::RejectTypeError(RESOLVE_TYPE_ERROR)
}

#[cfg(test)]
mod tests {
    use bamts_bytecode::EdgeId;

    use super::*;

    fn text(value: &str) -> EcmaString {
        EcmaString::encode(value)
    }

    fn request() -> HostResolveRequest {
        HostResolveRequest {
            specifier: text("./data.json"),
            attributes: vec![ForwardedAttribute::new("type", "json")],
            referrer: text("/app/main.js"),
        }
    }

    fn lookup() -> DynamicImportLookup {
        DynamicImportLookup {
            static_coalesced: None,
            local: None,
            external: None,
            provider_available: false,
        }
    }

    #[test]
    fn classification_precedence_covers_static_local_external_and_host() {
        let local = ModuleId::new(3);
        let static_external = ImportTarget::External(EdgeId::new(4));
        let cases = [
            (
                DynamicImportLookup {
                    static_coalesced: Some(static_external),
                    local: Some(local),
                    external: Some(text("./data.json")),
                    provider_available: true,
                },
                DynamicImportPlan::StaticCoalesced(static_external),
            ),
            (
                DynamicImportLookup {
                    local: Some(local),
                    external: Some(text("./data.json")),
                    provider_available: true,
                    ..lookup()
                },
                DynamicImportPlan::Local(local),
            ),
            (
                DynamicImportLookup {
                    external: Some(text("./data.json")),
                    provider_available: true,
                    ..lookup()
                },
                DynamicImportPlan::External(text("./data.json")),
            ),
            (
                DynamicImportLookup {
                    provider_available: true,
                    ..lookup()
                },
                DynamicImportPlan::HostResolve(request()),
            ),
        ];

        for (lookup, expected) in cases {
            assert_eq!(plan_dynamic_import(request(), lookup), expected);
        }
    }

    #[test]
    fn no_provider_is_an_asynchronous_type_error_plan() {
        assert_eq!(
            plan_dynamic_import(request(), lookup()),
            DynamicImportPlan::RejectTypeError(RESOLVE_TYPE_ERROR)
        );
    }

    #[test]
    fn provider_attributes_are_forwarded_exactly() {
        let request = HostResolveRequest {
            specifier: text("pkg"),
            attributes: vec![
                ForwardedAttribute::new("integrity", "sha256-a"),
                ForwardedAttribute::new("type", "json"),
            ],
            referrer: text("entry.js"),
        };
        let DynamicImportPlan::HostResolve(forwarded) = plan_dynamic_import(
            request.clone(),
            DynamicImportLookup {
                provider_available: true,
                ..lookup()
            },
        ) else {
            panic!("provider path must retain its request");
        };
        assert_eq!(forwarded, request);
        assert_eq!(
            forwarded
                .attributes
                .iter()
                .map(ForwardedAttribute::as_pair)
                .collect::<Vec<_>>(),
            vec![("integrity", "sha256-a"), ("type", "json")]
        );
    }
}
