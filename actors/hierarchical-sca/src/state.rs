// Copyright: ConsensusLab
//
use anyhow::anyhow;
use cid::Cid;
use fil_actors_runtime::runtime::Runtime;
use fil_actors_runtime::{
    make_empty_map, make_map_with_root_and_bitwidth, ActorDowncast, Array, Map,
};
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::tuple::*;
use fvm_ipld_encoding::Cbor;
use fvm_ipld_hamt::BytesKey;
use fvm_shared::address::SubnetID;
use fvm_shared::bigint::{bigint_ser, BigInt};
use fvm_shared::clock::ChainEpoch;
use fvm_shared::econ::TokenAmount;
use fvm_shared::error::ExitCode;
use fvm_shared::HAMT_BIT_WIDTH;
use lazy_static::lazy_static;
use num_traits::Zero;
use std::collections::HashMap;
use std::str::FromStr;

use super::checkpoint::*;
use super::cross::*;
use super::subnet::*;
use super::types::*;

/// Storage power actor state
#[derive(Default, Serialize_tuple, Deserialize_tuple)]
pub struct State {
    pub network_name: SubnetID,
    pub total_subnets: u64,
    #[serde(with = "bigint_ser")]
    pub min_stake: TokenAmount,
    pub subnets: Cid, // HAMT[cid.Cid]Subnet
    pub check_period: ChainEpoch,
    pub checkpoints: Cid,        // HAMT[epoch]Checkpoint
    pub check_msg_registry: Cid, // HAMT[cid]CrossMsgs
    pub nonce: u64,
    pub bottomup_nonce: u64,
    pub bottomup_msg_meta: Cid, // AMT[CrossMsgMeta] from child subnets to apply.
    pub applied_bottomup_nonce: u64,
    pub applied_topdown_nonce: u64,
    pub atomic_exec_registry: Cid, // HAMT[cid]AtomicExec
}

lazy_static! {
    static ref MIN_SUBNET_COLLATERAL: BigInt = TokenAmount::from(MIN_COLLATERAL_AMOUNT);
}

impl Cbor for State {}

impl State {
    pub fn new<BS: Blockstore>(store: &BS, params: ConstructorParams) -> anyhow::Result<State> {
        let empty_sn_map = make_empty_map::<_, ()>(store, HAMT_BIT_WIDTH)
            .flush()
            .map_err(|e| anyhow!("Failed to create empty map: {}", e))?;
        let empty_checkpoint_map = make_empty_map::<_, ()>(store, HAMT_BIT_WIDTH)
            .flush()
            .map_err(|e| anyhow!("Failed to create empty map: {}", e))?;
        let empty_meta_map = make_empty_map::<_, ()>(store, HAMT_BIT_WIDTH)
            .flush()
            .map_err(|e| anyhow!("Failed to create empty map: {}", e))?;
        let empty_atomic_map = make_empty_map::<_, ()>(store, HAMT_BIT_WIDTH)
            .flush()
            .map_err(|e| anyhow!("Failed to create empty map: {}", e))?;
        let empty_bottomup_array =
            Array::<(), BS>::new_with_bit_width(store, CROSSMSG_AMT_BITWIDTH)
                .flush()
                .map_err(|e| anyhow!("Failed to create empty messages array: {}", e))?;
        Ok(State {
            network_name: SubnetID::from_str(&params.network_name)?,
            min_stake: MIN_SUBNET_COLLATERAL.clone(),
            check_period: match params.checkpoint_period > DEFAULT_CHECKPOINT_PERIOD {
                true => params.checkpoint_period,
                false => DEFAULT_CHECKPOINT_PERIOD,
            },
            subnets: empty_sn_map,
            checkpoints: empty_checkpoint_map,
            check_msg_registry: empty_meta_map,
            bottomup_msg_meta: empty_bottomup_array,
            applied_bottomup_nonce: MAX_NONCE,
            atomic_exec_registry: empty_atomic_map,
            ..Default::default()
        })
    }

    /// Get content for a child subnet.
    pub fn get_subnet<BS: Blockstore>(
        &self,
        store: &BS,
        id: &SubnetID,
    ) -> anyhow::Result<Option<Subnet>> {
        let subnets =
            make_map_with_root_and_bitwidth::<_, Subnet>(&self.subnets, store, HAMT_BIT_WIDTH)
                .map_err(|e| {
                    e.downcast_default(ExitCode::USR_ILLEGAL_STATE, "failed to load subnets")
                })?;

        let subnet = get_subnet(&subnets, id)?;
        Ok(subnet.cloned())
    }

    /// Register a subnet in the map of subnets and flush.
    pub(crate) fn register_subnet<BS, RT>(&mut self, rt: &RT, id: &SubnetID) -> anyhow::Result<()>
    where
        BS: Blockstore,
        RT: Runtime<BS>,
    {
        let val = rt.message().value_received();
        if val < self.min_stake {
            return Err(anyhow!("call to register doesn't include enough funds"));
        }
        let mut subnets =
            make_map_with_root_and_bitwidth::<_, Subnet>(&self.subnets, rt.store(), HAMT_BIT_WIDTH)
                .map_err(|e| {
                    e.downcast_default(ExitCode::USR_ILLEGAL_STATE, "failed to load subnets")
                })?;

        let empty_topdown_array =
            Array::<(), BS>::new_with_bit_width(rt.store(), CROSSMSG_AMT_BITWIDTH)
                .flush()
                .map_err(|e| anyhow!("Failed to create empty messages array: {}", e))?;

        let subnet = Subnet {
            id: id.clone(),
            stake: val,
            top_down_msgs: empty_topdown_array,
            circ_supply: TokenAmount::zero(),
            status: Status::Active,
            nonce: 0,
            prev_checkpoint: Checkpoint::default(),
        };
        set_subnet(&mut subnets, &id, subnet)?;
        self.subnets = subnets.flush().map_err(|e| {
            e.downcast_default(ExitCode::USR_ILLEGAL_STATE, "failed to flush subnets")
        })?;
        self.total_subnets += 1;
        Ok(())
    }

    /// Remove a subnet from the map of subnets and flush.
    pub(crate) fn rm_subnet<BS: Blockstore>(
        &mut self,
        store: &BS,
        id: &SubnetID,
    ) -> anyhow::Result<()> {
        let mut subnets =
            make_map_with_root_and_bitwidth::<_, Subnet>(&self.subnets, store, HAMT_BIT_WIDTH)
                .map_err(|e| {
                    e.downcast_default(ExitCode::USR_ILLEGAL_STATE, "failed to load subnets")
                })?;
        subnets
            .delete(&id.to_bytes())
            .map_err(|e| e.downcast_wrap(format!("failed to delete subnet for id {}", id)))?;
        self.subnets = subnets.flush().map_err(|e| {
            e.downcast_default(ExitCode::USR_ILLEGAL_STATE, "failed to flush subnets")
        })?;
        self.total_subnets -= 1;
        Ok(())
    }

    /// flush a subnet
    pub(crate) fn flush_subnet<BS: Blockstore>(
        &mut self,
        store: &BS,
        sub: &Subnet,
    ) -> anyhow::Result<()> {
        let mut subnets =
            make_map_with_root_and_bitwidth::<_, Subnet>(&self.subnets, store, HAMT_BIT_WIDTH)
                .map_err(|e| anyhow!("error loading subnets: {}", e))?;
        set_subnet(&mut subnets, &sub.id, sub.clone())?;
        self.subnets = subnets.flush().map_err(|e| anyhow!("error flushing subnets: {}", e))?;
        Ok(())
    }

    /// flush a checkpoint
    pub(crate) fn flush_checkpoint<BS: Blockstore>(
        &mut self,
        store: &BS,
        ch: &Checkpoint,
    ) -> anyhow::Result<()> {
        let mut checkpoints = make_map_with_root_and_bitwidth::<_, Checkpoint>(
            &self.checkpoints,
            store,
            HAMT_BIT_WIDTH,
        )
        .map_err(|e| anyhow!("error loading checkpoints: {}", e))?;
        set_checkpoint(&mut checkpoints, ch.clone())?;
        self.checkpoints =
            checkpoints.flush().map_err(|e| anyhow!("error flushing checkpoints: {}", e))?;
        Ok(())
    }

    /// get checkpoint being populated in the current window.
    pub fn get_window_checkpoint<'m, BS: Blockstore>(
        &self,
        store: &'m BS,
        epoch: ChainEpoch,
    ) -> anyhow::Result<Checkpoint> {
        if epoch < 0 {
            return Err(anyhow!("epoch can't be negative"));
        }
        let ch_epoch = checkpoint_epoch(epoch, self.check_period);
        let checkpoints = make_map_with_root_and_bitwidth::<_, Checkpoint>(
            &self.checkpoints,
            store,
            HAMT_BIT_WIDTH,
        )
        .map_err(|e| {
            e.downcast_default(ExitCode::USR_ILLEGAL_STATE, "failed to load checkpoints")
        })?;

        let out_ch = match get_checkpoint(&checkpoints, &ch_epoch)? {
            Some(ch) => ch.clone(),
            None => Checkpoint::new(self.network_name.clone(), ch_epoch),
        };

        Ok(out_ch)
    }

    /// apply the cross-messages included in a checkpoint.
    pub(crate) fn apply_check_msgs<'m, BS: Blockstore>(
        &mut self,
        store: &'m BS,
        sub: &mut Subnet,
        commit: &'m Checkpoint,
    ) -> anyhow::Result<(TokenAmount, HashMap<SubnetID, Vec<&'m CrossMsgMeta>>)> {
        let mut burn_val = TokenAmount::zero();
        let mut aux: HashMap<SubnetID, Vec<&CrossMsgMeta>> = HashMap::new();

        // if cross-msgs directed to current network
        for mm in commit.cross_msgs() {
            if mm.to == self.network_name {
                self.store_bottomup_msg(&store, mm)
                    .map_err(|e| anyhow!("error storing bottomup msg: {}", e))?;
            } else {
                // if we are not the parent, someone is trying to forge messages
                if mm.from.parent().unwrap_or_else(|| SubnetID::default()) != self.network_name {
                    continue;
                }
                let meta = aux.entry(mm.to.clone()).or_insert(vec![mm]);
                (*meta).push(mm);
            }
            burn_val += &mm.value;
            self.release_circ_supply(store, sub, &mm.from, &mm.value)?;
        }

        Ok((burn_val, aux))
    }

    /// aggregate child message meta that are not directed for the current
    /// subnet to propagate them further.
    pub(crate) fn agg_child_msgmeta<BS: Blockstore>(
        &mut self,
        store: &BS,
        ch: &mut Checkpoint,
        aux: HashMap<SubnetID, Vec<&CrossMsgMeta>>,
    ) -> anyhow::Result<()> {
        for (to, mm) in aux.into_iter() {
            // aggregate values inside msgmeta
            let value = mm.iter().fold(TokenAmount::zero(), |acc, x| acc + &x.value);
            let metas = mm.into_iter().cloned().collect();

            match ch.crossmsg_meta_index(&self.network_name, &to) {
                Some(index) => {
                    let msgmeta = &mut ch.data.cross_msgs[index];
                    let prev_cid = msgmeta.msgs_cid;
                    let m_cid = self.append_metas_to_meta(store, &prev_cid, metas)?;
                    msgmeta.msgs_cid = m_cid;
                    msgmeta.value += value;
                }
                None => {
                    let mut msgmeta = CrossMsgMeta::new(&self.network_name, &to);
                    let mut n_mt = CrossMsgs::new();
                    n_mt.metas = metas;
                    let mut cross_reg = make_map_with_root_and_bitwidth::<_, CrossMsgs>(
                        &self.check_msg_registry,
                        store,
                        HAMT_BIT_WIDTH,
                    )?;

                    let meta_cid = put_msgmeta(&mut cross_reg, n_mt)?;
                    self.check_msg_registry = cross_reg.flush()?;
                    msgmeta.value += &value;
                    msgmeta.msgs_cid = meta_cid;
                    ch.append_msgmeta(msgmeta)?;
                }
            };
        }

        Ok(())
    }

    /// store a cross message in the current checkpoint for propagation
    // TODO: We can probably de-duplicate a lot of code from agg_child_msgmeta
    pub(crate) fn store_msg_in_checkpoint<BS: Blockstore>(
        &mut self,
        store: &BS,
        msg: &StorableMsg,
        curr_epoch: ChainEpoch,
    ) -> anyhow::Result<()> {
        let mut ch = self.get_window_checkpoint(store, curr_epoch)?;

        let sto = msg.to.subnet()?;
        let sfrom = msg.from.subnet()?;
        match ch.crossmsg_meta_index(&sfrom, &sto) {
            Some(index) => {
                let msgmeta = &mut ch.data.cross_msgs[index];
                let prev_cid = msgmeta.msgs_cid;
                let m_cid = self.append_msg_to_meta(store, &prev_cid, msg)?;
                msgmeta.msgs_cid = m_cid;
                msgmeta.value += &msg.value;
            }
            None => {
                let mut msgmeta = CrossMsgMeta::new(&sfrom, &sto);
                let mut n_mt = CrossMsgs::new();
                n_mt.msgs = vec![msg.clone()];
                let mut cross_reg = make_map_with_root_and_bitwidth::<_, CrossMsgs>(
                    &self.check_msg_registry,
                    store,
                    HAMT_BIT_WIDTH,
                )?;

                let meta_cid = put_msgmeta(&mut cross_reg, n_mt)?;
                self.check_msg_registry = cross_reg.flush()?;
                msgmeta.value += &msg.value;
                msgmeta.msgs_cid = meta_cid;
                ch.append_msgmeta(msgmeta)?;
            }
        };

        // flush checkpoint
        self.flush_checkpoint(store, &ch).map_err(|e| {
            e.downcast_default(ExitCode::USR_ILLEGAL_STATE, "error flushing checkpoint")
        })?;

        Ok(())
    }

    /// append crossmsg_meta to a specific mesasge meta.
    pub(crate) fn append_metas_to_meta<BS: Blockstore>(
        &mut self,
        store: &BS,
        meta_cid: &Cid,
        metas: Vec<CrossMsgMeta>,
    ) -> anyhow::Result<Cid> {
        let mut cross_reg = make_map_with_root_and_bitwidth::<_, CrossMsgs>(
            &self.check_msg_registry,
            store,
            HAMT_BIT_WIDTH,
        )?;

        // get previous meta stored
        let mut prev_meta = match cross_reg.get(&meta_cid.to_bytes())? {
            Some(m) => m.clone(),
            None => return Err(anyhow!("no msgmeta found for cid")),
        };

        prev_meta.add_metas(metas)?;

        // if the cid hasn't changed
        let cid = prev_meta.cid()?;
        if &cid == meta_cid {
            return Ok(cid);
        }
        // else we persist the new msgmeta
        self.put_delete_flush_meta(&mut cross_reg, meta_cid, prev_meta)
    }

    /// append crossmsg to a specific mesasge meta.
    // TODO: Consider de-duplicating code from append_metas_to_meta
    // if possible
    pub(crate) fn append_msg_to_meta<BS: Blockstore>(
        &mut self,
        store: &BS,
        meta_cid: &Cid,
        msg: &StorableMsg,
    ) -> anyhow::Result<Cid> {
        let mut cross_reg = make_map_with_root_and_bitwidth::<_, CrossMsgs>(
            &self.check_msg_registry,
            store,
            HAMT_BIT_WIDTH,
        )?;

        // get previous meta stored
        let mut prev_meta = match cross_reg.get(&meta_cid.to_bytes())? {
            Some(m) => m.clone(),
            None => return Err(anyhow!("no msgmeta found for cid")),
        };

        prev_meta.add_msg(&msg)?;

        // if the cid hasn't changed
        let cid = prev_meta.cid()?;
        if &cid == meta_cid {
            return Ok(cid);
        }
        // else we persist the new msgmeta
        self.put_delete_flush_meta(&mut cross_reg, meta_cid, prev_meta)
    }

    /// update a message meta and remove the old one.
    pub(crate) fn put_delete_flush_meta<BS: Blockstore>(
        &mut self,
        registry: &mut Map<BS, CrossMsgs>,
        prev_cid: &Cid,
        meta: CrossMsgs,
    ) -> anyhow::Result<Cid> {
        // add new meta
        let m_cid = put_msgmeta(registry, meta)?;
        // remove the previous one
        registry.delete(&prev_cid.to_bytes())?;
        // flush
        self.check_msg_registry =
            registry.flush().map_err(|e| anyhow!("error flushing crossmsg registry: {}", e))?;

        Ok(m_cid)
    }

    /// release circulating supply from a subent
    ///
    /// This is triggered through bottom-up messages sending subnet tokens
    /// to some other subnet in the hierarchy.
    pub(crate) fn release_circ_supply<BS: Blockstore>(
        &mut self,
        store: &BS,
        curr: &mut Subnet,
        id: &SubnetID,
        val: &TokenAmount,
    ) -> anyhow::Result<()> {
        // if current subnet, we don't need to get the
        // subnet again
        if curr.id == *id {
            curr.release_supply(val)?;
            return Ok(());
        }

        let sub =
            self.get_subnet(store, id).map_err(|e| anyhow!("failed to load subnet: {}", e))?;
        match sub {
            Some(mut sub) => {
                sub.release_supply(val)?;
                self.flush_subnet(store, &sub)
            }
            None => return Err(anyhow!("subnet with id {} not registered", id)),
        }?;
        Ok(())
    }

    /// store bottomup messages for their execution in the subnet
    pub(crate) fn store_bottomup_msg<BS: Blockstore>(
        &mut self,
        store: &BS,
        meta: &CrossMsgMeta,
    ) -> anyhow::Result<()> {
        let mut crossmsgs = CrossMsgMetaArray::load(&self.bottomup_msg_meta, store)
            .map_err(|e| anyhow!("failed to load crossmsg meta array: {}", e))?;

        let mut new_meta = meta.clone();
        new_meta.nonce = self.bottomup_nonce;
        crossmsgs
            .set(new_meta.nonce, new_meta)
            .map_err(|e| anyhow!("failed to set crossmsg meta array: {}", e))?;
        self.bottomup_msg_meta = crossmsgs.flush()?;

        self.bottomup_nonce += 1;
        Ok(())
    }

    /// commit topdown messages for their execution in the subnet
    pub(crate) fn commit_topdown_msg<BS: Blockstore>(
        &mut self,
        store: &BS,
        msg: &mut StorableMsg,
    ) -> anyhow::Result<()> {
        let sto = msg.to.subnet()?;
        // let sfrom = msg.from.subnet()?;

        let sub = self
            .get_subnet(
                store,
                match &sto.down(&self.network_name) {
                    Some(sub) => sub,
                    None => return Err(anyhow!("couldn't compute the next subnet in route")),
                },
            )
            .map_err(|e| {
                e.downcast_default(ExitCode::USR_ILLEGAL_STATE, "failed to load subnet")
            })?;
        match sub {
            Some(mut sub) => {
                msg.nonce = sub.nonce;
                sub.store_topdown_msg(store, &msg)?;
                sub.nonce += 1;
                sub.circ_supply += &msg.value;
                self.flush_subnet(store, &sub)?;
            }
            None => {
                if sto == self.network_name {
                    return Err(anyhow!("can't direct top-down message to the current subnet"));
                } else {
                    self.noop_msg();
                }
            }
        }
        Ok(())
    }

    /// commit bottomup messages for their execution in the subnet
    pub(crate) fn commit_bottomup_msg<BS: Blockstore>(
        &mut self,
        store: &BS,
        msg: &StorableMsg,
        curr_epoch: ChainEpoch,
    ) -> anyhow::Result<()> {
        // store msg in checkpoint for propagation
        self.store_msg_in_checkpoint(store, &msg, curr_epoch)?;
        // increment nonce
        self.nonce += 1;

        Ok(())
    }

    /// commits a cross-msg for propagation
    pub(crate) fn send_cross<BS: Blockstore>(
        &mut self,
        store: &BS,
        msg: &mut StorableMsg,
        curr_epoch: ChainEpoch,
    ) -> anyhow::Result<HCMsgType> {
        let tp = msg.hc_type()?;
        match tp {
            HCMsgType::TopDown => self.commit_topdown_msg(store, msg)?,
            HCMsgType::BottomUp => self.commit_bottomup_msg(store, msg, curr_epoch)?,
            _ => return Err(anyhow!("cross-msg is not of the right type")),
        };
        Ok(tp)
    }

    /// noop is triggered to notify when a crossMsg fails to be applied successfully.
    pub fn noop_msg(&self) {
        panic!("error committing cross-msg. noop should be returned but not implemented yet");
    }
}

pub fn set_subnet<BS: Blockstore>(
    subnets: &mut Map<BS, Subnet>,
    id: &SubnetID,
    subnet: Subnet,
) -> anyhow::Result<()> {
    subnets
        .set(id.to_bytes().into(), subnet)
        .map_err(|e| e.downcast_wrap(format!("failed to set subnet for id {}", id)))?;
    Ok(())
}

fn get_subnet<'m, BS: Blockstore>(
    subnets: &'m Map<BS, Subnet>,
    id: &SubnetID,
) -> anyhow::Result<Option<&'m Subnet>> {
    subnets
        .get(&id.to_bytes())
        .map_err(|e| e.downcast_wrap(format!("failed to get subnet for id {}", id)))
}

pub fn set_checkpoint<BS: Blockstore>(
    checkpoints: &mut Map<BS, Checkpoint>,
    ch: Checkpoint,
) -> anyhow::Result<()> {
    let epoch = ch.epoch();
    checkpoints
        .set(BytesKey::from(epoch.to_ne_bytes().to_vec()), ch)
        .map_err(|e| e.downcast_wrap(format!("failed to set checkpoint for epoch {}", epoch)))?;
    Ok(())
}

fn get_checkpoint<'m, BS: Blockstore>(
    checkpoints: &'m Map<BS, Checkpoint>,
    epoch: &ChainEpoch,
) -> anyhow::Result<Option<&'m Checkpoint>> {
    checkpoints
        .get(&BytesKey::from(epoch.to_ne_bytes().to_vec()))
        .map_err(|e| e.downcast_wrap(format!("failed to get checkpoint for id {}", epoch)))
}

fn put_msgmeta<BS: Blockstore>(
    registry: &mut Map<BS, CrossMsgs>,
    metas: CrossMsgs,
) -> anyhow::Result<Cid> {
    let m_cid = metas.cid()?;
    registry
        .set(m_cid.to_bytes().into(), metas)
        .map_err(|e| e.downcast_wrap(format!("failed to set crossmsg meta for cid {}", m_cid)))?;
    Ok(m_cid)
}

pub fn get_bottomup_msg<'m, BS: Blockstore>(
    crossmsgs: &'m CrossMsgMetaArray<BS>,
    nonce: u64,
) -> anyhow::Result<Option<&'m CrossMsgMeta>> {
    crossmsgs.get(nonce).map_err(|e| anyhow!("failed to get crossmsg meta by nonce: {}", e))
}

pub fn get_topdown_msg<'m, BS: Blockstore>(
    crossmsgs: &'m CrossMsgArray<BS>,
    nonce: u64,
) -> anyhow::Result<Option<&'m StorableMsg>> {
    crossmsgs.get(nonce).map_err(|e| anyhow!("failed to get msg by nonce: {}", e))
}
