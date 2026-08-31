//! `/remindme <when> <what>` — the bot DMs you when the time comes.
//!
//! Two shapes of "when", because those are the two people actually type: a
//! duration from now ("7 days", "6h", "90 minutes") and a calendar date
//! ("9/7/2026"). Everything after it is the reminder.

use crate::{now_ms, SharedState};

/// How often the sweep looks for anything due. A reminder is not a stopwatch;
/// half a minute of slack costs nothing and the query is one indexed read.
pub const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// A parsed `/remindme`.
#[derive(Debug, PartialEq, Eq)]
pub struct Reminder {
    /// Absolute, ms since epoch.
    pub due_at: i64,
    pub text: String,
}

/// Nothing further out than this. Catches a typo'd year before it becomes a
/// row that sits in the table until the heat death of the server.
const MAX_AHEAD_MS: i64 = 366 * 5 * 86_400_000;

fn unit_ms(unit: &str) -> Option<i64> {
    // Trimming the plural turns "s" itself into nothing, so keep the original
    // when there'd be no word left — "45s" is seconds, not a mystery.
    let base = unit.trim_end_matches('s');
    let unit = if base.is_empty() { unit } else { base };
    // Singular, plural and the one-letter form people actually type.
    Some(match unit {
        "second" | "sec" | "s" => 1_000,
        "minute" | "min" | "m" => 60_000,
        "hour" | "hr" | "h" => 3_600_000,
        "day" | "d" => 86_400_000,
        "week" | "wk" | "w" => 604_800_000,
        "month" => 30 * 86_400_000,
        "year" | "y" => 365 * 86_400_000,
        _ => return None,
    })
}

/// Days in a month, Gregorian.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// Midnight UTC on a date, ms since epoch. None for a date that doesn't exist
/// (31 February is a typo, not a day).
fn utc_midnight(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    // Days since 1970 by counting whole years then whole months. Simple beats
    // clever here, and the range this is used over is tiny.
    let mut days: i64 = 0;
    if year >= 1970 {
        for y in 1970..year {
            days += if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 366 } else { 365 };
        }
    } else {
        return None;
    }
    for m in 1..month {
        days += days_in_month(year, m);
    }
    days += day - 1;
    Some(days * 86_400_000)
}

/// The hour of day a bare date lands on, in the asker's own time.
///
/// A date carries no time, and midnight is a bad guess — it is the middle of
/// somebody's night. Nine in the morning is a reasonable reading of "on the
/// 9th", and now that the server knows the offset it is nine in the morning
/// where they actually are.
const DATE_HOUR_LOCAL: i64 = 9;

/// This person's minutes-to-add-to-UTC, as their client reported on connect.
/// Zero for anyone who has never connected with a client that sends it, which
/// is exactly the old behaviour rather than a wrong one.
pub async fn offset_for(state: &SharedState, user_id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT tz_offset_minutes FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
}

/// Parse the text following `/remindme`.
///
/// `now` is passed in rather than read, so the tests aren't a coin flip.
/// `offset_min` is the asker's minutes from UTC; it moves dates and leaves
/// durations alone, because six hours is six hours anywhere.
pub fn parse(rest: &str, now: i64, offset_min: i64) -> Result<Reminder, &'static str> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Err("give me a time and a message: `/remindme 2 hours check the oven`");
    }
    let mut words = rest.split_whitespace();
    let first = words.next().unwrap_or_default();

    // "9/7/2026" or "9/7" — month/day, US order, since that is what this
    // crew writes.
    if first.contains('/') {
        let parts: Vec<&str> = first.split('/').collect();
        let nums: Option<Vec<i64>> = parts.iter().map(|p| p.parse::<i64>().ok()).collect();
        let Some(nums) = nums else {
            return Err("I couldn't read that date — try 9/7/2026, or `3 days`");
        };
        let (month, day, year) = match nums.as_slice() {
            [m, d] => {
                // No year given: this year, or next if it has already gone —
                // judged by their calendar, not the server's.
                let year = year_of(now + offset_min * 60_000);
                match utc_midnight(year, *m, *d) {
                    Some(at) if local_nine(at, offset_min) > now => (*m, *d, year),
                    _ => (*m, *d, year + 1),
                }
            }
            [m, d, y] => (*m, *d, if *y < 100 { 2000 + *y } else { *y }),
            _ => return Err("I couldn't read that date — try 9/7/2026, or `3 days`"),
        };
        let Some(midnight) = utc_midnight(year, month, day) else {
            return Err("that date doesn't exist");
        };
        let due_at = local_nine(midnight, offset_min);
        let text = words.collect::<Vec<_>>().join(" ");
        return finish(due_at, text, now);
    }

    // "7 days ..." or "7days ..." or "7d ..."
    let (amount, unit, remainder): (i64, String, Vec<&str>) = {
        let digits: String = first.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return Err("start with a time: `/remindme 30 minutes stretch` or `/remindme 9/7/2026 ...`");
        }
        let amount: i64 = digits.parse().map_err(|_| "that number is too big")?;
        let glued: String = first[digits.len()..].to_owned();
        if glued.is_empty() {
            // The unit is the next word.
            let Some(unit) = words.next() else {
                return Err("hours? days? weeks? — `/remindme 6 hours ...`");
            };
            (amount, unit.to_lowercase(), words.collect())
        } else {
            (amount, glued.to_lowercase(), words.collect())
        }
    };

    let Some(step) = unit_ms(&unit) else {
        return Err("I know seconds, minutes, hours, days, weeks, months and years");
    };
    let Some(delta) = amount.checked_mul(step) else {
        return Err("that's further ahead than I can count");
    };
    finish(now + delta, remainder.join(" "), now)
}

/// 09:00 local on the day `midnight` names, as a UTC instant.
///
/// Subtracting the offset is the part worth staring at: being AHEAD of UTC
/// means your morning happens EARLIER in UTC, so a positive offset makes the
/// instant smaller.
fn local_nine(midnight: i64, offset_min: i64) -> i64 {
    midnight + DATE_HOUR_LOCAL * 3_600_000 - offset_min * 60_000
}

fn finish(due_at: i64, text: String, now: i64) -> Result<Reminder, &'static str> {
    if due_at <= now {
        return Err("that's in the past");
    }
    if due_at - now > MAX_AHEAD_MS {
        return Err("that's more than five years out — I'd forget");
    }
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err("what should I remind you about?");
    }
    Ok(Reminder { due_at, text })
}

/// The UTC year a timestamp falls in.
fn year_of(ms: i64) -> i64 {
    let mut days = ms / 86_400_000;
    let mut year = 1970;
    loop {
        let len = if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 { 366 } else { 365 };
        if days < len {
            return year;
        }
        days -= len;
        year += 1;
    }
}

/// Y-M-D H:M in the reader's own time, for saying back exactly when this
/// will arrive. UTC is only what it degrades to when nobody told us better.
pub fn describe_local(ms: i64, offset_min: i64) -> String {
    let stamp = describe(ms + offset_min * 60_000);
    if offset_min == 0 {
        stamp
    } else {
        stamp.trim_end_matches(" UTC").to_owned()
    }
}

/// Y-M-D H:M UTC, for saying back exactly when this will arrive.
pub fn describe(ms: i64) -> String {
    let year = year_of(ms);
    let mut days = ms / 86_400_000;
    for y in 1970..year {
        days -= if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 366 } else { 365 };
    }
    let mut month = 1;
    while month <= 12 && days >= days_in_month(year, month) {
        days -= days_in_month(year, month);
        month += 1;
    }
    let day = days + 1;
    let mins = (ms % 86_400_000) / 60_000;
    format!("{year}-{month:02}-{day:02} {:02}:{:02} UTC", mins / 60, mins % 60)
}

/// Store one. Returns what to say back.
pub async fn schedule(
    state: &SharedState,
    user_id: i64,
    reminder: &Reminder,
    offset_min: i64,
) -> anyhow::Result<String> {
    sqlx::query("INSERT INTO reminders (user_id, due_at, text, created_at) VALUES (?, ?, ?, ?)")
        .bind(user_id)
        .bind(reminder.due_at)
        .bind(&reminder.text)
        .bind(now_ms())
        .execute(&state.db)
        .await?;
    Ok(format!(
        "👍 I'll DM you at {}",
        describe_local(reminder.due_at, offset_min)
    ))
}

/// The DM channel between the bot and this person, created if it's their
/// first. Mirrors routes::create_dm's naming, which the UNIQUE on
/// channels.name depends on.
async fn bot_dm_channel(state: &SharedState, user_id: i64) -> anyhow::Result<i64> {
    let bot_id = state.bot_user().id;
    let (low, high) = if bot_id < user_id { (bot_id, user_id) } else { (user_id, bot_id) };
    if let Some(id) = sqlx::query_scalar::<_, i64>(
        "SELECT c.id FROM channels c \
         JOIN dm_members a ON a.channel_id = c.id AND a.user_id = ? \
         JOIN dm_members b ON b.channel_id = c.id AND b.user_id = ? \
         WHERE c.kind = 'dm' LIMIT 1",
    )
    .bind(bot_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(id);
    }
    let id = sqlx::query("INSERT INTO channels (name, kind, created_at) VALUES (?, 'dm', ?)")
        .bind(format!("dm:{low}:{high}"))
        .bind(now_ms())
        .execute(&state.db)
        .await?
        .last_insert_rowid();
    for member in [bot_id, user_id] {
        sqlx::query("INSERT INTO dm_members (channel_id, user_id) VALUES (?, ?)")
            .bind(id)
            .bind(member)
            .execute(&state.db)
            .await?;
    }
    Ok(id)
}

/// Deliver anything due. Runs forever; started from main.
pub async fn sweep_forever(state: SharedState) {
    loop {
        tokio::time::sleep(SWEEP_EVERY).await;
        if let Err(e) = sweep_once(&state).await {
            tracing::warn!("reminder sweep failed: {e}");
        }
    }
}

async fn sweep_once(state: &SharedState) -> anyhow::Result<()> {
    let due: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT id, user_id, text FROM reminders \
         WHERE delivered_at IS NULL AND due_at <= ? ORDER BY due_at LIMIT 50",
    )
    .bind(now_ms())
    .fetch_all(&state.db)
    .await?;

    for (id, user_id, text) in due {
        // Marked first. A send that fails is better than one delivered twice
        // every thirty seconds because the mark never landed.
        sqlx::query("UPDATE reminders SET delivered_at = ? WHERE id = ?")
            .bind(now_ms())
            .bind(id)
            .execute(&state.db)
            .await?;
        let channel_id = match bot_dm_channel(state, user_id).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("no DM channel for reminder {id}: {e}");
                continue;
            }
        };
        if let Err(e) = crate::bot::post_message(state, channel_id, &format!("⏰ {text}")).await {
            tracing::warn!("reminder {id} not delivered: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-31 12:00 UTC, so the assertions below are arithmetic rather
    /// than a race with the clock.
    const NOW: i64 = 1_788_177_600_000;

    /// US Eastern in summer: five hours behind UTC.
    const EASTERN: i64 = -300;

    #[test]
    fn durations_in_the_shapes_people_type() {
        let cases = [
            ("6 hours check the oven", 6 * 3_600_000, "check the oven"),
            ("6h check the oven", 6 * 3_600_000, "check the oven"),
            ("6hours check the oven", 6 * 3_600_000, "check the oven"),
            ("7 days water the plants", 7 * 86_400_000, "water the plants"),
            ("1 day singular unit", 86_400_000, "singular unit"),
            ("30 minutes stretch", 30 * 60_000, "stretch"),
            ("2 weeks rent", 2 * 604_800_000, "rent"),
            ("45s soft boiled", 45_000, "soft boiled"),
        ];
        for (input, delta, text) in cases {
            let r = parse(input, NOW, 0).unwrap_or_else(|e| panic!("{input:?} -> {e}"));
            assert_eq!(r.due_at, NOW + delta, "{input:?}");
            assert_eq!(r.text, text, "{input:?}");
        }
    }

    #[test]
    fn a_duration_means_the_same_thing_in_every_timezone() {
        // Six hours is six hours. Only dates move.
        for tz in [-720, -300, 0, 330, 780] {
            let r = parse("6 hours check the oven", NOW, tz).unwrap();
            assert_eq!(r.due_at, NOW + 6 * 3_600_000, "offset {tz}");
        }
    }

    #[test]
    fn dates_land_on_the_right_day() {
        let r = parse("9/7/2026 dentist", NOW, 0).expect("date parses");
        assert_eq!(describe(r.due_at), "2026-09-07 09:00 UTC");
        assert_eq!(r.text, "dentist");

        assert_eq!(describe(parse("12/25/26 turkey", NOW, 0).unwrap().due_at), "2026-12-25 09:00 UTC");
        assert_eq!(describe(parse("1/1 new year", NOW, 0).unwrap().due_at), "2027-01-01 09:00 UTC");
        assert_eq!(describe(parse("12/25 soon", NOW, 0).unwrap().due_at), "2026-12-25 09:00 UTC");
    }

    #[test]
    fn a_date_means_nine_in_the_morning_where_the_asker_is() {
        // Asked from US Eastern, "the 7th" is 09:00 Eastern. Being five hours
        // BEHIND UTC means their morning happens five hours LATER in UTC, so
        // 14:00 UTC — not 09:00 UTC, which would have been 4am for them.
        let r = parse("9/7/2026 dentist", NOW, EASTERN).unwrap();
        assert_eq!(describe(r.due_at), "2026-09-07 14:00 UTC");
        assert_eq!(describe_local(r.due_at, EASTERN), "2026-09-07 09:00");

        // Ahead of UTC, their morning happens earlier in UTC. India is +5:30,
        // which also checks that a half-hour offset survives.
        let india = parse("9/7/2026 dentist", NOW, 330).unwrap();
        assert_eq!(describe(india.due_at), "2026-09-07 03:30 UTC");
        assert_eq!(describe_local(india.due_at, 330), "2026-09-07 09:00");

        // Whatever the offset, it is 9am on the 7th to the person who asked.
        // (describe_local keeps the " UTC" label when the offset really is
        // zero, so compare the date and time rather than the whole string.)
        for tz in [-720, -480, -300, -60, 0, 60, 330, 540, 780] {
            let r = parse("9/7/2026 x", NOW, tz).unwrap();
            let local = describe_local(r.due_at, tz);
            assert!(
                local.starts_with("2026-09-07 09:00"),
                "offset {tz} gave {local}"
            );
        }
    }

    #[test]
    fn a_yearless_date_rolls_forward_by_the_askers_calendar() {
        // NOW is 2026-08-31 12:00 UTC. Typing "8/31" means different years to
        // two people, and that is the whole point of holding the offset:
        //
        //   on UTC        it is already noon, so 9am today has gone -> 2027
        //   on UTC-12     it is only just midnight, 9am is ahead     -> 2026
        let on_utc = parse("8/31 late", NOW, 0).unwrap();
        assert!(
            describe_local(on_utc.due_at, 0).starts_with("2027-08-31 09:00"),
            "{}", describe_local(on_utc.due_at, 0)
        );

        let way_west = parse("8/31 late", NOW, -720).unwrap();
        assert!(
            describe_local(way_west.due_at, -720).starts_with("2026-08-31 09:00"),
            "{}", describe_local(way_west.due_at, -720)
        );
        // And it really is still in the future for them, not merely labelled so.
        assert!(way_west.due_at > NOW);
    }

    #[test]
    fn nonsense_gets_a_useful_answer_rather_than_a_wrong_one() {
        for bad in [
            "",
            "hello there",
            "5 bananas peel them",
            "2 hours",
            "9/7/2026",
            "2/31/2027 impossible",
            "1/1/2020 last year",
            "900 years outlive me",
        ] {
            assert!(parse(bad, NOW, 0).is_err(), "{bad:?} should not have parsed");
        }
    }

    #[test]
    fn leap_years_are_counted_properly() {
        assert_eq!(days_in_month(2028, 2), 29);
        assert_eq!(days_in_month(2027, 2), 28);
        assert_eq!(days_in_month(2100, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(describe(parse("2/29/2028 leap day", NOW, 0).unwrap().due_at), "2028-02-29 09:00 UTC");
        assert!(parse("2/29/2027 not a leap year", NOW, 0).is_err());
    }

    #[test]
    fn describe_round_trips_the_epoch_and_a_known_date() {
        assert_eq!(describe(0), "1970-01-01 00:00 UTC");
        assert_eq!(describe(NOW), "2026-08-31 12:00 UTC");
    }
}
