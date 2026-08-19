//! `d3home daemon`: watch the configured devices and say when something
//! happens.

use syncleo::codec::command::{Event, PowerMode};

use crate::notify::Notification;

/// Every event a notification can be asked for.
pub const EVENTS: &[&str] = &["boiled", "error", "started", "stopped", "offline", "online"];

/// What is reported when the config says nothing. `offline`/`online` are not
/// among them: a kettle is lifted off its base many times a day and the
/// notifications would be pure noise.
pub const DEFAULT_EVENTS: &[&str] = &["boiled", "error"];

/// How close to the target counts as having got there.
///
/// Told to boil to 100 the device stops at 98, so an exact comparison would
/// mean the commonest case never fires. `output::WatchView` draws the same
/// line with the same tolerance, and the two must agree -- a kettle called
/// `boiled` in one place and `stopped` in the other would be worse than
/// either answer on its own.
const REACHED_TOLERANCE: u8 = 2;

/// Folds the device's event stream into the moments worth telling somebody
/// about. Pure: no I/O, no clock, so every rule is testable instantly.
pub struct Watcher {
    device: String,
    wanted: Vec<String>,
    heating: bool,
    current: Option<u8>,
    target: Option<u8>,
    error: Option<bool>,
    /// The device replays its whole state on every connection. Until the
    /// first report of a thing has been seen, a change cannot be told from
    /// an introduction.
    seen_mode: bool,
}

impl Watcher {
    pub fn new(device: String, wanted: Vec<String>) -> Self {
        Self {
            device,
            wanted,
            heating: false,
            current: None,
            target: None,
            error: None,
            seen_mode: false,
        }
    }

    fn wants(&self, event: &str) -> bool {
        self.wanted.iter().any(|w| w == event)
    }

    fn make(&self, event: &'static str, body: String) -> Option<Notification> {
        if !self.wants(event) {
            return None;
        }
        Some(Notification {
            event,
            device: self.device.clone(),
            title: self.device.clone(),
            body,
            temperature: self.current,
            target: self.target,
        })
    }

    pub fn observe(&mut self, event: &Event) -> Option<Notification> {
        match event {
            Event::CurrentTemperature(t) => {
                self.current = Some(*t);
                None
            }
            Event::TargetTemperature(t) => {
                self.target = Some(*t);
                None
            }
            Event::Error(flag) => {
                let previous = self.error.replace(*flag);
                match (previous, flag) {
                    // Only the transition into an error is news. A repeat is
                    // the same error; clearing it is not worth a popup.
                    (Some(false), true) => self.make("error", "reports an error".into()),
                    _ => None,
                }
            }
            Event::Mode(mode) => {
                let was_heating = self.heating;
                self.heating = *mode != PowerMode::Off;

                if !self.seen_mode {
                    self.seen_mode = true;
                    return None;
                }
                if was_heating == self.heating {
                    return None;
                }

                if self.heating {
                    let body = match self.target {
                        Some(target) => format!("heating to {target} \u{b0}C"),
                        None => "heating".to_string(),
                    };
                    return self.make("started", body);
                }

                match (self.current, self.target) {
                    // Saturating, because nothing stops a device from
                    // reporting a reading that would overflow the addition.
                    (Some(current), Some(target))
                        if current.saturating_add(REACHED_TOLERANCE) >= target =>
                    {
                        self.make("boiled", format!("boiled at {current} \u{b0}C"))
                    }
                    (Some(current), _) => {
                        self.make("stopped", format!("switched off at {current} \u{b0}C"))
                    }
                    _ => self.make("stopped", "switched off".into()),
                }
            }
            _ => None,
        }
    }

    pub fn disconnected(&mut self) -> Option<Notification> {
        self.make("offline", "went away".into())
    }

    pub fn reconnected(&mut self) -> Option<Notification> {
        // The device replays its state on a new connection, so the next
        // burst must not be mistaken for a series of changes.
        self.seen_mode = false;
        self.make("online", "came back".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syncleo::codec::command::{Event, PowerMode};

    fn watcher(wanted: &[&str]) -> Watcher {
        Watcher::new(
            "kettle".into(),
            wanted.iter().map(|s| s.to_string()).collect(),
        )
    }

    fn heat_to(w: &mut Watcher, target: u8, from: u8) {
        w.observe(&Event::TargetTemperature(target));
        w.observe(&Event::Mode(PowerMode::Custom));
        w.observe(&Event::CurrentTemperature(from));
    }

    #[test]
    fn reaching_the_target_is_a_boil() {
        let mut w = watcher(&["boiled"]);
        heat_to(&mut w, 80, 40);
        assert!(
            w.observe(&Event::CurrentTemperature(79)).is_none(),
            "still heating"
        );

        let notification = w.observe(&Event::Mode(PowerMode::Off)).expect("boiled");
        assert_eq!(notification.event, "boiled");
        assert_eq!(notification.device, "kettle");
        assert_eq!(notification.temperature, Some(79));
        assert!(
            notification.body.contains("79"),
            "the body should carry the reading"
        );
    }

    #[test]
    fn stopping_at_ninety_eight_still_counts_as_boiled() {
        // Told to boil to 100 the device stops at 98. An exact comparison
        // would mean the commonest case of all never fires.
        let mut w = watcher(&["boiled"]);
        heat_to(&mut w, 100, 40);
        w.observe(&Event::CurrentTemperature(98));
        assert_eq!(
            w.observe(&Event::Mode(PowerMode::Off)).map(|n| n.event),
            Some("boiled")
        );
    }

    #[test]
    fn switching_off_early_is_not_a_boil() {
        let mut w = watcher(&["boiled", "stopped"]);
        heat_to(&mut w, 100, 40);
        w.observe(&Event::CurrentTemperature(60));
        assert_eq!(
            w.observe(&Event::Mode(PowerMode::Off)).map(|n| n.event),
            Some("stopped")
        );
    }

    #[test]
    fn heating_that_somebody_else_started_is_still_a_start() {
        // The daemon is watching whether or not you are, so it notices the
        // kettle being started from the vendor's app. The idle mode first,
        // because a change can only be told from an introduction once the
        // device has said what it was doing to begin with.
        let mut w = watcher(&["started"]);
        w.observe(&Event::Mode(PowerMode::Off));
        w.observe(&Event::TargetTemperature(100));

        let notification = w.observe(&Event::Mode(PowerMode::On)).expect("started");
        assert_eq!(notification.event, "started");
        assert_eq!(notification.target, Some(100));
    }

    #[test]
    fn an_event_not_asked_for_produces_nothing() {
        let mut w = watcher(&["error"]);
        heat_to(&mut w, 80, 40);
        w.observe(&Event::CurrentTemperature(79));
        assert!(
            w.observe(&Event::Mode(PowerMode::Off)).is_none(),
            "boiled was not wanted"
        );
    }

    #[test]
    fn an_error_fires_once_when_it_appears_and_not_again() {
        let mut w = watcher(&["error"]);
        assert!(w.observe(&Event::Error(false)).is_none());
        assert_eq!(
            w.observe(&Event::Error(true)).map(|n| n.event),
            Some("error")
        );
        assert!(
            w.observe(&Event::Error(true)).is_none(),
            "the same error must not repeat"
        );
        assert!(
            w.observe(&Event::Error(false)).is_none(),
            "clearing is not itself news"
        );
    }

    #[test]
    fn the_first_report_of_a_state_is_not_treated_as_a_change() {
        // The device replays everything on connection. A kettle found
        // already heating must not announce a start that happened before we
        // were watching.
        let mut w = watcher(&["started", "error"]);
        assert!(
            w.observe(&Event::Mode(PowerMode::On)).is_none(),
            "the burst is not news"
        );
        assert!(
            w.observe(&Event::Error(true)).is_none(),
            "nor is a pre-existing error"
        );
    }

    #[test]
    fn a_reconnection_does_not_replay_the_burst_as_changes() {
        // The kettle is lifted off its base while heating and put back. The
        // fresh connection repeats mode, target and temperature; none of
        // that is a change somebody needs to be told about.
        let mut w = watcher(&["started", "boiled"]);
        w.observe(&Event::Mode(PowerMode::Off));
        w.reconnected();
        assert!(
            w.observe(&Event::Mode(PowerMode::Custom)).is_none(),
            "the replayed state is not a start"
        );
    }

    #[test]
    fn coming_and_going_are_reported_only_when_asked_for() {
        let mut quiet = watcher(&["boiled"]);
        assert!(
            quiet.disconnected().is_none(),
            "lifted off its base ten times a day"
        );
        assert!(quiet.reconnected().is_none());

        let mut loud = watcher(&["offline", "online"]);
        assert_eq!(loud.disconnected().map(|n| n.event), Some("offline"));
        assert_eq!(loud.reconnected().map(|n| n.event), Some("online"));
    }

    #[test]
    fn a_reading_far_above_the_target_does_not_overflow() {
        // Nothing stops a device -- or something pretending to be one --
        // from reporting 255. Arithmetic that panics on it would take the
        // daemon down with it.
        let mut w = watcher(&["boiled"]);
        heat_to(&mut w, 100, 40);
        w.observe(&Event::CurrentTemperature(255));
        assert_eq!(
            w.observe(&Event::Mode(PowerMode::Off)).map(|n| n.event),
            Some("boiled")
        );
    }

    #[test]
    fn every_advertised_event_name_can_actually_fire() {
        // A name in EVENTS that nothing produces would be a lie in the
        // config's documentation.
        assert!(EVENTS.iter().all(|e| !e.is_empty()));
        for name in DEFAULT_EVENTS {
            assert!(
                EVENTS.contains(name),
                "{name} is a default but not a known event"
            );
        }
    }
}
