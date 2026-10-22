# Accessibility Coverage: Keyboard, Focus, and Async Status States (Issue #296)

## 1. Overview & Objectives

In decentralized web3 applications, users frequently encounter complex multi-step transaction lifecycles—wallet connection, input validation, hardware wallet signature prompts, asynchronous ledger broadcasting, and final settlement confirmation.

Without explicit accessibility coverage:
- Screen readers fail to announce background transaction settlement or sudden execution errors.
- Focus is lost when wallet popups close, disorienting keyboard-only users.
- Fast-paced animations and loading spinners fail WCAG 2.1 AA standards for cognitive and motor accessibility.

The **Trellis Accessibility Framework** provides contract-level deterministic state models (`shared::accessibility`) paired with client implementation patterns to ensure 100% WCAG 2.1 AA compliance across all user interfaces interacting with Trellis contracts.

---

## 2. Async Status State Taxonomy & ARIA Mapping

Every transaction or asynchronous contract operation transitions through standardized lifecycle states:

```mermaid
stateDiagram-v2
    [*] --> Idle
    Idle --> Validating: User Submits Action
    Validating --> AwaitingSignature: Preflight Checks Pass
    Validating --> RecoverableError: Simulation/Input Failure
    AwaitingSignature --> Broadcasting: Signature Granted
    AwaitingSignature --> RecoverableError: User Rejection
    Broadcasting --> Succeeded: Ledger Confirmation
    Broadcasting --> FatalError: Execution Revert
    RecoverableError --> Validating: Retry Action
    Succeeded --> Idle: Reset / Done
    FatalError --> Idle: Acknowledged
```

### State-to-ARIA Matrix

| Async State | `aria-live` | `role` | `aria-busy` | Focus Mode | WCAG Semantic Announcement |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Idle** | `off` | `region` | `false` | Linear Tab | Quiet state. Form elements interactive. |
| **Validating** | `polite` | `progressbar` | `true` | Linear Tab | "Validating transaction parameters and preflight safety..." |
| **AwaitingSignature** | `polite` | `status` | `true` | Modal Trap | "Please confirm and sign transaction in your Stellar wallet." |
| **Broadcasting** | `polite` | `progressbar` | `true` | Modal Trap (Locked) | "Broadcasting transaction to Soroban network..." |
| **Succeeded** | `polite` | `status` | `false` | Focus Return | "Transaction confirmed successfully. Block ledger sequence recorded." |
| **RecoverableError** | `assertive` | `alert` | `false` | Modal Trap | "Transaction failed: Insufficient allowance. Press Retry or Cancel." |
| **FatalError** | `assertive` | `alert` | `false` | Modal Trap | "Critical transaction revert. Operation halted." |

---

## 3. Focus Trapping & Management

When transactions enter modal verification states (`AwaitingSignature`, `RecoverableError`, `FatalError`):
1. **Focus Trap Containment**:
   - Focus is automatically trapped within the modal container.
   - Pressing <kbd>Tab</kbd> on the last interactive element loops to the first interactive element.
   - Pressing <kbd>Shift</kbd> + <kbd>Tab</kbd> on the first interactive element loops to the last interactive element.
2. **Initial Focus**:
   - Automatically focused on the primary confirmation action button (or error dismissal button).
3. **Focus Restoration**:
   - The triggering button's DOM identifier (`return_focus`) is stored in the contract/client metadata.
   - Upon completion or dismissal, focus returns smoothly to the trigger element, preventing focus loss.
4. **Broadcasting Lock**:
   - During `Broadcasting`, `dismissable` is set to `false`, preventing accidental escape key dismissal while state is writing to the ledger.

---

## 4. Keyboard Navigation: Roving Tabindex

For multi-item asset selectors (e.g. NFT marketplace grids, vault selection, token lists):
- **Single Tab Stop**: The container has one tab stop (<kbd>Tab</kbd> enters, <kbd>Tab</kbd> exits).
- **Active Item**: The selected item maintains `tabIndex="0"`.
- **Inactive Items**: All other items maintain `tabIndex="-1"`.
- **Arrow Navigation**:
  - <kbd>&rarr;</kbd> / <kbd>&darr;</kbd>: Focus next item.
  - <kbd>&larr;</kbd> / <kbd>&uarr;</kbd>: Focus previous item.
  - <kbd>Home</kbd>: Jump to first item.
  - <kbd>End</kbd>: Jump to last item.

---

## 5. Execution Paths

1. **Normal Path**:
   - `Idle` &rarr; `Validating` (`aria-busy="true"`) &rarr; `AwaitingSignature` (Modal Trap active) &rarr; `Broadcasting` (dismissal locked) &rarr; `Succeeded` (focus restored to trigger button).
2. **Failure Path**:
   - `Broadcasting` fails &rarr; `FatalError` triggered with `aria-live="assertive"` and `role="alert"`. Screen reader interrupts ongoing speech to alert user.
3. **Retry Path**:
   - `RecoverableError` gives retry option. User selects retry &rarr; transitions back to `Validating`.
4. **Authorization Path**:
   - `record_accessibility_state` requires authorized caller signature, preventing unauthorized tampering with client auditing events.

---

## 6. Backwards Compatibility & Rollback

- Zero storage schema migration required.
- All accessibility data structures (`shared::accessibility`) are additive helper models.
- Client applications can consume accessibility metadata without requiring protocol hard forks.
