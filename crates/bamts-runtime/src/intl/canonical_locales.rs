//! Cross-service ECMA-402 adapter contracts.
//!
//! Implements `Intl.getCanonicalLocales`, `Intl.supportedValuesOf`, and the
//! shared constructor/call/options plumbing every Intl service constructor
//! uses. All locale parsing and canonicalization delegate to the C10.1
//! negotiation module through [`LocaleDataProvider`]; all
//! implementation-defined value inventories (calendar, collation, currency,
//! numberingSystem, timeZone, unit) arrive through the injected
//! [`SupportedValuesInventory`] trait. This module reads no environment,
//! filesystem, clock, or other ambient host state and embeds no CLDR guesses.
//!
//! UTF-16 boundary: JavaScript strings are sequences of UTF-16 code units.
//! The runtime adapter decodes them to Rust `String` values and performs any
//! observable `Get`/`ToString` side effects before calling this module, which
//! then applies only the ECMA-402 validation layer to the decoded text.
//!
//! Specification sources:
//! - <https://tc39.es/ecma402/#sec-canonicalizelocalelist>
//! - <https://tc39.es/ecma402/#sec-supported-values-of>
//! - <https://tc39.es/ecma402/#sec-getoptionsobject>
//! - <https://tc39.es/ecma402/#sec-getoption>
//! - <https://tc39.es/ecma402/#sec-defaultnumberoption>

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use super::locale_negotiation::{
    JsErrorKind, LocaleDataProvider, LocaleError, LocaleMatcher, canonicalize_unicode_locale_id,
};

/// The ECMAScript error class surfaced by a cross-service adapter failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntlErrorKind {
    /// Wrong invocation form, wrong argument type, or a type-checked failure.
    TypeError,
    /// An input string or option value is outside the permitted range.
    RangeError,
}

/// An adapter failure carrying its ECMAScript error class and message text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntlError {
    /// ECMAScript error class of the thrown value.
    pub kind: IntlErrorKind,
    /// Human-readable `Error.prototype.message` text.
    pub message: String,
}

impl IntlError {
    /// Builds a `TypeError` failure.
    #[must_use]
    pub fn type_error(message: impl Into<String>) -> Self {
        Self { kind: IntlErrorKind::TypeError, message: message.into() }
    }

    /// Builds a `RangeError` failure.
    #[must_use]
    pub fn range_error(message: impl Into<String>) -> Self {
        Self { kind: IntlErrorKind::RangeError, message: message.into() }
    }

    /// Converts a C10.1 locale failure, preserving its JS error class.
    #[must_use]
    pub fn from_locale_error(error: LocaleError) -> Self {
        let message = error.to_string();
        match error.js_error_kind() {
            JsErrorKind::RangeError => Self::range_error(message),
        }
    }
}

impl fmt::Display for IntlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for IntlError {}

/// A JavaScript value at the Intl adapter boundary, carrying its ECMAScript
/// type tag and, for objects, the text the host obtained by coercion.
///
/// The host performs observable coercions before constructing this enum:
/// `ToString` on plain object values, and the internal-slot string of
/// `Intl.Locale` instances (whose user-defined `toString` must NOT be called
/// per `CanonicalizeLocaleList`). This module applies only validation.
#[derive(Clone, Debug, PartialEq)]
pub enum IntlValue {
    /// `undefined`.
    Undefined,
    /// `null`.
    Null,
    /// A boolean primitive.
    Boolean(bool),
    /// A number primitive.
    Number(f64),
    /// A string primitive, decoded to scalar text by the host.
    String(String),
    /// An object value, carried as the host-provided `ToString` result.
    Object(String),
}

/// Invocation form of a JavaScript call crossing the adapter boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntlCallForm {
    /// Called with `new` (`NewTarget` is defined).
    Construct,
    /// Called as a plain function (`NewTarget` is undefined).
    Apply,
}

/// Enforces the constructor-only contract shared by every Intl service
/// constructor (`Intl.Collator`, `Intl.NumberFormat`, `Intl.DateTimeFormat`,
/// and peers): a call with an undefined `NewTarget` throws `TypeError`.
///
/// # Errors
/// Returns `TypeError` when the constructor was invoked as a plain function.
pub fn require_constructor_call(name: &str, form: IntlCallForm) -> Result<(), IntlError> {
    match form {
        IntlCallForm::Construct => Ok(()),
        IntlCallForm::Apply => {
            Err(IntlError::type_error(format!("Constructor {name} requires 'new'")))
        }
    }
}

/// Enforces the not-a-constructor contract of plain Intl functions such as
/// `Intl.getCanonicalLocales` and `Intl.supportedValuesOf`: using them with
/// `new` throws `TypeError`.
///
/// # Errors
/// Returns `TypeError` when the function was invoked with `new`.
pub fn require_ordinary_call(name: &str, form: IntlCallForm) -> Result<(), IntlError> {
    match form {
        IntlCallForm::Apply => Ok(()),
        IntlCallForm::Construct => {
            Err(IntlError::type_error(format!("{name} is not a constructor")))
        }
    }
}

/// The typed `locales` argument of `Intl.getCanonicalLocales` and of every
/// Intl service constructor.
#[derive(Clone, Debug, PartialEq)]
pub enum LocaleListArgument {
    /// The argument is `undefined`; the canonical list is empty.
    Undefined,
    /// A string argument, or a single `Intl.Locale` instance: per
    /// `CanonicalizeLocaleList` step 3 it is wrapped as a single-element list,
    /// using the internal slot text without calling `toString`.
    Text(String),
    /// An object argument materialized by the host into its indexed elements
    /// in ascending index order, after `Get(O, "length")` and each `Get(O, Pk)`.
    Elements(Vec<IntlValue>),
    /// The argument is `null`; the `ToObject` in `CanonicalizeLocaleList`
    /// step 3b throws `TypeError`.
    Null,
    /// A boolean, number, bigint, or symbol primitive argument: `ToObject`
    /// wraps it into an object without a `length` own property, so the
    /// canonical list is empty.
    Primitive,
}

/// Applies ECMA-402 `Intl.getCanonicalLocales`. Structural validation and
/// canonicalization delegate to C10.1 using the provider's alias data; the
/// result is order-preserving and deduplicated on first occurrence.
///
/// # Errors
/// Returns `TypeError` for a `null` locales argument or a non-string,
/// non-object list element; returns `RangeError` for a structurally invalid
/// language tag.
pub fn get_canonical_locales(
    argument: &LocaleListArgument,
    provider: &dyn LocaleDataProvider,
) -> Result<Vec<String>, IntlError> {
    match argument {
        LocaleListArgument::Undefined | LocaleListArgument::Primitive => Ok(Vec::new()),
        LocaleListArgument::Null => {
            Err(IntlError::type_error("invalid locales argument: null"))
        }
        LocaleListArgument::Text(text) => Ok(vec![canonicalize_single_tag(text, provider)?]),
        LocaleListArgument::Elements(elements) => {
            let mut seen = BTreeSet::new();
            let mut canonical = Vec::new();
            for element in elements {
                let text = match element {
                    IntlValue::Undefined => continue,
                    IntlValue::String(text) | IntlValue::Object(text) => text,
                    IntlValue::Null | IntlValue::Boolean(_) | IntlValue::Number(_) => {
                        return Err(IntlError::type_error(
                            "locale list element must be a string or an object",
                        ));
                    }
                };
                let tag = canonicalize_single_tag(text, provider)?;
                if seen.insert(tag.clone()) {
                    canonical.push(tag);
                }
            }
            Ok(canonical)
        }
    }
}

fn canonicalize_single_tag(
    tag: &str,
    provider: &dyn LocaleDataProvider,
) -> Result<String, IntlError> {
    canonicalize_unicode_locale_id(tag, provider).map_err(IntlError::from_locale_error)
}

/// The six keys accepted by `Intl.supportedValuesOf`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SupportedValuesKey {
    /// `"calendar"`.
    Calendar,
    /// `"collation"`.
    Collation,
    /// `"currency"`.
    Currency,
    /// `"numberingSystem"`.
    NumberingSystem,
    /// `"timeZone"`.
    TimeZone,
    /// `"unit"`.
    Unit,
}

impl SupportedValuesKey {
    /// The exact ECMA-402 spelling of this key.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Calendar => "calendar",
            Self::Collation => "collation",
            Self::Currency => "currency",
            Self::NumberingSystem => "numberingSystem",
            Self::TimeZone => "timeZone",
            Self::Unit => "unit",
        }
    }
}

impl TryFrom<&str> for SupportedValuesKey {
    type Error = IntlError;

    /// Exact, case-sensitive key match; every other spelling is `RangeError`.
    fn try_from(key: &str) -> Result<Self, IntlError> {
        for candidate in [
            Self::Calendar,
            Self::Collation,
            Self::Currency,
            Self::NumberingSystem,
            Self::TimeZone,
            Self::Unit,
        ] {
            if key == candidate.name() {
                return Ok(candidate);
            }
        }
        Err(IntlError::range_error("Invalid key"))
    }
}

/// Supplies the implementation-defined canonical value inventories backing
/// `Intl.supportedValuesOf`. An ICU4X/CLDR adapter sources these from
/// formatter provider data; implementers must not consult ambient host state.
pub trait SupportedValuesInventory {
    /// Canonical calendar identifiers available to date formatting.
    fn supported_calendars(&self) -> &[String];
    /// Canonical collation type identifiers available to collation.
    fn supported_collations(&self) -> &[String];
    /// Currency identifiers available to currency-aware number formatting.
    fn supported_currencies(&self) -> &[String];
    /// Canonical numbering system identifiers available to number formatting.
    fn supported_numbering_systems(&self) -> &[String];
    /// Case-sensitive IANA time zone names available to date formatting.
    fn supported_time_zones(&self) -> &[String];
    /// Simple and compound unit identifiers available to unit formatting.
    fn supported_units(&self) -> &[String];
}

/// Caller-populated, deterministic value inventory useful for embedding and
/// tests. Contains no CLDR guesses of its own.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MapSupportedValues {
    values: BTreeMap<SupportedValuesKey, Vec<String>>,
}

impl MapSupportedValues {
    /// Creates an empty inventory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the inventory for one key. Entries may be unsorted and
    /// duplicate; normalization happens per query.
    #[must_use]
    pub fn with_values(mut self, key: SupportedValuesKey, values: Vec<String>) -> Self {
        self.values.insert(key, values);
        self
    }

    fn raw(&self, key: SupportedValuesKey) -> &[String] {
        self.values.get(&key).map_or(&[], Vec::as_slice)
    }
}

impl SupportedValuesInventory for MapSupportedValues {
    fn supported_calendars(&self) -> &[String] {
        self.raw(SupportedValuesKey::Calendar)
    }
    fn supported_collations(&self) -> &[String] {
        self.raw(SupportedValuesKey::Collation)
    }
    fn supported_currencies(&self) -> &[String] {
        self.raw(SupportedValuesKey::Currency)
    }
    fn supported_numbering_systems(&self) -> &[String] {
        self.raw(SupportedValuesKey::NumberingSystem)
    }
    fn supported_time_zones(&self) -> &[String] {
        self.raw(SupportedValuesKey::TimeZone)
    }
    fn supported_units(&self) -> &[String] {
        self.raw(SupportedValuesKey::Unit)
    }
}

/// Applies ECMA-402 `Intl.supportedValuesOf`. `key` is the host-coerced
/// `ToString` of the property-key argument. The result carries the canonical
/// casing for the key, is ASCII-byte-order sorted (equal to the UTF-16
/// code-unit order of the JavaScript default sort for these identifiers), and
/// is deduplicated.
///
/// # Errors
/// Returns `RangeError` for an invalid key or a provider inventory entry that
/// violates the structural validity rule for its key.
pub fn supported_values_of(
    key: &str,
    inventory: &dyn SupportedValuesInventory,
) -> Result<Vec<String>, IntlError> {
    let key = SupportedValuesKey::try_from(key)?;
    let raw = match key {
        SupportedValuesKey::Calendar => inventory.supported_calendars(),
        SupportedValuesKey::Collation => inventory.supported_collations(),
        SupportedValuesKey::Currency => inventory.supported_currencies(),
        SupportedValuesKey::NumberingSystem => inventory.supported_numbering_systems(),
        SupportedValuesKey::TimeZone => inventory.supported_time_zones(),
        SupportedValuesKey::Unit => inventory.supported_units(),
    };
    normalize_supported_values(key, raw)
}

fn normalize_supported_values(
    key: SupportedValuesKey,
    raw: &[String],
) -> Result<Vec<String>, IntlError> {
    let mut values = Vec::with_capacity(raw.len());
    for entry in raw {
        let canonical = match key {
            // Currency codes are canonical uppercase; Unicode type values are
            // canonical lowercase; time zone names and unit identifiers are
            // case-sensitive and pass through unchanged.
            SupportedValuesKey::Currency => entry.to_ascii_uppercase(),
            SupportedValuesKey::Calendar
            | SupportedValuesKey::Collation
            | SupportedValuesKey::NumberingSystem => entry.to_ascii_lowercase(),
            SupportedValuesKey::TimeZone | SupportedValuesKey::Unit => entry.clone(),
        };
        let valid = match key {
            SupportedValuesKey::Calendar
            | SupportedValuesKey::Collation
            | SupportedValuesKey::NumberingSystem => is_unicode_type_value(&canonical),
            SupportedValuesKey::Currency => is_currency_code(&canonical),
            SupportedValuesKey::TimeZone => is_time_zone_name(&canonical),
            SupportedValuesKey::Unit => is_unit_identifier(&canonical),
        };
        if !valid {
            return Err(IntlError::range_error(format!(
                "Invalid {} inventory value: {canonical}",
                key.name()
            )));
        }
        values.push(canonical);
    }
    if key == SupportedValuesKey::Collation {
        // ECMA-402 AvailableCollations: "search" and "standard" do not
        // represent collations and must never surface in the result.
        values.retain(|value| value != "search" && value != "standard");
    }
    values.sort();
    values.dedup();
    Ok(values)
}

/// Unicode `unicode_type`: one or more `-`-separated 3-to-8-character ASCII
/// alphanumeric sequences (the calendar/collation/numberingSystem shape).
fn is_unicode_type_value(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|part| {
            (3..=8).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

/// Three uppercase ASCII letters (the ISO 4217 alpha-code shape).
fn is_currency_code(value: &str) -> bool {
    value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_uppercase())
}

/// A case-sensitive IANA time zone name: nonempty ASCII letters, digits,
/// `_`, `-`, `+`, and `/`. IANA zone names contain no `.` runs.
fn is_time_zone_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'/')
        })
}

/// A well-formed simple or compound Unicode unit identifier: `-`-separated
/// nonempty ASCII alphanumeric segments containing at most one inner `-per-`
/// denominator separator.
fn is_unit_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|part| {
            !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        && value.matches("-per-").count() <= 1
        && !value.starts_with("per-")
        && !value.ends_with("-per")
}

/// A JavaScript property bag backing an `options` argument. The host
/// implements observable `[[Get]]` semantics; a throwing getter surfaces as
/// the returned error. Each shared option reader performs exactly one `get`
/// per call so the observable read order equals the caller's option order.
pub trait OptionsPropertySource {
    /// Returns the property value, `IntlValue::Undefined` when absent.
    ///
    /// # Errors
    /// Propagates a throwing getter as the host-provided error.
    fn get(&mut self, key: &str) -> Result<IntlValue, IntlError>;
}

/// The typed `options` argument accepted by ECMA-402 `GetOptionsObject`.
pub enum OptionsArgument<'a> {
    /// `undefined`: replaced by a null-prototype empty object.
    Undefined,
    /// `null` or a boolean/number/string/symbol/bigint primitive.
    NotAnObject,
    /// A JavaScript object delegate.
    Object(&'a mut dyn OptionsPropertySource),
}

/// Applies ECMA-402 `GetOptionsObject`. Returns `None` for the synthetic
/// null-prototype object created from `undefined` (every property read on it
/// is `undefined`, and prototype chains are never consulted).
///
/// # Errors
/// Returns `TypeError` when the argument is `null` or a primitive.
pub fn get_options_object(
    argument: OptionsArgument<'_>,
) -> Result<Option<&mut dyn OptionsPropertySource>, IntlError> {
    match argument {
        OptionsArgument::Undefined => Ok(None),
        OptionsArgument::Object(source) => Ok(Some(source)),
        OptionsArgument::NotAnObject => {
            Err(IntlError::type_error("options must be an object or undefined"))
        }
    }
}

/// ECMA-402 `GetOption` with `type` string: a single `Get`, then `ToString`,
/// then validation against `values` when non-empty; `undefined` yields the
/// fallback.
///
/// # Errors
/// Propagates getter errors and returns `RangeError` when a defined,
/// coerced value is not an element of `values`.
pub fn get_string_option(
    options: &mut dyn OptionsPropertySource,
    name: &str,
    values: &[&str],
    fallback: Option<&str>,
) -> Result<Option<String>, IntlError> {
    let value = options.get(name)?;
    if matches!(value, IntlValue::Undefined) {
        return Ok(fallback.map(str::to_owned));
    }
    let text = to_string(&value);
    if !values.is_empty() && !values.contains(&text.as_str()) {
        return Err(IntlError::range_error(format!("invalid option value: {text}")));
    }
    Ok(Some(text))
}

/// ECMA-402 `GetOption` with `type` boolean: a single `Get`, then
/// `ToBoolean`; `undefined` yields the fallback. Boolean options carry an
/// empty `values` list per spec, so no membership check applies.
///
/// # Errors
/// Propagates getter errors.
pub fn get_boolean_option(
    options: &mut dyn OptionsPropertySource,
    name: &str,
    fallback: Option<bool>,
) -> Result<Option<bool>, IntlError> {
    let value = options.get(name)?;
    if matches!(value, IntlValue::Undefined) {
        return Ok(fallback);
    }
    Ok(Some(to_boolean(&value)))
}

/// ECMA-402 `GetNumberOption`: a single `Get`, then `ToNumber`; `NaN` and
/// values outside `[minimum, maximum]` throw `RangeError`; the result is
/// floored.
///
/// # Errors
/// Propagates getter errors and returns `RangeError` for `NaN` or
/// out-of-range defined values.
pub fn get_number_option(
    options: &mut dyn OptionsPropertySource,
    name: &str,
    minimum: f64,
    maximum: f64,
    fallback: Option<f64>,
) -> Result<Option<f64>, IntlError> {
    let value = options.get(name)?;
    if matches!(value, IntlValue::Undefined) {
        return Ok(fallback);
    }
    let number = to_number(&value);
    if number.is_nan() || number < minimum || number > maximum {
        return Err(IntlError::range_error("invalid option value"));
    }
    Ok(Some(number.floor()))
}

/// ECMA-402 `DefaultNumberOption`: a single `Get`, then `ToNumber`; `NaN`
/// and out-of-range values collapse to `otherwise` instead of throwing; the
/// result is floored.
///
/// # Errors
/// Propagates getter errors.
pub fn get_default_number_option(
    options: &mut dyn OptionsPropertySource,
    name: &str,
    minimum: f64,
    maximum: f64,
    otherwise: f64,
) -> Result<f64, IntlError> {
    let value = options.get(name)?;
    if matches!(value, IntlValue::Undefined) {
        return Ok(otherwise);
    }
    let number = to_number(&value);
    if number.is_nan() || number < minimum || number > maximum {
        return Ok(otherwise);
    }
    Ok(number.floor())
}

/// Shared `localeMatcher` option read, mapping its text to the C10.1 matcher.
/// Per each service's spec order this is read immediately after any leading
/// `usage` option and before service data options.
///
/// # Errors
/// Propagates getter errors and returns `RangeError` for other spellings.
pub fn get_locale_matcher_option(
    options: &mut dyn OptionsPropertySource,
) -> Result<LocaleMatcher, IntlError> {
    let value =
        get_string_option(options, "localeMatcher", &["lookup", "best fit"], Some("best fit"))?;
    Ok(match value.as_deref() {
        Some("lookup") => LocaleMatcher::Lookup,
        _ => LocaleMatcher::BestFit,
    })
}

/// ECMA-262 `ToString` for the represented primitives; `Object` holds the
/// host-provided coercion result.
fn to_string(value: &IntlValue) -> String {
    match value {
        IntlValue::Undefined => "undefined".to_owned(),
        IntlValue::Null => "null".to_owned(),
        IntlValue::Boolean(boolean) => boolean.to_string(),
        IntlValue::Number(number) => number_to_string(*number),
        IntlValue::String(text) | IntlValue::Object(text) => text.clone(),
    }
}

fn number_to_string(number: f64) -> String {
    if number.is_nan() {
        return "NaN".to_owned();
    }
    // Both zeroes print "0"; keeps the option-string surface integral.
    if number == 0.0 {
        return "0".to_owned();
    }
    if number.is_infinite() {
        return if number > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    // Integral values take the shortest exact decimal form. Exponential-edge
    // differences from ECMA-262 above 1e21 can only leak into error message
    // text, never into a validated option value.
    if number.fract() == 0.0 && number.abs() < 1e15 {
        return format!("{number:.0}");
    }
    format!("{number}")
}

/// ECMA-262 `ToBoolean` for the represented primitives.
fn to_boolean(value: &IntlValue) -> bool {
    match value {
        IntlValue::Undefined | IntlValue::Null => false,
        IntlValue::Boolean(boolean) => *boolean,
        IntlValue::Number(number) => *number != 0.0 && !number.is_nan(),
        IntlValue::String(text) => !text.is_empty(),
        IntlValue::Object(_) => true,
    }
}

/// ECMA-262 `ToNumber` for the represented primitives; `Object` carries the
/// host-provided primitive string, which is then parsed.
fn to_number(value: &IntlValue) -> f64 {
    match value {
        IntlValue::Undefined => f64::NAN,
        IntlValue::Null => 0.0,
        IntlValue::Boolean(boolean) => f64::from(u8::from(*boolean)),
        IntlValue::Number(number) => *number,
        IntlValue::String(text) | IntlValue::Object(text) => string_to_number(text),
    }
}

/// ECMA-262 `StringNumericLiteral` parse: whitespace-trimmed decimal with
/// optional sign and exponent, `Infinity` with optional sign, and unsigned
/// `0x`/`0o`/`0b` integer forms; everything else is `NaN`.
fn string_to_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(is_ecma_whitespace);
    if trimmed.is_empty() {
        return 0.0;
    }
    match trimmed {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Rust's float parser accepts spellings that are not ECMA-262
    // StringNumericLiterals ("inf", "infinity", "NaN" in any case); force
    // them to the JavaScript result.
    if trimmed.eq_ignore_ascii_case("nan")
        || trimmed.trim_start_matches(['-', '+']).eq_ignore_ascii_case("inf")
        || trimmed.trim_start_matches(['-', '+']).eq_ignore_ascii_case("infinity")
    {
        return f64::NAN;
    }
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if let Some(prefixed) = parse_prefixed_radix(digits) {
        return if negative { -prefixed } else { prefixed };
    }
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

/// Parses `0x`/`0X`, `0o`/`0O`, and `0b`/`0B` integer literals, returning
/// `None` when the text carries no recognized prefix so the caller can fall
/// through to the decimal parser, and `Some(NaN)` for malformed bodies.
fn parse_prefixed_radix(digits: &str) -> Option<f64> {
    let (radix, body) = [
        (["0x", "0X"], 16_u32),
        (["0o", "0O"], 8_u32),
        (["0b", "0B"], 2_u32),
    ]
    .into_iter()
    .find_map(|(prefixes, radix)| {
        prefixes
            .into_iter()
            .find_map(|prefix| digits.strip_prefix(prefix).map(|rest| (radix, rest)))
    })?;
    if body.is_empty() || !body.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Some(f64::NAN);
    }
    if radix != 16 && !body.bytes().all(|byte| byte.is_ascii_digit() && byte < b'0' + radix as u8)
    {
        return Some(f64::NAN);
    }
    let mut value = 0.0_f64;
    for byte in body.bytes() {
        let digit = (byte as char).to_digit(radix)?;
        value = value.mul_add(f64::from(radix), f64::from(digit));
    }
    Some(value)
}

/// ECMA-262 WhiteSpace ∪ LineTerminator.
fn is_ecma_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n'
            | '\u{000B}'
            | '\u{000C}'
            | '\r'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intl::locale_negotiation::MapLocaleData;

    fn tag_list(argument: &LocaleListArgument, data: &dyn LocaleDataProvider) -> Vec<String> {
        get_canonical_locales(argument, data).unwrap_or_else(|error| panic!("{error}"))
    }

    fn inventory() -> MapSupportedValues {
        MapSupportedValues::new()
            .with_values(SupportedValuesKey::Calendar, vec![
                "gregory".into(),
                "ROC".into(),
                "Buddhist".into(),
                "gregory".into(),
            ])
            .with_values(SupportedValuesKey::Collation, vec![
                "search".into(),
                "emoji".into(),
                "standard".into(),
                "EMOJI".into(),
                "big5han".into(),
            ])
            .with_values(SupportedValuesKey::Currency, vec![
                "thb".into(),
                "usd".into(),
                "EUR".into(),
                "USD".into(),
            ])
            .with_values(SupportedValuesKey::NumberingSystem, vec![
                "thai".into(),
                "Latn".into(),
                "thai".into(),
            ])
            .with_values(SupportedValuesKey::TimeZone, vec![
                "Europe/Paris".into(),
                "UTC".into(),
                "America/New_York".into(),
                "Europe/Paris".into(),
            ])
            .with_values(SupportedValuesKey::Unit, vec![
                "square-meter".into(),
                "celsius".into(),
                "liter-per-100kilometer".into(),
                "celsius".into(),
            ])
    }

    #[test]
    fn canonical_locales_argument_forms() {
        let data = MapLocaleData::new();
        assert_eq!(tag_list(&LocaleListArgument::Undefined, &data), Vec::<String>::new());
        assert_eq!(tag_list(&LocaleListArgument::Primitive, &data), Vec::<String>::new());
        assert_eq!(
            get_canonical_locales(&LocaleListArgument::Null, &data)
                .map_err(|error| error.kind),
            Err(IntlErrorKind::TypeError),
        );
        // A single string argument is wrapped, not iterated character-wise.
        assert_eq!(
            tag_list(&LocaleListArgument::Text("EN-gb".into()), &data),
            vec!["en-GB".to_owned()],
        );
    }

    #[test]
    fn canonical_list_applies_element_rules_dedup_and_order() {
        let data = MapLocaleData::new().with_language_alias("iw", "he");
        let elements = vec![
            IntlValue::String("EN-gb".into()),
            IntlValue::Undefined,
            IntlValue::Object("he-IL".into()),
            IntlValue::String("iw-IL".into()),
            IntlValue::String("fr".into()),
        ];
        assert_eq!(
            tag_list(&LocaleListArgument::Elements(elements), &data),
            vec!["en-GB".to_owned(), "he-IL".to_owned(), "fr".to_owned()],
        );
    }

    #[test]
    fn canonical_list_rejects_non_string_elements() {
        let data = MapLocaleData::new();
        for element in [IntlValue::Null, IntlValue::Boolean(true), IntlValue::Number(3.0)] {
            let error = get_canonical_locales(&LocaleListArgument::Elements(vec![element]), &data)
                .expect_err("accepted a non-string element");
            assert_eq!(error.kind, IntlErrorKind::TypeError);
        }
    }

    #[test]
    fn malformed_tags_are_range_errors() {
        let data = MapLocaleData::new();
        for tag in ["", "en_US", "en--US", "en-u", "-en"] {
            let error = get_canonical_locales(&LocaleListArgument::Text(tag.to_owned()), &data)
                .expect_err("accepted a malformed tag");
            assert_eq!(error.kind, IntlErrorKind::RangeError, "wrong class for {tag}");
        }
    }

    #[test]
    fn canonical_list_is_deterministic_across_calls() {
        let data = MapLocaleData::new().with_language_alias("sh", "sr-Latn");
        let elements = vec![IntlValue::String("SH".into()), IntlValue::String("sh".into())];
        let argument = LocaleListArgument::Elements(elements);
        assert_eq!(
            tag_list(&argument, &data),
            tag_list(&argument, &data),
        );
        assert_eq!(tag_list(&argument, &data), vec!["sr-Latn".to_owned()]);
    }

    #[test]
    fn supported_values_returns_each_key_sorted_unique_canonical() {
        let data = inventory();
        assert_eq!(
            supported_values_of("calendar", &data)
                .unwrap_or_else(|error| panic!("{error}")),
            vec!["buddhist".to_owned(), "gregory".to_owned(), "roc".to_owned()],
        );
        assert_eq!(
            supported_values_of("collation", &data)
                .unwrap_or_else(|error| panic!("{error}")),
            vec!["big5han".to_owned(), "emoji".to_owned()],
        );
        assert_eq!(
            supported_values_of("currency", &data)
                .unwrap_or_else(|error| panic!("{error}")),
            vec!["EUR".to_owned(), "THB".to_owned(), "USD".to_owned()],
        );
        assert_eq!(
            supported_values_of("numberingSystem", &data)
                .unwrap_or_else(|error| panic!("{error}")),
            vec!["latn".to_owned(), "thai".to_owned()],
        );
        assert_eq!(
            supported_values_of("timeZone", &data)
                .unwrap_or_else(|error| panic!("{error}")),
            vec![
                "America/New_York".to_owned(),
                "Europe/Paris".to_owned(),
                "UTC".to_owned(),
            ],
        );
        assert_eq!(
            supported_values_of("unit", &data)
                .unwrap_or_else(|error| panic!("{error}")),
            vec![
                "celsius".to_owned(),
                "liter-per-100kilometer".to_owned(),
                "square-meter".to_owned(),
            ],
        );
    }

    #[test]
    fn supported_values_rejects_invalid_keys() {
        let data = MapSupportedValues::new();
        for key in ["", "Calendar", "calendars", "ca", "timezone", "units"] {
            let error = supported_values_of(key, &data).expect_err("accepted an invalid key");
            assert_eq!(error.kind, IntlErrorKind::RangeError, "wrong class for {key}");
            assert_eq!(error.message, "Invalid key", "wrong message for {key}");
        }
    }

    #[test]
    fn supported_values_rejects_malformed_inventory_entries() {
        let cases: [(SupportedValuesKey, &str); 7] = [
            (SupportedValuesKey::Currency, "US"),
            (SupportedValuesKey::Currency, "U2D"),
            (SupportedValuesKey::Calendar, "ab"),
            (SupportedValuesKey::NumberingSystem, "abcdefghi"),
            (SupportedValuesKey::TimeZone, ""),
            (SupportedValuesKey::Unit, "per-second"),
            (SupportedValuesKey::Unit, "meter--second"),
        ];
        for (key, entry) in cases {
            let data = MapSupportedValues::new().with_values(key, vec![entry.to_owned()]);
            let error = supported_values_of(key.name(), &data)
                .expect_err("accepted a malformed inventory entry");
            assert_eq!(error.kind, IntlErrorKind::RangeError, "wrong class for {entry}");
            assert!(
                error.message.contains(key.name()) && error.message.contains(entry),
                "message {error:?} names neither key nor value",
            );
        }
    }

    #[test]
    fn collation_exclusions_apply_before_sort_and_dedup() {
        let data = MapSupportedValues::new().with_values(SupportedValuesKey::Collation, vec![
            "standard".into(),
            "search".into(),
            "EMO".into(),
        ]);
        assert_eq!(
            supported_values_of("collation", &data).unwrap_or_else(|error| panic!("{error}")),
            vec!["emo".to_owned()],
        );
    }

    #[test]
    fn supported_values_is_deterministic_across_calls() {
        let data = inventory();
        for key in ["calendar", "collation", "currency", "numberingSystem", "timeZone", "unit"] {
            assert_eq!(
                supported_values_of(key, &data),
                supported_values_of(key, &data),
            );
        }
    }

    struct RecordingSource {
        values: BTreeMap<String, IntlValue>,
        reads: Vec<String>,
    }

    impl RecordingSource {
        fn new(values: &[(&str, IntlValue)]) -> Self {
            Self {
                values: values
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.clone()))
                    .collect(),
                reads: Vec::new(),
            }
        }
    }

    impl OptionsPropertySource for RecordingSource {
        fn get(&mut self, key: &str) -> Result<IntlValue, IntlError> {
            self.reads.push(key.to_owned());
            Ok(self.values.get(key).cloned().unwrap_or(IntlValue::Undefined))
        }
    }

    struct ThrowingSource;

    impl OptionsPropertySource for ThrowingSource {
        fn get(&mut self, key: &str) -> Result<IntlValue, IntlError> {
            Err(IntlError::type_error(format!("getter for {key} threw")))
        }
    }

    #[test]
    fn options_read_happens_once_per_option_in_caller_order() {
        let mut source = RecordingSource::new(&[
            ("usage", IntlValue::String("sort".into())),
            ("localeMatcher", IntlValue::String("lookup".into())),
            ("numeric", IntlValue::Boolean(false)),
        ]);
        let usage = get_string_option(&mut source, "usage", &["sort", "search"], None)
            .unwrap_or_else(|error| panic!("{error}"));
        let matcher = get_locale_matcher_option(&mut source)
            .unwrap_or_else(|error| panic!("{error}"));
        let numeric = get_boolean_option(&mut source, "numeric", None)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(usage, Some("sort".to_owned()));
        assert_eq!(matcher, LocaleMatcher::Lookup);
        assert_eq!(numeric, Some(false));
        assert_eq!(
            source.reads,
            vec!["usage".to_owned(), "localeMatcher".to_owned(), "numeric".to_owned()],
        );
    }

    #[test]
    fn string_option_fallback_validation_and_coercions() {
        let mut missing = RecordingSource::new(&[]);
        assert_eq!(
            get_string_option(&mut missing, "localeMatcher", &["lookup", "best fit"], Some("best fit"))
                .unwrap_or_else(|error| panic!("{error}")),
            Some("best fit".to_owned()),
        );
        let mut wrong = RecordingSource::new(&[("localeMatcher", IntlValue::String("fuzzy".into()))]);
        let error = get_string_option(&mut wrong, "localeMatcher", &["lookup", "best fit"], None)
            .expect_err("accepted an out-of-list value");
        assert_eq!(error.kind, IntlErrorKind::RangeError);
        // `null` coerces to the string "null" and fails list validation.
        let mut null = RecordingSource::new(&[("sensitivity", IntlValue::Null)]);
        let error = get_string_option(
            &mut null,
            "sensitivity",
            &["base", "accent", "case", "variant"],
            None,
        )
        .expect_err("accepted null for a valued option");
        assert_eq!(error.kind, IntlErrorKind::RangeError);
        // Boolean values stringify before list validation.
        let mut boolean = RecordingSource::new(&[("caseFirst", IntlValue::Boolean(true))]);
        assert_eq!(
            get_string_option(&mut boolean, "caseFirst", &[], None)
                .unwrap_or_else(|error| panic!("{error}")),
            Some("true".to_owned()),
        );
    }

    #[test]
    fn boolean_option_applies_to_boolean_rules() {
        let cases: [(IntlValue, bool); 5] = [
            (IntlValue::Null, false),
            (IntlValue::Number(0.0), false),
            (IntlValue::Number(f64::NAN), false),
            (IntlValue::String("x".into()), true),
            (IntlValue::Object("".into()), true),
        ];
        for (value, expected) in cases {
            let mut source = RecordingSource::new(&[("pinGroup", value)]);
            assert_eq!(
                get_boolean_option(&mut source, "pinGroup", Some(false))
                    .unwrap_or_else(|error| panic!("{error}")),
                Some(expected),
            );
        }
        let mut missing = RecordingSource::new(&[]);
        assert_eq!(
            get_boolean_option(&mut missing, "pinGroup", Some(true))
                .unwrap_or_else(|error| panic!("{error}")),
            Some(true),
        );
    }

    #[test]
    fn number_options_follow_spec_edges() {
        let mut text = RecordingSource::new(&[("minimumIntegerDigits", IntlValue::String("3".into()))]);
        assert_eq!(
            get_number_option(&mut text, "minimumIntegerDigits", 1.0, 21.0, None)
                .unwrap_or_else(|error| panic!("{error}")),
            Some(3.0),
        );
        let mut outstanding = RecordingSource::new(&[("maximumFractionDigits", IntlValue::Number(20.9))]);
        assert_eq!(
            get_number_option(&mut outstanding, "maximumFractionDigits", 0.0, 20.0, None)
                .map_err(|error| error.kind),
            Err(IntlErrorKind::RangeError),
        );
        let mut fractional = RecordingSource::new(&[("roundingIncrement", IntlValue::Number(10.9))]);
        assert_eq!(
            get_number_option(&mut fractional, "roundingIncrement", 1.0, 20.0, None)
                .unwrap_or_else(|error| panic!("{error}")),
            Some(10.0),
        );
        let mut absent = RecordingSource::new(&[]);
        assert_eq!(
            get_number_option(&mut absent, "minimumIntegerDigits", 1.0, 21.0, Some(1.0))
                .unwrap_or_else(|error| panic!("{error}")),
            Some(1.0),
        );
        let mut bad = RecordingSource::new(&[("sign", IntlValue::String("abc".into()))]);
        let error = get_number_option(&mut bad, "sign", 1.0, 5.0, None)
            .expect_err("accepted a NaN option value");
        assert_eq!(error.kind, IntlErrorKind::RangeError);
        // DefaultNumberOption collapses NaN and out-of-range to `otherwise`.
        let mut nan = RecordingSource::new(&[("signDisplay", IntlValue::String("xyz".into()))]);
        assert_eq!(
            get_default_number_option(&mut nan, "signDisplay", 1.0, 5.0, 1.0)
                .unwrap_or_else(|error| panic!("{error}")),
            1.0,
        );
        let mut high = RecordingSource::new(&[("digits", IntlValue::Number(99.0))]);
        assert_eq!(
            get_default_number_option(&mut high, "digits", 1.0, 2.0, 1.0)
                .unwrap_or_else(|error| panic!("{error}")),
            1.0,
        );
    }

    #[test]
    fn string_numeric_coercions_match_ecma_262() {
        let cases: [(&str, f64); 9] = [
            ("0x10", 16.0),
            ("0b101", 5.0),
            ("0o17", 15.0),
            ("", 0.0),
            ("  1.5\t", 1.5),
            ("-0", -0.0),
            ("Infinity", f64::INFINITY),
            ("abc", f64::NAN),
            ("inf", f64::NAN),
        ];
        for (text, expected) in cases {
            let mut source = RecordingSource::new(&[("value", IntlValue::String(text.to_owned()))]);
            let result = get_default_number_option(&mut source, "value", f64::NEG_INFINITY, f64::INFINITY, f64::NAN);
            if expected.is_nan() {
                // DefaultNumberOption maps NaN to `otherwise` (NaN): compare NaN-ness.
                assert!(result.map(f64::is_nan) == Ok(true), "wrong result for {text:?}");
            } else {
                assert_eq!(result, Ok(expected), "wrong result for {text:?}");
            }
        }
        // `null` coerces to +0.
        let mut source = RecordingSource::new(&[("value", IntlValue::Null)]);
        assert_eq!(
            get_default_number_option(&mut source, "value", f64::NEG_INFINITY, f64::INFINITY, f64::NAN),
            Ok(0.0),
        );
        // Object values already carry their coerced string.
        let mut source = RecordingSource::new(&[("value", IntlValue::Object("2".into()))]);
        assert_eq!(
            get_default_number_option(&mut source, "value", f64::NEG_INFINITY, f64::INFINITY, f64::NAN),
            Ok(2.0),
        );
    }

    #[test]
    fn constructor_and_plain_call_contracts() {
        assert!(require_constructor_call("Collator", IntlCallForm::Construct).is_ok());
        let error = require_constructor_call("Collator", IntlCallForm::Apply)
            .expect_err("accepted a plain constructor call");
        assert_eq!(error.kind, IntlErrorKind::TypeError);
        assert!(error.message.contains("Collator"));
        assert!(require_ordinary_call("Intl.getCanonicalLocales", IntlCallForm::Apply).is_ok());
        let error = require_ordinary_call("Intl.getCanonicalLocales", IntlCallForm::Construct)
            .expect_err("accepted constructing a plain function");
        assert_eq!(error.kind, IntlErrorKind::TypeError);
    }

    #[test]
    fn get_options_object_accepts_only_object_or_undefined() {
        let empty = get_options_object(OptionsArgument::Undefined)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(empty.is_none());
        let error = get_options_object(OptionsArgument::NotAnObject)
            .expect_err("accepted a non-object options argument");
        assert_eq!(error.kind, IntlErrorKind::TypeError);
        let mut source = RecordingSource::new(&[("style", IntlValue::String("currency".into()))]);
        let object = get_options_object(OptionsArgument::Object(&mut source))
            .unwrap_or_else(|error| panic!("{error}"));
        let object = object.expect("object options vanished");
        assert_eq!(
            get_string_option(object, "style", &["decimal", "currency", "percent"], None)
                .unwrap_or_else(|error| panic!("{error}")),
            Some("currency".to_owned()),
        );
    }

    #[test]
    fn getter_errors_propagate_out_of_option_readers() {
        let mut source = ThrowingSource;
        let error = get_boolean_option(&mut source, "numeric", None)
            .expect_err("swallowed a getter error");
        assert_eq!(error.kind, IntlErrorKind::TypeError);
        assert!(error.message.contains("numeric"));
    }
}
