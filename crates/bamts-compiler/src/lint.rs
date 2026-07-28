use std::{fmt, str::FromStr, sync::Arc};

/// A lint's ordered severity. A `forbid` level is an immutable lock.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LintLevel {
    Allow,
    Warn,
    Deny,
    Forbid,
}

impl LintLevel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Warn => "warn",
            Self::Deny => "deny",
            Self::Forbid => "forbid",
        }
    }
}

impl fmt::Display for LintLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LintLevel {
    type Err = ParseLintLevelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "allow" => Ok(Self::Allow),
            "warn" => Ok(Self::Warn),
            "deny" => Ok(Self::Deny),
            "forbid" => Ok(Self::Forbid),
            _ => Err(ParseLintLevelError(Arc::from(value))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseLintLevelError(Arc<str>);

impl fmt::Display for ParseLintLevelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown lint level {:?}", self.0)
    }
}

impl std::error::Error for ParseLintLevelError {}

/// The eleven stable semantic families exposed by `bamts.toml` and the CLI.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RuleGroup {
    Unsoundness,
    EscapeHatches,
    NonErasable,
    LegacySyntax,
    Modules,
    ClassSemantics,
    EnumSemantics,
    DeclarationMerging,
    JavaScriptCompatibility,
    Opinionated,
    ControlFlow,
}

impl RuleGroup {
    pub const ALL: [Self; 11] = [
        Self::Unsoundness,
        Self::EscapeHatches,
        Self::NonErasable,
        Self::LegacySyntax,
        Self::Modules,
        Self::ClassSemantics,
        Self::EnumSemantics,
        Self::DeclarationMerging,
        Self::JavaScriptCompatibility,
        Self::Opinionated,
        Self::ControlFlow,
    ];

    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Unsoundness => "unsoundness",
            Self::EscapeHatches => "escape-hatches",
            Self::NonErasable => "non-erasable",
            Self::LegacySyntax => "legacy-syntax",
            Self::Modules => "modules",
            Self::ClassSemantics => "class-semantics",
            Self::EnumSemantics => "enum-semantics",
            Self::DeclarationMerging => "declaration-merging",
            Self::JavaScriptCompatibility => "javascript-compatibility",
            Self::Opinionated => "opinionated",
            Self::ControlFlow => "control-flow",
        }
    }
}

impl FromStr for RuleGroup {
    type Err = ParseRuleGroupError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|group| group.slug() == value)
            .ok_or_else(|| ParseRuleGroupError(Arc::from(value)))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseRuleGroupError(Arc<str>);

impl fmt::Display for ParseRuleGroupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown lint group {:?}", self.0)
    }
}

impl std::error::Error for ParseRuleGroupError {}

/// The inseparable stable and human-readable identity of one registered rule.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RuleId {
    code: &'static str,
    slug: &'static str,
}

impl RuleId {
    #[must_use]
    pub const fn code(self) -> &'static str {
        self.code
    }

    #[must_use]
    pub const fn slug(self) -> &'static str {
        self.slug
    }
}

/// The complete metadata required to register a lint rule.
///
/// Fields and construction are private: a rule identity can only come from [`RULES`],
/// so a code or slug cannot exist without its group and default level.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RuleDefinition {
    id: RuleId,
    group: RuleGroup,
    default_level: LintLevel,
}

impl RuleDefinition {
    const fn new(
        code: &'static str,
        slug: &'static str,
        group: RuleGroup,
        default_level: LintLevel,
    ) -> Self {
        Self {
            id: RuleId { code, slug },
            group,
            default_level,
        }
    }

    #[must_use]
    pub const fn id(&self) -> RuleId {
        self.id
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.id.code()
    }

    #[must_use]
    pub const fn slug(&self) -> &'static str {
        self.id.slug()
    }

    #[must_use]
    pub const fn group(&self) -> RuleGroup {
        self.group
    }

    #[must_use]
    pub const fn default_level(&self) -> LintLevel {
        self.default_level
    }
}

macro_rules! rule {
    ($code:literal, $slug:literal, $group:ident, $level:ident) => {
        RuleDefinition::new($code, $slug, RuleGroup::$group, LintLevel::$level)
    };
}

/// The single authoritative identity and default-level registry for all BamTS rules.
pub static RULES: [RuleDefinition; 86] = [
    rule!(
        "BAMTS-W001",
        "method-parameter-bivariance",
        Unsoundness,
        Warn
    ),
    rule!("BAMTS-W002", "mutable-array-covariance", Unsoundness, Warn),
    rule!("BAMTS-W003", "non-fresh-excess-property", Unsoundness, Warn),
    rule!("BAMTS-W004", "delete-required-property", Unsoundness, Warn),
    rule!("BAMTS-W005", "unchecked-catch-member", Unsoundness, Warn),
    rule!("BAMTS-W006", "generic-any-downcast", EscapeHatches, Warn),
    rule!("BAMTS-W007", "dynamic-tuple-index", Unsoundness, Warn),
    rule!(
        "BAMTS-W008",
        "unchecked-index-signature-read",
        Unsoundness,
        Warn
    ),
    rule!(
        "BAMTS-W009",
        "explicit-undefined-for-optional",
        Unsoundness,
        Warn
    ),
    rule!("BAMTS-W010", "detached-this-method", Unsoundness, Warn),
    rule!("BAMTS-W011", "divergent-accessor-types", Unsoundness, Warn),
    rule!("BAMTS-W012", "readonly-alias-mutation", Unsoundness, Warn),
    rule!("BAMTS-W013", "fewer-callback-parameters", Unsoundness, Warn),
    rule!(
        "BAMTS-W014",
        "value-returning-void-callback",
        Unsoundness,
        Warn
    ),
    rule!(
        "BAMTS-W015",
        "open-object-keys-assumption",
        Unsoundness,
        Warn
    ),
    rule!(
        "BAMTS-W016",
        "index-signature-dot-access",
        Unsoundness,
        Warn
    ),
    rule!("BAMTS-W017", "explicit-any", EscapeHatches, Warn),
    rule!("BAMTS-W018", "implicit-any", EscapeHatches, Warn),
    rule!(
        "BAMTS-W019",
        "unchecked-type-assertion",
        EscapeHatches,
        Warn
    ),
    rule!("BAMTS-W020", "double-assertion", EscapeHatches, Warn),
    rule!("BAMTS-W021", "non-null-assertion", EscapeHatches, Warn),
    rule!(
        "BAMTS-W022",
        "definite-assignment-assertion",
        EscapeHatches,
        Warn
    ),
    rule!(
        "BAMTS-W023",
        "diagnostic-suppression-directive",
        EscapeHatches,
        Warn
    ),
    rule!("BAMTS-W024", "runtime-namespace", NonErasable, Warn),
    rule!("BAMTS-W025", "parameter-property", NonErasable, Warn),
    rule!(
        "BAMTS-W026",
        "legacy-decorator-semantics",
        LegacySyntax,
        Warn
    ),
    rule!("BAMTS-W027", "angle-bracket-assertion", LegacySyntax, Warn),
    rule!(
        "BAMTS-W028",
        "declaration-inference-dependency",
        LegacySyntax,
        Warn
    ),
    rule!("BAMTS-W029", "jsx-transform-required", LegacySyntax, Warn),
    rule!("BAMTS-W030", "import-export-equals", Modules, Warn),
    rule!("BAMTS-W031", "type-imported-as-value", Modules, Warn),
    rule!("BAMTS-W032", "type-reexported-as-value", Modules, Warn),
    rule!("BAMTS-W033", "commonjs-in-esm", Modules, Allow),
    rule!("BAMTS-W034", "implicit-script-file", Modules, Allow),
    rule!("BAMTS-W035", "unchecked-side-effect-import", Modules, Warn),
    rule!("BAMTS-W036", "extensionless-relative-import", Modules, Warn),
    rule!(
        "BAMTS-W037",
        "interop-dependent-default-import",
        Modules,
        Warn
    ),
    rule!(
        "BAMTS-W038",
        "virtual-call-in-constructor",
        ClassSemantics,
        Allow
    ),
    rule!(
        "BAMTS-W039",
        "uninitialized-field-emit-split",
        ClassSemantics,
        Allow
    ),
    rule!(
        "BAMTS-W040",
        "field-overrides-accessor",
        ClassSemantics,
        Allow
    ),
    rule!("BAMTS-W041", "implicit-override", ClassSemantics, Allow),
    rule!(
        "BAMTS-W042",
        "typescript-private-field",
        ClassSemantics,
        Allow
    ),
    rule!("BAMTS-W043", "runtime-enum", EnumSemantics, Warn),
    rule!("BAMTS-W044", "const-enum", EnumSemantics, Warn),
    rule!(
        "BAMTS-W045",
        "numeric-enum-number-flow",
        EnumSemantics,
        Warn
    ),
    rule!("BAMTS-W046", "heterogeneous-enum", EnumSemantics, Warn),
    rule!("BAMTS-W047", "computed-enum-member", EnumSemantics, Warn),
    rule!(
        "BAMTS-W048",
        "numeric-enum-reverse-lookup",
        EnumSemantics,
        Warn
    ),
    rule!(
        "BAMTS-W049",
        "interface-declaration-merge",
        DeclarationMerging,
        Warn
    ),
    rule!(
        "BAMTS-W050",
        "namespace-value-merge",
        DeclarationMerging,
        Warn
    ),
    rule!(
        "BAMTS-W051",
        "global-augmentation",
        DeclarationMerging,
        Warn
    ),
    rule!(
        "BAMTS-W052",
        "module-augmentation",
        DeclarationMerging,
        Warn
    ),
    rule!(
        "BAMTS-W053",
        "ambient-value-declaration",
        DeclarationMerging,
        Warn
    ),
    rule!(
        "BAMTS-W054",
        "javascript-input",
        JavaScriptCompatibility,
        Allow
    ),
    rule!(
        "BAMTS-W055",
        "jsdoc-type-syntax",
        JavaScriptCompatibility,
        Allow
    ),
    rule!(
        "BAMTS-W056",
        "prototype-class-pattern",
        JavaScriptCompatibility,
        Allow
    ),
    rule!(
        "BAMTS-W057",
        "ts-check-directive",
        JavaScriptCompatibility,
        Allow
    ),
    rule!("BAMTS-W058", "prefer-type-alias", Opinionated, Allow),
    rule!("BAMTS-W059", "prefer-readonly-array", Opinionated, Allow),
    rule!("BAMTS-W060", "prefer-function-property", Opinionated, Allow),
    rule!("BAMTS-W061", "no-barrel-star-export", Opinionated, Allow),
    rule!("BAMTS-W062", "no-default-export", Opinionated, Allow),
    rule!(
        "BAMTS-W063",
        "exhaustive-discriminated-switch",
        Opinionated,
        Allow
    ),
    rule!("BAMTS-W064", "long-parameter-list", Opinionated, Allow),
    rule!("BAMTS-W065", "implicit-return-path", ControlFlow, Warn),
    rule!("BAMTS-W066", "switch-fallthrough", ControlFlow, Warn),
    rule!("BAMTS-W067", "unreachable-code", ControlFlow, Warn),
    rule!("BAMTS-W068", "unused-label", ControlFlow, Warn),
    rule!("BAMTS-W069", "unused-local", ControlFlow, Warn),
    rule!("BAMTS-W070", "unused-parameter", ControlFlow, Warn),
    rule!(
        "BAMTS-W071",
        "invalid-number-formatting-options",
        Unsoundness,
        Warn
    ),
    rule!(
        "BAMTS-W072",
        "unsound-numeric-key-order-assumption",
        Unsoundness,
        Warn
    ),
    rule!(
        "BAMTS-W073",
        "json-stringify-unserializable-type",
        Unsoundness,
        Warn
    ),
    rule!("BAMTS-W074", "unchecked-json-parse-any", Unsoundness, Warn),
    rule!(
        "BAMTS-W075",
        "numeric-array-default-sort",
        Unsoundness,
        Warn
    ),
    rule!("BAMTS-W076", "loose-equality-coercion", Unsoundness, Warn),
    rule!(
        "BAMTS-W077",
        "object-implicit-toprimitive-coercion",
        Unsoundness,
        Warn
    ),
    rule!(
        "BAMTS-W078",
        "symbol-template-interpolation-throw",
        Unsoundness,
        Warn
    ),
    rule!("BAMTS-W079", "nan-strict-comparison", Unsoundness, Warn),
    rule!(
        "BAMTS-W080",
        "unsafe-tostringtag-override",
        Unsoundness,
        Warn
    ),
    rule!(
        "BAMTS-W081",
        "uninitialized-class-field-shadowing",
        ClassSemantics,
        Allow
    ),
    rule!(
        "BAMTS-W082",
        "preserve-const-enums-option",
        NonErasable,
        Warn
    ),
    rule!(
        "BAMTS-W083",
        "emit-decorator-metadata-option",
        LegacySyntax,
        Warn
    ),
    rule!(
        "BAMTS-W084",
        "legacy-class-field-set-semantics",
        ClassSemantics,
        Allow
    ),
    rule!(
        "BAMTS-W085",
        "javascript-syntax-rejection",
        JavaScriptCompatibility,
        Deny
    ),
    rule!("BAMTS-W086", "cjs-esm-named-export-mismatch", Modules, Warn),
];

#[must_use]
pub fn rule_by_code(code: &str) -> Option<&'static RuleDefinition> {
    RULES.iter().find(|rule| rule.code() == code)
}

#[must_use]
pub fn rule_by_slug(slug: &str) -> Option<&'static RuleDefinition> {
    RULES.iter().find(|rule| rule.slug() == slug)
}

#[must_use]
pub fn rule_by_name(name: &str) -> Option<&'static RuleDefinition> {
    rule_by_code(name)
        .or_else(|| rule_by_slug(name))
        .or_else(|| alias_by_name(name).and_then(|alias| rule_by_code(alias.target_code)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuleAlias {
    alias: &'static str,
    target_code: &'static str,
}

impl RuleAlias {
    #[must_use]
    pub const fn alias(self) -> &'static str {
        self.alias
    }

    #[must_use]
    pub const fn target_code(self) -> &'static str {
        self.target_code
    }
}

/// Permanent migrations for published slugs. Entries are never removed.
pub static RULE_ALIASES: [RuleAlias; 4] = [
    RuleAlias {
        alias: "any-downcast",
        target_code: "BAMTS-W006",
    },
    RuleAlias {
        alias: "excess-property-bypass",
        target_code: "BAMTS-W003",
    },
    RuleAlias {
        alias: "unchecked-catch-property-access",
        target_code: "BAMTS-W005",
    },
    RuleAlias {
        alias: "dynamic-tuple-out-of-bounds-indexing",
        target_code: "BAMTS-W007",
    },
];

fn alias_by_name(name: &str) -> Option<RuleAlias> {
    RULE_ALIASES
        .iter()
        .copied()
        .find(|alias| alias.alias == name)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuleTombstone {
    code: &'static str,
}

impl RuleTombstone {
    #[must_use]
    pub const fn code(self) -> &'static str {
        self.code
    }
}

/// Reserved and retired rule codes. These values can never enter [`RULES`].
pub static RULE_TOMBSTONES: [RuleTombstone; 1] = [RuleTombstone { code: "BAMTS-W000" }];

fn is_tombstone(name: &str) -> bool {
    RULE_TOMBSTONES.iter().any(|entry| entry.code == name)
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum LintProfile {
    #[default]
    Default,
    Strict,
    Pedantic,
}

impl LintProfile {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Strict => "strict",
            Self::Pedantic => "pedantic",
        }
    }

    fn level(self, rule: &RuleDefinition) -> LintLevel {
        let strict = match rule.group() {
            RuleGroup::Unsoundness
            | RuleGroup::EscapeHatches
            | RuleGroup::NonErasable
            | RuleGroup::LegacySyntax => LintLevel::Deny,
            RuleGroup::ClassSemantics => LintLevel::Warn,
            RuleGroup::JavaScriptCompatibility if rule.code() != "BAMTS-W085" => LintLevel::Warn,
            RuleGroup::EnumSemantics if rule.code() != "BAMTS-W044" => LintLevel::Deny,
            _ => rule.default_level(),
        };
        match self {
            Self::Default => rule.default_level(),
            Self::Strict => strict,
            Self::Pedantic => match rule.group() {
                RuleGroup::EscapeHatches => LintLevel::Forbid,
                RuleGroup::Opinionated => LintLevel::Warn,
                RuleGroup::ClassSemantics | RuleGroup::JavaScriptCompatibility => LintLevel::Deny,
                _ => strict,
            },
        }
    }

    const fn unknown_level(self) -> LintLevel {
        match self {
            Self::Default => LintLevel::Warn,
            Self::Strict | Self::Pedantic => LintLevel::Deny,
        }
    }
}

impl FromStr for LintProfile {
    type Err = ParseLintProfileError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "default" => Ok(Self::Default),
            "strict" => Ok(Self::Strict),
            "pedantic" => Ok(Self::Pedantic),
            _ => Err(ParseLintProfileError(Arc::from(value))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseLintProfileError(Arc<str>);

impl fmt::Display for ParseLintProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown lint profile {:?}", self.0)
    }
}

impl std::error::Error for ParseLintProfileError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LintSetting {
    name: Arc<str>,
    level: LintLevel,
    source: Arc<str>,
}

impl LintSetting {
    #[must_use]
    pub fn new(name: impl Into<Arc<str>>, level: LintLevel, source: impl Into<Arc<str>>) -> Self {
        Self {
            name: name.into(),
            level,
            source: source.into(),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn level(&self) -> LintLevel {
        self.level
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// Parsed project baseline. Section order is irrelevant; rules are more specific than groups.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LintConfig {
    groups: Vec<LintSetting>,
    rules: Vec<LintSetting>,
}

impl LintConfig {
    #[must_use]
    pub const fn new(groups: Vec<LintSetting>, rules: Vec<LintSetting>) -> Self {
        Self { groups, rules }
    }

    #[must_use]
    pub fn groups(&self) -> &[LintSetting] {
        &self.groups
    }

    #[must_use]
    pub fn rules(&self) -> &[LintSetting] {
        &self.rules
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverrideTargetKind {
    Group,
    Rule,
    Either,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LintOverride {
    setting: LintSetting,
    target_kind: OverrideTargetKind,
}

impl LintOverride {
    #[must_use]
    pub fn new(name: impl Into<Arc<str>>, level: LintLevel, source: impl Into<Arc<str>>) -> Self {
        Self {
            setting: LintSetting::new(name, level, source),
            target_kind: OverrideTargetKind::Either,
        }
    }

    #[must_use]
    pub fn group(group: RuleGroup, level: LintLevel, source: impl Into<Arc<str>>) -> Self {
        Self {
            setting: LintSetting::new(group.slug(), level, source),
            target_kind: OverrideTargetKind::Group,
        }
    }

    #[must_use]
    pub fn rule(rule: RuleId, level: LintLevel, source: impl Into<Arc<str>>) -> Self {
        Self {
            setting: LintSetting::new(rule.code(), level, source),
            target_kind: OverrideTargetKind::Rule,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd)]
enum Specificity {
    Profile,
    Group,
    Rule,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppliedLevel {
    level: LintLevel,
    source: Arc<str>,
    specificity: Specificity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RuleState {
    profile: AppliedLevel,
    group: Option<AppliedLevel>,
    rule: Option<AppliedLevel>,
}

impl RuleState {
    fn effective(&self) -> &AppliedLevel {
        self.rule
            .as_ref()
            .or(self.group.as_ref())
            .unwrap_or(&self.profile)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LintIssueKind {
    RenamedRule { canonical: &'static str },
    RetiredCode,
    UnknownName { suggestion: Option<Arc<str>> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LintIssue {
    name: Arc<str>,
    level: LintLevel,
    source: Arc<str>,
    kind: LintIssueKind,
}

impl LintIssue {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn level(&self) -> LintLevel {
        self.level
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub const fn kind(&self) -> &LintIssueKind {
        &self.kind
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForbidOverrideError {
    rule: RuleId,
    forbidden_by: Arc<str>,
    lowered_by: Arc<str>,
}

impl ForbidOverrideError {
    #[must_use]
    pub const fn rule(&self) -> RuleId {
        self.rule
    }

    #[must_use]
    pub fn forbidden_by(&self) -> &str {
        &self.forbidden_by
    }

    #[must_use]
    pub fn lowered_by(&self) -> &str {
        &self.lowered_by
    }
}

impl fmt::Display for ForbidOverrideError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "rule {} ({}) was forbidden by {}; {} cannot lower it",
            self.rule.code(),
            self.rule.slug(),
            self.forbidden_by,
            self.lowered_by
        )
    }
}

impl std::error::Error for ForbidOverrideError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceDialect {
    TypeScript,
    JavaScript,
}

/// A resolved table with independent group and rule lanes, preserving specificity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LintTable {
    profile: LintProfile,
    states: Vec<RuleState>,
}

impl LintTable {
    #[must_use]
    pub fn new(profile: LintProfile) -> Self {
        let profile_source: Arc<str> = Arc::from(format!("{} profile", profile.as_str()));
        let states = RULES
            .iter()
            .map(|rule| RuleState {
                profile: AppliedLevel {
                    level: profile.level(rule),
                    source: Arc::clone(&profile_source),
                    specificity: Specificity::Profile,
                },
                group: None,
                rule: None,
            })
            .collect();
        Self { profile, states }
    }

    #[must_use]
    pub const fn profile(&self) -> LintProfile {
        self.profile
    }

    #[must_use]
    pub fn level(&self, rule: RuleId) -> LintLevel {
        self.state(rule).effective().level
    }

    #[must_use]
    pub fn source(&self, rule: RuleId) -> &str {
        &self.state(rule).effective().source
    }

    /// Applies the JavaScript compatibility dialect after ordinary resolution.
    #[must_use]
    pub fn level_for_source(&self, rule: RuleId, dialect: SourceDialect) -> LintLevel {
        if dialect == SourceDialect::TypeScript {
            return self.level(rule);
        }
        let spec_footgun = matches!(
            rule.code(),
            "BAMTS-W071"
                | "BAMTS-W072"
                | "BAMTS-W073"
                | "BAMTS-W074"
                | "BAMTS-W075"
                | "BAMTS-W076"
                | "BAMTS-W077"
                | "BAMTS-W078"
                | "BAMTS-W079"
                | "BAMTS-W080"
        );
        let control_flow = RULES[rule_index(rule)].group() == RuleGroup::ControlFlow;
        if spec_footgun || control_flow {
            LintLevel::Warn
        } else {
            LintLevel::Allow
        }
    }

    /// Applies the project baseline: groups first, then the more-specific rule lane.
    pub fn apply_config(
        &mut self,
        config: &LintConfig,
    ) -> Result<Vec<LintIssue>, ForbidOverrideError> {
        let mut issues = Vec::new();
        for setting in config.groups() {
            self.apply_setting(setting, OverrideTargetKind::Group, &mut issues)?;
        }
        for setting in config.rules() {
            self.apply_setting(setting, OverrideTargetKind::Rule, &mut issues)?;
        }
        Ok(issues)
    }

    /// Applies CLI overrides in declaration order. Rule specificity beats group order.
    pub fn apply_cli(
        &mut self,
        overrides: impl IntoIterator<Item = LintOverride>,
    ) -> Result<Vec<LintIssue>, ForbidOverrideError> {
        let mut issues = Vec::new();
        for lint_override in overrides {
            self.apply_setting(
                &lint_override.setting,
                lint_override.target_kind,
                &mut issues,
            )?;
        }
        Ok(issues)
    }

    fn apply_setting(
        &mut self,
        setting: &LintSetting,
        kind: OverrideTargetKind,
        issues: &mut Vec<LintIssue>,
    ) -> Result<(), ForbidOverrideError> {
        if kind != OverrideTargetKind::Rule {
            if let Ok(group) = RuleGroup::from_str(setting.name()) {
                return self.apply_group(group, setting);
            }
            if kind == OverrideTargetKind::Group {
                issues.push(self.unknown_issue(setting, group_suggestion(setting.name())));
                return Ok(());
            }
        }

        if is_tombstone(setting.name()) {
            issues.push(LintIssue {
                name: Arc::clone(&setting.name),
                level: LintLevel::Deny,
                source: Arc::clone(&setting.source),
                kind: LintIssueKind::RetiredCode,
            });
            return Ok(());
        }
        if let Some(rule) = rule_by_code(setting.name()).or_else(|| rule_by_slug(setting.name())) {
            return self.apply_rule(rule, setting);
        }
        if let Some(alias) = alias_by_name(setting.name()) {
            let rule = rule_by_code(alias.target_code).expect("alias target must be registered");
            issues.push(LintIssue {
                name: Arc::clone(&setting.name),
                level: LintLevel::Warn,
                source: Arc::clone(&setting.source),
                kind: LintIssueKind::RenamedRule {
                    canonical: rule.slug(),
                },
            });
            return self.apply_rule(rule, setting);
        }
        issues.push(self.unknown_issue(setting, rule_suggestion(setting.name())));
        Ok(())
    }

    fn apply_group(
        &mut self,
        group: RuleGroup,
        setting: &LintSetting,
    ) -> Result<(), ForbidOverrideError> {
        let targets: Vec<usize> = RULES
            .iter()
            .enumerate()
            .filter_map(|(index, rule)| (rule.group() == group).then_some(index))
            .collect();
        self.check_forbid(&targets, setting, Specificity::Group)?;
        for index in targets {
            self.states[index].group = Some(AppliedLevel {
                level: setting.level,
                source: Arc::clone(&setting.source),
                specificity: Specificity::Group,
            });
        }
        Ok(())
    }

    fn apply_rule(
        &mut self,
        rule: &'static RuleDefinition,
        setting: &LintSetting,
    ) -> Result<(), ForbidOverrideError> {
        let index = rule_index(rule.id());
        self.check_forbid(&[index], setting, Specificity::Rule)?;
        self.states[index].rule = Some(AppliedLevel {
            level: setting.level,
            source: Arc::clone(&setting.source),
            specificity: Specificity::Rule,
        });
        Ok(())
    }

    fn check_forbid(
        &self,
        indices: &[usize],
        setting: &LintSetting,
        specificity: Specificity,
    ) -> Result<(), ForbidOverrideError> {
        if setting.level == LintLevel::Forbid {
            return Ok(());
        }
        for &index in indices {
            let active = self.states[index].effective();
            if active.level == LintLevel::Forbid && specificity >= active.specificity {
                return Err(ForbidOverrideError {
                    rule: RULES[index].id(),
                    forbidden_by: Arc::clone(&active.source),
                    lowered_by: Arc::clone(&setting.source),
                });
            }
        }
        Ok(())
    }

    fn unknown_issue(&self, setting: &LintSetting, suggestion: Option<Arc<str>>) -> LintIssue {
        LintIssue {
            name: Arc::clone(&setting.name),
            level: self.profile.unknown_level(),
            source: Arc::clone(&setting.source),
            kind: LintIssueKind::UnknownName { suggestion },
        }
    }

    fn state(&self, rule: RuleId) -> &RuleState {
        &self.states[rule_index(rule)]
    }
}

fn rule_index(id: RuleId) -> usize {
    let code = id.code().as_bytes();
    let number = usize::from(code[7] - b'0') * 100
        + usize::from(code[8] - b'0') * 10
        + usize::from(code[9] - b'0');
    number - 1
}

fn group_suggestion(name: &str) -> Option<Arc<str>> {
    nearest_name(name, RuleGroup::ALL.into_iter().map(RuleGroup::slug))
}

fn rule_suggestion(name: &str) -> Option<Arc<str>> {
    nearest_name(
        name,
        RULES
            .iter()
            .flat_map(|rule| [rule.code(), rule.slug()])
            .chain(RULE_ALIASES.iter().map(|alias| alias.alias)),
    )
}

fn nearest_name<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<Arc<str>> {
    candidates
        .map(|candidate| (levenshtein(name, candidate), candidate))
        .min_by_key(|(distance, candidate)| (*distance, *candidate))
        .map(|(_, candidate)| Arc::from(candidate))
}

fn levenshtein(left: &str, right: &str) -> usize {
    let right_chars: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current = vec![0; right_chars.len() + 1];
    for (left_index, left_char) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right_chars.iter().enumerate() {
            let substitution = previous[right_index] + usize::from(left_char != *right_char);
            current[right_index + 1] = (current[right_index] + 1)
                .min(previous[right_index + 1] + 1)
                .min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(slug: &str) -> RuleId {
        rule_by_slug(slug).expect("test rule must exist").id()
    }

    #[test]
    fn registry_is_complete_and_unique() {
        assert_eq!(RULES.len(), 86);
        for (index, rule) in RULES.iter().enumerate() {
            assert!(rule.code().starts_with("BAMTS-W"));
            assert!(!rule.slug().is_empty());
            assert!(
                !RULES[..index]
                    .iter()
                    .any(|other| other.code() == rule.code())
            );
            assert!(
                !RULES[..index]
                    .iter()
                    .any(|other| other.slug() == rule.slug())
            );
            assert!(
                !RULE_TOMBSTONES
                    .iter()
                    .any(|entry| entry.code() == rule.code())
            );
        }
    }

    #[test]
    fn ordered_overrides_keep_rule_specificity_over_later_group() {
        let target = rule("explicit-any");
        let mut table = LintTable::new(LintProfile::Default);
        table
            .apply_cli([
                LintOverride::group(
                    RuleGroup::EscapeHatches,
                    LintLevel::Deny,
                    "-D escape-hatches",
                ),
                LintOverride::rule(target, LintLevel::Allow, "-A explicit-any"),
                LintOverride::group(
                    RuleGroup::EscapeHatches,
                    LintLevel::Warn,
                    "-W escape-hatches",
                ),
            ])
            .unwrap();
        assert_eq!(table.level(target), LintLevel::Allow);
        assert_eq!(table.source(target), "-A explicit-any");
        assert_eq!(table.level(rule("implicit-any")), LintLevel::Warn);
    }

    #[test]
    fn later_override_wins_within_the_same_specificity() {
        let target = rule("unused-local");
        let mut table = LintTable::new(LintProfile::Default);
        table
            .apply_cli([
                LintOverride::rule(target, LintLevel::Deny, "first"),
                LintOverride::rule(target, LintLevel::Warn, "second"),
            ])
            .unwrap();
        assert_eq!(table.level(target), LintLevel::Warn);
        assert_eq!(table.source(target), "second");
    }

    #[test]
    fn forbid_lock_reports_both_sources() {
        let target = rule("explicit-any");
        let mut table = LintTable::new(LintProfile::Default);
        table
            .apply_cli([LintOverride::rule(
                target,
                LintLevel::Forbid,
                "security policy",
            )])
            .unwrap();
        let error = table
            .apply_cli([LintOverride::rule(
                target,
                LintLevel::Warn,
                "developer flag",
            )])
            .unwrap_err();
        assert_eq!(error.rule(), target);
        assert_eq!(error.forbidden_by(), "security policy");
        assert_eq!(error.lowered_by(), "developer flag");
    }

    #[test]
    fn profiles_expand_the_settled_families() {
        let escape = rule("explicit-any");
        let opinionated = rule("prefer-type-alias");
        let module_exception = rule("commonjs-in-esm");
        let const_enum = rule("const-enum");
        assert_eq!(
            LintTable::new(LintProfile::Default).level(escape),
            LintLevel::Warn
        );
        assert_eq!(
            LintTable::new(LintProfile::Strict).level(escape),
            LintLevel::Deny
        );
        assert_eq!(
            LintTable::new(LintProfile::Pedantic).level(escape),
            LintLevel::Forbid
        );
        assert_eq!(
            LintTable::new(LintProfile::Default).level(opinionated),
            LintLevel::Allow
        );
        assert_eq!(
            LintTable::new(LintProfile::Strict).level(opinionated),
            LintLevel::Allow
        );
        assert_eq!(
            LintTable::new(LintProfile::Pedantic).level(opinionated),
            LintLevel::Warn
        );
        assert_eq!(
            LintTable::new(LintProfile::Strict).level(module_exception),
            LintLevel::Allow
        );
        assert_eq!(
            LintTable::new(LintProfile::Strict).level(const_enum),
            LintLevel::Warn
        );
        assert_eq!(
            LintTable::new(LintProfile::Strict).level(rule("runtime-enum")),
            LintLevel::Deny
        );
    }

    #[test]
    fn aliases_resolve_and_warn_without_losing_the_setting() {
        let mut table = LintTable::new(LintProfile::Default);
        let issues = table
            .apply_cli([LintOverride::new(
                "any-downcast",
                LintLevel::Deny,
                "legacy config",
            )])
            .unwrap();
        assert_eq!(table.level(rule("generic-any-downcast")), LintLevel::Deny);
        assert!(matches!(
            issues[0].kind(),
            LintIssueKind::RenamedRule {
                canonical: "generic-any-downcast"
            }
        ));
    }

    #[test]
    fn tombstones_are_rejected() {
        let mut table = LintTable::new(LintProfile::Default);
        let issues = table
            .apply_cli([LintOverride::new("BAMTS-W000", LintLevel::Warn, "config")])
            .unwrap();
        assert_eq!(issues[0].level(), LintLevel::Deny);
        assert_eq!(issues[0].kind(), &LintIssueKind::RetiredCode);
    }

    #[test]
    fn unknown_names_warn_by_default_and_deny_in_stricter_profiles() {
        for (profile, level) in [
            (LintProfile::Default, LintLevel::Warn),
            (LintProfile::Strict, LintLevel::Deny),
            (LintProfile::Pedantic, LintLevel::Deny),
        ] {
            let mut table = LintTable::new(profile);
            let issues = table
                .apply_cli([LintOverride::new("explicit-ang", LintLevel::Warn, "config")])
                .unwrap();
            assert_eq!(issues[0].level(), level);
            assert!(matches!(
                issues[0].kind(),
                LintIssueKind::UnknownName { suggestion: Some(name) } if name.as_ref() == "explicit-any"
            ));
        }
    }

    #[test]
    fn javascript_dialect_is_warning_only_for_footguns_and_control_flow() {
        let table = LintTable::new(LintProfile::Pedantic);
        assert_eq!(
            table.level_for_source(
                rule("invalid-number-formatting-options"),
                SourceDialect::JavaScript
            ),
            LintLevel::Warn
        );
        assert_eq!(
            table.level_for_source(rule("unused-local"), SourceDialect::JavaScript),
            LintLevel::Warn
        );
        assert_eq!(
            table.level_for_source(rule("explicit-any"), SourceDialect::JavaScript),
            LintLevel::Allow
        );
    }
}
