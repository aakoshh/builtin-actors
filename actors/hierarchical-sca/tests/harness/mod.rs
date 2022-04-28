use cid::Cid;
use fil_actors_runtime::Array;
use fvm_ipld_encoding::RawBytes;
use fvm_shared::address::Address;
use fvm_shared::bigint::bigint_ser::BigIntDe;
use fvm_shared::econ::TokenAmount;
use fvm_shared::MethodNum;
use lazy_static::lazy_static;

use fil_actors_runtime::builtin::HAMT_BIT_WIDTH;
use fil_actors_runtime::test_utils::{MockRuntime, SYSTEM_ACTOR_CODE_ID};
use fil_actors_runtime::{
    make_map_with_root_and_bitwidth, STORAGE_POWER_ACTOR_ADDR, SYSTEM_ACTOR_ADDR,
};
use hierarchical_sca::{
    ConstructorParams, Method, State, CROSSMSG_AMT_BITWIDTH, DEFAULT_CHECKPOINT_PERIOD, MAX_NONCE,
    MIN_COLLATERAL_AMOUNT,
};

use crate::SCAActor;

lazy_static! {
    pub static ref OWNER: Address = Address::new_id(101);
    pub static ref MINER: Address = Address::new_id(201);
    pub static ref ACTOR: Address = Address::new_actor("actor".as_bytes());
}

pub fn new_runtime() -> MockRuntime {
    MockRuntime {
        receiver: *STORAGE_POWER_ACTOR_ADDR,
        caller: *SYSTEM_ACTOR_ADDR,
        caller_type: *SYSTEM_ACTOR_CODE_ID,
        ..Default::default()
    }
}

pub fn new_harness() -> Harness {
    Harness { net_name: String::from("/root") }
}

pub fn setup() -> (Harness, MockRuntime) {
    let mut rt = new_runtime();
    let h = new_harness();
    h.construct(&mut rt);
    (h, rt)
}

#[allow(dead_code)]
pub struct Harness {
    net_name: String,
}

impl Harness {
    pub fn construct(&self, rt: &mut MockRuntime) {
        rt.expect_validate_caller_addr(vec![*SYSTEM_ACTOR_ADDR]);
        let params =
            ConstructorParams { network_name: self.net_name.clone(), checkpoint_period: 10 };
        rt.call::<SCAActor>(
            Method::Constructor as MethodNum,
            &RawBytes::serialize(params).unwrap(),
        )
        .unwrap();
        rt.verify()
    }

    pub fn construct_and_verify(&self, rt: &mut MockRuntime) {
        self.construct(rt);
        let st: State = rt.get_state().unwrap();
        let store = &rt.store;

        let empty_bottomup_array =
            Array::<(), _>::new_with_bit_width(store, CROSSMSG_AMT_BITWIDTH).flush().unwrap();

        assert_eq!(st.network_name, self.net_name);
        assert_eq!(st.min_stake, TokenAmount::from(MIN_COLLATERAL_AMOUNT));
        assert_eq!(st.check_period, DEFAULT_CHECKPOINT_PERIOD);
        assert_eq!(st.applied_bottomup_nonce, MAX_NONCE);
        assert_eq!(st.bottom_up_msg_meta, empty_bottomup_array);
        verify_empty_map(rt, st.subnets);
        verify_empty_map(rt, st.checkpoints);
        verify_empty_map(rt, st.check_msg_registry);
        verify_empty_map(rt, st.atomic_exec_registry);
    }

    pub fn check_state(&self) {
        // TODO: https://github.com/filecoin-project/builtin-actors/issues/44
    }
}

pub fn verify_empty_map(rt: &MockRuntime, key: Cid) {
    let map =
        make_map_with_root_and_bitwidth::<_, BigIntDe>(&key, &rt.store, HAMT_BIT_WIDTH).unwrap();
    map.for_each(|_key, _val| panic!("expected no keys")).unwrap();
}
