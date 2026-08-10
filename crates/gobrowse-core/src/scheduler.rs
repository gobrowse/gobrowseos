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

    #[test]
    fn interval_is_strictly_after_reference_time() {
        let schedule = Schedule::Interval {
            seconds: 60,
            anchor: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(
            schedule.next_simple_run(OffsetDateTime::UNIX_EPOCH + Duration::seconds(60)),
            Some(OffsetDateTime::UNIX_EPOCH + Duration::seconds(120))
        );
    }
}
