use soroban_sdk::contracterror;

/// Oracle-specific error codes (range 500-599 per shared conventions).
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OracleError {
    /// The submitter is not registered or not active.
    SubmitterNotAuthorized = 500,
    /// The submission was already submitted (replay detected).
    DuplicateSubmission = 501,
    /// The feed does not exist or has no submissions.
    FeedNotFound = 502,
    /// The submitter is already registered.
    SubmitterAlreadyRegistered = 503,
    /// The submission is stale (outside the staleness window).
    SubmissionStale = 504,
    /// The price value is invalid (e.g., negative for certain feeds).
    InvalidPrice = 505,
    /// The decimal places value is invalid.
    InvalidDecimals = 506,
    /// The feed identifier is invalid (e.g., empty).
    InvalidFeedId = 507,
    /// The caller is not authorized to perform this action.
    Unauthorized = 508,
    /// A generic error occurred.
    InternalError = 509,
    /// The contract has already been initialized.
    AlreadyInitialized = 510,
    /// The requested quorum is invalid or exceeds the active submitter count.
    InvalidQuorum = 511,
    /// The feed does not currently have enough active submissions to meet quorum.
    InsufficientQuorum = 512,
    /// The submitter stake is below the required minimum.
    InsufficientStake = 513,
    /// An unbonding cooldown period is still active.
    UnbondingCooldownActive = 514,
    /// No unbonding request was found.
    NoUnbondingRequest = 515,
    /// The slash penalty amount is invalid or exceeds the available stake.
    InvalidPenaltyAmount = 516,
    /// Treasury contract is not configured or invalid.
    TreasuryNotConfigured = 517,
    /// Staking configuration has not been set.
    StakingConfigNotSet = 518,
    /// Submitter is already in the process of unbonding.
    AlreadyUnbonding = 519,
    /// Math overflow occurred.
    Overflow = 520,
}

