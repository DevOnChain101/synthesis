//! The welcome greeting: every user-visible string lives in [`GREETING`], so a translation is one
//! more constant of this type. The line before the prompt line follows the local hour.

pub struct Greeting {
    /// Hours 5 to 11.
    pub morning: &'static str,
    /// Hours 12 to 16.
    pub afternoon: &'static str,
    /// Hours 17 to 21.
    pub evening: &'static str,
    /// Hours 22 to 4.
    pub night: &'static str,
    /// The line under it.
    pub prompt: &'static str,
}

pub const EN: Greeting = Greeting {
    morning: "Good morning.",
    afternoon: "Good afternoon.",
    evening: "Good evening.",
    night: "Working late.",
    prompt: "What shall we build?",
};

/// The language of the TUI strings.
pub const GREETING: &Greeting = &EN;

impl Greeting {
    /// `(greeting, prompt)` for `now` (epoch ms) in the zone `tz_offset_min` minutes east of UTC.
    pub fn at(&self, now: i64, tz_offset_min: i32) -> (&'static str, &'static str) {
        let hour = (now + i64::from(tz_offset_min) * 60_000)
            .div_euclid(3_600_000)
            .rem_euclid(24);
        let hello = match hour {
            5..=11 => self.morning,
            12..=16 => self.afternoon,
            17..=21 => self.evening,
            _ => self.night,
        };
        (hello, self.prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_local_hour() {
        let h = |utc_h: i64, tz: i32| GREETING.at(utc_h * 3_600_000, tz).0;
        assert_eq!(h(12, 0), EN.afternoon);
        assert_eq!(h(23, 0), EN.night);
        assert_eq!(h(23, 120), EN.night); // 01:00
        assert_eq!(h(20, 60), EN.evening); // 21:00
        assert_eq!(h(4, 60), EN.morning); // 05:00
        assert_eq!(h(19, 0), EN.evening);
        assert_eq!(h(0, -60), EN.night); // 23:00 the day before
    }
}
