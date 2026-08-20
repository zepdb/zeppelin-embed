//! Pure command-line parsing for the frontier binary.

use std::error::Error;
use std::fmt;

/// Default deterministic seed used by the smoke tuner.
pub const DEFAULT_SEED: u64 = 0x27_2026_0820;

const USAGE: &str = "usage: frontier tune --campaign kernels-i8 --smoke [--seed N] | report | denominators [--persist --date YYYY-MM-DD [--allow-lower-ceiling]]";
const TUNE_RESTRICTION: &str =
    "27-H exposes only the non-campaign smoke: tune --campaign kernels-i8 --smoke";

/// A validated frontier command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Run the bounded task-27-H tuner smoke.
    Tune(TuneCommand),
    /// Print the optimization ledger summary.
    Report,
    /// Measure or persist denominator calibration.
    Denominators(DenominatorCommand),
}

/// The only tuning campaign exposed by task 27-H.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuneCampaign {
    /// The task-03 i8 kernel grid.
    KernelsI8,
}

/// Validated tuning arguments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TuneCommand {
    /// Selected campaign.
    pub campaign: TuneCampaign,
    /// Whether the bounded smoke mode was requested.
    pub smoke: bool,
    /// Deterministic search seed.
    pub seed: u64,
}

/// Validated denominator-calibration arguments.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DenominatorCommand {
    /// Persist the measured calibration.
    pub persist: bool,
    /// Caller-supplied measurement date.
    pub date: Option<String>,
    /// Permit an explicitly audited downward ceiling revision.
    pub allow_lower_ceiling: bool,
}

/// Typed command-line rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliError {
    /// No command, or invalid arity for a command without options.
    Usage,
    /// The top-level command name is not recognized.
    UnknownSubcommand { command: String },
    /// Tune did not receive a campaign.
    TuneCampaignRequired,
    /// Tune received a campaign other than `kernels-i8`.
    UnknownCampaign { campaign: String },
    /// The supported tune command omitted `--smoke`.
    SmokeRequired,
    /// `--seed` did not have a following value.
    SeedValueRequired,
    /// The seed was not a `u64`.
    InvalidSeed { value: String, reason: String },
    /// Tune received an unrecognized flag or positional argument.
    UnknownTuneArgument { argument: String },
    /// `--date` did not have a following value.
    DateValueRequired,
    /// Denominator calibration received an unrecognized argument.
    UnknownDenominatorsArgument { argument: String },
    /// Persistence-only options were supplied without `--persist`.
    PersistenceRequired,
    /// `--persist` was supplied without `--date`.
    PersistenceDateRequired,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage | Self::UnknownSubcommand { .. } => formatter.write_str(USAGE),
            Self::TuneCampaignRequired | Self::UnknownCampaign { .. } | Self::SmokeRequired => {
                formatter.write_str(TUNE_RESTRICTION)
            }
            Self::SeedValueRequired => formatter.write_str("--seed requires a value"),
            Self::InvalidSeed { reason, .. } => write!(formatter, "invalid seed: {reason}"),
            Self::UnknownTuneArgument { argument } => {
                write!(formatter, "unknown tune argument {argument}")
            }
            Self::DateValueRequired => formatter.write_str("--date requires YYYY-MM-DD"),
            Self::UnknownDenominatorsArgument { argument } => {
                write!(formatter, "unknown denominators argument {argument}")
            }
            Self::PersistenceRequired => {
                formatter.write_str("--date and --allow-lower-ceiling apply only with --persist")
            }
            Self::PersistenceDateRequired => {
                formatter.write_str("--persist requires caller-supplied --date YYYY-MM-DD")
            }
        }
    }
}

impl Error for CliError {}

/// Parses command-line arguments after the executable name without side effects.
pub fn parse_command(arguments: &[String]) -> Result<Command, CliError> {
    match arguments.first().map(String::as_str) {
        Some("tune") => parse_tune(&arguments[1..]).map(Command::Tune),
        Some("report") if arguments.len() == 1 => Ok(Command::Report),
        Some("report") => Err(CliError::Usage),
        Some("denominators") => parse_denominators(&arguments[1..]).map(Command::Denominators),
        Some(command) => Err(CliError::UnknownSubcommand {
            command: command.to_owned(),
        }),
        None => Err(CliError::Usage),
    }
}

fn parse_tune(arguments: &[String]) -> Result<TuneCommand, CliError> {
    let mut campaign = None;
    let mut smoke = false;
    let mut seed = DEFAULT_SEED;
    let mut index = 0;
    while index < arguments.len() {
        match arguments.get(index).map(String::as_str) {
            Some("--campaign") => {
                campaign = arguments.get(index + 1).cloned();
                index += 2;
            }
            Some("--smoke") => {
                smoke = true;
                index += 1;
            }
            Some("--seed") => {
                let value = arguments
                    .get(index + 1)
                    .ok_or(CliError::SeedValueRequired)?;
                seed = value
                    .parse::<u64>()
                    .map_err(|error| CliError::InvalidSeed {
                        value: value.clone(),
                        reason: error.to_string(),
                    })?;
                index += 2;
            }
            Some(argument) => {
                return Err(CliError::UnknownTuneArgument {
                    argument: argument.to_owned(),
                });
            }
            None => break,
        }
    }
    let campaign = match campaign.as_deref() {
        Some("kernels-i8") => TuneCampaign::KernelsI8,
        Some(campaign) => {
            return Err(CliError::UnknownCampaign {
                campaign: campaign.to_owned(),
            });
        }
        None => return Err(CliError::TuneCampaignRequired),
    };
    if !smoke {
        return Err(CliError::SmokeRequired);
    }
    Ok(TuneCommand {
        campaign,
        smoke,
        seed,
    })
}

fn parse_denominators(arguments: &[String]) -> Result<DenominatorCommand, CliError> {
    let mut options = DenominatorCommand::default();
    let mut index = 0;
    while index < arguments.len() {
        match arguments.get(index).map(String::as_str) {
            Some("--persist") => {
                options.persist = true;
                index += 1;
            }
            Some("--date") => {
                options.date = Some(
                    arguments
                        .get(index + 1)
                        .ok_or(CliError::DateValueRequired)?
                        .clone(),
                );
                index += 2;
            }
            Some("--allow-lower-ceiling") => {
                options.allow_lower_ceiling = true;
                index += 1;
            }
            Some(argument) => {
                return Err(CliError::UnknownDenominatorsArgument {
                    argument: argument.to_owned(),
                });
            }
            None => break,
        }
    }
    if (options.date.is_some() || options.allow_lower_ceiling) && !options.persist {
        return Err(CliError::PersistenceRequired);
    }
    if options.persist && options.date.is_none() {
        return Err(CliError::PersistenceDateRequired);
    }
    Ok(options)
}
