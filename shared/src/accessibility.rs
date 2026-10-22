//! Accessibility Coverage for Keyboard, Focus, and Async Status States (Issue #296).
//!
//! Provides deterministic mappings between contract operational states and
//! WCAG 2.1 AA accessible UI attributes, focus trap configurations, keyboard
//! navigation roving tabindex states, and screen reader ARIA live region announcements.

use soroban_sdk::{
    contracterror, contracttype, symbol_short, Address, Env, Symbol,
};

use crate::errors::Error;
use crate::storage::{persistent_get, persistent_set};

// ---------------------------------------------------------------------------
// Error Taxonomy (Range 960–969)
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum AccessibilityError {
    /// Invalid focus trap configuration (e.g. missing return focus target).
    InvalidFocusTrap = 960,
    /// Invalid state transition for async operation status.
    InvalidStateTransition = 961,
    /// Caller unauthorized to set accessibility audit state.
    Unauthorized = 962,
}

impl From<AccessibilityError> for Error {
    fn from(err: AccessibilityError) -> Self {
        match err {
            AccessibilityError::InvalidFocusTrap => Error::InvalidArgument,
            AccessibilityError::InvalidStateTransition => Error::InvalidArgument,
            AccessibilityError::Unauthorized => Error::Unauthorized,
        }
    }
}

// ---------------------------------------------------------------------------
// Accessibility Models & Taxonomy
// ---------------------------------------------------------------------------

/// Asynchronous transaction lifecycle states for wallet and contract operations.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum AsyncStatusState {
    Idle = 0,
    Validating = 1,
    AwaitingSignature = 2,
    Broadcasting = 3,
    Succeeded = 4,
    RecoverableError = 5,
    FatalError = 6,
}

impl AsyncStatusState {
    pub fn as_symbol(&self) -> Symbol {
        match self {
            AsyncStatusState::Idle => symbol_short!("idle"),
            AsyncStatusState::Validating => symbol_short!("validate"),
            AsyncStatusState::AwaitingSignature => symbol_short!("await_sig"),
            AsyncStatusState::Broadcasting => symbol_short!("broadcast"),
            AsyncStatusState::Succeeded => symbol_short!("tx_ok"),
            AsyncStatusState::RecoverableError => symbol_short!("tx_retry"),
            AsyncStatusState::FatalError => symbol_short!("tx_failed"),
        }
    }

    /// Whether this state represents an in-flight operation (`aria-busy="true"`).
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            AsyncStatusState::Validating
                | AsyncStatusState::AwaitingSignature
                | AsyncStatusState::Broadcasting
        )
    }

    /// Whether this state is terminal.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AsyncStatusState::Succeeded | AsyncStatusState::FatalError
        )
    }
}

/// ARIA live region announcement priority mode.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum AriaLiveMode {
    Off = 0,
    Polite = 1,
    Assertive = 2,
}

/// Semantic ARIA role for assistive technology.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum AriaRole {
    Status = 1,
    Alert = 2,
    Progressbar = 3,
    Region = 4,
}

/// Keyboard navigation paradigm.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum KeyboardNavMode {
    LinearTab = 1,
    RovingTabindex = 2,
    ModalTrap = 3,
}

/// Modal focus trap specification for high-impact actions.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FocusTrapConfig {
    pub trap_active: bool,
    pub initial_focus: Symbol,
    pub return_focus: Symbol,
    pub dismissable: bool,
}

/// Consolidated accessibility metadata payload returned to client dApps and indexers.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessibilityMetadata {
    pub state: AsyncStatusState,
    pub aria_live: AriaLiveMode,
    pub aria_busy: bool,
    pub role: AriaRole,
    pub focus_trap: FocusTrapConfig,
    pub keyboard_mode: KeyboardNavMode,
}

// ---------------------------------------------------------------------------
// Storage Keys
// ---------------------------------------------------------------------------

const KEY_LAST_A11Y_STATE: Symbol = symbol_short!("last_a11y");

// ---------------------------------------------------------------------------
// Core Accessibility Functions
// ---------------------------------------------------------------------------

/// Maps any contract asynchronous status state to standardized WCAG 2.1 AA attributes.
pub fn map_state_to_accessibility(
    state: AsyncStatusState,
    return_focus: Symbol,
) -> AccessibilityMetadata {
    let aria_live = match state {
        AsyncStatusState::Idle => AriaLiveMode::Off,
        AsyncStatusState::Validating => AriaLiveMode::Polite,
        AsyncStatusState::AwaitingSignature => AriaLiveMode::Polite,
        AsyncStatusState::Broadcasting => AriaLiveMode::Polite,
        AsyncStatusState::Succeeded => AriaLiveMode::Polite,
        AsyncStatusState::RecoverableError => AriaLiveMode::Assertive,
        AsyncStatusState::FatalError => AriaLiveMode::Assertive,
    };

    let role = match state {
        AsyncStatusState::Idle => AriaRole::Region,
        AsyncStatusState::Validating => AriaRole::Progressbar,
        AsyncStatusState::AwaitingSignature => AriaRole::Status,
        AsyncStatusState::Broadcasting => AriaRole::Progressbar,
        AsyncStatusState::Succeeded => AriaRole::Status,
        AsyncStatusState::RecoverableError => AriaRole::Alert,
        AsyncStatusState::FatalError => AriaRole::Alert,
    };

    let aria_busy = state.is_busy();

    let is_modal = matches!(
        state,
        AsyncStatusState::AwaitingSignature
            | AsyncStatusState::RecoverableError
            | AsyncStatusState::FatalError
    );

    let focus_trap = FocusTrapConfig {
        trap_active: is_modal,
        initial_focus: if is_modal {
            symbol_short!("modal_btn")
        } else {
            symbol_short!("main")
        },
        return_focus,
        dismissable: state != AsyncStatusState::Broadcasting,
    };

    let keyboard_mode = if is_modal {
        KeyboardNavMode::ModalTrap
    } else {
        KeyboardNavMode::LinearTab
    };

    AccessibilityMetadata {
        state,
        aria_live,
        aria_busy,
        role,
        focus_trap,
        keyboard_mode,
    }
}

/// Validates that a focus trap has valid initial and return focus identifiers.
pub fn validate_focus_trap_config(config: &FocusTrapConfig) -> Result<(), AccessibilityError> {
    if config.trap_active {
        if config.initial_focus == symbol_short!("") || config.return_focus == symbol_short!("") {
            return Err(AccessibilityError::InvalidFocusTrap);
        }
    }
    Ok(())
}

/// Records accessibility metadata for an audited transaction lifecycle step.
pub fn record_accessibility_state(
    env: &Env,
    caller: &Address,
    metadata: &AccessibilityMetadata,
) -> Result<(), Error> {
    caller.require_auth();

    validate_focus_trap_config(&metadata.focus_trap)?;

    persistent_set(env, &KEY_LAST_A11Y_STATE, metadata);

    env.events().publish(
        (symbol_short!("a11y_upd"), metadata.state.as_symbol()),
        (
            caller.clone(),
            metadata.aria_busy,
            env.ledger().timestamp(),
        ),
    );

    Ok(())
}

/// Retrieves the last recorded accessibility state from storage.
pub fn get_last_accessibility_state(env: &Env) -> Option<AccessibilityMetadata> {
    persistent_get(env, &KEY_LAST_A11Y_STATE)
}
