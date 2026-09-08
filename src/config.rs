//! The `[log-rotate]` section of `dogs.toml`.
//!
//! The daemon serves this per request rather than caching it, so this dog
//! re-reads it every tick and never caches it either. Changing `max_size`
//! should not need a `shep disable` and `shep enable`.
//!
//! It used to be `[dog.log-rotate]` in `shep.toml`. A shepherd carrying the
//! move reads any such section still there on its first boot, writes it into
//! `dogs.toml` under the bare name, and strikes it from `shep.toml`. The
//! body that reaches this module is unchanged either way, because the
//! daemon serves the table without its header. What did change is the
//! header [`PRINT_CONFIG`] prints: pasting the old one back into
//! `shep.toml` after a migration leaves the same dog named in both files,
//! which the daemon refuses to boot on rather than guess between.

use core::fmt;

use schemars::JsonSchema;
use serde::Deserialize;
use shep_client::{
    dogs::DogConfig,
    shep_core::values::{MemSize, ParseMemSizeError, ParseUpDurationError, UpDuration},
};

/// How rotated generations are named. See the README for the trade-off.
///
/// `JsonSchema` so [`Section::naming`] can publish the two spellings as a
/// closed set rather than as an open string: lookout renders a schema's
/// `enum` as a value an operator cycles through, and a bare `string` as one
/// they have to know how to spell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[schemars(
    rename_all = "lowercase",
    description = "How a rotated generation is named."
)]
pub enum Naming {
    /// `web-0-out.2026-08-20T15-04-05.log`. The default.
    #[schemars(description = "web-0-out.2026-08-20T15-04-05.log, in UTC, still matching *.log.")]
    Dated,
    /// `web-0-out.log.1`, shifting on every rotation. Newest is `.1`.
    #[schemars(description = "web-0-out.log.1, shifting along on every rotation. Newest is .1.")]
    Numeric,
}

/// The dog's settings, with a default for every field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Rotate a log once it reaches this size.
    pub max_size: MemSize,
    /// Optionally also rotate this long after the last rotation, whatever
    /// the size. A log never rotated counts from when it appeared, or from
    /// its last write where the filesystem keeps no birth time.
    pub max_age: Option<UpDuration>,
    /// Generations to keep. Older ones are deleted.
    pub keep: usize,
    /// How rotated generations are named.
    pub naming: Naming,
    /// gzip rotated generations, newest one left plain so it stays greppable.
    pub compress: bool,
    /// How often to look.
    pub interval: UpDuration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_size: MemSize::from_bytes(10 * 1024 * 1024),
            max_age: None,
            keep: 5,
            naming: Naming::Dated,
            compress: true,
            interval: UpDuration::from_millis(60_000),
        }
    }
}

/// What could not be understood in a `[log-rotate]` section.
///
/// Every variant names the offending field, and where possible the value
/// that was rejected, so an operator can find the typo without reading this
/// dog's source.
#[derive(Debug)]
pub enum ConfigError {
    /// The text was not valid TOML, or carried a key this dog does not know.
    ///
    /// Names no section. [`Error::Config`](crate::error::Error::Config) wraps
    /// this and supplies the `[<name>]` the text came from, so spelling one
    /// here would print two sections for one fault and get one of them wrong
    /// for any dog not adopted under the default name.
    Toml(String),
    /// A `max_size` or similar was not spelled the way shep spells it.
    Size {
        /// The field the offending value was read from.
        field: &'static str,
        /// The value as written in `dogs.toml`.
        value: String,
        /// The underlying parse failure.
        source: ParseMemSizeError,
    },
    /// A `max_age`/`interval` or similar was not spelled the way shep spells
    /// it.
    Duration {
        /// The field the offending value was read from.
        field: &'static str,
        /// The value as written in `dogs.toml`.
        value: String,
        /// The underlying parse failure.
        source: ParseUpDurationError,
    },
    /// `naming` was neither `dated` nor `numeric`.
    Naming(String),
    /// `keep = 0`, which would delete every rotation the moment it was made.
    Keep,
    /// `interval = 0`, which would ask the shepherd again the moment an
    /// answer came back.
    Interval,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(message) => write!(f, "invalid TOML: {message}"),
            Self::Size {
                field,
                value,
                source,
            } => write!(
                f,
                "{field} = \"{value}\" is not a size shep accepts: {source}"
            ),
            Self::Duration {
                field,
                value,
                source,
            } => write!(
                f,
                "{field} = \"{value}\" is not a duration shep accepts: {source}"
            ),
            Self::Naming(value) => write!(
                f,
                "naming = \"{value}\" is not a naming scheme; use \"dated\" or \"numeric\""
            ),
            Self::Keep => write!(
                f,
                "keep = 0 would delete every rotation the moment it was made; keep must be at least 1"
            ),
            Self::Interval => write!(
                f,
                "interval = 0 would ask the shepherd without pause; interval must be above 0"
            ),
        }
    }
}

impl core::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Size { source, .. } => Some(source),
            Self::Duration { source, .. } => Some(source),
            Self::Toml(_) | Self::Naming(_) | Self::Keep | Self::Interval => None,
        }
    }
}

/// The section's fields, and the type shep reads this dog's config schema
/// off. `main` hands it to `shep_client::dogs::probe`, which answers
/// `--schema` with it during `shep adopt` and again whenever lookout opens
/// the settings pane.
///
/// Every field is read as a string so `max_size` and `max_age` go through
/// shep's own `FromStr` rather than serde's numeric deserializers.
/// Deserializing a bare number would silently accept spellings shep itself
/// refuses.
///
/// The schema says something narrower than the Rust type, deliberately.
/// `#[schemars(with = ...)]` publishes shep's own grammar for each of those
/// fields instead of the bare `string` an `Option<String>` would produce,
/// which buys two things. The pattern stays in shep-core, so this crate
/// carries no second copy of a grammar to drift from. And lookout reads the
/// `$ref` name to decide what a unitless number means, so `max_size = "10"`
/// renders as bytes and `interval = "10"` as milliseconds, which is what
/// each of them actually is.
///
/// `DogConfig` carries no `#[shep(secret)]`, because nothing in here is a
/// credential: this dog is told about sizes and durations and never about
/// where to send anything. The derive is still what lets `config_schema`
/// publish the section at all, and a config type with nothing to mark still
/// wants the impl.
///
/// Field docs are not decoration here. lookout shows a property's
/// `description` beside it in the pane, on one line and clipped to the
/// terminal's width, so each of them leads with the sentence an operator
/// needs and leaves the rest to [`PRINT_CONFIG`] and the README.
///
/// `title` and `description` are given rather than taken from this comment
/// for the same reason. schemars would publish everything above as the
/// section's own description, and everything above is written for whoever
/// maintains this file. `Section` is not a name an operator has any use for
/// either.
///
/// `Debug` is derived rather than written by hand, which is the decision the
/// checked-in rules ask for out loud. Every field here is a size, a
/// duration, a count or a flag an operator typed into `dogs.toml`. None of
/// them is a path, an environment, or a credential, so there is nothing for
/// a hand-written impl to redact and no exact-string test to pin it with.
/// [`Live`](crate::tick::Live) is the type in this crate that goes the other
/// way, and it does so because it holds a socket path.
#[derive(Debug, Deserialize, JsonSchema, DogConfig)]
#[serde(deny_unknown_fields)]
#[schemars(
    title = "log-rotate",
    description = "Settings for the shep-log-rotate dog: when to rotate a log, how to name \
                   what it rotates into, and how much of it to keep."
)]
pub struct Section {
    /// Rotate a log once it reaches this size. shep's spelling: 10M, not 10MB.
    #[schemars(with = "Option<MemSize>")]
    max_size: Option<String>,
    /// Also rotate this long after the last rotation, whatever the size.
    /// Unset means size only.
    #[schemars(with = "Option<UpDuration>")]
    max_age: Option<String>,
    /// Generations to keep. Older ones are deleted.
    // The floor is here as well as in `from_toml` because this is the copy a
    // settings pane reads. Without it the pane offers a `keep = 0` that the
    // dog then refuses on its next tick, in a file the operator has already
    // saved and moved on from.
    #[schemars(range(min = 1))]
    keep: Option<usize>,
    /// How rotated generations are named: "dated" stamps the time into the
    /// name, "numeric" shifts a .1 suffix along on every rotation.
    #[schemars(with = "Option<Naming>")]
    naming: Option<String>,
    /// gzip rotated generations. The newest is left plain so it stays greppable.
    compress: Option<bool>,
    /// How often to look for a log worth rotating.
    #[schemars(with = "Option<UpDuration>")]
    interval: Option<String>,
}

/// Parse `value` into a [`MemSize`], naming `field` in the error.
fn parse_size(value: String, field: &'static str) -> Result<MemSize, ConfigError> {
    value
        .parse::<MemSize>()
        .map_err(|source| ConfigError::Size {
            field,
            value,
            source,
        })
}

/// Parse `value` into an [`UpDuration`], naming `field` in the error.
fn parse_duration(value: String, field: &'static str) -> Result<UpDuration, ConfigError> {
    value
        .parse::<UpDuration>()
        .map_err(|source| ConfigError::Duration {
            field,
            value,
            source,
        })
}

impl Config {
    /// Parse the `[log-rotate]` table's body.
    ///
    /// The empty string is the ordinary case: a dog with no section in
    /// `dogs.toml` gets every default.
    ///
    /// # Errors
    /// - [`ConfigError::Toml`] - the text is not valid TOML, or carries a key
    ///   this dog does not know.
    /// - [`ConfigError::Size`] / [`ConfigError::Duration`] - a value is not
    ///   spelled the way shep spells it.
    /// - [`ConfigError::Naming`] - `naming` is neither `dated` nor `numeric`.
    /// - [`ConfigError::Keep`] - `keep = 0`, which would delete every
    ///   rotation the moment it was made.
    /// - [`ConfigError::Interval`] - `interval = 0`, which would poll
    ///   without pause.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let raw: Section =
            toml::from_str(text).map_err(|err| ConfigError::Toml(err.to_string()))?;
        let defaults = Self::default();
        let interval = raw
            .interval
            .map(|value| parse_duration(value, "interval"))
            .transpose()?
            .unwrap_or(defaults.interval);
        if interval.as_duration().is_zero() {
            return Err(ConfigError::Interval);
        }
        Ok(Self {
            max_size: raw
                .max_size
                .map(|value| parse_size(value, "max_size"))
                .transpose()?
                .unwrap_or(defaults.max_size),
            max_age: raw
                .max_age
                .map(|value| parse_duration(value, "max_age"))
                .transpose()?,
            keep: match raw.keep {
                Some(0) => return Err(ConfigError::Keep),
                Some(keep) => keep,
                None => defaults.keep,
            },
            naming: match raw.naming.as_deref() {
                None => defaults.naming,
                Some("dated") => Naming::Dated,
                Some("numeric") => Naming::Numeric,
                Some(other) => return Err(ConfigError::Naming(other.to_owned())),
            },
            compress: raw.compress.unwrap_or(defaults.compress),
            interval,
        })
    }
}

/// A commented block naming every option and its default, for
/// `shep-log-rotate --print-config`.
///
/// Every line is commented, so appending it to `dogs.toml` changes nothing
/// until the operator uncomments a line. A test asserts that what survives
/// uncommenting parses back to [`Config::default()`], so this text cannot
/// drift away from the code it documents.
///
/// The header is the bare name, and the file is `dogs.toml`. It was
/// `[dog.log-rotate]` in `shep.toml` until shep moved a dog's settings into
/// a file of their own, and a shepherd finding the same dog named in both
/// files refuses to boot rather than guess which one the operator meant. So
/// this block pasted into the old place after a migration is not a stale
/// header, it is a daemon that will not start.
pub const PRINT_CONFIG: &str = r#"[log-rotate]
# Rotate a log once it reaches this size. shep's spelling: 10M, not 10MB.
#max_size = "10M"
# Optionally also rotate this long after the last rotation, whatever the
# size. A log never rotated counts from when it appeared, or from its last
# write where the filesystem keeps no birth time. Unset means size only.
# shep's UpDuration has no day unit: spell a week as hours, not "7d".
#max_age = "168h"
# Generations to keep. Older ones are deleted. Must be at least 1.
#keep = 5
# "dated" writes web-0-out.2026-08-20T15-04-05.log, in UTC, and still
# matches *.log. "numeric" writes web-0-out.log.1 and shifts on every
# rotation, the logrotate convention. Switching does not migrate existing
# files: they stop being pruned and are left for you.
#naming = "dated"
# gzip rotated generations. The newest is left plain so it stays greppable.
#compress = true
# How often to look. Must be above 0.
#interval = "60s"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::assert_no_dashes;

    #[test]
    fn an_absent_section_is_a_working_configuration() {
        let config = Config::from_toml("").expect("empty is valid");
        assert_eq!(config, Config::default());
        assert_eq!(config.max_size.bytes(), 10 * 1024 * 1024);
        assert_eq!(config.keep, 5);
        assert_eq!(config.naming, Naming::Dated);
        assert!(config.compress);
        assert_eq!(config.interval.as_duration().as_secs(), 60);
        assert_eq!(config.max_age, None);
    }

    #[test]
    fn every_field_is_read() {
        let config = Config::from_toml(
            r#"
max_size = "1M"
max_age = "168h"
keep = 3
naming = "numeric"
compress = false
interval = "5s"
"#,
        )
        .expect("valid");
        assert_eq!(config.max_size.bytes(), 1024 * 1024);
        // 168h, not "7d": UpDuration's grammar is `^\d+(h|m|s)?$`, with no
        // day unit, so "7d" is a spelling shep itself refuses. 168h is the
        // same span written the way shep actually accepts it.
        assert_eq!(
            config.max_age.expect("set").as_duration().as_secs(),
            7 * 86_400
        );
        assert_eq!(config.keep, 3);
        assert_eq!(config.naming, Naming::Numeric);
        assert!(!config.compress);
        assert_eq!(config.interval.as_duration().as_secs(), 5);
    }

    #[test]
    fn a_size_shep_refuses_is_refused_here_too() {
        // shep spells it 10M. A dog that also took 10MB would teach the wrong
        // thing about the ecosystem it lives in.
        let err = Config::from_toml(r#"max_size = "10MB""#).expect_err("refused");
        let shown = err.to_string();
        assert!(shown.contains("max_size"), "{shown}");
        assert!(shown.contains("10MB"), "{shown}");
    }

    #[test]
    fn an_unknown_key_is_reported_not_ignored() {
        let err = Config::from_toml(r#"max_sixe = "10M""#).expect_err("refused");
        assert!(err.to_string().contains("max_sixe"), "{err}");
    }

    #[test]
    fn an_unknown_naming_scheme_names_the_two_that_exist() {
        let err = Config::from_toml(r#"naming = "rolling""#).expect_err("refused");
        let shown = err.to_string();
        assert!(shown.contains("rolling"), "{shown}");
        assert!(shown.contains("dated"), "{shown}");
        assert!(shown.contains("numeric"), "{shown}");
    }

    #[test]
    fn keep_zero_is_refused_because_it_would_delete_every_rotation() {
        let err = Config::from_toml("keep = 0").expect_err("refused");
        assert!(err.to_string().contains("keep"), "{err}");
    }

    #[test]
    fn interval_zero_is_refused_because_it_would_poll_without_pause() {
        for spelling in ["0", "0s", "0m"] {
            let err =
                Config::from_toml(&format!("interval = \"{spelling}\"")).expect_err("refused");
            assert!(err.to_string().contains("interval"), "{err}");
        }
    }

    #[test]
    fn every_value_the_printed_block_documents_is_the_value_the_code_uses() {
        // PRINT_CONFIG has three kinds of line: the `[log-rotate]` header,
        // prose comments (`# ` with a space), and commented settings
        // (`#key = value`, no space). Uncomment only the settings.
        let uncommented: Vec<&str> = PRINT_CONFIG
            .lines()
            .filter_map(|line| line.strip_prefix('#'))
            .filter(|rest| !rest.starts_with(' '))
            .collect();

        // The guard needs its own guard. The first version of this test used a
        // filter that matched NOTHING, so it asserted `from_toml("")` equals
        // the defaults -- true, vacuous, and a duplicate of
        // `an_absent_section_is_a_working_configuration`. It would have passed
        // with a `7d` in the block, the one spelling `UpDuration` refuses,
        // which is precisely the drift this test exists to catch.
        assert_eq!(
            uncommented.len(),
            6,
            "expected one line per setting, got {uncommented:?}"
        );

        let config =
            Config::from_toml(&uncommented.join("\n")).expect("the printed block is valid");

        // `max_age` is the one field whose default is "unset", so the block
        // documents a sample rather than a default. Everything else must be
        // exactly what the code already does.
        assert_eq!(
            config,
            Config {
                max_age: Some("168h".parse().expect("a spelling shep accepts")),
                ..Config::default()
            }
        );
    }

    #[test]
    fn the_printed_block_carries_no_em_dash() {
        assert_no_dashes(PRINT_CONFIG);
    }

    #[test]
    fn the_printed_block_names_the_file_the_shepherd_reads_from() {
        // Not cosmetic. A shepherd finding `[dog.log-rotate]` in shep.toml
        // migrates it into dogs.toml and strikes it from the old file; find
        // it in BOTH and it refuses to boot rather than guess. So a block
        // that reverted to the old header would hand an operator a way to
        // stop their daemon by following this dog's own instructions.
        let header = PRINT_CONFIG.lines().next().expect("a first line");
        assert_eq!(header, "[log-rotate]");
    }

    /// `probe` prints this to stdout for `--schema` and exits 0. The one way
    /// it can fail is a `#[shep(secret)]` that landed on no property, and
    /// then it prints the fault and exits 1 instead, which shep reads as a
    /// dog with an unreadable schema. Nothing here is marked today, so this
    /// is the guard for the day something is.
    #[test]
    fn the_schema_is_publishable() {
        shep_client::dogs::config_schema::<Section>().expect("no marked field is missing");
    }

    /// The schema's property names, sorted.
    fn schema_keys() -> Vec<String> {
        let schema = shep_client::dogs::config_schema::<Section>().expect("publishable");
        let mut keys: Vec<String> = schema
            .as_value()
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("an object schema has properties")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn the_schema_and_the_printed_block_name_the_same_settings() {
        // Three things have to agree about what this dog's settings are:
        // `Section`'s fields, the block `--print-config` prints, and the
        // schema a settings pane draws a form from. The first two already
        // had a test between them. This is the third edge, and it is the one
        // that matters most, because a pane writing a key `from_toml`
        // refuses produces a section the dog rejects on its next tick, in a
        // file the operator has already saved and walked away from.
        let mut printed: Vec<String> = PRINT_CONFIG
            .lines()
            .filter_map(|line| line.strip_prefix('#'))
            .filter(|rest| !rest.starts_with(' '))
            .filter_map(|setting| setting.split_once(' '))
            .map(|(key, _)| key.to_owned())
            .collect();
        printed.sort();

        assert_eq!(printed.len(), 6, "one key per setting, got {printed:?}");
        assert_eq!(schema_keys(), printed);
    }

    #[test]
    fn the_schema_borrows_sheps_grammars_rather_than_copying_them() {
        // A `$ref` and not an inlined pattern, for two reasons. The pattern
        // stays shep-core's, so this crate has no second copy to drift from.
        // And lookout reads the ref's NAME to decide what a unitless value
        // means, so `max_size = "10"` is shown as bytes and `interval =
        // "10"` as milliseconds. Inline the pattern and both read as raw
        // digits, which is exactly the confusion the two grammars exist to
        // settle.
        let schema = shep_client::dogs::config_schema::<Section>().expect("publishable");
        let schema = schema.as_value();

        for (field, grammar) in [
            ("max_size", "MemSize"),
            ("max_age", "UpDuration"),
            ("interval", "UpDuration"),
        ] {
            let reference = schema
                .pointer(&format!("/properties/{field}/anyOf/0/$ref"))
                .and_then(serde_json::Value::as_str);
            assert_eq!(
                reference,
                Some(format!("#/$defs/{grammar}").as_str()),
                "{field} should name shep's own {grammar}"
            );
        }

        assert!(
            schema.pointer("/$defs/MemSize/pattern").is_some(),
            "the referenced grammar carries the pattern: {schema}"
        );
    }

    #[test]
    fn the_schema_offers_the_two_naming_schemes_and_no_others() {
        // A closed set rather than a string. lookout cycles a field whose
        // schema is a `oneOf` of consts and makes an operator spell one whose
        // schema is a bare string, and `from_toml` refuses everything but
        // these two either way.
        let schema = shep_client::dogs::config_schema::<Section>().expect("publishable");
        let choices: Vec<String> = schema
            .as_value()
            .pointer("/$defs/Naming/oneOf")
            .and_then(serde_json::Value::as_array)
            .expect("a unit enum is a oneOf")
            .iter()
            .filter_map(|arm| arm.get("const").and_then(serde_json::Value::as_str))
            .map(str::to_owned)
            .collect();
        assert_eq!(choices, ["dated", "numeric"]);

        for choice in &choices {
            Config::from_toml(&format!("naming = \"{choice}\""))
                .expect("every spelling the schema offers is one from_toml takes");
        }
    }

    #[test]
    fn the_schema_refuses_a_key_this_dog_would_refuse_anyway() {
        // `deny_unknown_fields` on the Rust type and `additionalProperties:
        // false` in the schema are the same rule stated twice, once for each
        // reader. Without the second one a pane offers to add a key that
        // `from_toml` then reports as a typo.
        let schema = shep_client::dogs::config_schema::<Section>().expect("publishable");
        assert_eq!(
            schema.as_value().get("additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn the_schema_carries_no_em_dash_and_none_of_this_files_own_reasoning() {
        // Every description in here is printed to an operator, so the dash
        // rule reaches it. The second half is the one that actually slipped:
        // schemars publishes a type's doc comment as its description, and
        // this file's doc comments are long on purpose and written for
        // whoever maintains it. `Section` and `Naming` both override theirs.
        let schema = shep_client::dogs::config_schema::<Section>().expect("publishable");
        let rendered = schema.as_value().to_string();
        assert_no_dashes(&rendered);
        assert!(
            !rendered.contains("schemars"),
            "an internal doc comment reached the published schema: {rendered}"
        );
    }
}
