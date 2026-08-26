//! Provider-driven Temporal time-zone and `ZonedDateTime` core.
//!
//! This module implements the ECMA-262 Temporal algorithms without consulting
//! ambient clocks, operating-system time-zone state, or locale data. Hosts
//! supply those data through the typed provider traits below. Epoch arithmetic
//! is exact `i128` nanoseconds throughout.

use std::{fmt, sync::Arc};

use super::instant_duration::{
    Duration, INSTANT_NS_MAX, INSTANT_NS_MIN, Instant, NS_PER_DAY, NS_PER_HOUR,
    NS_PER_MICROSECOND, NS_PER_MILLISECOND, NS_PER_MINUTE, NS_PER_SECOND, Overflow,
    Precision, RoundingMode, TemporalError, TemporalResult, Unit, UnitCategory,
    epoch_days_from_ymd, format_iso_date, format_time_ns, round_to_increment,
    validate_rounding_increment, ymd_from_epoch_days,
};
use super::plain_types::{
    PlainDate, PlainDateTime, PlainTime, iso_day_of_week, iso_day_of_year, iso_week_of_year,
};

/// A canonical time-zone identifier. Canonicalization and availability are a
/// host/provider concern; this type rejects values that cannot be embedded in
/// an RFC 9557 time-zone annotation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimeZoneId(Arc<str>);

impl TimeZoneId {
    pub fn new(identifier: impl AsRef<str>) -> TemporalResult<Self> {
        let identifier = identifier.as_ref();
        if identifier.is_empty()
            || identifier
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace() || matches!(byte, b'[' | b']'))
        {
            return Err(TemporalError::range("invalid time-zone identifier"));
        }
        Ok(Self(Arc::from(identifier)))
    }

    pub fn utc() -> Self {
        Self(Arc::from("UTC"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TimeZoneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A calendar identifier. `iso8601` is implemented by `IsoCalendarProvider`;
/// other identifiers may be interpreted by another `CalendarProvider`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CalendarId(Arc<str>);

impl CalendarId {
    pub fn new(identifier: impl AsRef<str>) -> TemporalResult<Self> {
        let identifier = identifier.as_ref();
        if identifier.is_empty()
            || !identifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(TemporalError::range("invalid calendar identifier"));
        }
        Ok(Self(Arc::from(identifier.to_ascii_lowercase())))
    }

    pub fn iso8601() -> Self {
        Self(Arc::from("iso8601"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_iso8601(&self) -> bool {
        self.as_str() == "iso8601"
    }
}

impl fmt::Display for CalendarId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// UTC offset with the Temporal invariant `-24h < offset < +24h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UtcOffsetNanoseconds(i64);

impl UtcOffsetNanoseconds {
    pub const UTC: Self = Self(0);

    pub fn new(nanoseconds: i64) -> TemporalResult<Self> {
        if !(-(NS_PER_DAY as i64)..NS_PER_DAY as i64).contains(&nanoseconds) {
            return Err(TemporalError::range(
                "time-zone offset must be strictly between -24h and +24h",
            ));
        }
        Ok(Self(nanoseconds))
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// FormatUTCOffsetNanoseconds, preserving seconds and fractional seconds when
/// the provider supplies sub-minute precision.
pub fn format_utc_offset(offset: UtcOffsetNanoseconds) -> String {
    let value = i128::from(offset.get());
    let sign = if value < 0 { '-' } else { '+' };
    let absolute = value.abs();
    let hours = absolute / NS_PER_HOUR;
    let minutes = absolute / NS_PER_MINUTE % 60;
    let seconds = absolute / NS_PER_SECOND % 60;
    let fraction = absolute % NS_PER_SECOND;
    let mut result = format!("{sign}{hours:02}:{minutes:02}");
    if seconds != 0 || fraction != 0 {
        result.push_str(&format!(":{seconds:02}"));
        if fraction != 0 {
            let mut digits = format!("{fraction:09}");
            while digits.ends_with('0') {
                digits.pop();
            }
            result.push('.');
            result.push_str(&digits);
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Disambiguation {
    #[default]
    Compatible,
    Earlier,
    Later,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OffsetOption {
    Ignore,
    Use,
    Prefer,
    #[default]
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OffsetMatch {
    #[default]
    Exactly,
    Minutes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionDirection {
    Next,
    Previous,
}

/// The only time-zone data boundary used by this module. Implementations must
/// return possible instants in strictly increasing order and transitions must
/// be strict with respect to the supplied instant.
pub trait TimeZoneProvider {
    fn possible_instants_for(
        &self,
        time_zone: &TimeZoneId,
        local: PlainDateTime,
    ) -> TemporalResult<Vec<Instant>>;

    fn offset_nanoseconds_for(
        &self,
        time_zone: &TimeZoneId,
        instant: Instant,
    ) -> TemporalResult<UtcOffsetNanoseconds>;

    fn transition(
        &self,
        time_zone: &TimeZoneId,
        instant: Instant,
        direction: TransitionDirection,
    ) -> TemporalResult<Option<Instant>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarDateFields {
    pub era: Option<Arc<str>>,
    pub era_year: Option<i32>,
    pub year: i32,
    pub month: u8,
    pub month_code: Arc<str>,
    pub day: u8,
    pub day_of_week: u8,
    pub day_of_year: u16,
    pub week_of_year: Option<u8>,
    pub year_of_week: Option<i32>,
    pub days_in_week: u8,
    pub days_in_month: u8,
    pub days_in_year: u16,
    pub months_in_year: u8,
    pub in_leap_year: bool,
}

/// Calendar operations needed by ZonedDateTime. `PlainDate` remains the ISO
/// storage record; providers interpret it using `calendar` for observable
/// fields and calendar-relative arithmetic.
pub trait CalendarProvider {
    fn fields(
        &self,
        calendar: &CalendarId,
        iso_date: PlainDate,
    ) -> TemporalResult<CalendarDateFields>;

    fn date_from_fields(
        &self,
        calendar: &CalendarId,
        current: PlainDate,
        year: Option<i32>,
        month: Option<i64>,
        day: Option<i64>,
        overflow: Overflow,
    ) -> TemporalResult<PlainDate>;

    fn date_add(
        &self,
        calendar: &CalendarId,
        iso_date: PlainDate,
        duration: &Duration,
        overflow: Overflow,
    ) -> TemporalResult<PlainDate>;

    fn date_until(
        &self,
        calendar: &CalendarId,
        one: PlainDate,
        two: PlainDate,
        largest_unit: Unit,
    ) -> TemporalResult<Duration>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IsoCalendarProvider;

impl IsoCalendarProvider {
    fn require_iso(calendar: &CalendarId) -> TemporalResult<()> {
        if calendar.is_iso8601() {
            Ok(())
        } else {
            Err(TemporalError::range(format!(
                "calendar {} is not supported by IsoCalendarProvider",
                calendar
            )))
        }
    }
}

impl CalendarProvider for IsoCalendarProvider {
    fn fields(
        &self,
        calendar: &CalendarId,
        iso_date: PlainDate,
    ) -> TemporalResult<CalendarDateFields> {
        Self::require_iso(calendar)?;
        let (year_of_week, week_of_year) = iso_week_of_year(iso_date.year, iso_date.month, iso_date.day);
        Ok(CalendarDateFields {
            era: None,
            era_year: None,
            year: iso_date.year,
            month: iso_date.month,
            month_code: Arc::from(format!("M{:02}", iso_date.month)),
            day: iso_date.day,
            day_of_week: iso_day_of_week(iso_date.year, iso_date.month, iso_date.day),
            day_of_year: iso_day_of_year(iso_date.year, iso_date.month, iso_date.day),
            week_of_year: Some(week_of_year),
            year_of_week: Some(year_of_week),
            days_in_week: 7,
            days_in_month: iso_date.days_in_month(),
            days_in_year: iso_date.days_in_year(),
            months_in_year: 12,
            in_leap_year: iso_date.in_leap_year(),
        })
    }

    fn date_from_fields(
        &self,
        calendar: &CalendarId,
        current: PlainDate,
        year: Option<i32>,
        month: Option<i64>,
        day: Option<i64>,
        overflow: Overflow,
    ) -> TemporalResult<PlainDate> {
        Self::require_iso(calendar)?;
        current.with(year, month, day, overflow)
    }

    fn date_add(
        &self,
        calendar: &CalendarId,
        iso_date: PlainDate,
        duration: &Duration,
        overflow: Overflow,
    ) -> TemporalResult<PlainDate> {
        Self::require_iso(calendar)?;
        iso_date.add(duration, overflow)
    }

    fn date_until(
        &self,
        calendar: &CalendarId,
        one: PlainDate,
        two: PlainDate,
        largest_unit: Unit,
    ) -> TemporalResult<Duration> {
        Self::require_iso(calendar)?;
        one.until(
            two,
            largest_unit,
            Unit::Day,
            1,
            RoundingMode::Trunc,
            Overflow::Constrain,
        )
    }
}

/// Explicit provider bundle passed to every operation that can observe time-zone
/// or calendar data.
#[derive(Clone, Copy)]
pub struct TemporalProviders<'a> {
    pub time_zone: &'a dyn TimeZoneProvider,
    pub calendar: &'a dyn CalendarProvider,
}

impl<'a> TemporalProviders<'a> {
    pub fn new(
        time_zone: &'a dyn TimeZoneProvider,
        calendar: &'a dyn CalendarProvider,
    ) -> Self {
        Self { time_zone, calendar }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveOptions {
    pub disambiguation: Disambiguation,
    pub offset: OffsetOption,
    pub offset_match: OffsetMatch,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            disambiguation: Disambiguation::Compatible,
            offset: OffsetOption::Reject,
            offset_match: OffsetMatch::Exactly,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithOptions {
    pub disambiguation: Disambiguation,
    pub offset: OffsetOption,
    pub overflow: Overflow,
}

impl Default for WithOptions {
    fn default() -> Self {
        Self {
            disambiguation: Disambiguation::Compatible,
            offset: OffsetOption::Prefer,
            overflow: Overflow::Constrain,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArithmeticOptions {
    pub overflow: Overflow,
}

impl Default for ArithmeticOptions {
    fn default() -> Self {
        Self { overflow: Overflow::Constrain }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DifferenceOptions {
    pub largest_unit: Unit,
    pub smallest_unit: Unit,
    pub rounding_increment: i128,
    pub rounding_mode: RoundingMode,
}

impl Default for DifferenceOptions {
    fn default() -> Self {
        Self {
            largest_unit: Unit::Hour,
            smallest_unit: Unit::Nanosecond,
            rounding_increment: 1,
            rounding_mode: RoundingMode::Trunc,
        }
    }
}

impl DifferenceOptions {
    fn validate(self) -> TemporalResult<()> {
        if self.smallest_unit < self.largest_unit {
            return Err(TemporalError::range(
                "smallestUnit must not be larger than largestUnit",
            ));
        }
        if self.rounding_increment < 1 {
            return Err(TemporalError::range("roundingIncrement must be >= 1"));
        }
        if self.smallest_unit.category() == UnitCategory::Time {
            validate_rounding_increment(
                self.smallest_unit,
                self.rounding_increment,
                false,
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundOptions {
    pub smallest_unit: Unit,
    pub rounding_increment: i128,
    pub rounding_mode: RoundingMode,
}

impl Default for RoundOptions {
    fn default() -> Self {
        Self {
            smallest_unit: Unit::Nanosecond,
            rounding_increment: 1,
            rounding_mode: RoundingMode::HalfExpand,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ZonedDateTimeFields {
    pub year: Option<i32>,
    pub month: Option<i64>,
    pub day: Option<i64>,
    pub hour: Option<i64>,
    pub minute: Option<i64>,
    pub second: Option<i64>,
    pub millisecond: Option<i64>,
    pub microsecond: Option<i64>,
    pub nanosecond: Option<i64>,
    pub offset: Option<UtcOffsetNanoseconds>,
}

impl ZonedDateTimeFields {
    fn has_any(self) -> bool {
        self.year.is_some()
            || self.month.is_some()
            || self.day.is_some()
            || self.hour.is_some()
            || self.minute.is_some()
            || self.second.is_some()
            || self.millisecond.is_some()
            || self.microsecond.is_some()
            || self.nanosecond.is_some()
            || self.offset.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZonedFields {
    pub date_time: PlainDateTime,
    pub calendar: CalendarDateFields,
    pub offset: UtcOffsetNanoseconds,
}

impl ZonedFields {
    pub fn hour(&self) -> u8 {
        self.date_time.time.hour
    }

    pub fn minute(&self) -> u8 {
        self.date_time.time.minute
    }

    pub fn second(&self) -> u8 {
        self.date_time.time.second
    }

    pub fn millisecond(&self) -> u16 {
        self.date_time.time.millisecond
    }

    pub fn microsecond(&self) -> u16 {
        self.date_time.time.microsecond
    }

    pub fn nanosecond(&self) -> u16 {
        self.date_time.time.nanosecond
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ZonedDateTime {
    instant: Instant,
    time_zone: TimeZoneId,
    calendar: CalendarId,
}

impl ZonedDateTime {
    pub fn new(
        epoch_nanoseconds: i128,
        time_zone: TimeZoneId,
        calendar: CalendarId,
    ) -> TemporalResult<Self> {
        Ok(Self {
            instant: Instant::from_epoch_nanoseconds(epoch_nanoseconds)?,
            time_zone,
            calendar,
        })
    }

    pub fn from_instant(
        instant: Instant,
        time_zone: TimeZoneId,
        calendar: CalendarId,
    ) -> Self {
        Self { instant, time_zone, calendar }
    }

    pub fn from_local(
        providers: &TemporalProviders<'_>,
        local: PlainDateTime,
        time_zone: TimeZoneId,
        calendar: CalendarId,
        supplied_offset: Option<UtcOffsetNanoseconds>,
        options: ResolveOptions,
    ) -> TemporalResult<Self> {
        let instant = resolve_local(
            providers.time_zone,
            &time_zone,
            local,
            supplied_offset,
            options,
        )?;
        Ok(Self::from_instant(instant, time_zone, calendar))
    }

    pub fn epoch_nanoseconds(&self) -> i128 {
        self.instant.epoch_nanoseconds()
    }

    pub fn epoch_milliseconds(&self) -> i64 {
        self.instant.epoch_milliseconds()
    }

    pub fn time_zone_id(&self) -> &TimeZoneId {
        &self.time_zone
    }

    pub fn calendar_id(&self) -> &CalendarId {
        &self.calendar
    }

    pub fn to_instant(&self) -> Instant {
        self.instant
    }

    pub fn fields(&self, providers: &TemporalProviders<'_>) -> TemporalResult<ZonedFields> {
        let offset = providers
            .time_zone
            .offset_nanoseconds_for(&self.time_zone, self.instant)?;
        let date_time = plain_date_time_for(self.instant, offset)?;
        let calendar = providers.calendar.fields(&self.calendar, date_time.date)?;
        Ok(ZonedFields { date_time, calendar, offset })
    }

    pub fn to_plain_date_time(
        &self,
        time_zone: &dyn TimeZoneProvider,
    ) -> TemporalResult<PlainDateTime> {
        local_date_time(time_zone, &self.time_zone, self.instant)
    }

    pub fn to_plain_date(&self, time_zone: &dyn TimeZoneProvider) -> TemporalResult<PlainDate> {
        Ok(self.to_plain_date_time(time_zone)?.date)
    }

    pub fn to_plain_time(&self, time_zone: &dyn TimeZoneProvider) -> TemporalResult<PlainTime> {
        Ok(self.to_plain_date_time(time_zone)?.time)
    }

    pub fn with(
        &self,
        providers: &TemporalProviders<'_>,
        replacements: ZonedDateTimeFields,
        options: WithOptions,
    ) -> TemporalResult<Self> {
        if !replacements.has_any() {
            return Err(TemporalError::type_error(
                "ZonedDateTime.with requires at least one field",
            ));
        }
        let current_offset = providers
            .time_zone
            .offset_nanoseconds_for(&self.time_zone, self.instant)?;
        let current = plain_date_time_for(self.instant, current_offset)?;
        let date = providers.calendar.date_from_fields(
            &self.calendar,
            current.date,
            replacements.year,
            replacements.month,
            replacements.day,
            options.overflow,
        )?;
        let time = PlainTime::new(
            replacements.hour.unwrap_or(i64::from(current.time.hour)),
            replacements.minute.unwrap_or(i64::from(current.time.minute)),
            replacements.second.unwrap_or(i64::from(current.time.second)),
            replacements
                .millisecond
                .unwrap_or(i64::from(current.time.millisecond)),
            replacements
                .microsecond
                .unwrap_or(i64::from(current.time.microsecond)),
            replacements
                .nanosecond
                .unwrap_or(i64::from(current.time.nanosecond)),
            options.overflow,
        )?;
        let local = PlainDateTime::from_parts(date, time)?;
        let instant = resolve_local(
            providers.time_zone,
            &self.time_zone,
            local,
            Some(replacements.offset.unwrap_or(current_offset)),
            ResolveOptions {
                disambiguation: options.disambiguation,
                offset: options.offset,
                offset_match: OffsetMatch::Exactly,
            },
        )?;
        Ok(Self::from_instant(
            instant,
            self.time_zone.clone(),
            self.calendar.clone(),
        ))
    }

    pub fn with_time_zone(&self, time_zone: TimeZoneId) -> Self {
        Self::from_instant(self.instant, time_zone, self.calendar.clone())
    }

    pub fn with_calendar(&self, calendar: CalendarId) -> Self {
        Self::from_instant(self.instant, self.time_zone.clone(), calendar)
    }

    pub fn add(
        &self,
        providers: &TemporalProviders<'_>,
        duration: &Duration,
        options: ArithmeticOptions,
    ) -> TemporalResult<Self> {
        if duration.years == 0
            && duration.months == 0
            && duration.weeks == 0
            && duration.days == 0
        {
            let instant = self.instant.add(duration)?;
            return Ok(Self::from_instant(
                instant,
                self.time_zone.clone(),
                self.calendar.clone(),
            ));
        }

        let local = self.to_plain_date_time(providers.time_zone)?;
        let date_duration = Duration::new(
            duration.years,
            duration.months,
            duration.weeks,
            duration.days,
            0,
            0,
            0,
            0,
            0,
            0,
        )?;
        let added_date = providers.calendar.date_add(
            &self.calendar,
            local.date,
            &date_duration,
            options.overflow,
        )?;
        let intermediate_local = PlainDateTime::from_parts(added_date, local.time)?;
        let intermediate = resolve_local(
            providers.time_zone,
            &self.time_zone,
            intermediate_local,
            None,
            ResolveOptions {
                disambiguation: Disambiguation::Compatible,
                offset: OffsetOption::Ignore,
                offset_match: OffsetMatch::Exactly,
            },
        )?;
        let time_duration = Duration::from_time_nanoseconds(
            duration.time_total_nanoseconds(),
            Unit::Hour,
        )?;
        let instant = intermediate.add(&time_duration)?;
        Ok(Self::from_instant(
            instant,
            self.time_zone.clone(),
            self.calendar.clone(),
        ))
    }

    pub fn subtract(
        &self,
        providers: &TemporalProviders<'_>,
        duration: &Duration,
        options: ArithmeticOptions,
    ) -> TemporalResult<Self> {
        self.add(providers, &duration.negated(), options)
    }

    pub fn until(
        &self,
        providers: &TemporalProviders<'_>,
        other: &Self,
        options: DifferenceOptions,
    ) -> TemporalResult<Duration> {
        options.validate()?;
        if self.calendar != other.calendar {
            return Err(TemporalError::range(
                "ZonedDateTime difference requires equal calendars",
            ));
        }
        if options.largest_unit.category() == UnitCategory::Time {
            return self.instant.until(
                other.instant,
                options.largest_unit,
                options.smallest_unit,
                options.rounding_increment,
                options.rounding_mode,
            );
        }
        if self.time_zone != other.time_zone {
            return Err(TemporalError::range(
                "date-unit ZonedDateTime difference requires equal time zones",
            ));
        }

        let unrounded = difference_zoned_date_units(providers, self, other, options.largest_unit)?;
        round_zoned_difference(providers, self, other, unrounded, options)
    }

    pub fn since(
        &self,
        providers: &TemporalProviders<'_>,
        other: &Self,
        options: DifferenceOptions,
    ) -> TemporalResult<Duration> {
        other.until(providers, self, options)
    }

    pub fn round(
        &self,
        providers: &TemporalProviders<'_>,
        options: RoundOptions,
    ) -> TemporalResult<Self> {
        if options.smallest_unit < Unit::Day {
            return Err(TemporalError::range(
                "ZonedDateTime.round smallestUnit must be day..nanosecond",
            ));
        }
        if options.rounding_increment < 1 {
            return Err(TemporalError::range("roundingIncrement must be >= 1"));
        }

        if options.smallest_unit == Unit::Day {
            if options.rounding_increment != 1 {
                return Err(TemporalError::range(
                    "roundingIncrement must be 1 for day rounding",
                ));
            }
            return self.round_to_day(providers, options.rounding_mode);
        }

        validate_rounding_increment(
            options.smallest_unit,
            options.rounding_increment,
            false,
        )?;
        let current_offset = providers
            .time_zone
            .offset_nanoseconds_for(&self.time_zone, self.instant)?;
        let local = plain_date_time_for(self.instant, current_offset)?;
        let rounded_local = local.round(
            options.smallest_unit,
            options.rounding_increment,
            options.rounding_mode,
        )?;
        let instant = resolve_local(
            providers.time_zone,
            &self.time_zone,
            rounded_local,
            Some(current_offset),
            ResolveOptions {
                disambiguation: Disambiguation::Compatible,
                offset: OffsetOption::Prefer,
                offset_match: OffsetMatch::Exactly,
            },
        )?;
        Ok(Self::from_instant(
            instant,
            self.time_zone.clone(),
            self.calendar.clone(),
        ))
    }

    fn round_to_day(
        &self,
        providers: &TemporalProviders<'_>,
        mode: RoundingMode,
    ) -> TemporalResult<Self> {
        let local = self.to_plain_date_time(providers.time_zone)?;
        let start = start_of_day(providers.time_zone, &self.time_zone, local.date)?;
        let one_day = Duration::new(0, 0, 0, 1, 0, 0, 0, 0, 0, 0)?;
        let tomorrow = providers.calendar.date_add(
            &self.calendar,
            local.date,
            &one_day,
            Overflow::Constrain,
        )?;
        let end = start_of_day(providers.time_zone, &self.time_zone, tomorrow)?;
        let progress = self.instant.epoch_nanoseconds() - start.epoch_nanoseconds();
        let length = end.epoch_nanoseconds() - start.epoch_nanoseconds();
        if length <= 0 || !(0..=length).contains(&progress) {
            return Err(TemporalError::range(
                "provider returned a non-positive or inconsistent calendar day",
            ));
        }
        let choose_end = choose_upper_boundary(progress, length, 0, mode);
        Ok(Self::from_instant(
            if choose_end { end } else { start },
            self.time_zone.clone(),
            self.calendar.clone(),
        ))
    }

    pub fn start_of_day(
        &self,
        providers: &TemporalProviders<'_>,
    ) -> TemporalResult<Self> {
        let date = self.to_plain_date(providers.time_zone)?;
        let instant = start_of_day(providers.time_zone, &self.time_zone, date)?;
        Ok(Self::from_instant(
            instant,
            self.time_zone.clone(),
            self.calendar.clone(),
        ))
    }

    pub fn day_length_nanoseconds(
        &self,
        providers: &TemporalProviders<'_>,
    ) -> TemporalResult<i128> {
        let local = self.to_plain_date_time(providers.time_zone)?;
        let start = start_of_day(providers.time_zone, &self.time_zone, local.date)?;
        let one_day = Duration::new(0, 0, 0, 1, 0, 0, 0, 0, 0, 0)?;
        let tomorrow = providers.calendar.date_add(
            &self.calendar,
            local.date,
            &one_day,
            Overflow::Constrain,
        )?;
        let end = start_of_day(providers.time_zone, &self.time_zone, tomorrow)?;
        let length = end.epoch_nanoseconds() - start.epoch_nanoseconds();
        if length <= 0 {
            return Err(TemporalError::range("provider returned a non-positive calendar day"));
        }
        Ok(length)
    }

    pub fn get_time_zone_transition(
        &self,
        provider: &dyn TimeZoneProvider,
        direction: TransitionDirection,
    ) -> TemporalResult<Option<Self>> {
        let Some(transition) = provider.transition(&self.time_zone, self.instant, direction)? else {
            return Ok(None);
        };
        let valid_direction = match direction {
            TransitionDirection::Next => transition > self.instant,
            TransitionDirection::Previous => transition < self.instant,
        };
        if !valid_direction {
            return Err(TemporalError::range(
                "time-zone provider returned a non-strict transition",
            ));
        }
        if transition.epoch_nanoseconds() > INSTANT_NS_MIN {
            let before = Instant::from_epoch_nanoseconds(transition.epoch_nanoseconds() - 1)?;
            let before_offset = provider.offset_nanoseconds_for(&self.time_zone, before)?;
            let after_offset = provider.offset_nanoseconds_for(&self.time_zone, transition)?;
            if before_offset == after_offset {
                return Err(TemporalError::range(
                    "time-zone provider transition does not change the offset",
                ));
            }
        }
        Ok(Some(Self::from_instant(
            transition,
            self.time_zone.clone(),
            self.calendar.clone(),
        )))
    }

    pub fn equals(&self, other: &Self) -> bool {
        self.instant == other.instant
            && self.time_zone == other.time_zone
            && self.calendar == other.calendar
    }

    pub fn compare(one: &Self, two: &Self) -> std::cmp::Ordering {
        one.instant.cmp(&two.instant)
    }

    pub fn format(
        &self,
        providers: &TemporalProviders<'_>,
        options: ToStringOptions,
    ) -> TemporalResult<String> {
        let rounded = if let Some(round) = options.round {
            self.round(providers, round)?
        } else {
            self.clone()
        };
        let fields = rounded.fields(providers)?;
        let mut result = format_iso_date(
            fields.date_time.date.year,
            fields.date_time.date.month,
            fields.date_time.date.day,
        );
        result.push('T');
        result.push_str(&format_time_ns(
            fields.date_time.time.to_nanoseconds_of_day(),
            options.precision,
        ));
        if options.show_offset == ShowOffset::Auto {
            result.push_str(&format_utc_offset(fields.offset));
        }
        match options.show_time_zone {
            ShowAnnotation::Never => {}
            ShowAnnotation::Critical => {
                result.push_str("[!");
                result.push_str(rounded.time_zone.as_str());
                result.push(']');
            }
            ShowAnnotation::Auto | ShowAnnotation::Always => {
                result.push('[');
                result.push_str(rounded.time_zone.as_str());
                result.push(']');
            }
        }
        let show_calendar = match options.show_calendar {
            ShowAnnotation::Never => false,
            ShowAnnotation::Auto => !rounded.calendar.is_iso8601(),
            ShowAnnotation::Always | ShowAnnotation::Critical => true,
        };
        if show_calendar {
            result.push('[');
            if options.show_calendar == ShowAnnotation::Critical {
                result.push('!');
            }
            result.push_str("u-ca=");
            result.push_str(rounded.calendar.as_str());
            result.push(']');
        }
        Ok(result)
    }

    pub fn to_json(&self, providers: &TemporalProviders<'_>) -> TemporalResult<String> {
        self.format(providers, ToStringOptions::default())
    }

    pub fn to_json_with<A: ZonedDateTimeJsonAdapter>(
        &self,
        providers: &TemporalProviders<'_>,
        adapter: &A,
    ) -> TemporalResult<A::Output> {
        adapter.from_temporal_string(self.to_json(providers)?)
    }

    pub fn to_locale_string(
        &self,
        providers: &TemporalProviders<'_>,
        adapter: &dyn ZonedDateTimeLocaleAdapter,
        locales: &[&str],
        options: &LocaleFormatOptions,
    ) -> TemporalResult<String> {
        let fields = self.fields(providers)?;
        adapter.format(self, &fields, locales, options)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShowAnnotation {
    #[default]
    Auto,
    Always,
    Never,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShowOffset {
    #[default]
    Auto,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToStringOptions {
    pub precision: Precision,
    pub show_calendar: ShowAnnotation,
    pub show_time_zone: ShowAnnotation,
    pub show_offset: ShowOffset,
    pub round: Option<RoundOptions>,
}

impl Default for ToStringOptions {
    fn default() -> Self {
        Self {
            precision: Precision::Auto,
            show_calendar: ShowAnnotation::Auto,
            show_time_zone: ShowAnnotation::Auto,
            show_offset: ShowOffset::Auto,
            round: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocaleFormatOptions {
    pub calendar: Option<CalendarId>,
    pub numbering_system: Option<Arc<str>>,
    pub date_style: Option<Arc<str>>,
    pub time_style: Option<Arc<str>>,
}

/// ECMA-402 adapter boundary. The core resolves fields exactly once before
/// calling the adapter, so locale data cannot change Temporal arithmetic.
pub trait ZonedDateTimeLocaleAdapter {
    fn format(
        &self,
        value: &ZonedDateTime,
        fields: &ZonedFields,
        locales: &[&str],
        options: &LocaleFormatOptions,
    ) -> TemporalResult<String>;
}

/// Runtime JSON adapter boundary for converting the normative Temporal string
/// into the host runtime's string/value representation.
pub trait ZonedDateTimeJsonAdapter {
    type Output;

    fn from_temporal_string(&self, value: String) -> TemporalResult<Self::Output>;
}

fn local_epoch_nanoseconds(local: PlainDateTime) -> i128 {
    i128::from(epoch_days_from_ymd(
        local.date.year,
        local.date.month,
        local.date.day,
    )) * NS_PER_DAY
        + local.time.to_nanoseconds_of_day()
}

fn plain_date_time_from_local_epoch(nanoseconds: i128) -> TemporalResult<PlainDateTime> {
    let days = nanoseconds.div_euclid(NS_PER_DAY);
    let days = i64::try_from(days)
        .map_err(|_| TemporalError::range("local date-time day count overflow"))?;
    let time_ns = nanoseconds.rem_euclid(NS_PER_DAY);
    let (year, month, day) = ymd_from_epoch_days(days);
    let date = PlainDate::new(year, i64::from(month), i64::from(day), Overflow::Reject)?;
    let hour = time_ns / NS_PER_HOUR;
    let minute = time_ns / NS_PER_MINUTE % 60;
    let second = time_ns / NS_PER_SECOND % 60;
    let fraction = time_ns % NS_PER_SECOND;
    let time = PlainTime::new(
        hour as i64,
        minute as i64,
        second as i64,
        (fraction / NS_PER_MILLISECOND) as i64,
        (fraction % NS_PER_MILLISECOND / NS_PER_MICROSECOND) as i64,
        (fraction % NS_PER_MICROSECOND) as i64,
        Overflow::Reject,
    )?;
    PlainDateTime::from_parts(date, time)
}

fn plain_date_time_for(
    instant: Instant,
    offset: UtcOffsetNanoseconds,
) -> TemporalResult<PlainDateTime> {
    let local_ns = instant
        .epoch_nanoseconds()
        .checked_add(i128::from(offset.get()))
        .ok_or_else(|| TemporalError::range("local date-time arithmetic overflow"))?;
    plain_date_time_from_local_epoch(local_ns)
}

fn local_date_time(
    provider: &dyn TimeZoneProvider,
    time_zone: &TimeZoneId,
    instant: Instant,
) -> TemporalResult<PlainDateTime> {
    let offset = provider.offset_nanoseconds_for(time_zone, instant)?;
    plain_date_time_for(instant, offset)
}

fn possible_instants_checked(
    provider: &dyn TimeZoneProvider,
    time_zone: &TimeZoneId,
    local: PlainDateTime,
) -> TemporalResult<Vec<Instant>> {
    let possible = provider.possible_instants_for(time_zone, local)?;
    for pair in possible.windows(2) {
        if pair[0] >= pair[1] {
            return Err(TemporalError::range(
                "time-zone provider possible instants must be strictly increasing",
            ));
        }
    }
    for &candidate in &possible {
        let projected = local_date_time(provider, time_zone, candidate)?;
        if projected != local {
            return Err(TemporalError::range(
                "time-zone provider returned an instant for a different local date-time",
            ));
        }
    }
    Ok(possible)
}

fn resolve_local(
    provider: &dyn TimeZoneProvider,
    time_zone: &TimeZoneId,
    local: PlainDateTime,
    supplied_offset: Option<UtcOffsetNanoseconds>,
    options: ResolveOptions,
) -> TemporalResult<Instant> {
    if let Some(offset) = supplied_offset {
        if options.offset == OffsetOption::Use {
            return Instant::from_epoch_nanoseconds(
                local_epoch_nanoseconds(local) - i128::from(offset.get()),
            );
        }
    }

    let possible = possible_instants_checked(provider, time_zone, local)?;
    if let Some(supplied) = supplied_offset
        && options.offset != OffsetOption::Ignore
    {
        let naive = local_epoch_nanoseconds(local);
        for &candidate in &possible {
            let candidate_offset = naive - candidate.epoch_nanoseconds();
            let matches = match options.offset_match {
                OffsetMatch::Exactly => candidate_offset == i128::from(supplied.get()),
                OffsetMatch::Minutes => {
                    round_to_increment(
                        candidate_offset,
                        NS_PER_MINUTE,
                        RoundingMode::HalfExpand,
                    )? == i128::from(supplied.get())
                }
            };
            if matches {
                return Ok(candidate);
            }
        }
        if options.offset == OffsetOption::Reject {
            return Err(TemporalError::range(
                "supplied offset does not match the time zone at this local date-time",
            ));
        }
    }
    disambiguate_possible(provider, time_zone, local, possible, options.disambiguation)
}

fn disambiguate_possible(
    provider: &dyn TimeZoneProvider,
    time_zone: &TimeZoneId,
    local: PlainDateTime,
    possible: Vec<Instant>,
    disambiguation: Disambiguation,
) -> TemporalResult<Instant> {
    match possible.len() {
        1 => return Ok(possible[0]),
        n if n > 1 => {
            return match disambiguation {
                Disambiguation::Compatible | Disambiguation::Earlier => Ok(possible[0]),
                Disambiguation::Later => Ok(possible[n - 1]),
                Disambiguation::Reject => Err(TemporalError::range(
                    "multiple instants correspond to this local date-time",
                )),
            };
        }
        _ => {}
    }
    if disambiguation == Disambiguation::Reject {
        return Err(TemporalError::range(
            "no instant corresponds to this local date-time",
        ));
    }

    // Temporal's gap rule compares the offsets on the nearest valid sides of
    // the gap. A one-day probe is sufficient under the protocol invariant that
    // an individual offset discontinuity is at most one day; candidate
    // validation below rejects inconsistent provider data.
    let naive = local_epoch_nanoseconds(local);
    let before_probe = Instant::from_epoch_nanoseconds(
        naive.saturating_sub(NS_PER_DAY).clamp(INSTANT_NS_MIN, INSTANT_NS_MAX),
    )?;
    let after_probe = Instant::from_epoch_nanoseconds(
        naive.saturating_add(NS_PER_DAY).clamp(INSTANT_NS_MIN, INSTANT_NS_MAX),
    )?;
    let offset_before = provider.offset_nanoseconds_for(time_zone, before_probe)?;
    let offset_after = provider.offset_nanoseconds_for(time_zone, after_probe)?;
    let gap = i128::from(offset_after.get()) - i128::from(offset_before.get());
    if gap == 0 || gap.abs() > NS_PER_DAY {
        return Err(TemporalError::range(
            "time-zone provider returned an inconsistent local-time gap",
        ));
    }
    let adjusted_ns = match disambiguation {
        Disambiguation::Earlier => naive - gap,
        Disambiguation::Compatible | Disambiguation::Later => naive + gap,
        Disambiguation::Reject => unreachable!(),
    };
    let adjusted = plain_date_time_from_local_epoch(adjusted_ns)?;
    let adjusted_possible = possible_instants_checked(provider, time_zone, adjusted)?;
    if adjusted_possible.is_empty() {
        return Err(TemporalError::range(
            "time-zone provider could not resolve a local-time gap",
        ));
    }
    match disambiguation {
        Disambiguation::Earlier => Ok(adjusted_possible[0]),
        Disambiguation::Compatible | Disambiguation::Later => {
            Ok(adjusted_possible[adjusted_possible.len() - 1])
        }
        Disambiguation::Reject => unreachable!(),
    }
}

fn start_of_day(
    provider: &dyn TimeZoneProvider,
    time_zone: &TimeZoneId,
    date: PlainDate,
) -> TemporalResult<Instant> {
    let midnight = PlainDateTime::from_parts(date, PlainTime::midnight())?;
    resolve_local(
        provider,
        time_zone,
        midnight,
        None,
        ResolveOptions {
            disambiguation: Disambiguation::Compatible,
            offset: OffsetOption::Ignore,
            offset_match: OffsetMatch::Exactly,
        },
    )
}

fn combine_duration(date: Duration, time_nanoseconds: i128) -> TemporalResult<Duration> {
    let time = Duration::from_time_nanoseconds(time_nanoseconds, Unit::Hour)?;
    Duration::new(
        date.years,
        date.months,
        date.weeks,
        date.days,
        time.hours,
        time.minutes,
        time.seconds,
        time.milliseconds,
        time.microseconds,
        time.nanoseconds,
    )
}

fn difference_zoned_date_units(
    providers: &TemporalProviders<'_>,
    start: &ZonedDateTime,
    end: &ZonedDateTime,
    largest_unit: Unit,
) -> TemporalResult<Duration> {
    if start.instant == end.instant {
        return Ok(Duration::default());
    }
    if end.instant < start.instant {
        return Ok(difference_zoned_date_units(providers, end, start, largest_unit)?.negated());
    }

    let start_local = start.to_plain_date_time(providers.time_zone)?;
    let end_local = end.to_plain_date_time(providers.time_zone)?;
    if start_local.date == end_local.date {
        return Duration::from_time_nanoseconds(
            end.instant.epoch_nanoseconds() - start.instant.epoch_nanoseconds(),
            Unit::Hour,
        );
    }

    let wall_time_difference = end_local.time.to_nanoseconds_of_day()
        - start_local.time.to_nanoseconds_of_day();
    let mut day_correction = if wall_time_difference < 0 { 1 } else { 0 };
    while day_correction <= 2 {
        let correction = Duration::new(0, 0, 0, -day_correction, 0, 0, 0, 0, 0, 0)?;
        let intermediate_date = providers.calendar.date_add(
            &start.calendar,
            end_local.date,
            &correction,
            Overflow::Constrain,
        )?;
        let intermediate_local = PlainDateTime::from_parts(intermediate_date, start_local.time)?;
        let intermediate = resolve_local(
            providers.time_zone,
            &start.time_zone,
            intermediate_local,
            None,
            ResolveOptions {
                disambiguation: Disambiguation::Compatible,
                offset: OffsetOption::Ignore,
                offset_match: OffsetMatch::Exactly,
            },
        )?;
        let time_nanoseconds = end.instant.epoch_nanoseconds() - intermediate.epoch_nanoseconds();
        if time_nanoseconds >= 0 {
            let date = providers.calendar.date_until(
                &start.calendar,
                start_local.date,
                intermediate_date,
                largest_unit.max(Unit::Day),
            )?;
            return combine_duration(date, time_nanoseconds);
        }
        day_correction += 1;
    }
    Err(TemporalError::range(
        "time-zone provider prevented ZonedDateTime difference convergence",
    ))
}

fn round_zoned_difference(
    providers: &TemporalProviders<'_>,
    start: &ZonedDateTime,
    end: &ZonedDateTime,
    unrounded: Duration,
    options: DifferenceOptions,
) -> TemporalResult<Duration> {
    if options.smallest_unit == Unit::Nanosecond && options.rounding_increment == 1 {
        return Ok(unrounded);
    }
    if options.smallest_unit.category() == UnitCategory::Time {
        let step = options
            .smallest_unit
            .ns_per()
            .and_then(|unit| unit.checked_mul(options.rounding_increment))
            .ok_or_else(|| TemporalError::range("difference rounding increment overflow"))?;
        let rounded_time = round_to_increment(
            unrounded.time_total_nanoseconds(),
            step,
            options.rounding_mode,
        )?;
        let candidate_duration = combine_duration(unrounded, rounded_time)?;
        let rounded_end = start.add(
            providers,
            &candidate_duration,
            ArithmeticOptions::default(),
        )?;
        return difference_zoned_date_units(
            providers,
            start,
            &rounded_end,
            options.largest_unit,
        );
    }

    let sign = if end.instant > start.instant { 1 } else { -1 };
    let whole = match options.smallest_unit {
        Unit::Year => unrounded.years,
        Unit::Month => unrounded.years * 12 + unrounded.months,
        Unit::Week => unrounded.weeks,
        Unit::Day => unrounded.days,
        _ => unreachable!(),
    };
    let increment = i64::try_from(options.rounding_increment)
        .map_err(|_| TemporalError::range("calendar rounding increment overflow"))?;
    let toward_count = whole / increment * increment;
    let away_count = toward_count
        .checked_add(sign * increment)
        .ok_or_else(|| TemporalError::range("calendar rounding overflow"))?;
    let toward_duration = duration_from_unit_count(options.smallest_unit, toward_count)?;
    let away_duration = duration_from_unit_count(options.smallest_unit, away_count)?;
    let toward = start.add(providers, &toward_duration, ArithmeticOptions::default())?;
    let away = start.add(providers, &away_duration, ArithmeticOptions::default())?;
    let toward_distance = (end.epoch_nanoseconds() - toward.epoch_nanoseconds()).abs();
    let away_distance = (away.epoch_nanoseconds() - end.epoch_nanoseconds()).abs();
    let choose_away = choose_away_boundary(
        toward_distance,
        away_distance,
        i128::from(toward_count / increment),
        sign,
        options.rounding_mode,
    );
    difference_zoned_date_units(
        providers,
        start,
        if choose_away { &away } else { &toward },
        options.largest_unit,
    )
}

fn duration_from_unit_count(unit: Unit, count: i64) -> TemporalResult<Duration> {
    match unit {
        Unit::Year => Duration::new(count, 0, 0, 0, 0, 0, 0, 0, 0, 0),
        Unit::Month => Duration::new(0, count, 0, 0, 0, 0, 0, 0, 0, 0),
        Unit::Week => Duration::new(0, 0, count, 0, 0, 0, 0, 0, 0, 0),
        Unit::Day => Duration::new(0, 0, 0, count, 0, 0, 0, 0, 0, 0),
        _ => Err(TemporalError::range("calendar unit required")),
    }
}

fn choose_upper_boundary(
    progress: i128,
    length: i128,
    lower_quotient: i128,
    mode: RoundingMode,
) -> bool {
    choose_away_boundary(progress, length - progress, lower_quotient, 1, mode)
}

fn choose_away_boundary(
    toward_distance: i128,
    away_distance: i128,
    toward_quotient: i128,
    sign: i64,
    mode: RoundingMode,
) -> bool {
    match mode {
        RoundingMode::Trunc => false,
        RoundingMode::Expand => true,
        RoundingMode::Ceil => sign > 0,
        RoundingMode::Floor => sign < 0,
        RoundingMode::HalfExpand
        | RoundingMode::HalfTrunc
        | RoundingMode::HalfCeil
        | RoundingMode::HalfFloor
        | RoundingMode::HalfEven => match toward_distance.cmp(&away_distance) {
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => match mode {
                RoundingMode::HalfExpand => true,
                RoundingMode::HalfTrunc => false,
                RoundingMode::HalfCeil => sign > 0,
                RoundingMode::HalfFloor => sign < 0,
                RoundingMode::HalfEven => toward_quotient.rem_euclid(2) != 0,
                _ => unreachable!(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use super::super::instant_duration::TemporalErrorKind;

    fn local(
        year: i32,
        month: i64,
        day: i64,
        hour: i64,
        minute: i64,
    ) -> PlainDateTime {
        PlainDateTime::new(
            year,
            month,
            day,
            hour,
            minute,
            0,
            0,
            0,
            0,
            Overflow::Reject,
        )
        .unwrap()
    }

    fn epoch(year: i32, month: u8, day: u8, hour: i128, minute: i128) -> i128 {
        i128::from(epoch_days_from_ymd(year, month, day)) * NS_PER_DAY
            + hour * NS_PER_HOUR
            + minute * NS_PER_MINUTE
    }

    const WINTER_OFFSET: i64 = -(5 * NS_PER_HOUR) as i64;
    const SUMMER_OFFSET: i64 = -(4 * NS_PER_HOUR) as i64;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TimeZoneCall {
        Possible(PlainDateTime),
        Offset(i128),
        Transition(i128, TransitionDirection),
    }

    struct RecordingEastern {
        calls: RefCell<Vec<TimeZoneCall>>,
    }

    impl RecordingEastern {
        fn new() -> Self {
            Self { calls: RefCell::new(Vec::new()) }
        }

        fn spring() -> i128 {
            epoch(2024, 3, 10, 7, 0)
        }

        fn autumn() -> i128 {
            epoch(2024, 11, 3, 6, 0)
        }

        fn raw_offset(epoch_ns: i128) -> i64 {
            if (Self::spring()..Self::autumn()).contains(&epoch_ns) {
                SUMMER_OFFSET
            } else {
                WINTER_OFFSET
            }
        }
    }

    impl TimeZoneProvider for RecordingEastern {
        fn possible_instants_for(
            &self,
            _time_zone: &TimeZoneId,
            local: PlainDateTime,
        ) -> TemporalResult<Vec<Instant>> {
            self.calls.borrow_mut().push(TimeZoneCall::Possible(local));
            let naive = local_epoch_nanoseconds(local);
            let mut result = Vec::with_capacity(2);
            for offset in [WINTER_OFFSET, SUMMER_OFFSET] {
                let candidate_ns = naive - i128::from(offset);
                if Self::raw_offset(candidate_ns) == offset {
                    let candidate = Instant::from_epoch_nanoseconds(candidate_ns)?;
                    if plain_date_time_for(
                        candidate,
                        UtcOffsetNanoseconds::new(offset)?,
                    )? == local
                    {
                        result.push(candidate);
                    }
                }
            }
            result.sort_unstable();
            result.dedup();
            Ok(result)
        }

        fn offset_nanoseconds_for(
            &self,
            _time_zone: &TimeZoneId,
            instant: Instant,
        ) -> TemporalResult<UtcOffsetNanoseconds> {
            self.calls
                .borrow_mut()
                .push(TimeZoneCall::Offset(instant.epoch_nanoseconds()));
            UtcOffsetNanoseconds::new(Self::raw_offset(instant.epoch_nanoseconds()))
        }

        fn transition(
            &self,
            _time_zone: &TimeZoneId,
            instant: Instant,
            direction: TransitionDirection,
        ) -> TemporalResult<Option<Instant>> {
            self.calls.borrow_mut().push(TimeZoneCall::Transition(
                instant.epoch_nanoseconds(),
                direction,
            ));
            let transitions = [Self::spring(), Self::autumn()];
            let found = match direction {
                TransitionDirection::Next => transitions
                    .into_iter()
                    .find(|&candidate| candidate > instant.epoch_nanoseconds()),
                TransitionDirection::Previous => transitions
                    .into_iter()
                    .rev()
                    .find(|&candidate| candidate < instant.epoch_nanoseconds()),
            };
            found.map(Instant::from_epoch_nanoseconds).transpose()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum CalendarCall {
        Fields(PlainDate),
        FromFields(PlainDate),
        Add(PlainDate, [i64; 4]),
        Until(PlainDate, PlainDate, Unit),
    }

    struct RecordingCalendar {
        calls: RefCell<Vec<CalendarCall>>,
    }

    impl RecordingCalendar {
        fn new() -> Self {
            Self { calls: RefCell::new(Vec::new()) }
        }
    }

    impl CalendarProvider for RecordingCalendar {
        fn fields(
            &self,
            calendar: &CalendarId,
            iso_date: PlainDate,
        ) -> TemporalResult<CalendarDateFields> {
            self.calls.borrow_mut().push(CalendarCall::Fields(iso_date));
            IsoCalendarProvider.fields(calendar, iso_date)
        }

        fn date_from_fields(
            &self,
            calendar: &CalendarId,
            current: PlainDate,
            year: Option<i32>,
            month: Option<i64>,
            day: Option<i64>,
            overflow: Overflow,
        ) -> TemporalResult<PlainDate> {
            self.calls.borrow_mut().push(CalendarCall::FromFields(current));
            IsoCalendarProvider.date_from_fields(
                calendar, current, year, month, day, overflow,
            )
        }

        fn date_add(
            &self,
            calendar: &CalendarId,
            iso_date: PlainDate,
            duration: &Duration,
            overflow: Overflow,
        ) -> TemporalResult<PlainDate> {
            self.calls.borrow_mut().push(CalendarCall::Add(
                iso_date,
                [duration.years, duration.months, duration.weeks, duration.days],
            ));
            IsoCalendarProvider.date_add(calendar, iso_date, duration, overflow)
        }

        fn date_until(
            &self,
            calendar: &CalendarId,
            one: PlainDate,
            two: PlainDate,
            largest_unit: Unit,
        ) -> TemporalResult<Duration> {
            self.calls
                .borrow_mut()
                .push(CalendarCall::Until(one, two, largest_unit));
            IsoCalendarProvider.date_until(calendar, one, two, largest_unit)
        }
    }

    fn zone() -> TimeZoneId {
        TimeZoneId::new("Test/Eastern").unwrap()
    }

    fn calendar() -> CalendarId {
        CalendarId::iso8601()
    }

    fn providers<'a>(
        time_zone: &'a RecordingEastern,
        calendar: &'a RecordingCalendar,
    ) -> TemporalProviders<'a> {
        TemporalProviders::new(time_zone, calendar)
    }

    fn from_local(
        providers: &TemporalProviders<'_>,
        local: PlainDateTime,
        disambiguation: Disambiguation,
    ) -> TemporalResult<ZonedDateTime> {
        ZonedDateTime::from_local(
            providers,
            local,
            zone(),
            calendar(),
            None,
            ResolveOptions {
                disambiguation,
                offset: OffsetOption::Ignore,
                offset_match: OffsetMatch::Exactly,
            },
        )
    }

    #[test]
    fn fold_and_gap_disambiguation_follow_temporal_rules() {
        let tz = RecordingEastern::new();
        let cal = RecordingCalendar::new();
        let providers = providers(&tz, &cal);

        let fold = local(2024, 11, 3, 1, 30);
        assert_eq!(
            from_local(&providers, fold, Disambiguation::Compatible)
                .unwrap()
                .epoch_nanoseconds(),
            epoch(2024, 11, 3, 5, 30)
        );
        assert_eq!(
            from_local(&providers, fold, Disambiguation::Earlier)
                .unwrap()
                .epoch_nanoseconds(),
            epoch(2024, 11, 3, 5, 30)
        );
        assert_eq!(
            from_local(&providers, fold, Disambiguation::Later)
                .unwrap()
                .epoch_nanoseconds(),
            epoch(2024, 11, 3, 6, 30)
        );
        assert_eq!(
            from_local(&providers, fold, Disambiguation::Reject)
                .unwrap_err()
                .kind,
            TemporalErrorKind::Range
        );

        let gap = local(2024, 3, 10, 2, 30);
        assert_eq!(
            from_local(&providers, gap, Disambiguation::Earlier)
                .unwrap()
                .epoch_nanoseconds(),
            epoch(2024, 3, 10, 6, 30)
        );
        assert_eq!(
            from_local(&providers, gap, Disambiguation::Compatible)
                .unwrap()
                .epoch_nanoseconds(),
            epoch(2024, 3, 10, 7, 30)
        );
        assert_eq!(
            from_local(&providers, gap, Disambiguation::Later)
                .unwrap()
                .epoch_nanoseconds(),
            epoch(2024, 3, 10, 7, 30)
        );
        assert_eq!(
            from_local(&providers, gap, Disambiguation::Reject)
                .unwrap_err()
                .kind,
            TemporalErrorKind::Range
        );
    }

    #[test]
    fn offset_mismatch_modes_are_distinct() {
        let tz = RecordingEastern::new();
        let cal = RecordingCalendar::new();
        let providers = providers(&tz, &cal);
        let fold = local(2024, 11, 3, 1, 30);
        let winter = UtcOffsetNanoseconds::new(WINTER_OFFSET).unwrap();
        let mismatch = UtcOffsetNanoseconds::new(-(6 * NS_PER_HOUR) as i64).unwrap();

        let resolve = |offset, option| {
            ZonedDateTime::from_local(
                &providers,
                fold,
                zone(),
                calendar(),
                Some(offset),
                ResolveOptions {
                    disambiguation: Disambiguation::Compatible,
                    offset: option,
                    offset_match: OffsetMatch::Exactly,
                },
            )
        };
        assert_eq!(
            resolve(winter, OffsetOption::Prefer)
                .unwrap()
                .epoch_nanoseconds(),
            epoch(2024, 11, 3, 6, 30)
        );
        assert_eq!(
            resolve(mismatch, OffsetOption::Ignore)
                .unwrap()
                .epoch_nanoseconds(),
            epoch(2024, 11, 3, 5, 30)
        );
        assert_eq!(
            resolve(mismatch, OffsetOption::Prefer)
                .unwrap()
                .epoch_nanoseconds(),
            epoch(2024, 11, 3, 5, 30)
        );
        assert_eq!(
            resolve(mismatch, OffsetOption::Use)
                .unwrap()
                .epoch_nanoseconds(),
            epoch(2024, 11, 3, 7, 30)
        );
        assert_eq!(
            resolve(mismatch, OffsetOption::Reject)
                .unwrap_err()
                .kind,
            TemporalErrorKind::Range
        );
    }

    #[test]
    fn negative_epoch_fields_use_euclidean_decomposition() {
        let tz = RecordingEastern::new();
        let cal = RecordingCalendar::new();
        let providers = providers(&tz, &cal);
        let value = ZonedDateTime::new(-1, zone(), calendar()).unwrap();
        let fields = value.fields(&providers).unwrap();
        assert_eq!((fields.calendar.year, fields.calendar.month, fields.calendar.day), (1969, 12, 31));
        assert_eq!((fields.hour(), fields.minute(), fields.second()), (18, 59, 59));
        assert_eq!(fields.nanosecond(), 999);
        assert_eq!(value.epoch_milliseconds(), -1);
    }

    #[test]
    fn date_and_time_arithmetic_diverge_across_spring_transition() {
        let tz = RecordingEastern::new();
        let cal = RecordingCalendar::new();
        let providers = providers(&tz, &cal);
        let start = from_local(
            &providers,
            local(2024, 3, 9, 12, 0),
            Disambiguation::Compatible,
        )
        .unwrap();
        let one_day = Duration::new(0, 0, 0, 1, 0, 0, 0, 0, 0, 0).unwrap();
        let hours_24 = Duration::new(0, 0, 0, 0, 24, 0, 0, 0, 0, 0).unwrap();
        let by_date = start
            .add(&providers, &one_day, ArithmeticOptions::default())
            .unwrap();
        let by_time = start
            .add(&providers, &hours_24, ArithmeticOptions::default())
            .unwrap();
        assert_eq!(
            by_date.epoch_nanoseconds() - start.epoch_nanoseconds(),
            23 * NS_PER_HOUR
        );
        assert_eq!(
            by_time.epoch_nanoseconds() - start.epoch_nanoseconds(),
            24 * NS_PER_HOUR
        );
        assert_eq!(by_date.to_plain_date_time(&tz).unwrap(), local(2024, 3, 10, 12, 0));
        assert_eq!(by_time.to_plain_date_time(&tz).unwrap(), local(2024, 3, 10, 13, 0));
    }

    #[test]
    fn largest_unit_difference_observes_variable_day_length() {
        let tz = RecordingEastern::new();
        let cal = RecordingCalendar::new();
        let providers = providers(&tz, &cal);
        let start = from_local(
            &providers,
            local(2024, 3, 9, 12, 0),
            Disambiguation::Compatible,
        )
        .unwrap();
        let end = from_local(
            &providers,
            local(2024, 3, 10, 12, 0),
            Disambiguation::Compatible,
        )
        .unwrap();
        let days = start
            .until(
                &providers,
                &end,
                DifferenceOptions {
                    largest_unit: Unit::Day,
                    ..DifferenceOptions::default()
                },
            )
            .unwrap();
        let hours = start
            .until(
                &providers,
                &end,
                DifferenceOptions {
                    largest_unit: Unit::Hour,
                    ..DifferenceOptions::default()
                },
            )
            .unwrap();
        assert_eq!(days.days, 1);
        assert_eq!(days.time_total_nanoseconds(), 0);
        assert_eq!(hours.hours, 23);
        assert_eq!(end.since(&providers, &start, DifferenceOptions { largest_unit: Unit::Day, ..DifferenceOptions::default() }).unwrap(), days);
    }

    #[test]
    fn rounding_resolves_a_result_inside_a_gap() {
        let tz = RecordingEastern::new();
        let cal = RecordingCalendar::new();
        let providers = providers(&tz, &cal);
        let value = from_local(
            &providers,
            local(2024, 3, 10, 1, 31),
            Disambiguation::Compatible,
        )
        .unwrap();
        let rounded = value
            .round(
                &providers,
                RoundOptions {
                    smallest_unit: Unit::Hour,
                    rounding_increment: 1,
                    rounding_mode: RoundingMode::HalfExpand,
                },
            )
            .unwrap();
        assert_eq!(rounded.to_plain_date_time(&tz).unwrap(), local(2024, 3, 10, 3, 0));
        assert_eq!(
            value.day_length_nanoseconds(&providers).unwrap(),
            23 * NS_PER_HOUR
        );
    }

    #[test]
    fn transition_lookup_is_strict_and_returns_first_new_offset_nanosecond() {
        let tz = RecordingEastern::new();
        let value = ZonedDateTime::new(RecordingEastern::spring() - 1, zone(), calendar()).unwrap();
        let next = value
            .get_time_zone_transition(&tz, TransitionDirection::Next)
            .unwrap()
            .unwrap();
        assert_eq!(next.epoch_nanoseconds(), RecordingEastern::spring());
        let following = next
            .get_time_zone_transition(&tz, TransitionDirection::Next)
            .unwrap()
            .unwrap();
        assert_eq!(following.epoch_nanoseconds(), RecordingEastern::autumn());
        let previous = following
            .get_time_zone_transition(&tz, TransitionDirection::Previous)
            .unwrap()
            .unwrap();
        assert_eq!(previous.epoch_nanoseconds(), RecordingEastern::spring());
    }

    struct FailingProvider;

    impl TimeZoneProvider for FailingProvider {
        fn possible_instants_for(
            &self,
            _time_zone: &TimeZoneId,
            _local: PlainDateTime,
        ) -> TemporalResult<Vec<Instant>> {
            Err(TemporalError::type_error("provider boom"))
        }

        fn offset_nanoseconds_for(
            &self,
            _time_zone: &TimeZoneId,
            _instant: Instant,
        ) -> TemporalResult<UtcOffsetNanoseconds> {
            Err(TemporalError::type_error("provider boom"))
        }

        fn transition(
            &self,
            _time_zone: &TimeZoneId,
            _instant: Instant,
            _direction: TransitionDirection,
        ) -> TemporalResult<Option<Instant>> {
            Err(TemporalError::type_error("provider boom"))
        }
    }

    #[test]
    fn protocol_errors_propagate_without_reclassification() {
        let calendar = RecordingCalendar::new();
        let providers = TemporalProviders::new(&FailingProvider, &calendar);
        let error = ZonedDateTime::from_local(
            &providers,
            local(2024, 1, 1, 0, 0),
            zone(),
            calendar(),
            None,
            ResolveOptions::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind, TemporalErrorKind::Type);
        assert_eq!(error.message, "provider boom");
    }

    #[test]
    fn field_and_serialization_access_is_deterministic() {
        let tz = RecordingEastern::new();
        let cal = RecordingCalendar::new();
        let providers = providers(&tz, &cal);
        let value = ZonedDateTime::new(epoch(2024, 1, 2, 8, 4), zone(), calendar()).unwrap();
        tz.calls.borrow_mut().clear();
        cal.calls.borrow_mut().clear();
        let fields = value.fields(&providers).unwrap();
        assert_eq!(fields.date_time, local(2024, 1, 2, 3, 4));
        assert_eq!(
            *tz.calls.borrow(),
            vec![TimeZoneCall::Offset(value.epoch_nanoseconds())]
        );
        assert_eq!(
            *cal.calls.borrow(),
            vec![CalendarCall::Fields(fields.date_time.date)]
        );

        tz.calls.borrow_mut().clear();
        cal.calls.borrow_mut().clear();
        assert_eq!(
            value.to_json(&providers).unwrap(),
            "2024-01-02T03:04:00-05:00[Test/Eastern]"
        );
        assert_eq!(
            *tz.calls.borrow(),
            vec![TimeZoneCall::Offset(value.epoch_nanoseconds())]
        );
        assert_eq!(cal.calls.borrow().len(), 1);
    }
}
