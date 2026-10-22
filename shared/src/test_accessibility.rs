#[cfg(test)]
mod tests {
    use soroban_sdk::{
        symbol_short, testutils::Address as _, Address, Env,
    };

    use crate::accessibility::{
        get_last_accessibility_state, map_state_to_accessibility, record_accessibility_state,
        validate_focus_trap_config, AccessibilityError, AccessibilityMetadata, AriaLiveMode,
        AriaRole, AsyncStatusState, FocusTrapConfig, KeyboardNavMode,
    };

    fn setup() -> (Env, Address) {
        let env = Env::default();
        let user = Address::generate(&env);
        (env, user)
    }

    #[test]
    fn test_normal_lifecycle_state_progression() {
        let return_target = symbol_short!("tx_btn");

        // 1. Idle
        let a11y_idle = map_state_to_accessibility(AsyncStatusState::Idle, return_target.clone());
        assert_eq!(a11y_idle.state, AsyncStatusState::Idle);
        assert_eq!(a11y_idle.aria_live, AriaLiveMode::Off);
        assert!(!a11y_idle.aria_busy);
        assert_eq!(a11y_idle.role, AriaRole::Region);
        assert!(!a11y_idle.focus_trap.trap_active);

        // 2. Validating
        let a11y_val = map_state_to_accessibility(AsyncStatusState::Validating, return_target.clone());
        assert_eq!(a11y_val.state, AsyncStatusState::Validating);
        assert_eq!(a11y_val.aria_live, AriaLiveMode::Polite);
        assert!(a11y_val.aria_busy);
        assert_eq!(a11y_val.role, AriaRole::Progressbar);

        // 3. AwaitingSignature
        let a11y_sig = map_state_to_accessibility(AsyncStatusState::AwaitingSignature, return_target.clone());
        assert_eq!(a11y_sig.state, AsyncStatusState::AwaitingSignature);
        assert_eq!(a11y_sig.aria_live, AriaLiveMode::Polite);
        assert!(a11y_sig.aria_busy);
        assert_eq!(a11y_sig.keyboard_mode, KeyboardNavMode::ModalTrap);
        assert!(a11y_sig.focus_trap.trap_active);

        // 4. Broadcasting
        let a11y_broad = map_state_to_accessibility(AsyncStatusState::Broadcasting, return_target.clone());
        assert_eq!(a11y_broad.state, AsyncStatusState::Broadcasting);
        assert!(a11y_broad.aria_busy);
        assert_eq!(a11y_broad.role, AriaRole::Progressbar);

        // 5. Succeeded
        let a11y_ok = map_state_to_accessibility(AsyncStatusState::Succeeded, return_target.clone());
        assert_eq!(a11y_ok.state, AsyncStatusState::Succeeded);
        assert_eq!(a11y_ok.aria_live, AriaLiveMode::Polite);
        assert!(!a11y_ok.aria_busy);
        assert_eq!(a11y_ok.role, AriaRole::Status);
        assert!(!a11y_ok.focus_trap.trap_active);
    }

    #[test]
    fn test_failure_path_uses_assertive_alert() {
        let return_target = symbol_short!("tx_btn");

        let a11y_err = map_state_to_accessibility(AsyncStatusState::FatalError, return_target);
        assert_eq!(a11y_err.state, AsyncStatusState::FatalError);
        assert_eq!(a11y_err.aria_live, AriaLiveMode::Assertive);
        assert!(!a11y_err.aria_busy);
        assert_eq!(a11y_err.role, AriaRole::Alert);
        assert!(a11y_err.focus_trap.trap_active);
        assert_eq!(a11y_err.keyboard_mode, KeyboardNavMode::ModalTrap);
    }

    #[test]
    fn test_retry_path_recoverable_error_to_retry() {
        let return_target = symbol_short!("retry_btn");

        // Recoverable error: assertive alert, dismissable modal
        let a11y_recover = map_state_to_accessibility(AsyncStatusState::RecoverableError, return_target.clone());
        assert_eq!(a11y_recover.aria_live, AriaLiveMode::Assertive);
        assert_eq!(a11y_recover.role, AriaRole::Alert);
        assert!(a11y_recover.focus_trap.dismissable);

        // User hits retry: back to Validating
        let a11y_retry = map_state_to_accessibility(AsyncStatusState::Validating, return_target.clone());
        assert_eq!(a11y_retry.aria_live, AriaLiveMode::Polite);
        assert!(a11y_retry.aria_busy);
    }

    #[test]
    fn test_focus_trap_validation() {
        // Valid trap
        let valid_trap = FocusTrapConfig {
            trap_active: true,
            initial_focus: symbol_short!("modal_btn"),
            return_focus: symbol_short!("main_btn"),
            dismissable: true,
        };
        assert!(validate_focus_trap_config(&valid_trap).is_ok());

        // Inactive trap ignores empty fields
        let inactive_trap = FocusTrapConfig {
            trap_active: false,
            initial_focus: symbol_short!(""),
            return_focus: symbol_short!(""),
            dismissable: false,
        };
        assert!(validate_focus_trap_config(&inactive_trap).is_ok());
    }

    #[test]
    fn test_record_and_retrieve_accessibility_state() {
        let (env, user) = setup();
        let return_target = symbol_short!("tx_btn");

        let a11y_state = map_state_to_accessibility(AsyncStatusState::Broadcasting, return_target.clone());

        // Record state
        let res = record_accessibility_state(&env, &user, &a11y_state);
        assert!(res.is_ok());

        // Retrieve stored state
        let stored = get_last_accessibility_state(&env).unwrap();
        assert_eq!(stored.state, AsyncStatusState::Broadcasting);
        assert!(stored.aria_busy);
        assert_eq!(stored.role, AriaRole::Progressbar);
    }
}
