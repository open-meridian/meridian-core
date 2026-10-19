//! When a held booking is due (contract v26, W11.34; the v26 plan's question
//! 230): the `operations` plugin's affirmation_deadline, minutes after local
//! midnight, on the booking's trade date in the deployment's time zone.
//!
//! The conductor answers both in one read, ReadDeploymentTimeZone: the zone
//! a deployment admin set (America/New_York until then) and the earliest
//! deadline a launched operations instance declares. The book reads it on
//! start and again every few minutes, and keeps it here for every command:
//! a decision never waits on the conductor. Until it has read one, or where
//! no instance declares a deadline, a held booking's alert names no moment
//! (`due_by` zero), as W11.34 reads it with none known.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::date::Date;
use meridian_domain::v1::{DeploymentTimeZoneSetting, ReadDeploymentTimeZoneRequest};
use meridian_domain::zone::Zone;
use prost::Message;

/// The conductor's read (W6.35).
pub const READ_DEPLOYMENT_TIME_ZONE: &str = "platform.config.query.read-deployment-time-zone";

/// How often the book reads it again.
pub const READ_EVERY: Duration = Duration::from_secs(300);

/// The deadline in force: the zone, and minutes after its local midnight.
#[derive(Debug, Clone, PartialEq)]
pub struct Affirmation {
    pub zone_name: String,
    pub zone: Arc<Zone>,
    pub minutes: u32,
}

impl Affirmation {
    /// The moment it falls on `trade_date`, in nanoseconds since the epoch;
    /// zero for a date that does not read.
    pub fn due_by(&self, trade_date: &str) -> i64 {
        Date::parse(trade_date)
            .map(|date| self.zone.instant_of(date, i64::from(self.minutes) * 60))
            .unwrap_or(0)
    }
}

static HELD: RwLock<Option<Affirmation>> = RwLock::new(None);

/// The deadline the book holds now, if any.
pub fn current() -> Option<Affirmation> {
    HELD.read().ok().and_then(|held| held.clone())
}

/// What a setting read makes of the deadline: none where no instance
/// declares one (0) or the minutes are out of range; the zone loaded from
/// the host's tz database, or why not.
pub fn of_setting(
    setting: &DeploymentTimeZoneSetting,
    load: impl Fn(&str) -> Result<Zone, String>,
) -> Result<Option<Affirmation>, String> {
    if setting.affirmation_deadline == 0 || setting.affirmation_deadline > 1440 {
        return Ok(None);
    }
    let name = setting.deployment_time_zone.trim();
    let zone = load(name)?;
    Ok(Some(Affirmation {
        zone_name: name.to_string(),
        zone: Arc::new(zone),
        minutes: setting.affirmation_deadline,
    }))
}

/// Keep `held` as the deadline in force, saying a change in the log.
pub fn keep(held: Option<Affirmation>) {
    let Ok(mut slot) = HELD.write() else {
        return;
    };
    if *slot != held {
        match &held {
            Some(affirmation) => tracing::info!(
                zone = affirmation.zone_name,
                minutes = affirmation.minutes,
                "a held booking is due by the operations plugin's affirmation deadline"
            ),
            None => tracing::info!("no affirmation deadline is declared; held bookings name none"),
        }
        *slot = held;
    }
}

/// Read the setting from the conductor, now and every `every`: kept where it
/// reads, the last kept where the conductor does not answer.
pub async fn follow_forever(bus: Arc<Bus>, every: Duration) {
    loop {
        let asked = bus
            .call(
                READ_DEPLOYMENT_TIME_ZONE,
                "meridian.v1.ReadDeploymentTimeZoneRequest",
                ReadDeploymentTimeZoneRequest {}.encode_to_vec(),
                None,
                Some(Duration::from_secs(10)),
            )
            .await;
        match asked {
            Ok((_, payload)) => match DeploymentTimeZoneSetting::decode(&payload[..]) {
                Ok(setting) => match of_setting(&setting, Zone::load) {
                    Ok(held) => keep(held),
                    Err(why) => tracing::warn!(why, "the deployment's time zone could not be read"),
                },
                Err(failed) => tracing::warn!(%failed, "the time zone's answer did not decode"),
            },
            Err(failed) => {
                tracing::debug!(%failed, "the deployment's time zone was not answered; kept as it was")
            }
        }
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_york(_: &str) -> Result<Zone, String> {
        Zone::of_rule("EST5EDT,M3.2.0,M11.1.0")
    }

    #[test]
    fn a_deadline_declared_falls_on_the_trade_date_in_the_zone() {
        let setting = DeploymentTimeZoneSetting {
            deployment_time_zone: "America/New_York".into(),
            affirmation_deadline: 21 * 60,
            affirmation_deadline_instance_id: "operations-1".into(),
            ..Default::default()
        };
        let held = of_setting(&setting, new_york).unwrap().unwrap();
        // 21:00 EDT on 2026-10-16 is 01:00 UTC the next day.
        assert_eq!(held.due_by("2026-10-16"), 1_792_198_800 * 1_000_000_000);
        assert_eq!(held.due_by("not a date"), 0);
    }

    #[test]
    fn none_declared_or_out_of_range_is_no_deadline_and_a_zone_not_held_is_said() {
        let none = DeploymentTimeZoneSetting {
            deployment_time_zone: "America/New_York".into(),
            ..Default::default()
        };
        assert_eq!(of_setting(&none, new_york), Ok(None));
        let beyond = DeploymentTimeZoneSetting {
            affirmation_deadline: 1441,
            ..none.clone()
        };
        assert_eq!(of_setting(&beyond, new_york), Ok(None));
        let declared = DeploymentTimeZoneSetting {
            affirmation_deadline: 60,
            ..none
        };
        assert!(of_setting(&declared, |_| Err("not held".into())).is_err());
    }
}
