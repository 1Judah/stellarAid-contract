//! Gradual rollout of new contract versions (closes #684).
//!
//! Supports:
//!
//! 1. **Canary deployments** — a canary contract ID plus a traffic share.
//! 2. **Feature flags** — named, admin-toggled booleans.
//! 3. **Traffic splitting** — sticky per-caller routing via SHA-256 of the
//!    caller address, compared against `canary_bps`.
//! 4. **Rollback triggers** — when health error-rate exceeds a threshold,
//!    `should_rollback` is true and `health_check` auto-disables the canary.
//! 5. **Manual rollback** — `trigger_rollback` zeroes canary traffic, disables
//!    flags, and pauses the contract.
//!
//! Procedures are documented in `docs/DEPLOY.md`.

use crate::health;
use soroban_sdk::{
    contracterror, contracttype, panic_with_error, symbol_short, xdr::ToXdr, Address, Env, Symbol, Vec,
};

const BPS_DENOM: u32 = 10_000;
/// Default error-rate (bps) that arms an automatic canary rollback.
pub const DEFAULT_ROLLBACK_ERROR_BPS: u32 = 500;

/// Hard ceiling on the number of named feature flags a single contract may
/// carry (closes #985).
///
/// The flag index lives in **instance** storage and is rewritten on every flag
/// change, so an unbounded index grows until it hits the contract's instance
/// size limit — at which point *every* flag write fails, including the one an
/// operator would use to roll back. The index is therefore capped.
///
/// 32 is deliberately small: feature flags are a fixed vocabulary ("new
/// checkout", "beta feed"), not a growing registry, and the whole index is
/// rewritten on every change. It is well inside the instance-size budget while
/// leaving room for the canary and health blocks (`shared::rollout`,
/// `shared::health`) that share the same tier. This value is the **ceiling**:
/// an admin may lower the effective cap with [`set_flag_cap`], but never raise
/// it without shipping new code.
pub const MAX_FEATURE_FLAGS: u32 = 32;

/// Typed failures of the feature-flag cap (closes #985).
///
/// The matching entry in the unified catalogue
/// (`shared::errors::SharedErrorCode`) is `TooManyFeatureFlags = 19`.
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RolloutError {
    /// The contract already holds the effective cap of feature flags; adding
    /// another would grow the instance-stored index past the limit. The
    /// current cap is returned by [`flag_cap`].
    FlagLimitReached = 1,
    /// A requested cap of `0` (every flag write blocked) or one above
    /// [`MAX_FEATURE_FLAGS`].
    InvalidFlagCap = 2,
}

impl core::fmt::Display for RolloutError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::FlagLimitReached => write!(
                f,
                "feature-flag cap reached; see rollout::flag_cap for the current limit"
            ),
            Self::InvalidFlagCap => write!(f, "feature-flag cap must be between 1 and the ceiling"),
        }
    }
}

pub fn get_suggestion(error: RolloutError) -> Symbol {
    match error {
        RolloutError::FlagLimitReached => symbol_short!("FLAG_CAP"),
        RolloutError::InvalidFlagCap => symbol_short!("BAD_CAP"),
    }
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RolloutKey {
    Phase,
    CanaryBps,
    Canary,
    Stable,
    RollbackErrorBps,
    Flag(Symbol),
    FlagIndex,
    /// Optional admin-lowered feature-flag cap; absent means
    /// [`MAX_FEATURE_FLAGS`] (#985). Kept separate from `FlagIndex` so reading
    /// the cap never walks the index.
    MaxFlags,
}


#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RolloutPhase {
    /// No canary traffic; flags may still be used independently.
    Off = 0,
    /// A fraction of callers are sent to the canary contract.
    Canary = 1,
    /// All traffic is on the new version (`canary_bps == 10000`).
    Full = 2,
    /// Canary has been disabled after a trigger or manual rollback.
    RolledBack = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RolloutState {
    pub phase: RolloutPhase,
    pub canary_bps: u32,
    pub canary: Option<Address>,
    pub stable: Option<Address>,
    pub rollback_error_bps: u32,
    pub flag_count: u32,
    /// Effective feature-flag cap, never above [`MAX_FEATURE_FLAGS`] (#985).
    pub flag_cap: u32,
}

fn flag_index(env: &Env) -> Vec<Symbol> {
    env.storage()
        .instance()
        .get(&RolloutKey::FlagIndex)
        .unwrap_or_else(|| Vec::new(env))
}

/// The effective feature-flag cap for this contract (#985).
///
/// Defaults to [`MAX_FEATURE_FLAGS`] and is clamped to it on read, so even a
/// value written by an older/newer build cannot exceed the code ceiling.
pub fn flag_cap(env: &Env) -> u32 {
    match env
        .storage()
        .instance()
        .get::<RolloutKey, u32>(&RolloutKey::MaxFlags)
    {
        Some(cap) if cap > 0 => cap.min(MAX_FEATURE_FLAGS),
        _ => MAX_FEATURE_FLAGS,
    }
}

/// Lower the feature-flag cap, within the code ceiling (#985).
///
/// Allowing only a *lowering* means future tuning never requires shipping new
/// code, while the ceiling in [`MAX_FEATURE_FLAGS`] keeps the hard bound
/// auditable. Lowering below the current flag count is permitted (existing
/// flags keep working) but blocks any new flag until it is raised again.
pub fn set_flag_cap(env: &Env, cap: u32) -> Result<(), RolloutError> {
    if cap == 0 || cap > MAX_FEATURE_FLAGS {
        return Err(RolloutError::InvalidFlagCap);
    }
    env.storage().instance().set(&RolloutKey::MaxFlags, &cap);
    env.events()
        .publish((symbol_short!("flag_cap"),), cap);
    Ok(())
}

/// Persist the flag index, refusing to grow it past [`flag_cap`] (#985).
///
/// This is the single enforcement point: every caller of
/// `set_feature_flag` in the workspace goes through here, so a new contract
/// cannot reintroduce the unbounded-index problem. The abort reverts the whole
/// invocation, so the `Flag(...)` value written just before this is discarded
/// too and the write is atomic.
fn save_flag_index(env: &Env, index: &Vec<Symbol>) {
    let cap = flag_cap(env);
    if index.len() > cap {
        panic_with_error!(env, RolloutError::FlagLimitReached);
    }
    env.storage()
        .instance()
        .set(&RolloutKey::FlagIndex, index);
}

pub fn get_canary_bps(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&RolloutKey::CanaryBps)
        .unwrap_or(0)
}

pub fn get_phase(env: &Env) -> RolloutPhase {
    env.storage()
        .instance()
        .get(&RolloutKey::Phase)
        .unwrap_or(RolloutPhase::Off)
}

pub fn get_rollback_error_bps(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&RolloutKey::RollbackErrorBps)
        .unwrap_or(DEFAULT_ROLLBACK_ERROR_BPS)
}

pub fn get_state(env: &Env) -> RolloutState {
    RolloutState {
        phase: get_phase(env),
        canary_bps: get_canary_bps(env),
        canary: env.storage().instance().get(&RolloutKey::Canary),
        stable: env.storage().instance().get(&RolloutKey::Stable),
        rollback_error_bps: get_rollback_error_bps(env),
        flag_count: flag_index(env).len(),
        flag_cap: flag_cap(env),
    }
}

fn phase_from_bps(bps: u32) -> RolloutPhase {
    if bps == 0 {
        RolloutPhase::Off
    } else if bps >= BPS_DENOM {
        RolloutPhase::Full
    } else {
        RolloutPhase::Canary
    }
}

/// Register canary/stable contract IDs and the share of traffic sent to canary.
pub fn set_canary_deployment(
    env: &Env,
    canary: Address,
    stable: Address,
    canary_bps: u32,
) {
    if canary_bps > BPS_DENOM {
        panic!("canary_bps exceeds 10000");
    }
    env.storage().instance().set(&RolloutKey::Canary, &canary);
    env.storage().instance().set(&RolloutKey::Stable, &stable);
    env.storage()
        .instance()
        .set(&RolloutKey::CanaryBps, &canary_bps);
    let phase = phase_from_bps(canary_bps);
    env.storage().instance().set(&RolloutKey::Phase, &phase);
    env.events()
        .publish((symbol_short!("canary"),), (canary, stable, canary_bps));
}

pub fn set_canary_bps(env: &Env, canary_bps: u32) {
    if canary_bps > BPS_DENOM {
        panic!("canary_bps exceeds 10000");
    }
    env.storage()
        .instance()
        .set(&RolloutKey::CanaryBps, &canary_bps);
    env.storage()
        .instance()
        .set(&RolloutKey::Phase, &phase_from_bps(canary_bps));
}

/// Sticky traffic split: SHA-256(caller) mod 10000 < canary_bps.
///
/// Returns `false` when the rollout has been rolled back or the share is 0.
pub fn route_to_canary(env: &Env, caller: &Address) -> bool {
    if get_phase(env) == RolloutPhase::RolledBack {
        return false;
    }
    let canary_bps = get_canary_bps(env);
    if canary_bps == 0 {
        return false;
    }
    if canary_bps >= BPS_DENOM {
        return true;
    }
    let digest = env.crypto().sha256(&caller.clone().to_xdr(env));
    let bytes = digest.to_array();
    let bucket = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % BPS_DENOM;
    bucket < canary_bps
}

pub fn set_feature_flag(env: &Env, flag: &Symbol, enabled: bool) {
    env.storage()
        .instance()
        .set(&RolloutKey::Flag(flag.clone()), &enabled);
    let mut index = flag_index(env);
    let mut found = false;
    for existing in index.iter() {
        if existing == *flag {
            found = true;
            break;
        }
    }
    if !found {
        index.push_back(flag.clone());
        save_flag_index(env, &index);
    }
    env.events()
        .publish((symbol_short!("feat_flg"),), (flag.clone(), enabled));
}

pub fn is_feature_enabled(env: &Env, flag: &Symbol) -> bool {
    if get_phase(env) == RolloutPhase::RolledBack {
        return false;
    }
    env.storage()
        .instance()
        .get(&RolloutKey::Flag(flag.clone()))
        .unwrap_or(false)
}

pub fn set_rollback_trigger(env: &Env, error_bps: u32) {
    if error_bps == 0 || error_bps > BPS_DENOM {
        panic!("invalid rollback trigger");
    }
    env.storage()
        .instance()
        .set(&RolloutKey::RollbackErrorBps, &error_bps);
    env.events()
        .publish((symbol_short!("rb_trig"),), error_bps);
}

/// `true` when health error-rate meets the rollback trigger, or phase is already RolledBack.
pub fn should_rollback(env: &Env) -> bool {
    if get_phase(env) == RolloutPhase::RolledBack {
        return true;
    }
    let metrics = health::get_metrics(env);
    health::error_bps(&metrics) >= get_rollback_error_bps(env)
        && (metrics.ok_count + metrics.error_count) > 0
}

fn disable_all_flags(env: &Env) {
    let index = flag_index(env);
    for flag in index.iter() {
        env.storage()
            .instance()
            .set(&RolloutKey::Flag(flag.clone()), &false);
    }
}

fn apply_rollback(env: &Env) {
    env.storage()
        .instance()
        .set(&RolloutKey::CanaryBps, &0_u32);
    env.storage()
        .instance()
        .set(&RolloutKey::Phase, &RolloutPhase::RolledBack);
    disable_all_flags(env);
    env.events()
        .publish((symbol_short!("rollback"),), env.ledger().sequence());
}

/// Automatic rollback used by health checks when the trigger fires.
pub fn maybe_auto_rollback(env: &Env) -> bool {
    if get_phase(env) == RolloutPhase::RolledBack {
        return true;
    }
    if get_canary_bps(env) == 0 && flag_index(env).is_empty() {
        return false;
    }
    if should_rollback(env) {
        apply_rollback(env);
        return true;
    }
    false
}

/// Admin-initiated rollback: disable canary + flags and pause the contract.
/// Caller must already have authorized `admin` (the contract wrapper calls
/// `require_auth` once to avoid a double-auth HostError).
pub fn trigger_rollback(env: &Env, admin: &Address) {
    apply_rollback(env);
    env.storage()
        .instance()
        .set(&crate::pause::PauseDataKey::Paused, &true);
    env.events().publish(
        (soroban_sdk::Symbol::new(env, "contract_paused"),),
        crate::pause::ContractPausedEvent { admin: admin.clone() },
    );
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    /// A distinct symbol per index, so a cap of 32 means 32 real flags.
    fn flag(env: &Env, i: u32) -> Symbol {
        Symbol::new(env, std::format!("f{}", i).as_str())
    }

    #[test]
    fn flag_cap_defaults_to_the_ceiling() {
        let env = Env::default();
        assert_eq!(flag_cap(&env), MAX_FEATURE_FLAGS);
        assert_eq!(get_state(&env).flag_cap, MAX_FEATURE_FLAGS);
    }

    #[test]
    fn set_flag_cap_rejects_zero_and_anything_above_the_ceiling() {
        let env = Env::default();
        assert_eq!(set_flag_cap(&env, 0), Err(RolloutError::InvalidFlagCap));
        assert_eq!(
            set_flag_cap(&env, MAX_FEATURE_FLAGS + 1),
            Err(RolloutError::InvalidFlagCap)
        );
        // A lowering within the ceiling is accepted and reported.
        assert_eq!(set_flag_cap(&env, 4), Ok(()));
        assert_eq!(flag_cap(&env), 4);
    }

    #[test]
    fn exactly_the_cap_is_accepted() {
        let env = Env::default();
        for i in 0..MAX_FEATURE_FLAGS {
            set_feature_flag(&env, &flag(&env, i), true);
        }
        assert_eq!(get_state(&env).flag_count, MAX_FEATURE_FLAGS);
    }

    #[test]
    fn one_past_the_cap_is_rejected() {
        let env = Env::default();
        for i in 0..MAX_FEATURE_FLAGS {
            set_feature_flag(&env, &flag(&env, i), true);
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            set_feature_flag(&env, &flag(&env, MAX_FEATURE_FLAGS), true);
        }));
        assert!(res.is_err(), "flag {} must be refused", MAX_FEATURE_FLAGS);
        // The refused flag was not recorded.
        assert_eq!(get_state(&env).flag_count, MAX_FEATURE_FLAGS);
    }

    #[test]
    fn toggling_an_existing_flag_is_allowed_at_the_cap() {
        let env = Env::default();
        for i in 0..MAX_FEATURE_FLAGS {
            set_feature_flag(&env, &flag(&env, i), true);
        }
        // Re-setting an existing flag does not grow the index, so it is fine.
        set_feature_flag(&env, &flag(&env, 0), false);
        assert!(!is_feature_enabled(&env, &flag(&env, 0)));
        assert_eq!(get_state(&env).flag_count, MAX_FEATURE_FLAGS);
    }

    #[test]
    fn a_lowered_cap_blocks_new_flags_but_leaves_existing_ones_working() {
        let env = Env::default();
        for i in 0..4 {
            set_feature_flag(&env, &flag(&env, i), true);
        }
        set_flag_cap(&env, 2).unwrap();
        // Existing flags keep their value...
        assert!(is_feature_enabled(&env, &flag(&env, 1)));
        // ...but a new one is refused.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            set_feature_flag(&env, &flag(&env, 9), true);
        }));
        assert!(res.is_err());
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(RolloutError::FlagLimitReached as u32, 1);
        assert_eq!(RolloutError::InvalidFlagCap as u32, 2);
        assert_eq!(MAX_FEATURE_FLAGS, 32);
    }
}
