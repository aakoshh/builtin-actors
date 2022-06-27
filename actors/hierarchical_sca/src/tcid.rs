use std::{any::type_name, marker::PhantomData};

use anyhow::{anyhow, Error, Result};
use cid::Cid;
use fil_actors_runtime::{
    builtin::HAMT_BIT_WIDTH, fvm_ipld_amt::Amt, make_empty_map, make_map_with_root_and_bitwidth,
};
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::{Cbor, CborStore};
use fvm_ipld_hamt::Hamt;

const AMT_BIT_WIDTH: u32 = 32;

/// Helper type to be able to define `Code` as a generic parameter.
pub trait CodeType {
    fn code() -> cid::multihash::Code;
}

pub mod codes {
    use super::CodeType;

    /// Define a unit struct for a `Code` element that
    /// can be used as a generic parameter.
    macro_rules! code_types {
    ($($code:ident => $typ:ident),+) => {
        $(
          #[derive(PartialEq, Eq, Clone, Debug)]
          pub struct $typ;

          impl CodeType for $typ {
              fn code() -> cid::multihash::Code {
                  cid::multihash::Code::$code
              }
          }
        )*
    };
  }

    // XXX: For some reason none of the other code types work,
    // not even on their own as a variable:
    // let c = multihash::Code::Keccak256;
    // ERROR: no variant or associated item named `Keccak256` found for enum `Code`
    //        in the current scope variant or associated item not found in `Code`
    code_types! {
      Blake2b256 => Blake2b256
    }
}

/// Static typing information for `Cid` fields to help
/// read and write data safely.
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct TCid<T, C = codes::Blake2b256> {
    cid: Cid,
    _phantom_t: PhantomData<T>,
    _phantom_c: PhantomData<C>,
}

/// Static typing information for HAMT fields, a.k.a. `Map`.
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct THamt<K, V, const W: u32 = HAMT_BIT_WIDTH> {
    _phantom_k: PhantomData<K>,
    _phantom_v: PhantomData<V>,
}

/// Static typing information for AMT fields, a.k.a. `Array`.
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct TAmt<V, const W: u32 = AMT_BIT_WIDTH> {
    _phantom_v: PhantomData<V>,
}

impl<T, C> TCid<T, C> {
    /// Read the underlying `Cid`.
    pub fn cid(&self) -> Cid {
        self.cid
    }
}

/// `TCid` serializes exactly as its underling `Cid`.
impl<T, C> serde::Serialize for TCid<T, C> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.cid.serialize(serializer)
    }
}

/// `TCid` deserializes exactly as its underlying `Cid`.
impl<'d, T, C> serde::Deserialize<'d> for TCid<T, C> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'d>,
    {
        let cid = Cid::deserialize(deserializer)?;
        Ok(TCid { cid, _phantom_t: PhantomData, _phantom_c: PhantomData })
    }
}

/// Operations on primitive types that can directly be read/written from/to CBOR.
impl<T: Cbor, C: CodeType> TCid<T, C> {
    /// Initialize a `TCid` by storing a value as CBOR in the store and capturing the `Cid`.
    pub fn new_cbor<S: Blockstore>(store: &S, value: &T) -> Result<Self> {
        let cid = store.put_cbor(value, C::code())?;
        Ok(TCid { cid, _phantom_t: PhantomData, _phantom_c: PhantomData })
    }

    /// Read the underlying `Cid` from the store.
    pub fn get<S: Blockstore>(&self, store: &S) -> Result<Option<T>> {
        store.get_cbor(&self.cid)
    }

    /// Read the underlying `Cid` from the store or return an error if not found.
    pub fn get_or_err<S: Blockstore>(&self, store: &S) -> Result<T> {
        self.get(store).and_then(|x| match x {
            Some(x) => Ok(x),
            None => Err(anyhow!(
                "error loading {}: Cid ({}) did not match any in database",
                type_name::<Self>(),
                self.cid.to_string()
            )),
        })
    }

    /// Put the value into the store and overwrite the `Cid`.
    pub fn put<S: Blockstore>(&mut self, store: &S, value: &T) -> Result<()> {
        let cid = store.put_cbor(value, C::code())?;
        self.cid = cid;
        Ok(())
    }
}

/// Operations for HAMT types that, once read, hold a reference to the underlying storage.
impl<K, V: Cbor, const W: u32> TCid<THamt<K, V, W>, codes::Blake2b256> {
    /// Initialize an empty HAMT, flush it to the store and capture the `Cid`.
    pub fn new_hamt<S: Blockstore>(store: &S) -> Result<Self> {
        let cid = make_empty_map::<_, V>(store, W)
            .flush()
            .map_err(|e| anyhow!("Failed to create empty map: {}", e))?;

        Ok(TCid { cid, _phantom_t: PhantomData, _phantom_c: PhantomData })
    }

    /// Load a HAMT pointing at the store with the underlying `Cid` as its root.
    pub fn load<'s, S: Blockstore>(&self, store: &'s S) -> Result<Hamt<&'s S, V>> {
        make_map_with_root_and_bitwidth::<S, V>(&self.cid, store, W)
            .map_err(|e| anyhow!("error loading {}: {}", type_name::<Self>(), e))
    }

    /// Flush the HAMT to the store and overwrite the `Cid`.
    pub fn flush<'s, S: Blockstore>(
        &mut self,
        mut value: Hamt<&'s S, V>,
    ) -> Result<Hamt<&'s S, V>> {
        let cid =
            value.flush().map_err(|e| anyhow!("error flushing {}: {}", type_name::<Self>(), e))?;
        self.cid = cid;
        Ok(value)
    }

    /// Load, modify and flush a value, returning something as a result.
    pub fn modify<'s, S: Blockstore, R>(
        &mut self,
        store: &'s S,
        f: impl FnOnce(&mut Hamt<&'s S, V>) -> Result<R>,
    ) -> Result<R> {
        let mut value = self.load(store)?;
        let result = f(&mut value)?;
        self.flush(value)?;
        Ok(result)
    }

    /// Load, modify and flush a value.
    pub fn update<'s, S: Blockstore, R>(
        &mut self,
        store: &'s S,
        f: impl FnOnce(&mut Hamt<&'s S, V>) -> Result<R>,
    ) -> Result<()> {
        self.modify(store, |x| f(x)).map(|_| ())
    }
}

/// Operations for AMT types that, once read, hold a reference to the underlying storage.
impl<V: Cbor, const W: u32> TCid<TAmt<V, W>, codes::Blake2b256> {
    /// Initialize an empty AMT, flush it to the store and capture the `Cid`.
    pub fn new_amt<S: Blockstore>(store: &S) -> Result<Self> {
        let cid = Amt::<V, _>::new_with_bit_width(store, W)
            .flush()
            .map_err(|e| anyhow!("Failed to create empty array: {}", e))?;

        Ok(TCid { cid, _phantom_t: PhantomData, _phantom_c: PhantomData })
    }

    /// Load an AMT pointing at the store with the underlying `Cid` as its root.
    pub fn load<'s, S: Blockstore>(&self, store: &'s S) -> Result<Amt<V, &'s S>> {
        Amt::<V, _>::load(&self.cid, store)
            .map_err(|e| anyhow!("error loading {}: {}", type_name::<Self>(), e))
    }

    /// Flush the AMT to the store and overwrite the `Cid`.
    pub fn flush<'s, S: Blockstore>(&mut self, mut value: Amt<V, &'s S>) -> Result<Amt<V, &'s S>> {
        let cid =
            value.flush().map_err(|e| anyhow!("error flushing {}: {}", type_name::<Self>(), e))?;
        self.cid = cid;
        Ok(value)
    }

    /// Load, modify and flush a value, returning something as a result.
    pub fn modify<'s, S: Blockstore, R>(
        &mut self,
        store: &'s S,
        f: impl FnOnce(&mut Amt<V, &'s S>) -> Result<R>,
    ) -> Result<R> {
        let mut value = self.load(store)?;
        let result = f(&mut value)?;
        self.flush(value)?;
        Ok(result)
    }

    /// Load, modify and flush a value.
    pub fn update<'s, S: Blockstore, R>(
        &mut self,
        store: &'s S,
        f: impl FnOnce(&mut Amt<V, &'s S>) -> Result<R>,
    ) -> Result<()> {
        self.modify(store, |x| f(x)).map(|_| ())
    }
}

/// These `Default` implementations are unsound, in that while they
/// create `TCid` instances with correct `Cid` values, these values
/// are not stored anywhere, so there is no guarantee that any retrieval
/// attempt from a random store won't fail.
///
/// Their main purpose is to allow the `#[derive(Default)]` to be
/// applied on types that use `TCid` fields, if that's unavoidable.
mod defaults {
    use super::*;
    use fvm_ipld_blockstore::MemoryBlockstore;

    // This is different than just `Cid::default()`.
    // It's also different from what the default for HAMT or AMT is.
    impl<T: Cbor + Default, C: CodeType> Default for TCid<T, C> {
        fn default() -> Self {
            Self::new_cbor(&MemoryBlockstore::new(), &T::default()).unwrap()
        }
    }

    impl<K, V: Cbor, const W: u32> Default for TCid<THamt<K, V, W>, codes::Blake2b256> {
        fn default() -> Self {
            Self::new_hamt(&MemoryBlockstore::new()).unwrap()
        }
    }

    impl<V: Cbor, const W: u32> Default for TCid<TAmt<V, W>, codes::Blake2b256> {
        fn default() -> Self {
            Self::new_amt(&MemoryBlockstore::new()).unwrap()
        }
    }
}

#[allow(dead_code)]
#[cfg(test)]
mod test {
    use super::{TCid, THamt};
    use crate::Checkpoint;
    use anyhow::Result;
    use fil_actors_runtime::ActorDowncast;
    use fvm_ipld_blockstore::Blockstore;
    use fvm_ipld_encoding::{tuple::*, Cbor};
    use fvm_ipld_hamt::BytesKey;
    use fvm_shared::clock::ChainEpoch;

    #[derive(Serialize_tuple, Deserialize_tuple)]
    struct State {
        pub child_state: Option<TCid<State>>,
        pub checkpoints: TCid<THamt<ChainEpoch, Checkpoint>>,
    }

    impl Cbor for State {}

    impl State {
        pub fn new<S: Blockstore>(store: &S) -> Result<Self> {
            Ok(Self { child_state: None, checkpoints: TCid::new_hamt(store)? })
        }

        /// flush a checkpoint
        pub(crate) fn flush_checkpoint<BS: Blockstore>(
            &mut self,
            store: &BS,
            ch: &Checkpoint,
        ) -> anyhow::Result<()> {
            let mut checkpoints = self.checkpoints.load(store)?;

            let epoch = ch.epoch();
            checkpoints.set(BytesKey::from(epoch.to_ne_bytes().to_vec()), ch.clone()).map_err(
                |e| e.downcast_wrap(format!("failed to set checkpoint for epoch {}", epoch)),
            )?;

            self.checkpoints.flush(checkpoints)?;

            Ok(())
        }
    }

    // TODO: Test that a record defined with `Cid` fields has an identical CID as one that uses `TCid`.
}
