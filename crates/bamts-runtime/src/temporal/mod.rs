//! ECMA-402 Temporal primitives.

pub mod instant_duration;
pub mod now_rounding;
pub mod plain_types;
pub mod zoned_date_time;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum TemporalKind {
    Instant,
    Duration,
    PlainDate,
    PlainTime,
    PlainDateTime,
    PlainYearMonth,
    PlainMonthDay,
    ZonedDateTime,
}

impl TemporalKind {
    pub(crate) const COUNT: usize = 8;

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Debug)]
pub(crate) enum TemporalObject {
    Instant(instant_duration::Instant),
    Duration(instant_duration::Duration),
    PlainDate {
        value: plain_types::PlainDate,
        calendar: zoned_date_time::CalendarId,
    },
    PlainTime(plain_types::PlainTime),
    PlainDateTime {
        value: plain_types::PlainDateTime,
        calendar: zoned_date_time::CalendarId,
    },
    PlainYearMonth {
        value: plain_types::PlainYearMonth,
        calendar: zoned_date_time::CalendarId,
    },
    PlainMonthDay {
        value: plain_types::PlainMonthDay,
        calendar: zoned_date_time::CalendarId,
    },
    ZonedDateTime(zoned_date_time::ZonedDateTime),
}

impl TemporalObject {
    pub(crate) const fn kind(&self) -> TemporalKind {
        match self {
            Self::Instant(_) => TemporalKind::Instant,
            Self::Duration(_) => TemporalKind::Duration,
            Self::PlainDate { .. } => TemporalKind::PlainDate,
            Self::PlainTime(_) => TemporalKind::PlainTime,
            Self::PlainDateTime { .. } => TemporalKind::PlainDateTime,
            Self::PlainYearMonth { .. } => TemporalKind::PlainYearMonth,
            Self::PlainMonthDay { .. } => TemporalKind::PlainMonthDay,
            Self::ZonedDateTime(_) => TemporalKind::ZonedDateTime,
        }
    }
}
