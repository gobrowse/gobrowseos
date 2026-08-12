use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    Once {
        at: OffsetDateTime,
    },
    Interval {
        seconds: u64,
        anchor: OffsetDateTime,
    },
    Cron {
        expression: String,
        timezone: String,
    },
}

impl Schedule {
    /// Calculates simple one-time and interval schedules. Cron is evaluated by the server adapter.
    pub fn next_simple_run(&self, after: OffsetDateTime) -> Option<OffsetDateTime> {
        match self {
            Self::Once { at } => (*at > after).then_some(*at),
            Self::Interval { seconds, anchor } if *seconds > 0 => {
                if *anchor > after {
                    return Some(*anchor);
                }
                let elapsed = (after - *anchor).whole_seconds().max(0) as u64;
                let periods = elapsed / seconds + 1;
                let delta = i64::try_from(periods.checked_mul(*seconds)?).ok()?;
                anchor.checked_add(Duration::seconds(delta))
            }
            Self::Interval { .. } | Self::Cron { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    #[test]
    fn interval_is_strictly_after_reference_time() {
        let schedule = Schedule::Interval {
            seconds: 60,
            anchor: T,
        };
        assert_eq!(
            schedule.next_simple_run(T + Duration::seconds(60)),
            Some(T + Duration::seconds(120))
        );
    }

    // --- Once tests ---

    #[test]
    fn once_in_the_future_returns_some() {
        let schedule = Schedule::Once {
            at: T + Duration::seconds(60),
        };
        assert_eq!(schedule.next_simple_run(T), Some(T + Duration::seconds(60)));
    }

    #[test]
    fn once_in_the_past_returns_none() {
        let schedule = Schedule::Once {
            at: T - Duration::seconds(60),
        };
        assert_eq!(schedule.next_simple_run(T), None);
    }

    #[test]
    fn once_exactly_at_reference_returns_none() {
        // Strict > comparison: at == after → None.
        let schedule = Schedule::Once { at: T };
        assert_eq!(schedule.next_simple_run(T), None);
    }

    // --- Interval tests ---

    #[test]
    fn interval_returns_anchor_when_anchor_is_in_the_future() {
        let schedule = Schedule::Interval {
            seconds: 60,
            anchor: T + Duration::seconds(120),
        };
        assert_eq!(
            schedule.next_simple_run(T),
            Some(T + Duration::seconds(120))
        );
    }

    #[test]
    fn interval_with_zero_seconds_returns_none() {
        let schedule = Schedule::Interval {
            seconds: 0,
            anchor: T,
        };
        assert_eq!(schedule.next_simple_run(T), None);
    }

    #[test]
    fn interval_advances_to_next_period_strictly_after_reference() {
        // anchor = T, seconds = 60, after = T + 90s
        // elapsed = 90, periods = 90/60 + 1 = 2, delta = 120 → T + 120s.
        let schedule = Schedule::Interval {
            seconds: 60,
            anchor: T,
        };
        assert_eq!(
            schedule.next_simple_run(T + Duration::seconds(90)),
            Some(T + Duration::seconds(120))
        );
    }

    #[test]
    fn interval_overflow_returns_none() {
        // seconds = u64::MAX, after = anchor + 1s
        // elapsed = 1, periods = 1/u64::MAX + 1 = 1
        // checked_mul(1, u64::MAX) = Some(u64::MAX)
        // i64::try_from(u64::MAX) fails (u64::MAX > i64::MAX) → .ok()? → None.
        let schedule = Schedule::Interval {
            seconds: u64::MAX,
            anchor: T,
        };
        assert_eq!(schedule.next_simple_run(T + Duration::seconds(1)), None);
    }

    // --- Cron test ---

    #[test]
    fn cron_always_returns_none() {
        // Cron evaluation is delegated to the server adapter; next_simple_run
        // always returns None for Cron schedules.
        let schedule = Schedule::Cron {
            expression: "* * * * *".into(),
            timezone: "UTC".into(),
        };
        assert_eq!(schedule.next_simple_run(T), None);
        assert_eq!(schedule.next_simple_run(T + Duration::seconds(42)), None);
    }
}
