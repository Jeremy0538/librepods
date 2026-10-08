//! When this computer takes, keeps or lets go of the AirPods so that they follow the device
//! that is playing. Pure state: the controller feeds it observations and carries out the
//! action it returns. Port of the OmaPods `switchpolicy.hpp` (airpods-handover), with one
//! desktop change: Rust LibrePods doesn't read the case lid, and a desktop has speakers, so
//! playback only pulls the AirPods over when they are known to be out of the case.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buds {
    Unknown,
    InCase,
    Out,
}

/// Both buds charging means both are in the case. Nothing seen yet reads as Unknown rather
/// than as a change.
pub fn buds_from(left_available: bool, left_charging: bool, right_available: bool, right_charging: bool) -> Buds {
    if left_available && right_available && left_charging && right_charging {
        Buds::InCase
    } else if (left_available && !left_charging) || (right_available && !right_charging) {
        Buds::Out
    } else {
        Buds::Unknown
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Observation {
    pub now_ms: i64,
    pub connected: bool,
    /// An uncorked stream, or an MPRIS player reporting Playing. Chromium keeps a silent
    /// uncorked stream for about 8 s after a pause, so pressing play within that window only
    /// shows up on the player.
    pub stream_playing: bool,
    pub player_playing: bool,
    pub buds: Buds,
    pub other_audio_connected: bool,
    pub powered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// Hold playback, connect, then resume it on the AirPods.
    Connect,
    /// Disconnect without treating it as a handover, so the phone can have them.
    Release,
    /// Another device took them: pause playback here and offer to move them back.
    HandOver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub action: Action,
    pub reason: &'static str,
}

const NOTHING: Decision = Decision { action: Action::None, reason: "" };

/// Playback starting this soon after a handover is the paused player settling, not the user.
pub const HANDOVER_QUIET_MS: i64 = 3000;
/// A failed connect means the AirPods are out of reach; don't page them on every sound.
pub const RETRY_AFTER_MS: i64 = 10000;

#[derive(Debug)]
pub struct SwitchPolicy {
    was_connected: bool,
    released: bool,
    handed_over: bool,
    handover_at_ms: i64,
    last_failure_ms: i64,
    was_stream_playing: bool,
    was_player_playing: bool,
    was_buds: Buds,
}

impl Default for SwitchPolicy {
    fn default() -> Self {
        Self {
            was_connected: false,
            released: false,
            handed_over: false,
            handover_at_ms: 0,
            last_failure_ms: i64::MIN / 2,
            was_stream_playing: false,
            was_player_playing: false,
            was_buds: Buds::Unknown,
        }
    }
}

impl SwitchPolicy {
    pub fn update(&mut self, o: &Observation) -> Decision {
        let mut hand_over = NOTHING;
        if o.connected {
            self.was_connected = true;
            self.handed_over = false;
        } else if self.was_connected && self.released {
            self.was_connected = false;
            self.released = false;
        } else if self.was_connected {
            self.was_connected = false;
            self.handed_over = true;
            self.handover_at_ms = o.now_ms;
            hand_over = Decision { action: Action::HandOver, reason: "AirPods disconnected, another device took them" };
        }

        let playing = o.stream_playing || o.player_playing;
        let mut started = (o.stream_playing && !self.was_stream_playing) || (o.player_playing && !self.was_player_playing);
        self.was_stream_playing = o.stream_playing;
        self.was_player_playing = o.player_playing;

        // Taken out of the case: they go where something is playing, as with Apple devices.
        let mut release = NOTHING;
        if self.was_buds == Buds::InCase && o.buds == Buds::Out && !started {
            if playing && !self.was_connected {
                started = true;
            } else if !playing && self.was_connected {
                self.released = true;
                release = Decision { action: Action::Release, reason: "AirPods taken out of the case with nothing playing" };
            }
        }
        self.was_buds = o.buds;

        if hand_over.action != Action::None {
            return hand_over;
        }
        if release.action != Action::None {
            return release;
        }
        if !started {
            return NOTHING;
        }

        if self.handed_over {
            if o.now_ms - self.handover_at_ms < HANDOVER_QUIET_MS {
                return Decision { action: Action::None, reason: "playback started right after the handover" };
            }
            self.handed_over = false;
        }
        if o.now_ms - self.last_failure_ms < RETRY_AFTER_MS {
            return Decision { action: Action::None, reason: "the last connect failed moments ago" };
        }
        if self.was_connected || !o.powered || o.other_audio_connected {
            return NOTHING;
        }
        // A page to buds in the case can only time out, holding the paused playback for the
        // whole retry chain. As in OmaPods, an unknown state still tries: BLE adverts don't
        // always arrive (Rust has no lid state, so both buds charging stands in for the case).
        if o.buds == Buds::InCase {
            return Decision { action: Action::None, reason: "the AirPods are in the case" };
        }
        Decision { action: Action::Connect, reason: "playback started" }
    }

    pub fn connect_succeeded(&mut self) {
        self.was_connected = true;
    }

    pub fn connect_failed(&mut self, now_ms: i64) {
        self.last_failure_ms = now_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle(now_ms: i64, connected: bool) -> Observation {
        Observation {
            now_ms,
            connected,
            stream_playing: false,
            player_playing: false,
            buds: Buds::Out,
            other_audio_connected: false,
            powered: true,
        }
    }

    #[test]
    fn reads_buds_from_battery() {
        assert_eq!(buds_from(true, true, true, true), Buds::InCase);
        assert_eq!(buds_from(true, false, true, true), Buds::Out);
        assert_eq!(buds_from(false, false, true, false), Buds::Out);
        assert_eq!(buds_from(false, false, false, false), Buds::Unknown);
        // One bud seen charging and the other unseen could still be one bud in an ear.
        assert_eq!(buds_from(true, true, false, false), Buds::Unknown);
    }

    #[test]
    fn connects_when_playback_starts() {
        let mut p = SwitchPolicy::default();
        assert_eq!(p.update(&idle(0, false)).action, Action::None);
        let o = Observation { stream_playing: true, ..idle(1000, false) };
        assert_eq!(p.update(&o).action, Action::Connect);
    }

    #[test]
    fn ignores_playback_that_keeps_going() {
        let mut p = SwitchPolicy::default();
        let mut o = Observation { stream_playing: true, ..idle(0, false) };
        assert_eq!(p.update(&o).action, Action::Connect);
        p.connect_failed(500);
        o.now_ms = 20000;
        assert_eq!(p.update(&o).action, Action::None);
    }

    #[test]
    fn connects_when_a_player_resumes_on_a_silent_stream() {
        let mut p = SwitchPolicy::default();
        let mut o = Observation { stream_playing: true, ..idle(0, false) };
        p.update(&o);
        p.connect_failed(0);
        o.now_ms = 20000;
        o.player_playing = true;
        assert_eq!(p.update(&o).action, Action::Connect);
    }

    #[test]
    fn leaves_buds_in_the_case_alone() {
        let mut p = SwitchPolicy::default();
        let mut o = Observation { buds: Buds::InCase, ..idle(0, false) };
        p.update(&o);
        o.now_ms = 1000;
        o.stream_playing = true;
        let d = p.update(&o);
        assert_eq!(d.action, Action::None);
        assert_eq!(d.reason, "the AirPods are in the case");
    }

    #[test]
    fn tries_when_the_buds_state_is_unknown() {
        let mut p = SwitchPolicy::default();
        let mut o = Observation { buds: Buds::Unknown, ..idle(0, false) };
        p.update(&o);
        o.now_ms = 1000;
        o.stream_playing = true;
        assert_eq!(p.update(&o).action, Action::Connect);
    }

    #[test]
    fn hands_over_on_an_unexpected_disconnect() {
        let mut p = SwitchPolicy::default();
        p.update(&idle(0, true));
        assert_eq!(p.update(&idle(1000, false)).action, Action::HandOver);
        assert_eq!(p.update(&idle(2000, false)).action, Action::None);
    }

    #[test]
    fn stays_quiet_right_after_a_handover() {
        let mut p = SwitchPolicy::default();
        p.update(&idle(0, true));
        p.update(&idle(1000, false));
        let o = Observation { player_playing: true, ..idle(2000, false) };
        assert_eq!(p.update(&o).action, Action::None);

        p.update(&idle(9000, false));
        let o = Observation { player_playing: true, ..idle(10000, false) };
        assert_eq!(p.update(&o).action, Action::Connect);
    }

    #[test]
    fn waits_after_a_failed_connect() {
        let mut p = SwitchPolicy::default();
        let mut o = Observation { stream_playing: true, ..idle(0, false) };
        p.update(&o);
        p.connect_failed(1000);
        p.update(&idle(2000, false));
        o.now_ms = 3000;
        assert_eq!(p.update(&o).action, Action::None);
        p.update(&idle(12000, false));
        o.now_ms = 12500;
        assert_eq!(p.update(&o).action, Action::Connect);
    }

    #[test]
    fn skips_when_another_headphone_is_connected() {
        let mut p = SwitchPolicy::default();
        let mut o = Observation { other_audio_connected: true, ..idle(0, false) };
        p.update(&o);
        o.stream_playing = true;
        assert_eq!(p.update(&o).action, Action::None);
    }

    #[test]
    fn connects_when_taken_out_while_playing() {
        let mut p = SwitchPolicy::default();
        let mut o = Observation { stream_playing: true, buds: Buds::InCase, ..idle(0, false) };
        assert_eq!(p.update(&o).action, Action::None);
        o.now_ms = 5000;
        o.buds = Buds::Out;
        assert_eq!(p.update(&o).action, Action::Connect);
    }

    #[test]
    fn releases_when_taken_out_with_nothing_playing() {
        let mut p = SwitchPolicy::default();
        let mut o = Observation { buds: Buds::InCase, ..idle(0, true) };
        p.update(&o);
        o.buds = Buds::Out;
        assert_eq!(p.update(&o).action, Action::Release);
    }

    #[test]
    fn release_is_not_a_handover() {
        let mut p = SwitchPolicy::default();
        let mut o = Observation { buds: Buds::InCase, ..idle(0, true) };
        p.update(&o);
        o.buds = Buds::Out;
        p.update(&o);
        o.connected = false;
        o.now_ms = 1000;
        assert_eq!(p.update(&o).action, Action::None);
        // And playback afterwards connects without the handover quiet period.
        o.now_ms = 1500;
        o.player_playing = true;
        assert_eq!(p.update(&o).action, Action::Connect);
    }

    #[test]
    fn unknown_buds_are_not_a_take_out() {
        let mut p = SwitchPolicy::default();
        let mut o = Observation { buds: Buds::Unknown, ..idle(0, true) };
        p.update(&o);
        o.buds = Buds::Out;
        assert_eq!(p.update(&o).action, Action::None);
    }
}
