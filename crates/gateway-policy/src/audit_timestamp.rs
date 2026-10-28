//! Bounded Java-pattern audit timestamps. Clock/zone acquisition stays in adapters.
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Timelike};
#[derive(Clone, Copy, Debug)]
pub enum Locale {
    English,
    EnglishGb,
    Root,
}
pub struct TimeInput<'a> {
    pub date: DateTime<FixedOffset>,
    pub zone_id: &'a str,
    pub short_zone: &'a str,
    pub long_zone: &'a str,
    pub generic_short_zone: &'a str,
    pub generic_long_zone: &'a str,
    pub locale: Locale,
}
#[derive(Clone, Debug)]
enum Piece {
    Literal(String),
    Field(char, usize),
    Optional(Vec<Piece>),
    Pad(usize, Box<Piece>),
}
#[derive(Clone, Debug)]
pub struct AuditTimestamp {
    pieces: Vec<Piece>,
    pub needs_locale: bool,
    pub needs_zone_names: bool,
}
impl AuditTimestamp {
    pub fn parse(pattern: &str) -> Result<Self, &'static str> {
        if pattern.len() > 1026 {
            return Err("audit.timestampFormat: unbounded pattern");
        }
        let chars: Vec<_> = pattern.chars().collect();
        let mut at = 0;
        let pieces = parse_pieces(&chars, &mut at, 0, false)?;
        let needs_locale = has(&pieces, |c, n| {
            matches!(c, 'G' | 'E' | 'e' | 'c' | 'a' | 'B' | 'Y' | 'w' | 'W')
                || matches!(c, 'M' | 'L' | 'Q' | 'q') && n >= 3
        });
        let needs_zone_names = has(&pieces, |c, _| matches!(c, 'V' | 'z' | 'v'));
        Ok(Self {
            pieces,
            needs_locale,
            needs_zone_names,
        })
    }
    pub fn format(&self, input: &TimeInput<'_>) -> Result<String, &'static str> {
        if input.date.nanosecond() >= 1_000_000_000
            || [
                input.zone_id,
                input.short_zone,
                input.long_zone,
                input.generic_short_zone,
                input.generic_long_zone,
            ]
            .iter()
            .any(|s| s.len() > 256)
        {
            return Err("audit.timestampFormat: unsupported time metadata");
        }
        let mut out = String::new();
        render(&self.pieces, input, &mut out)?;
        Ok(out)
    }
}
fn has(pieces: &[Piece], pred: impl Fn(char, usize) -> bool + Copy) -> bool {
    pieces.iter().any(|p| match p {
        Piece::Field(c, n) => pred(*c, *n),
        Piece::Optional(p) => has(p, pred),
        Piece::Pad(_, p) => has(std::slice::from_ref(p.as_ref()), pred),
        _ => false,
    })
}
fn parse_pieces(
    chars: &[char],
    at: &mut usize,
    depth: usize,
    optional: bool,
) -> Result<Vec<Piece>, &'static str> {
    if depth > 16 {
        return Err("audit.timestampFormat: unbounded nesting");
    }
    let mut out = Vec::new();
    while *at < chars.len() {
        let c = chars[*at];
        *at += 1;
        if c == ']' {
            if !optional {
                return Err("audit.timestampFormat: malformed optional");
            }
            return Ok(out);
        }
        if c == '[' {
            out.push(Piece::Optional(parse_pieces(chars, at, depth + 1, true)?));
            continue;
        }
        if c == '\'' {
            if chars.get(*at) == Some(&'\'') {
                *at += 1;
                out.push(Piece::Literal("'".into()));
                continue;
            }
            let mut text = String::new();
            let mut closed = false;
            while *at < chars.len() {
                let c = chars[*at];
                *at += 1;
                if c == '\'' {
                    if chars.get(*at) == Some(&'\'') {
                        *at += 1;
                        text.push('\'');
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    text.push(c);
                }
            }
            if !closed {
                return Err("audit.timestampFormat: malformed literal");
            }
            out.push(Piece::Literal(text));
            continue;
        }
        if c.is_ascii_alphabetic() {
            let mut n = 1;
            while chars.get(*at) == Some(&c) {
                *at += 1;
                n += 1;
            }
            if c == 'p' {
                if *at == chars.len() {
                    return Err("audit.timestampFormat: malformed pad");
                }
                let mut remainder = parse_pieces(chars, at, depth + 1, optional)?;
                if remainder.is_empty() {
                    return Err("audit.timestampFormat: malformed pad");
                }
                out.push(Piece::Pad(n, Box::new(remainder.remove(0))));
                out.extend(remainder);
                return Ok(out);
            }
            let valid = match c {
                'G' | 'M' | 'L' | 'Q' | 'q' | 'E' | 'e' => n <= 5,
                'c' => n == 1 || (3..=5).contains(&n),
                'u' | 'y' | 'Y' | 'A' | 'n' | 'N' | 'g' => n <= 19,
                'D' => n <= 3,
                'd' | 'H' | 'h' | 'k' | 'K' | 'm' | 's' | 'w' => n <= 2,
                'W' | 'F' | 'a' => n == 1,
                'B' => [1, 4, 5].contains(&n),
                'S' => n <= 9,
                'X' | 'x' | 'Z' => n <= 5,
                'O' | 'v' => [1, 4].contains(&n),
                'V' => n == 2,
                'z' => n <= 4,
                _ => false,
            };
            if !valid {
                return Err("audit.timestampFormat: malformed field");
            }
            out.push(Piece::Field(c, n));
        } else {
            if ['#', '{', '}'].contains(&c) {
                return Err("audit.timestampFormat: reserved character");
            }
            out.push(Piece::Literal(c.to_string()));
        }
    }
    if optional {
        return Err("audit.timestampFormat: malformed optional");
    }
    Ok(out)
}

fn render(pieces: &[Piece], input: &TimeInput<'_>, out: &mut String) -> Result<(), &'static str> {
    for p in pieces {
        match p {
            Piece::Literal(s) => out.push_str(s),
            Piece::Field(c, n) => out.push_str(&field(*c, *n, input)?),
            Piece::Optional(p) => render(p, input, out)?,
            Piece::Pad(width, p) => {
                let mut value = String::new();
                render(std::slice::from_ref(p.as_ref()), input, &mut value)?;
                let len = value.encode_utf16().count();
                if len > *width {
                    return Err("audit.timestampFormat: pad overflow");
                }
                out.extend(std::iter::repeat_n(' ', width - len));
                out.push_str(&value);
            }
        }
    }
    Ok(())
}
fn number(value: u64, width: usize) -> String {
    format!("{value:0width$}")
}
fn year(value: i32, width: usize) -> String {
    if width == 2 {
        return number(u64::from(value.unsigned_abs() % 100), 2);
    }
    let magnitude = number(u64::from(value.unsigned_abs()), width);
    if value < 0 {
        format!("-{magnitude}")
    } else if width >= 4 && magnitude.len() > width {
        format!("+{magnitude}")
    } else {
        magnitude
    }
}
fn week_start(date: NaiveDate, iso: bool) -> Result<NaiveDate, &'static str> {
    let diff = if iso {
        date.weekday().num_days_from_monday()
    } else {
        date.weekday().num_days_from_sunday()
    };
    let mut start = date
        .checked_sub_signed(chrono::Duration::days(i64::from(diff)))
        .ok_or("audit.timestampFormat: calendar range")?;
    if iso && 7 - diff < 4 {
        start = start
            .checked_add_signed(chrono::Duration::days(7))
            .ok_or("audit.timestampFormat: calendar range")?;
    }
    Ok(start)
}
fn week_year(date: NaiveDate, iso: bool) -> Result<(i32, u64), &'static str> {
    let mut y = date.year();
    let mut start = week_start(
        NaiveDate::from_ymd_opt(y, 1, 1).ok_or("audit.timestampFormat: calendar range")?,
        iso,
    )?;
    let next = week_start(
        NaiveDate::from_ymd_opt(y + 1, 1, 1).ok_or("audit.timestampFormat: calendar range")?,
        iso,
    )?;
    if date >= next {
        y += 1;
        start = next;
    } else if date < start {
        y -= 1;
        start = week_start(
            NaiveDate::from_ymd_opt(y, 1, 1).ok_or("audit.timestampFormat: calendar range")?,
            iso,
        )?;
    }
    Ok((y, ((date - start).num_days() / 7 + 1) as u64))
}
fn offset(seconds: i32, c: char, n: usize) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let abs = seconds.unsigned_abs();
    let h = abs / 3600;
    let m = abs % 3600 / 60;
    let s = abs % 60;
    if seconds == 0 && (c == 'X' || c == 'Z' && n == 5) {
        return "Z".into();
    }
    if c == 'O' || c == 'Z' && n == 4 {
        if seconds == 0 {
            return "GMT".into();
        }
        let mut out = if n == 1 {
            format!("GMT{sign}{h}")
        } else {
            format!("GMT{sign}{h:02}:{m:02}")
        };
        if n == 1 && (m != 0 || s != 0) {
            out.push_str(&format!(":{m:02}"));
        }
        if s != 0 {
            out.push_str(&format!(":{s:02}"));
        }
        return out;
    }
    let effective = if c == 'Z' {
        if n < 4 { 2 } else { 5 }
    } else {
        n
    };
    let mut out = match effective {
        1 => {
            if m == 0 {
                format!("{sign}{h:02}")
            } else {
                format!("{sign}{h:02}{m:02}")
            }
        }
        2 | 4 => format!("{sign}{h:02}{m:02}"),
        _ => format!("{sign}{h:02}:{m:02}"),
    };
    if effective >= 4 && s != 0 {
        out.push_str(&if effective == 4 {
            format!("{s:02}")
        } else {
            format!(":{s:02}")
        });
    }
    out
}
fn field(c: char, n: usize, t: &TimeInput<'_>) -> Result<String, &'static str> {
    let d = t.date;
    let root = matches!(t.locale, Locale::Root);
    let gb = matches!(t.locale, Locale::EnglishGb);
    let numeric = match c {
        'y' => Some(if d.year() > 0 {
            d.year() as u64
        } else {
            (1 - d.year()) as u64
        }),
        'M' | 'L' if n <= 2 => Some(u64::from(d.month())),
        'Q' | 'q' if n <= 2 => Some(u64::from((d.month() - 1) / 3 + 1)),
        'd' => Some(u64::from(d.day())),
        'D' => Some(u64::from(d.ordinal())),
        'H' => Some(u64::from(d.hour())),
        'h' => Some(u64::from(if d.hour().is_multiple_of(12) {
            12
        } else {
            d.hour() % 12
        })),
        'k' => Some(u64::from(if d.hour() == 0 { 24 } else { d.hour() })),
        'K' => Some(u64::from(d.hour() % 12)),
        'm' => Some(u64::from(d.minute())),
        's' => Some(u64::from(d.second())),
        'A' => Some(
            u64::from(d.num_seconds_from_midnight()) * 1000 + u64::from(d.nanosecond() / 1_000_000),
        ),
        'n' => Some(u64::from(d.nanosecond())),
        'N' => Some(
            u64::from(d.num_seconds_from_midnight()) * 1_000_000_000 + u64::from(d.nanosecond()),
        ),
        'F' => Some(u64::from((d.day() - 1) / 7 + 1)),
        'w' => Some(week_year(d.date_naive(), gb)?.1),
        'W' => Some(
            ((d.date_naive()
                - week_start(
                    NaiveDate::from_ymd_opt(d.year(), d.month(), 1)
                        .ok_or("audit.timestampFormat: calendar range")?,
                    gb,
                )?)
            .num_days()
            .div_euclid(7)
                + 1)
            .max(0) as u64,
        ),
        'e' | 'c' if n <= 2 => Some(
            u64::from(if gb {
                d.weekday().num_days_from_monday()
            } else {
                d.weekday().num_days_from_sunday()
            }) + 1,
        ),
        _ => None,
    };
    if let Some(v) = numeric {
        return Ok(if c == 'y' {
            year(v as i32, n)
        } else {
            number(v, n)
        });
    }
    Ok(match c {
        'g' => {
            let v = (d.date_naive() - NaiveDate::from_ymd_opt(1858, 11, 17).unwrap()).num_days();
            if v < 0 {
                format!("-{}", number(v.unsigned_abs(), n))
            } else {
                number(v as u64, n)
            }
        }
        'u' => year(d.year(), n),
        'Y' => year(week_year(d.date_naive(), gb)?.0, n),
        'S' => format!("{:09}", d.nanosecond())[..n].into(),
        'G' => {
            let ce = d.year() > 0;
            if root {
                if ce { "CE" } else { "BCE" }
            } else {
                match n {
                    4 => {
                        if ce {
                            "Anno Domini"
                        } else {
                            "Before Christ"
                        }
                    }
                    5 => {
                        if ce {
                            "A"
                        } else {
                            "B"
                        }
                    }
                    _ => {
                        if ce {
                            "AD"
                        } else {
                            "BC"
                        }
                    }
                }
            }
            .into()
        }
        'M' | 'L' => {
            let month = [
                "January",
                "February",
                "March",
                "April",
                "May",
                "June",
                "July",
                "August",
                "September",
                "October",
                "November",
                "December",
            ][(d.month() - 1) as usize];
            if root && n == 5 {
                d.month().to_string()
            } else if n == 3 || root {
                month[..3].into()
            } else if n == 5 {
                month[..1].into()
            } else {
                month.into()
            }
        }
        'E' | 'e' | 'c' => {
            let day = [
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
                "Sunday",
            ][d.weekday().num_days_from_monday() as usize];
            if n == 5 {
                day[..1].into()
            } else if n <= 3 || root {
                day[..3].into()
            } else {
                day.into()
            }
        }
        'Q' | 'q' => {
            let q = (d.month() - 1) / 3 + 1;
            if n == 5 {
                q.to_string()
            } else if n == 3 || root {
                format!("Q{q}")
            } else {
                format!("{} quarter", ["1st", "2nd", "3rd", "4th"][(q - 1) as usize])
            }
        }
        'a' => if gb {
            if d.hour() < 12 { "am" } else { "pm" }
        } else if d.hour() < 12 {
            "AM"
        } else {
            "PM"
        }
        .into(),
        'B' => if root {
            if d.hour() < 12 { "AM" } else { "PM" }
        } else if d.minute() == 0 && d.second() == 0 && d.nanosecond() == 0 && d.hour() == 0 {
            "midnight"
        } else if d.minute() == 0 && d.second() == 0 && d.nanosecond() == 0 && d.hour() == 12 {
            "noon"
        } else {
            match d.hour() {
                0..=5 => "at night",
                6..=11 => "in the morning",
                12..=17 => "in the afternoon",
                18..=20 => "in the evening",
                _ => "at night",
            }
        }
        .into(),
        'X' | 'x' | 'Z' | 'O' => offset(d.offset().local_minus_utc(), c, n),
        'V' => t.zone_id.into(),
        'z' => if n == 4 { t.long_zone } else { t.short_zone }.into(),
        'v' => if n == 4 {
            t.generic_long_zone
        } else {
            t.generic_short_zone
        }
        .into(),
        _ => unreachable!("validated timestamp field"),
    })
}
