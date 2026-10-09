//! Five-field cron schedules (`minute hour day-of-month month day-of-week`)
//! with `*`, lists, ranges, steps, month/day names and the `@hourly`-style
//! shortcuts. Day-of-month and day-of-week combine like classic cron: when
//! both are restricted, either one matching is enough.

use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    minutes: u64, // bit n = minute n
    hours: u32,
    days: u32,    // bit 1..=31
    months: u16,  // bit 1..=12
    weekdays: u8, // bit 0..=6, Sunday = 0
    dom_any: bool,
    dow_any: bool,
}

/// A calendar minute, as `localtime` reports it.
#[derive(Debug, Clone, Copy)]
pub struct Moment {
    pub minute: u32,
    pub hour: u32,
    pub day: u32,
    pub month: u32,
    /// 0 = Sunday.
    pub weekday: u32,
}

impl Schedule {
    pub fn matches(&self, t: &Moment) -> bool {
        let dom = self.days & (1 << t.day) != 0;
        let dow = self.weekdays & (1 << t.weekday) != 0;
        let day = match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (false, true) => dom,
            (true, false) => dow,
            (false, false) => dom || dow,
        };
        self.minutes & (1 << t.minute) != 0
            && self.hours & (1 << t.hour) != 0
            && self.months & (1 << t.month) != 0
            && day
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn field(spec: &str, min: u32, max: u32, names: &[&str], name_base: u32) -> Result<u64, String> {
    let value = |s: &str| -> Result<u32, String> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(i as u32 + name_base);
        }
        s.parse::<u32>().map_err(|_| format!("invalid value {s:?}"))
    };
    let mut bits = 0u64;
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .ok()
                    .filter(|s| *s > 0)
                    .ok_or_else(|| format!("invalid step in {part:?}"))?,
            ),
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let v = value(range)?;
            // `5/15` means 5, 20, 35, 50.
            (v, if part.contains('/') { max } else { v })
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("{part:?} is outside {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok(bits)
}

impl FromStr for Schedule {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let expanded = match s.trim() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            other => other,
        };
        let f: Vec<&str> = expanded.split_whitespace().collect();
        let [mi, h, dom, mon, dow] = f.as_slice() else {
            return Err(format!("schedule {s:?} needs 5 fields"));
        };
        let err = |name: &str, e: String| format!("schedule {s:?}: {name}: {e}");
        let mut weekdays = field(dow, 0, 7, &DAYS, 0).map_err(|e| err("day of week", e))?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays | 1) & 0x7f; // 7 is Sunday too
        }
        Ok(Schedule {
            minutes: field(mi, 0, 59, &[], 0).map_err(|e| err("minute", e))?,
            hours: field(h, 0, 23, &[], 0).map_err(|e| err("hour", e))? as u32,
            days: field(dom, 1, 31, &[], 0).map_err(|e| err("day of month", e))? as u32,
            months: field(mon, 1, 12, &MONTHS, 1).map_err(|e| err("month", e))? as u16,
            weekdays: weekdays as u8,
            dom_any: *dom == "*",
            dow_any: *dow == "*",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: u32, hour: u32, day: u32, month: u32, weekday: u32) -> Moment {
        Moment {
            minute,
            hour,
            day,
            month,
            weekday,
        }
    }

    #[test]
    fn parses_and_matches() {
        let every: Schedule = "* * * * *".parse().unwrap();
        assert!(every.matches(&at(17, 3, 9, 10, 5)));

        let s: Schedule = "*/15 9-17 * * mon-fri".parse().unwrap();
        assert!(s.matches(&at(30, 9, 9, 10, 1)));
        assert!(!s.matches(&at(31, 9, 9, 10, 1)));
        assert!(!s.matches(&at(0, 9, 11, 10, 6)));

        let daily: Schedule = "@daily".parse().unwrap();
        assert!(daily.matches(&at(0, 0, 1, 1, 3)));
        assert!(!daily.matches(&at(1, 0, 1, 1, 3)));

        let sunday: Schedule = "0 12 * * 7".parse().unwrap();
        assert!(sunday.matches(&at(0, 12, 4, 10, 0)));

        // Classic OR semantics when both day fields are restricted.
        let either: Schedule = "0 0 1 * fri".parse().unwrap();
        assert!(either.matches(&at(0, 0, 1, 2, 2)));
        assert!(either.matches(&at(0, 0, 9, 10, 5)));
        assert!(!either.matches(&at(0, 0, 9, 10, 4)));

        let offset: Schedule = "5/20 * * jan,jul *".parse().unwrap();
        assert!(offset.matches(&at(45, 0, 1, 7, 0)));
        assert!(!offset.matches(&at(45, 0, 1, 8, 0)));
    }

    #[test]
    fn rejects_bad_schedules() {
        for bad in [
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "*/0 * * * *",
            "x * * * *",
            "5-1 * * * *",
        ] {
            assert!(bad.parse::<Schedule>().is_err(), "{bad}");
        }
    }
}
