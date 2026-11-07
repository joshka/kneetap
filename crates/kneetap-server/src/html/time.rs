use std::time::Duration;

/// Shortens an RFC 3339 UTC timestamp to its time of day, such as `11:52:30.212 UTC`.
///
/// Requests on one page are seconds apart, so the date is noise there; the page keeps the full
/// timestamp for hovering. Returns `None` for any other format, so the caller shows it unchanged.
pub fn time_of_day(timestamp: &str) -> Option<String> {
    let (_, time) = timestamp.split_once('T')?;
    let time = time.strip_suffix('Z')?;
    Some(format!("{time} UTC"))
}

/// Describes how long the server had been up when the request arrived, such as `3h 12m`.
pub fn uptime(started_at: &str, received_at: &str) -> Option<String> {
    let started = humantime::parse_rfc3339(started_at).ok()?;
    let received = humantime::parse_rfc3339(received_at).ok()?;
    let elapsed = received.duration_since(started).ok()?;
    Some(coarse(elapsed))
}

/// Writes a duration in its two largest units, since more precision doesn't help a reader here.
fn coarse(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (days, hours) = (seconds / 86_400, seconds / 3_600 % 24);
    let (minutes, seconds) = (seconds / 60 % 60, seconds % 60);
    match () {
        () if days > 0 => format!("{days}d {hours}h"),
        () if hours > 0 => format!("{hours}h {minutes}m"),
        () if minutes > 0 => format!("{minutes}m {seconds}s"),
        () => format!("{seconds}s"),
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("2026-11-07T11:52:30.212Z", Some("11:52:30.212 UTC"))]
    #[case("yesterday", None)]
    fn timestamps_shorten_to_the_time_of_day(#[case] timestamp: &str, #[case] short: Option<&str>) {
        assert_eq!(time_of_day(timestamp).as_deref(), short);
    }

    #[rstest]
    #[case("2026-11-07T11:00:00.000Z", Some("4s"))]
    #[case("2026-11-07T07:48:00.000Z", Some("3h 12m"))]
    #[case("2026-11-05T09:00:00.000Z", Some("2d 2h"))]
    #[case("2026-11-07T12:00:00.000Z", None)]
    fn uptime_uses_the_two_largest_units(
        #[case] started_at: &str,
        #[case] uptime_text: Option<&str>,
    ) {
        let uptime = uptime(started_at, "2026-11-07T11:00:04.000Z");

        assert_eq!(uptime.as_deref(), uptime_text);
    }
}
