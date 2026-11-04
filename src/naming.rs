//! Naming rotated generations, and recognising them again afterwards.
//!
//! [`match_generation`] is the only thing standing between this dog and
//! deleting a file it did not write, so it is strict by construction: a
//! candidate matches only if it has the exact shape [`dated_name`] or
//! [`numeric_name`] would have produced for this base path. Everything else
//! is somebody else's file. When an edge case is arguable, the answer here is
//! always "not a match" - a missed generation costs disk, a false match costs
//! an operator their data.

#[cfg(test)]
use crate::config::Naming;
#[cfg(test)]
use std::{path::Path, time::SystemTime};

// The split is building a name from recognising one: `build` writes it,
// `recognise` reads it back, and neither half means anything alone. What both
// sides have to agree on stays here, where a drift would show up as a name
// that never matches itself back.
mod build;
mod recognise;

pub use build::{LogPath, dated_name, numeric_name, stamp_utc, with_gz};
pub use recognise::{Order, match_generation, unstamp_utc};

/// The shape of a stamp, one byte per position: `d` is any ASCII digit, and
/// every other byte must appear literally.
///
/// Both the formatter and the matcher are written against this one constant,
/// so the two cannot drift apart.
const STAMP_SHAPE: &[u8] = b"dddd-dd-ddTdd-dd-dd";

/// How a stamp is rendered. Kept next to [`STAMP_SHAPE`], which it must agree
/// with, and deliberately colon-free: a colon is not a portable filename byte.
const STAMP_FORMAT: &str = "%Y-%m-%dT%H-%M-%S";

/// The `.gz` this dog adds when it compresses a generation.
///
/// Named once, here, because [`with_gz`] writes it and [`match_generation`]
/// strips it. Two copies in the two files is two places for that agreement to
/// drift.
const GZ_SUFFIX: &str = ".gz";

/// Whether `candidate` is exactly the 19 bytes of [`STAMP_SHAPE`], and every
/// field is in range for a real UTC civil datetime.
///
/// The range check is stricter than the shape alone needs. It costs nothing
/// and it means an operator's own `app.9999-99-99T99-99-99.log` is never
/// mistaken for something this dog wrote, while every stamp [`stamp_utc`]
/// produces still matches.
fn is_stamp(candidate: &[u8]) -> bool {
    if candidate.len() != STAMP_SHAPE.len() {
        return false;
    }
    let shaped = candidate
        .iter()
        .zip(STAMP_SHAPE)
        .all(|(byte, shape)| match shape {
            b'd' => byte.is_ascii_digit(),
            literal => byte == literal,
        });
    if !shaped {
        return false;
    }
    // Positions are fixed by the shape above, so every slice below is digits.
    let field = |from: usize, to: usize| -> u32 {
        candidate[from..to]
            .iter()
            .fold(0, |acc, byte| acc * 10 + u32::from(byte - b'0'))
    };
    // The date is handed to jiff rather than range-checked field by field.
    // Independent ranges accept dates that never happened -- 2026-02-30,
    // 2026-02-29 in a year that is not a leap year, April 31st -- and every
    // one of those is a name `stamp_utc` cannot write, so accepting it hands
    // the pruner a file this dog did not create.
    //
    // The year is bounded below at the epoch for the same reason: `stamp_utc`
    // renders a real instant, so a generation dated 1969 is somebody else's
    // file whatever else it looks like.
    let (year, month, day) = (field(0, 4), field(5, 7), field(8, 10));
    if !(1970..=9999).contains(&year) {
        return false;
    }
    let Ok(month) = i8::try_from(month) else {
        return false;
    };
    let Ok(day) = i8::try_from(day) else {
        return false;
    };
    let Ok(year) = i16::try_from(year) else {
        return false;
    };
    if jiff::civil::Date::new(year, month, day).is_err() {
        return false;
    }
    // The time stays hand-checked. Parsing it with jiff would accept second
    // 60 as a leap second, which `stamp_utc` never writes and which the
    // nonsense-digits test already pins as a refusal.
    field(11, 13) <= 23        // hour
        && field(14, 16) <= 59 // minute
        && field(17, 19) <= 59 // second
}

/// The log path every naming test splits: `/var/log/web-0-out.log`.
#[cfg(test)]
fn base() -> LogPath {
    LogPath::split(Path::new("/var/log/web-0-out.log")).expect("splits")
}

/// Assert that no entry of `decoys` is a generation of `base` under `naming`.
///
/// The message names the decoy that matched, so a failure points at the one
/// entry that changed rather than at the list.
#[cfg(test)]
fn assert_no_match(base: &LogPath, naming: Naming, decoys: &[&str]) {
    for decoy in decoys {
        assert!(
            match_generation(base, naming, decoy).is_none(),
            "{decoy} must not match"
        );
    }
}

/// A dated [`Order`] for a test to compare a match against.
#[cfg(test)]
fn dated(stamp: &str, counter: u32) -> Order {
    Order::Dated {
        stamp: stamp.into(),
        counter,
    }
}

/// The file name of a path a naming helper just built.
#[cfg(test)]
fn name_of(path: &Path) -> &str {
    path.file_name().expect("named").to_str().expect("utf8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn every_name_with_gz_writes_is_matched_back_as_compressed() {
        // The pairing that puts `with_gz` in this module: the suffix it
        // writes is the suffix `match_generation` strips. A generation
        // whose compressed name did not match back would be a file this
        // dog created and could never prune.
        let base = base();
        let dated_gen = dated_name(&base, "2026-08-20T15-04-05", 0);
        let numeric = numeric_name(&base, 3);
        for (path, naming) in [(dated_gen, Naming::Dated), (numeric, Naming::Numeric)] {
            let gz = with_gz(&path);
            let name = name_of(&gz);
            let (order, compressed) =
                match_generation(&base, naming, name).expect("its own .gz name matches back");
            assert!(compressed, "{name} is compressed");
            let plain = name_of(&path);
            let (plain_order, _) =
                match_generation(&base, naming, plain).expect("the plain name matches back");
            assert_eq!(
                order, plain_order,
                "{name} is the same generation as {plain}, wearing a second file"
            );
        }
    }

    #[test]
    fn a_generated_dated_name_matches_itself_back() {
        for counter in [0, 1, 42] {
            let made = dated_name(&base(), "2026-08-20T15-04-05", counter);
            let name = name_of(&made);
            let (order, compressed) =
                match_generation(&base(), Naming::Dated, name).expect("matches");
            assert!(!compressed);
            assert_eq!(order, dated("2026-08-20T15-04-05", counter));
        }
    }

    #[test]
    fn every_stamp_this_dog_writes_is_matched_back() {
        // The one property the safety of pruning rests on: whatever
        // stamp_utc produces, match_generation recognises. A stamp it could
        // not recognise would be a file it could never prune.
        let seconds = [
            0,             // the epoch itself
            1,             //
            951_782_400,   // 2000-02-29, a leap day
            1_787_324_645, // the pinned example
            4_102_444_800, // 2100-01-01, past the 32-bit rollover
        ];
        let times = seconds
            .into_iter()
            .map(|secs| std::time::UNIX_EPOCH + core::time::Duration::from_secs(secs))
            .chain([SystemTime::now()]);
        for at in times {
            let stamp = stamp_utc(at);
            assert!(is_stamp(stamp.as_bytes()), "{stamp} is not a valid stamp");
            for counter in [0, 1, 9, 4_294_967_295] {
                let made = dated_name(&base(), &stamp, counter);
                let name = name_of(&made);
                assert_eq!(
                    match_generation(&base(), Naming::Dated, name),
                    Some((dated(&stamp, counter), false)),
                    "{name} did not match itself back"
                );
            }
        }
    }

    #[test]
    fn every_numeric_name_this_dog_writes_is_matched_back() {
        for n in [1, 2, 9, 10, 99, 4_294_967_295] {
            let made = numeric_name(&base(), n);
            let name = name_of(&made);
            assert_eq!(
                match_generation(&base(), Naming::Numeric, name),
                Some((Order::Numeric { n }, false)),
                "{name} did not match itself back"
            );
        }
    }

    #[test]
    fn odd_base_paths_still_round_trip_through_both_schemes() {
        // Whatever a log file is called, a name this dog builds from it must
        // be a name this dog recognises. Anything it builds and cannot
        // recognise is a file it can never prune.
        let odd = [
            "/var/log/web-0-out.log",
            "/var/log/web-out",       // no extension
            "/var/log/web.0.out.log", // dots inside the stem
            "/var/log/.hidden.log",   // leading dot
            "/var/log/.hidden",       // leading dot, no extension
            "/var/log/web-0-out.",    // trailing dot
            "/var/log/a",             // one character
            "/var/log/log.log",       // stem equal to the extension
            "/var/log/web-0-out.gz",  // an extension we also use as a suffix
            "/var/log/w b.l g",       // spaces
            "/var/log/日誌.log",      // not ASCII
            "web-0-out.log",          // relative
        ];
        for path in odd {
            let base = LogPath::split(Path::new(path)).expect("splits");
            let made = dated_name(&base, "2026-08-20T15-04-05", 4);
            let name = name_of(&made);
            assert_eq!(
                match_generation(&base, Naming::Dated, name),
                Some((dated("2026-08-20T15-04-05", 4), false)),
                "{path}: {name} did not match itself back"
            );

            let numeric = numeric_name(&base, 2);
            let name = name_of(&numeric);
            assert_eq!(
                match_generation(&base, Naming::Numeric, name),
                Some((Order::Numeric { n: 2 }, false)),
                "{path}: {name} did not match itself back"
            );

            // The live file is never one of its own generations.
            let live = base.live();
            let name = name_of(&live);
            for naming in [Naming::Dated, Naming::Numeric] {
                assert!(
                    match_generation(&base, naming, name).is_none(),
                    "{path}: the live file matched under {naming:?}"
                );
            }
        }
    }

    #[test]
    fn a_base_without_an_extension_round_trips_and_still_refuses_decoys() {
        let bare = LogPath::split(Path::new("/var/log/web-out")).expect("splits");
        let made = dated_name(&bare, "2026-08-20T15-04-05", 3);
        assert_eq!(made, Path::new("/var/log/web-out.2026-08-20T15-04-05.3"));
        assert_eq!(
            match_generation(&bare, Naming::Dated, "web-out.2026-08-20T15-04-05.3"),
            Some((dated("2026-08-20T15-04-05", 3), false))
        );

        assert_eq!(numeric_name(&bare, 1), Path::new("/var/log/web-out.1"));
        assert_eq!(
            match_generation(&bare, Naming::Numeric, "web-out.1"),
            Some((Order::Numeric { n: 1 }, false))
        );

        // An extension where the base has none is a different file entirely.
        for decoy in ["web-out.2026-08-20T15-04-05.log", "web-out", "web-outer.1"] {
            for naming in [Naming::Dated, Naming::Numeric] {
                assert!(
                    match_generation(&bare, naming, decoy).is_none(),
                    "{decoy} must not match under {naming:?}"
                );
            }
        }
    }

    #[test]
    fn a_stem_with_its_own_dots_keeps_only_the_final_extension() {
        let dotted = LogPath::split(Path::new("/var/log/web.0.out.log")).expect("splits");
        assert_eq!(dotted.stem, "web.0.out");
        assert_eq!(dotted.ext.as_deref(), Some("log"));
        assert_eq!(dotted.live(), Path::new("/var/log/web.0.out.log"));
        assert_eq!(
            match_generation(&dotted, Naming::Dated, "web.0.out.2026-08-20T15-04-05.log"),
            Some((dated("2026-08-20T15-04-05", 0), false))
        );
        // Losing one of the stem's own dots makes it a different file.
        assert!(
            match_generation(&dotted, Naming::Dated, "web.0out.2026-08-20T15-04-05.log").is_none()
        );
    }

    #[test]
    fn every_stamp_reads_back_to_the_second_it_named() {
        for secs in [0, 1, 951_782_400, 1_787_324_645, 4_102_444_800] {
            let at = std::time::UNIX_EPOCH + core::time::Duration::from_secs(secs);
            let stamp = stamp_utc(at);
            assert_eq!(unstamp_utc(&stamp), Some(at), "{stamp}");
        }
        // Sub-second precision is not in the stamp, so it is not read back.
        let uneven = std::time::UNIX_EPOCH + core::time::Duration::from_millis(1_787_324_645_750);
        assert_eq!(
            unstamp_utc(&stamp_utc(uneven)),
            Some(std::time::UNIX_EPOCH + core::time::Duration::from_secs(1_787_324_645))
        );
        for junk in [
            "",
            "2026-08-21",
            "2026-08-21T15:04:05",
            "2026-02-30T00-00-00",
        ] {
            assert_eq!(unstamp_utc(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn a_broken_clock_still_produces_a_matchable_stamp() {
        // Before the epoch and past year 9999 both clamp, because a year that
        // is not four digits would make a name this dog could never prune.
        let before = std::time::UNIX_EPOCH - core::time::Duration::from_secs(86_400);
        assert_eq!(stamp_utc(before), "1970-01-01T00-00-00");

        let after = std::time::UNIX_EPOCH + core::time::Duration::from_secs(300_000_000_000);
        let stamp = stamp_utc(after);
        assert!(stamp.starts_with("9999-"), "{stamp}");
        assert!(is_stamp(stamp.as_bytes()), "{stamp}");
    }
}
