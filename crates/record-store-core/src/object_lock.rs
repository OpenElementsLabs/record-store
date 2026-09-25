//! Object Lock: retention periods and legal holds on immutable versions.
//!
//! These types are deliberately shaped so that an incoherent lock cannot be
//! constructed. A retention mode without an expiry date means nothing, so the
//! two are one value rather than two optional fields, and the only way to build
//! a duration is through a constructor that bounds it.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::*;

/// How a retained object version may be released before its date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionMode {
    /// A caller holding an explicit bypass permission may shorten or delete.
    Governance,
    /// Nobody may shorten or delete before the date, including the root
    /// credential. There is no bypass, by design.
    Compliance,
}

impl RetentionMode {
    /// Returns the S3 wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Governance => "GOVERNANCE",
            Self::Compliance => "COMPLIANCE",
        }
    }

    /// Parses an S3 wire name.
    pub fn parse(value: &str) -> Result<Self, CoreError> {
        match value {
            "GOVERNANCE" => Ok(Self::Governance),
            "COMPLIANCE" => Ok(Self::Compliance),
            other => Err(CoreError::InvalidObjectLock(format!(
                "retention mode must be GOVERNANCE or COMPLIANCE, not {other}"
            ))),
        }
    }
}

/// A retention period applied to one immutable object version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    /// Whether an authorized bypass exists.
    pub mode: RetentionMode,
    /// The instant the version stops being retained.
    pub retain_until: DateTime<Utc>,
}

impl Retention {
    /// Returns whether this retention still holds at the given instant.
    #[must_use]
    pub const fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        self.retain_until.timestamp_micros() > now.timestamp_micros()
    }
}

/// Everything Object Lock holds about one immutable version.
///
/// A version with neither a retention nor a hold is the same as a version with
/// no record at all, which is why the default is meaningful and is what a
/// caller gets for an unlocked version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectLockState {
    /// The retention period, when one was applied.
    #[serde(default)]
    pub retention: Option<Retention>,
    /// A legal hold blocks deletion on its own, in either retention mode, and
    /// for as long as it is left on.
    #[serde(default)]
    pub legal_hold: bool,
}

impl ObjectLockState {
    /// Returns whether nothing is held, so the record carries no information.
    #[must_use]
    pub const fn is_unlocked(&self) -> bool {
        self.retention.is_none() && !self.legal_hold
    }

    /// Returns why deletion is refused at this instant, or `None` when allowed.
    ///
    /// The legal hold is reported first because it is the condition an operator
    /// can actually clear; telling them about a retention date they cannot
    /// change while a hold is also present would send them the wrong way.
    #[must_use]
    pub fn deletion_block_at(&self, now: DateTime<Utc>) -> Option<LockBlock> {
        if self.legal_hold {
            return Some(LockBlock::LegalHold);
        }
        match self.retention {
            Some(retention) if retention.is_active_at(now) => Some(LockBlock::Retention {
                mode: retention.mode,
                retain_until: retention.retain_until,
            }),
            _ => None,
        }
    }
}

/// The reason one version cannot currently be deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum LockBlock {
    /// A legal hold is on. It has no expiry and must be removed explicitly.
    LegalHold,
    /// A retention period has not elapsed.
    Retention {
        /// Whether a bypass could exist for it.
        mode: RetentionMode,
        /// When the retention expires.
        retain_until: DateTime<Utc>,
    },
}

impl LockBlock {
    /// Returns a stable low-cardinality label for audit records and metrics.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::LegalHold => "legal_hold",
            Self::Retention {
                mode: RetentionMode::Governance,
                ..
            } => "governance_retention",
            Self::Retention {
                mode: RetentionMode::Compliance,
                ..
            } => "compliance_retention",
        }
    }

    /// Returns whether an authorized caller could bypass this block.
    ///
    /// A legal hold is never bypassable: it is removed or it holds.
    #[must_use]
    pub const fn is_bypassable(&self) -> bool {
        matches!(
            self,
            Self::Retention {
                mode: RetentionMode::Governance,
                ..
            }
        )
    }
}

/// Why a requested Object Lock change was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockChangeRefused {
    /// A compliance retention was still running. Nothing shortens it, and no
    /// bypass exists for it, including for the deployment's root credential.
    ComplianceRetentionIsFinal,
    /// A governance retention was still running and the caller presented no
    /// authorized bypass.
    GovernanceBypassRequired,
}

impl LockChangeRefused {
    /// Returns a stable low-cardinality label for audit records.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ComplianceRetentionIsFinal => "compliance_retention_is_final",
            Self::GovernanceBypassRequired => "governance_bypass_required",
        }
    }
}

impl RetentionMode {
    /// Orders the modes by how hard they are to release.
    ///
    /// Raising a version from governance to compliance only ever takes away a
    /// way out, so it counts as tightening and needs no bypass. Lowering it is
    /// a release like any other.
    const fn strength(self) -> u8 {
        match self {
            Self::Governance => 0,
            Self::Compliance => 1,
        }
    }
}

impl ObjectLockState {
    /// Applies a requested retention, enforcing the mode rules.
    ///
    /// Tightening is always free: a later date, a stronger mode, or both. Every
    /// other change releases something, and what that costs depends on the mode
    /// currently in force. An elapsed retention holds nothing, so a version past
    /// its date accepts any new retention.
    pub fn with_retention(
        self,
        requested: Option<Retention>,
        now: DateTime<Utc>,
        governance_bypass: bool,
    ) -> Result<Self, LockChangeRefused> {
        let Some(current) = self.retention.filter(|current| current.is_active_at(now)) else {
            return Ok(Self {
                retention: requested,
                ..self
            });
        };
        let tightens = requested.is_some_and(|requested| {
            requested.retain_until >= current.retain_until
                && requested.mode.strength() >= current.mode.strength()
        });
        if tightens {
            return Ok(Self {
                retention: requested,
                ..self
            });
        }
        match current.mode {
            RetentionMode::Compliance => Err(LockChangeRefused::ComplianceRetentionIsFinal),
            RetentionMode::Governance if governance_bypass => Ok(Self {
                retention: requested,
                ..self
            }),
            RetentionMode::Governance => Err(LockChangeRefused::GovernanceBypassRequired),
        }
    }

    /// Returns the state with the legal hold set as requested.
    ///
    /// A hold is independent of retention in both directions: placing one on an
    /// unretained version protects it, and removing one from a retained version
    /// leaves the retention doing its job.
    #[must_use]
    pub const fn with_legal_hold(self, legal_hold: bool) -> Self {
        Self { legal_hold, ..self }
    }

    /// Returns whether a requested change only ever adds protection.
    ///
    /// A change that adds protection needs no trustworthy clock, because getting
    /// it wrong can only over-retain. Everything else is checked against the
    /// observed-time high-water mark first.
    #[must_use]
    pub fn change_only_tightens(&self, requested: &Self) -> bool {
        let retention_tightens = match (self.retention, requested.retention) {
            (_, None) => self.retention.is_none(),
            (None, Some(_)) => true,
            (Some(current), Some(requested)) => {
                requested.retain_until >= current.retain_until
                    && requested.mode.strength() >= current.mode.strength()
            }
        };
        retention_tightens && requested.legal_hold >= self.legal_hold
    }
}

/// A bucket default retention period, in whole days or whole years.
///
/// S3 accepts exactly one of the two, so this is one value rather than two
/// optional fields that could both be set or both be absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "unit", content = "value", rename_all = "snake_case")]
pub enum RetentionPeriod {
    /// Whole days.
    Days(u16),
    /// Whole years, counted as 365 days each.
    Years(u16),
}

impl RetentionPeriod {
    /// Longest accepted period in either unit, matching the `Days` bound.
    pub const MAXIMUM_DAYS: u16 = 36_500;
    /// Longest accepted period in years.
    pub const MAXIMUM_YEARS: u16 = 100;
    const DAYS_PER_YEAR: i64 = 365;

    /// Builds a period in whole days.
    pub fn days(value: u16) -> Result<Self, CoreError> {
        if (1..=Self::MAXIMUM_DAYS).contains(&value) {
            Ok(Self::Days(value))
        } else {
            Err(CoreError::InvalidObjectLock(format!(
                "retention days must be between 1 and {}",
                Self::MAXIMUM_DAYS
            )))
        }
    }

    /// Builds a period in whole years.
    pub fn years(value: u16) -> Result<Self, CoreError> {
        if (1..=Self::MAXIMUM_YEARS).contains(&value) {
            Ok(Self::Years(value))
        } else {
            Err(CoreError::InvalidObjectLock(format!(
                "retention years must be between 1 and {}",
                Self::MAXIMUM_YEARS
            )))
        }
    }

    /// Re-validates a period that arrived through deserialization.
    pub fn validate(self) -> Result<Self, CoreError> {
        match self {
            Self::Days(value) => Self::days(value),
            Self::Years(value) => Self::years(value),
        }
    }

    /// Returns the retain-until date this period produces from a write time.
    ///
    /// Both bounds are small enough that the multiplication cannot overflow a
    /// `Duration`, and the checked addition covers a base date near the end of
    /// the representable range.
    pub fn retain_until_from(self, written_at: DateTime<Utc>) -> Result<DateTime<Utc>, CoreError> {
        let days = match self {
            Self::Days(value) => i64::from(value),
            Self::Years(value) => i64::from(value) * Self::DAYS_PER_YEAR,
        };
        written_at
            .checked_add_signed(Duration::days(days))
            .ok_or_else(|| {
                CoreError::InvalidObjectLock(
                    "retention period does not fit in a representable date".into(),
                )
            })
    }
}

/// A bucket's default Object Lock rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultRetention {
    /// Mode applied to versions written without an explicit mode.
    pub mode: RetentionMode,
    /// Period applied from the write time.
    pub period: RetentionPeriod,
}

impl DefaultRetention {
    /// Re-validates a default that arrived through deserialization.
    pub fn validate(self) -> Result<Self, CoreError> {
        Ok(Self {
            mode: self.mode,
            period: self.period.validate()?,
        })
    }
}

/// A bucket's Object Lock configuration.
///
/// Its presence on a bucket is what "Object Lock enabled" means. The default
/// rule is optional: a bucket can have lock enabled and apply retention only
/// per request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectLockConfiguration {
    /// Retention applied to versions written without explicit lock headers.
    #[serde(default)]
    pub default_retention: Option<DefaultRetention>,
}

impl ObjectLockConfiguration {
    /// Re-validates a configuration that arrived through deserialization.
    pub fn validate(self) -> Result<Self, CoreError> {
        Ok(Self {
            default_retention: self
                .default_retention
                .map(DefaultRetention::validate)
                .transpose()?,
        })
    }

    /// Returns the lock state a version written now would start with.
    pub fn initial_state_at(
        &self,
        written_at: DateTime<Utc>,
    ) -> Result<ObjectLockState, CoreError> {
        let retention = self
            .default_retention
            .map(|default| {
                Ok::<_, CoreError>(Retention {
                    mode: default.mode,
                    retain_until: default.period.retain_until_from(written_at)?,
                })
            })
            .transpose()?;
        Ok(ObjectLockState {
            retention,
            legal_hold: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legal_hold_blocks_deletion_in_either_mode_and_without_any_retention() {
        let now = Utc::now();
        for retention in [
            None,
            Some(Retention {
                mode: RetentionMode::Governance,
                // Already expired: the hold is doing all the work.
                retain_until: now - Duration::days(1),
            }),
            Some(Retention {
                mode: RetentionMode::Compliance,
                retain_until: now - Duration::days(1),
            }),
        ] {
            let state = ObjectLockState {
                retention,
                legal_hold: true,
            };
            assert_eq!(state.deletion_block_at(now), Some(LockBlock::LegalHold));
        }
    }

    #[test]
    fn an_elapsed_retention_stops_blocking_while_a_future_one_still_blocks() {
        let now = Utc::now();
        let elapsed = ObjectLockState {
            retention: Some(Retention {
                mode: RetentionMode::Compliance,
                retain_until: now - Duration::seconds(1),
            }),
            legal_hold: false,
        };
        assert_eq!(elapsed.deletion_block_at(now), None);

        let pending = ObjectLockState {
            retention: Some(Retention {
                mode: RetentionMode::Compliance,
                retain_until: now + Duration::seconds(1),
            }),
            legal_hold: false,
        };
        assert!(matches!(
            pending.deletion_block_at(now),
            Some(LockBlock::Retention {
                mode: RetentionMode::Compliance,
                ..
            })
        ));
    }

    /// The whole point of the two modes: exactly one of them can be bypassed,
    /// and a legal hold never can.
    #[test]
    fn only_governance_retention_is_bypassable() {
        let retain_until = Utc::now() + Duration::days(1);
        assert!(
            LockBlock::Retention {
                mode: RetentionMode::Governance,
                retain_until
            }
            .is_bypassable()
        );
        assert!(
            !LockBlock::Retention {
                mode: RetentionMode::Compliance,
                retain_until
            }
            .is_bypassable()
        );
        assert!(!LockBlock::LegalHold.is_bypassable());
    }

    /// The whole promise of compliance mode. If this ever passes a shortening,
    /// the mode means nothing.
    #[test]
    fn a_running_compliance_retention_refuses_every_release_including_with_bypass() {
        let now = Utc::now();
        let state = ObjectLockState {
            retention: Some(Retention {
                mode: RetentionMode::Compliance,
                retain_until: now + Duration::days(10),
            }),
            legal_hold: false,
        };
        let shortened = Some(Retention {
            mode: RetentionMode::Compliance,
            retain_until: now + Duration::days(1),
        });
        let downgraded = Some(Retention {
            mode: RetentionMode::Governance,
            retain_until: now + Duration::days(100),
        });
        for requested in [shortened, downgraded, None] {
            for bypass in [false, true] {
                assert_eq!(
                    state.with_retention(requested, now, bypass),
                    Err(LockChangeRefused::ComplianceRetentionIsFinal),
                    "requested {requested:?} with bypass {bypass}"
                );
            }
        }
    }

    #[test]
    fn a_compliance_retention_may_always_be_extended() {
        let now = Utc::now();
        let state = ObjectLockState {
            retention: Some(Retention {
                mode: RetentionMode::Compliance,
                retain_until: now + Duration::days(10),
            }),
            legal_hold: false,
        };
        let extended = Retention {
            mode: RetentionMode::Compliance,
            retain_until: now + Duration::days(11),
        };
        assert_eq!(
            state
                .with_retention(Some(extended), now, false)
                .expect("extension")
                .retention,
            Some(extended)
        );
    }

    #[test]
    fn a_governance_retention_is_shortened_only_with_a_bypass() {
        let now = Utc::now();
        let state = ObjectLockState {
            retention: Some(Retention {
                mode: RetentionMode::Governance,
                retain_until: now + Duration::days(10),
            }),
            legal_hold: false,
        };
        let shortened = Some(Retention {
            mode: RetentionMode::Governance,
            retain_until: now + Duration::days(1),
        });
        assert_eq!(
            state.with_retention(shortened, now, false),
            Err(LockChangeRefused::GovernanceBypassRequired)
        );
        assert_eq!(
            state
                .with_retention(shortened, now, true)
                .expect("bypassed")
                .retention,
            shortened
        );
    }

    /// Raising governance to compliance takes away the escape hatch, so it is a
    /// tightening and must not itself need the escape hatch to perform.
    #[test]
    fn raising_governance_to_compliance_needs_no_bypass() {
        let now = Utc::now();
        let state = ObjectLockState {
            retention: Some(Retention {
                mode: RetentionMode::Governance,
                retain_until: now + Duration::days(10),
            }),
            legal_hold: false,
        };
        let raised = Retention {
            mode: RetentionMode::Compliance,
            retain_until: now + Duration::days(10),
        };
        assert_eq!(
            state
                .with_retention(Some(raised), now, false)
                .expect("raise")
                .retention,
            Some(raised)
        );
    }

    /// An elapsed compliance retention held its version for as long as it
    /// promised. Afterwards the version is ordinary again.
    #[test]
    fn an_elapsed_compliance_retention_no_longer_constrains_a_change() {
        let now = Utc::now();
        let state = ObjectLockState {
            retention: Some(Retention {
                mode: RetentionMode::Compliance,
                retain_until: now - Duration::seconds(1),
            }),
            legal_hold: false,
        };
        assert_eq!(
            state.with_retention(None, now, false).expect("clear"),
            ObjectLockState::default()
        );
    }

    #[test]
    fn a_legal_hold_toggles_independently_of_any_retention() {
        let now = Utc::now();
        let retention = Some(Retention {
            mode: RetentionMode::Compliance,
            retain_until: now + Duration::days(5),
        });
        let held = ObjectLockState {
            retention,
            legal_hold: false,
        }
        .with_legal_hold(true);
        assert!(held.legal_hold);
        // Removing the hold leaves the compliance retention untouched.
        let released = held.with_legal_hold(false);
        assert_eq!(released.retention, retention);
        assert!(!released.legal_hold);
    }

    /// Only a change that can over-retain is safe to apply without trusting the
    /// clock, so this predicate decides when the watermark is consulted.
    #[test]
    fn only_protection_adding_changes_count_as_tightening() {
        let now = Utc::now();
        let short = Retention {
            mode: RetentionMode::Governance,
            retain_until: now + Duration::days(1),
        };
        let long = Retention {
            mode: RetentionMode::Governance,
            retain_until: now + Duration::days(2),
        };
        let unlocked = ObjectLockState::default();
        let retained = ObjectLockState {
            retention: Some(short),
            legal_hold: false,
        };
        let extended = ObjectLockState {
            retention: Some(long),
            legal_hold: false,
        };

        assert!(unlocked.change_only_tightens(&retained));
        assert!(retained.change_only_tightens(&extended));
        assert!(retained.change_only_tightens(&retained.with_legal_hold(true)));
        assert!(unlocked.change_only_tightens(&unlocked));

        assert!(!extended.change_only_tightens(&retained));
        assert!(!retained.change_only_tightens(&unlocked));
        assert!(
            !retained
                .with_legal_hold(true)
                .change_only_tightens(&retained)
        );
    }

    #[test]
    fn retention_periods_are_bounded_in_both_units() {
        assert!(RetentionPeriod::days(0).is_err());
        assert!(RetentionPeriod::days(1).is_ok());
        assert!(RetentionPeriod::days(RetentionPeriod::MAXIMUM_DAYS).is_ok());
        assert!(RetentionPeriod::days(RetentionPeriod::MAXIMUM_DAYS + 1).is_err());
        assert!(RetentionPeriod::years(0).is_err());
        assert!(RetentionPeriod::years(RetentionPeriod::MAXIMUM_YEARS).is_ok());
        assert!(RetentionPeriod::years(RetentionPeriod::MAXIMUM_YEARS + 1).is_err());
    }

    /// A period that arrived through deserialization never went through a
    /// constructor, so the bound has to be re-checked rather than assumed.
    #[test]
    fn a_period_that_skipped_the_constructor_is_refused_on_validation() {
        assert!(RetentionPeriod::Days(40_000).validate().is_err());
        assert!(RetentionPeriod::Years(500).validate().is_err());
        assert!(RetentionPeriod::Days(30).validate().is_ok());
    }

    #[test]
    fn a_year_is_counted_as_three_hundred_and_sixty_five_days() {
        let written_at = Utc::now();
        assert_eq!(
            RetentionPeriod::Years(2)
                .retain_until_from(written_at)
                .expect("retain until"),
            written_at + Duration::days(730)
        );
    }

    #[test]
    fn a_bucket_default_materializes_onto_a_version_at_write_time() {
        let written_at = Utc::now();
        let configuration = ObjectLockConfiguration {
            default_retention: Some(DefaultRetention {
                mode: RetentionMode::Governance,
                period: RetentionPeriod::Days(30),
            }),
        };
        let state = configuration
            .initial_state_at(written_at)
            .expect("initial state");
        let retention = state.retention.expect("retention");
        assert_eq!(retention.mode, RetentionMode::Governance);
        assert_eq!(retention.retain_until, written_at + Duration::days(30));
        // A default never places a hold: that is always an explicit act.
        assert!(!state.legal_hold);
    }

    #[test]
    fn a_bucket_with_lock_enabled_and_no_default_leaves_versions_unlocked() {
        let state = ObjectLockConfiguration::default()
            .initial_state_at(Utc::now())
            .expect("initial state");
        assert!(state.is_unlocked());
    }
}
