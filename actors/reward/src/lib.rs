// Copyright 2019-2022 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use fil_actors_runtime::runtime::{ActorCode, Runtime};
use fil_actors_runtime::{
    actor_error, cbor, ActorError, BURNT_FUNDS_ACTOR_ADDR, EXPECTED_LEADERS_PER_EPOCH,
    STORAGE_POWER_ACTOR_ADDR, SYSTEM_ACTOR_ADDR,
};
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::RawBytes;
use fvm_shared::actor::builtin::Type;
use fvm_shared::bigint::bigint_ser::BigIntDe;
use fvm_shared::bigint::{Integer, Sign};
use fvm_shared::econ::TokenAmount;
use fvm_shared::sector::StoragePower;
use fvm_shared::{MethodNum, METHOD_CONSTRUCTOR, METHOD_SEND};
use log::{error, warn};
use num_derive::FromPrimitive;
use num_traits::{FromPrimitive, Signed};

pub use self::logic::*;
pub use self::state::{Reward, State, VestingFunction};
pub use self::types::*;

#[cfg(feature = "fil-actor")]
fil_actors_runtime::wasm_trampoline!(Actor);

pub(crate) mod expneg;
mod logic;
mod state;
mod types;

// only exported for tests
#[doc(hidden)]
pub mod ext;

// * Updated to specs-actors commit: 999e57a151cc7ada020ca2844b651499ab8c0dec (v3.0.1)

/// PenaltyMultiplier is the factor miner penaltys are scaled up by
pub const PENALTY_MULTIPLIER: u64 = 3;

/// Reward actor methods available
#[derive(FromPrimitive)]
#[repr(u64)]
pub enum Method {
    Constructor = METHOD_CONSTRUCTOR,
    AwardBlockReward = 2,
    ThisEpochReward = 3,
    UpdateNetworkKPI = 4,
    ExternalFunding = 5,
}

/// Reward Actor
pub struct Actor;
impl Actor {
    /// Constructor for Reward actor
    fn constructor<BS, RT>(
        rt: &mut RT,
        curr_realized_power: Option<StoragePower>,
    ) -> Result<(), ActorError>
    where
        BS: Blockstore,
        RT: Runtime<BS>,
    {
        rt.validate_immediate_caller_is(std::iter::once(&*SYSTEM_ACTOR_ADDR))?;

        if let Some(power) = curr_realized_power {
            rt.create(&State::new(power))?;
            Ok(())
        } else {
            Err(actor_error!(illegal_argument, "argument should not be nil"))
        }
    }

    /// Awards a reward to a block producer.
    /// This method is called only by the system actor, implicitly, as the last message in the evaluation of a block.
    /// The system actor thus computes the parameters and attached value.
    ///
    /// The reward includes two components:
    /// - the epoch block reward, computed and paid from the reward actor's balance,
    /// - the block gas reward, expected to be transferred to the reward actor with this invocation.
    ///
    /// The reward is reduced before the residual is credited to the block producer, by:
    /// - a penalty amount, provided as a parameter, which is burnt,
    fn award_block_reward<BS, RT>(
        rt: &mut RT,
        params: AwardBlockRewardParams,
    ) -> Result<(), ActorError>
    where
        BS: Blockstore,
        RT: Runtime<BS>,
    {
        rt.validate_immediate_caller_is(std::iter::once(&*SYSTEM_ACTOR_ADDR))?;
        let prior_balance = rt.current_balance();
        if params.penalty.sign() == Sign::Minus {
            return Err(actor_error!(illegal_argument, "negative penalty {}", params.penalty));
        }
        if params.gas_reward.sign() == Sign::Minus {
            return Err(actor_error!(
                illegal_argument,
                "negative gas reward {}",
                params.gas_reward
            ));
        }
        if prior_balance < params.gas_reward {
            return Err(actor_error!(
                illegal_state,
                "actor current balance {} insufficient to pay gas reward {}",
                prior_balance,
                params.gas_reward
            ));
        }
        if params.win_count <= 0 {
            return Err(actor_error!(illegal_argument, "invalid win count {}", params.win_count));
        }

        let miner_addr = rt
            .resolve_address(&params.miner)
            .ok_or_else(|| actor_error!(not_found, "failed to resolve given owner address"))?;

        let penalty: TokenAmount = &params.penalty * PENALTY_MULTIPLIER;

        let total_reward = rt.transaction(|st: &mut State, rt| {
            let mut block_reward: TokenAmount = (&st.this_epoch_reward * params.win_count)
                .div_floor(&TokenAmount::from(EXPECTED_LEADERS_PER_EPOCH));
            let mut total_reward = &params.gas_reward + &block_reward;
            let curr_balance = rt.current_balance();
            if total_reward > curr_balance {
                warn!(
                    "reward actor balance {} below totalReward expected {},\
                    paying out rest of balance",
                    curr_balance, total_reward
                );
                total_reward = curr_balance;
                block_reward = &total_reward - &params.gas_reward;
                if block_reward.is_negative() {
                    return Err(actor_error!(
                        illegal_state,
                        "programming error, block reward {} below zero",
                        block_reward
                    ));
                }
            }
            st.total_storage_power_reward += block_reward;
            Ok(total_reward)
        })?;

        // * Go implementation added this and removed capping it -- this could potentially panic
        // * as they treat panics as an exit code. Revisit this.
        if total_reward > prior_balance {
            return Err(actor_error!(
                illegal_state,
                "reward {} exceeds balance {}",
                total_reward,
                prior_balance
            ));
        }

        // if this fails, we can assume the miner is responsible and avoid failing here.
        let reward_params = ext::miner::ApplyRewardParams { reward: total_reward.clone(), penalty };
        let res = rt.send(
            miner_addr,
            ext::miner::APPLY_REWARDS_METHOD,
            RawBytes::serialize(&reward_params)?,
            total_reward.clone(),
        );
        if let Err(e) = res {
            error!(
                "failed to send ApplyRewards call to the miner actor with funds {}, code: {:?}",
                total_reward,
                e.exit_code()
            );
            let res =
                rt.send(*BURNT_FUNDS_ACTOR_ADDR, METHOD_SEND, RawBytes::default(), total_reward);
            if let Err(e) = res {
                error!(
                    "failed to send unsent reward to the burnt funds actor, code: {:?}",
                    e.exit_code()
                );
            }
        }

        Ok(())
    }

    /// The award value used for the current epoch, updated at the end of an epoch
    /// through cron tick.  In the case previous epochs were null blocks this
    /// is the reward value as calculated at the last non-null epoch.
    fn this_epoch_reward<BS, RT>(rt: &mut RT) -> Result<ThisEpochRewardReturn, ActorError>
    where
        BS: Blockstore,
        RT: Runtime<BS>,
    {
        rt.validate_immediate_caller_accept_any()?;
        let st: State = rt.state()?;
        Ok(ThisEpochRewardReturn {
            this_epoch_baseline_power: st.this_epoch_baseline_power,
            this_epoch_reward_smoothed: st.this_epoch_reward_smoothed,
        })
    }

    /// Called at the end of each epoch by the power actor (in turn by its cron hook).
    /// This is only invoked for non-empty tipsets, but catches up any number of null
    /// epochs to compute the next epoch reward.
    fn update_network_kpi<BS, RT>(
        rt: &mut RT,
        curr_realized_power: Option<StoragePower>,
    ) -> Result<(), ActorError>
    where
        BS: Blockstore,
        RT: Runtime<BS>,
    {
        rt.validate_immediate_caller_is(std::iter::once(&*STORAGE_POWER_ACTOR_ADDR))?;
        let curr_realized_power = curr_realized_power
            .ok_or_else(|| actor_error!(illegal_argument, "argument cannot be None"))?;

        rt.transaction(|st: &mut State, rt| {
            let prev = st.epoch;
            // if there were null runs catch up the computation until
            // st.Epoch == rt.CurrEpoch()
            while st.epoch < rt.curr_epoch() {
                // Update to next epoch to process null rounds
                st.update_to_next_epoch(&curr_realized_power);
            }

            st.update_to_next_epoch_with_reward(&curr_realized_power);
            st.update_smoothed_estimates(st.epoch - prev);
            Ok(())
        })?;
        Ok(())
    }

    /// ExternalFunding sends to an address funding coming from another subnet.
    /// This is used as part of hierarchical consensus to orchestrate the minting and burning
    /// dance between the subnets exchanging cross-messages
    fn external_funding<BS, RT>(rt: &mut RT, params: FundingParams) -> Result<(), ActorError>
    where
        BS: Blockstore,
        RT: Runtime<BS>,
    {
        // NOTE: The reward actor in subnets should give no reward to miners, so much of this actor will be
        // simplified, giving "infinite" balance to this actor won't mess up with FIL's circulating supply because
        // the only way to move funds from here is by externally funding.
        rt.validate_immediate_caller_type(std::iter::once(&Type::SCA))?;
        if params.value > rt.current_balance() {
            return Err(actor_error!(illegal_state, "fatal error: reward actor balance below the value that needs to be minted for subnet"));
        }

        let addr = rt
            .resolve_address(&params.addr)
            .ok_or_else(|| actor_error!(not_found, "failed to resolve given owner address"))?;
        rt.send(addr, METHOD_SEND, RawBytes::default(), params.value)?;
        Ok(())
    }
}

impl ActorCode for Actor {
    fn invoke_method<BS, RT>(
        rt: &mut RT,
        method: MethodNum,
        params: &RawBytes,
    ) -> Result<RawBytes, ActorError>
    where
        BS: Blockstore,
        RT: Runtime<BS>,
    {
        match FromPrimitive::from_u64(method) {
            Some(Method::Constructor) => {
                let param: Option<BigIntDe> = cbor::deserialize_params(params)?;
                Self::constructor(rt, param.map(|v| v.0))?;
                Ok(RawBytes::default())
            }
            Some(Method::AwardBlockReward) => {
                Self::award_block_reward(rt, cbor::deserialize_params(params)?)?;
                Ok(RawBytes::default())
            }
            Some(Method::ThisEpochReward) => {
                let res = Self::this_epoch_reward(rt)?;
                Ok(RawBytes::serialize(&res)?)
            }
            Some(Method::UpdateNetworkKPI) => {
                let param: Option<BigIntDe> = cbor::deserialize_params(params)?;
                Self::update_network_kpi(rt, param.map(|v| v.0))?;
                Ok(RawBytes::default())
            }
            Some(Method::ExternalFunding) => {
                Self::external_funding(rt, cbor::deserialize_params(params)?)?;
                Ok(RawBytes::default())
            }
            None => Err(actor_error!(unhandled_message, "Invalid method")),
        }
    }
}
