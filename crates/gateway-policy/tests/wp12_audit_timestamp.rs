use chrono::DateTime;
use gateway_policy::audit_timestamp::{AuditTimestamp, Locale, TimeInput};
fn input(date: &str, locale: Locale) -> TimeInput<'static> {
    TimeInput {
        date: DateTime::parse_from_rfc3339(date).unwrap(),
        zone_id: "UTC",
        short_zone: "UTC",
        long_zone: "Coordinated Universal Time",
        generic_short_zone: "UTC",
        generic_long_zone: "Coordinated Universal Time",
        locale,
    }
}
#[test]
fn fixed_clock_java_default_english_component_patterns_match_recorded_results() {
    let t = input("2024-12-31T23:45:56.123456789Z", Locale::English);
    for (pattern, expected) in [
        ("yyyy-MM-dd'T'HH:mm:ss.SSSXXX", "2024-12-31T23:45:56.123Z"),
        ("G GGGG GGGGG", "AD Anno Domini A"),
        ("u uu uuuu", "2024 24 2024"),
        ("Y YY YYYY w ww W e ee c", "2025 25 2025 1 01 5 3 03 3"),
        (
            "MMM MMMM MMMMM LLL LLLL LLLLL",
            "Dec December D Dec December D",
        ),
        ("E EEEE EEEEE ccc cccc ccccc", "Tue Tuesday T Tue Tuesday T"),
        ("a h hh H HH k kk K KK", "PM 11 11 23 23 23 23 11 11"),
        (
            "A AA AAAAAAAAA n nn nnnnnnnnn N NN NNNNNNNNNNNNNNN",
            "85556123 85556123 085556123 123456789 123456789 123456789 85556123456789 85556123456789 085556123456789",
        ),
        ("D DD DDD F", "366 366 366 5"),
        (
            "X XX XXX XXXX XXXXX x xx xxx xxxx xxxxx Z ZZZZ ZZZZZ O OOOO",
            "Z Z Z Z Z +00 +0000 +00:00 +0000 +00:00 +0000 GMT Z GMT GMT",
        ),
        (
            "VV z zzzz v vvvv",
            "UTC UTC Coordinated Universal Time UTC Coordinated Universal Time",
        ),
        ("'a''b' [yyyy] pppH", "a'b 2024  23"),
    ] {
        assert_eq!(
            AuditTimestamp::parse(pattern).unwrap().format(&t).unwrap(),
            expected,
            "{pattern}"
        );
    }
}
#[test]
fn root_locale_and_eastern_offset_remain_distinct() {
    let root = input("2024-12-31T23:45:56.123456789Z", Locale::Root);
    for (p, v) in [
        ("G GGGG GGGGG", "CE CE CE"),
        ("MMM MMMM MMMMM", "Dec Dec 12"),
        ("E EEEE EEEEE", "Tue Tue T"),
    ] {
        assert_eq!(AuditTimestamp::parse(p).unwrap().format(&root).unwrap(), v);
    }
    let mut east = input("2024-12-31T18:45:56.123456789-05:00", Locale::English);
    east.zone_id = "America/Toronto";
    east.short_zone = "EST";
    east.long_zone = "Eastern Standard Time";
    east.generic_short_zone = "ET";
    east.generic_long_zone = "Eastern Time";
    for (p, v) in [
        (
            "X XX XXX XXXX XXXXX x xx xxx xxxx xxxxx Z ZZZZ ZZZZZ O OOOO",
            "-05 -0500 -05:00 -0500 -05:00 -05 -0500 -05:00 -0500 -05:00 -0500 GMT-05:00 -05:00 GMT-5 GMT-05:00",
        ),
        (
            "VV z zzzz v vvvv",
            "America/Toronto EST Eastern Standard Time ET Eastern Time",
        ),
    ] {
        assert_eq!(AuditTimestamp::parse(p).unwrap().format(&east).unwrap(), v);
    }
}
#[test]
fn malformed_or_unbounded_patterns_fail_without_disclosing_configuration() {
    for p in [
        "synthetic-sensitive-pattern",
        "'unclosed",
        "[yyyy",
        "yyyy]",
        "cc",
        "VVV",
        "SSSSSSSSSS",
        "pp",
        "#{x}",
    ] {
        let e = AuditTimestamp::parse(p).unwrap_err();
        assert!(!e.contains(p));
    }
    assert!(AuditTimestamp::parse(&"x".repeat(1027)).is_err());
    assert!(AuditTimestamp::parse(&format!("{}yyyy{}", "[".repeat(17), "]".repeat(17))).is_err());
    let t = input("2024-12-31T23:45:56.123456789Z", Locale::English);
    assert!(AuditTimestamp::parse("pH").unwrap().format(&t).is_err());
    assert_eq!(
        AuditTimestamp::parse("pppp'😀'")
            .unwrap()
            .format(&t)
            .unwrap(),
        "  😀"
    );
}

#[test]
fn time_metadata_and_calendar_boundaries_fail_without_panics_or_disclosure() {
    let oversized = "synthetic-sensitive-zone".repeat(20);
    let t = TimeInput {
        zone_id: &oversized,
        ..input("2024-12-31T23:45:56.123456789Z", Locale::English)
    };
    let error = AuditTimestamp::parse("VV").unwrap().format(&t).unwrap_err();
    assert!(!error.contains("synthetic-sensitive-zone"));
    for date in [chrono::NaiveDate::MIN, chrono::NaiveDate::MAX] {
        let mut t = input("2024-12-31T23:45:56.123456789Z", Locale::English);
        t.date = date.and_hms_opt(0, 0, 0).unwrap().and_utc().fixed_offset();
        assert!(AuditTimestamp::parse("Y-w").unwrap().format(&t).is_err());
    }
    let us = input("2021-01-01T12:00:00Z", Locale::English);
    let gb = input("2021-01-01T12:00:00Z", Locale::EnglishGb);
    assert_eq!(
        AuditTimestamp::parse("Y-w-W-e")
            .unwrap()
            .format(&us)
            .unwrap(),
        "2021-1-1-6"
    );
    assert_eq!(
        AuditTimestamp::parse("Y-w-W-e")
            .unwrap()
            .format(&gb)
            .unwrap(),
        "2020-53-0-5"
    );
}
