// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    loaded_data::runtime_types::Type,
    views::{ValueView, ValueVisitor},
};
use move_binary_format::{
    errors::*,
    file_format::{Constant, SignatureToken, VariantTag},
};
use move_core_types::{VARIANT_TAG_MAX_VALUE, annotated_value as A};
use move_core_types::{
    account_address::AccountAddress,
    effects::Op,
    gas_algebra::AbstractMemorySize,
    runtime_value::{MoveEnumLayout, MoveStructLayout, MoveTypeLayout},
    u256,
    vm_status::{StatusCode, sub_status::NFE_VECTOR_ERROR_BASE},
};
use std::{
    cell::RefCell,
    fmt::{self, Debug, Display},
    iter,
    ops::Add,
    rc::Rc,
};

/***************************************************************************************
 *
 * Internal Types
 *
 *   Internal representation of the Move value calculus. These types are abstractions
 *   over the concrete Move concepts and may carry additional information that is not
 *   defined by the language, but required by the implementation.
 *
 **************************************************************************************/

/// Runtime representation of a Move value.
#[derive(Debug)]
enum ValueImpl {
    Invalid,

    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    U128(Box<u128>),
    U256(Box<u256::U256>),
    Bool(bool),
    Address(Box<AccountAddress>),

    Container(Container),

    ContainerRef(Box<ContainerRef>),
    IndexedRef(Box<IndexedRef>),
}

/// A container is a collection of values. It is used to represent data structures like a
/// Move vector or struct.
///
/// There is one general container that can be used to store an array of any values, same
/// type or not, and a few specialized flavors to offer compact memory layout for small
/// primitive types.
///
/// Except when not owned by the VM stack, a container always lives inside an Rc<RefCell<>>,
/// making it possible to be shared by references.
#[derive(Debug, Clone)]
enum Container {
    Locals(Rc<RefCell<Vec<ValueImpl>>>),
    Vec(Rc<RefCell<Vec<ValueImpl>>>),
    Struct(Rc<RefCell<Vec<ValueImpl>>>),
    VecU8(Rc<RefCell<Vec<u8>>>),
    VecU64(Rc<RefCell<Vec<u64>>>),
    VecU128(Rc<RefCell<Vec<u128>>>),
    VecBool(Rc<RefCell<Vec<bool>>>),
    VecAddress(Rc<RefCell<Vec<AccountAddress>>>),
    VecU16(Rc<RefCell<Vec<u16>>>),
    VecU32(Rc<RefCell<Vec<u32>>>),
    VecU256(Rc<RefCell<Vec<u256::U256>>>),
    Variant(Rc<RefCell<(VariantTag, Vec<ValueImpl>)>>),
}

/// A ContainerRef is a direct reference to a container, which could live either in the frame
/// or in global storage. In the latter case, it also keeps a status flag indicating whether
/// the container has been possibly modified.
#[derive(Debug)]
enum ContainerRef {
    Local(Container),
    Global {
        status: Rc<RefCell<GlobalDataStatus>>,
        container: Container,
    },
}

/// Status for global (on-chain) data:
/// Clean - the data was only read.
/// Dirty - the data was possibly modified.
#[derive(Debug, Clone, Copy)]
enum GlobalDataStatus {
    Clean,
    Dirty,
}

/// A Move reference pointing to an element in a container.
#[derive(Debug)]
struct IndexedRef {
    idx: usize,
    container_ref: ContainerRef,
}

/// An umbrella enum for references. It is used to hide the internals of the public type
/// Reference.
#[derive(Debug)]
enum ReferenceImpl {
    IndexedRef(IndexedRef),
    ContainerRef(ContainerRef),
}

/***************************************************************************************
 *
 * Public Types
 *
 *   Types visible from outside the module. They are almost exclusively wrappers around
 *   the internal representation, acting as public interfaces. The methods they provide
 *   closely resemble the Move concepts their names suggest: move_local, borrow_field,
 *   pack, unpack, etc.
 *
 *   They are opaque to an external caller by design -- no knowledge about the internal
 *   representation is given and they can only be manipulated via the public methods,
 *   which is to ensure no arbitrary invalid states can be created unless some crucial
 *   internal invariants are violated.
 *
 **************************************************************************************/
/// A Move value -- a wrapper around `ValueImpl` which can be created only through valid
/// means.
#[derive(Debug)]
pub struct Value(ValueImpl);

/// An integer value in Move.
#[derive(Debug)]
pub enum IntegerValue {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    U128(u128),
    U256(u256::U256),
}

/// A Move struct.
#[derive(Debug)]
pub struct Struct {
    fields: Vec<ValueImpl>,
}

// A vector. This is an alias for a Container for now but we may change
// it once Containers are restructured.
// It's used from vector native functions to get a vector and operate on that.
// There is an impl for Vector which implements the API private to this module.
#[derive(Debug)]
pub struct Vector(Container);

/// A reference to a Move struct that allows you to take a reference to one of its fields.
#[derive(Debug)]
pub struct StructRef(ContainerRef);

/// A generic Move reference that offers two functionalities: read_ref & write_ref.
#[derive(Debug)]
pub struct Reference(ReferenceImpl);

// A reference to a signer. Clients can attempt a cast to this struct if they are
// expecting a Signer on the stack or as an argument.
#[derive(Debug)]
pub struct SignerRef(ContainerRef);

// A reference to a vector. This is an alias for a ContainerRef for now but we may change
// it once Containers are restructured.
// It's used from vector native functions to get a reference to a vector and operate on that.
// There is an impl for VectorRef which implements the API private to this module.
#[derive(Debug)]
pub struct VectorRef(ContainerRef);

/// A special "slot" in global storage that can hold a resource. It also keeps track of the status
/// of the resource relative to the global state, which is necessary to compute the effects to emit
/// at the end of transaction execution.
#[derive(Debug)]
enum GlobalValueImpl {
    /// No resource resides in this slot or in storage.
    None,
    /// A resource has been published to this slot and it did not previously exist in storage.
    Fresh { fields: Rc<RefCell<Vec<ValueImpl>>> },
    /// A resource resides in this slot and also in storage. The status flag indicates whether
    /// it has potentially been altered.
    Cached {
        fields: Rc<RefCell<Vec<ValueImpl>>>,
        status: Rc<RefCell<GlobalDataStatus>>,
    },
    /// A resource used to exist in storage but has been deleted by the current transaction.
    Deleted,
}

/// A wrapper around `GlobalValueImpl`, representing a "slot" in global storage that can
/// hold a resource.
#[derive(Debug)]
pub struct GlobalValue(GlobalValueImpl);

/// The locals for a function frame. It allows values to be read, written or taken
/// reference from.
#[derive(Debug)]
pub struct Locals(Rc<RefCell<Vec<ValueImpl>>>);

/// A Move enum value (aka a variant).
#[derive(Debug)]
pub struct Variant {
    tag: VariantTag,
    fields: Vec<ValueImpl>,
}

#[derive(Debug)]
pub struct VariantRef(ContainerRef);

/***************************************************************************************
 *
 * Misc
 *
 *   Miscellaneous helper functions.
 *
 **************************************************************************************/

impl Container {
    fn len(&self) -> usize {
        match self {
            Self::Locals(r) | Self::Struct(r) | Self::Vec(r) => r.borrow().len(),
            Self::VecU8(r) => r.borrow().len(),
            Self::VecU16(r) => r.borrow().len(),
            Self::VecU32(r) => r.borrow().len(),
            Self::VecU64(r) => r.borrow().len(),
            Self::VecU128(r) => r.borrow().len(),
            Self::VecU256(r) => r.borrow().len(),
            Self::VecBool(r) => r.borrow().len(),
            Self::VecAddress(r) => r.borrow().len(),
            Self::Variant(r) => r.borrow().1.len(),
        }
    }

    fn rc_count(&self) -> usize {
        match self {
            Self::Locals(r) | Self::Struct(r) | Self::Vec(r) => Rc::strong_count(r),
            Self::VecU8(r) => Rc::strong_count(r),
            Self::VecU16(r) => Rc::strong_count(r),
            Self::VecU32(r) => Rc::strong_count(r),
            Self::VecU64(r) => Rc::strong_count(r),
            Self::VecU128(r) => Rc::strong_count(r),
            Self::VecU256(r) => Rc::strong_count(r),
            Self::VecBool(r) => Rc::strong_count(r),
            Self::VecAddress(r) => Rc::strong_count(r),
            Self::Variant(r) => Rc::strong_count(r),
        }
    }

    fn signer(x: AccountAddress) -> Self {
        Container::Struct(Rc::new(RefCell::new(vec![ValueImpl::Address(Box::new(x))])))
    }
}

/***************************************************************************************
 *
 * Borrows (Internal)
 *
 *   Helper functions to handle Rust borrows. When borrowing from a RefCell, we want
 *   to return an error instead of panicking.
 *
 **************************************************************************************/

fn take_unique_ownership<T: Debug>(r: Rc<RefCell<T>>) -> PartialVMResult<T> {
    match Rc::try_unwrap(r) {
        Ok(cell) => Ok(cell.into_inner()),
        Err(r) => Err(
            PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                .with_message(format!("moving value {:?} with dangling references", r)),
        ),
    }
}

impl ContainerRef {
    fn container(&self) -> &Container {
        match self {
            Self::Local(container) | Self::Global { container, .. } => container,
        }
    }

    fn mark_dirty(&self) {
        if let Self::Global { status, .. } = self {
            *status.borrow_mut() = GlobalDataStatus::Dirty
        }
    }
}

/***************************************************************************************
 *
 * Reference Conversions (Internal)
 *
 *   Helpers to obtain a Rust reference to a value via a VM reference. Required for
 *   equalities.
 *
 **************************************************************************************/
trait VMValueRef<T> {
    fn value_ref(&self) -> PartialVMResult<&T>;
}

macro_rules! impl_vm_value_ref {
    ($ty: ty, $tc: ident) => {
        impl VMValueRef<$ty> for ValueImpl {
            fn value_ref(&self) -> PartialVMResult<&$ty> {
                match self {
                    ValueImpl::$tc(x) => Ok(x),
                    _ => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                        .with_message(format!("cannot take {:?} as &{}", self, stringify!($ty)))),
                }
            }
        }
    };
}

impl_vm_value_ref!(u8, U8);
impl_vm_value_ref!(u16, U16);
impl_vm_value_ref!(u32, U32);
impl_vm_value_ref!(u64, U64);
impl_vm_value_ref!(u128, U128);
impl_vm_value_ref!(u256::U256, U256);
impl_vm_value_ref!(bool, Bool);
impl_vm_value_ref!(AccountAddress, Address);

impl ValueImpl {
    fn as_value_ref<T>(&self) -> PartialVMResult<&T>
    where
        Self: VMValueRef<T>,
    {
        VMValueRef::value_ref(self)
    }
}

/***************************************************************************************
 *
 * Copy Value
 *
 *   Implementation of Move copy. Extra care needs to be taken when copying references.
 *   It is intentional we avoid implementing the standard library trait Clone, to prevent
 *   surprising behaviors from happening.
 *
 **************************************************************************************/
impl ValueImpl {
    fn copy_value(&self) -> PartialVMResult<Self> {
        use ValueImpl::*;

        Ok(match self {
            Invalid => Invalid,

            U8(x) => U8(*x),
            U16(x) => U16(*x),
            U32(x) => U32(*x),
            U64(x) => U64(*x),
            U128(x) => U128(x.clone()),
            U256(x) => U256(x.clone()),
            Bool(x) => Bool(*x),
            Address(x) => Address(x.clone()),

            ContainerRef(r) => ContainerRef(Box::new(r.copy_value())),
            IndexedRef(r) => IndexedRef(Box::new(r.copy_value())),

            // When cloning a container, we need to make sure we make a deep
            // copy of the data instead of a shallow copy of the Rc.
            Container(c) => Container(c.copy_value()?),
        })
    }
}

impl Container {
    fn copy_value(&self) -> PartialVMResult<Self> {
        let copy_rc_ref_vec_val = |r: &Rc<RefCell<Vec<ValueImpl>>>| {
            Ok(Rc::new(RefCell::new(
                r.borrow()
                    .iter()
                    .map(|v| v.copy_value())
                    .collect::<PartialVMResult<_>>()?,
            )))
        };

        Ok(match self {
            Self::Vec(r) => Self::Vec(copy_rc_ref_vec_val(r)?),
            Self::Struct(r) => Self::Struct(copy_rc_ref_vec_val(r)?),
            Self::Variant(r) => {
                let (tag, values) = &*r.borrow();
                let values = values
                    .iter()
                    .map(|v| v.copy_value())
                    .collect::<PartialVMResult<_>>()?;
                Self::Variant(Rc::new(RefCell::new((*tag, values))))
            }

            Self::VecU8(r) => Self::VecU8(Rc::new(RefCell::new(r.borrow().clone()))),
            Self::VecU16(r) => Self::VecU16(Rc::new(RefCell::new(r.borrow().clone()))),
            Self::VecU32(r) => Self::VecU32(Rc::new(RefCell::new(r.borrow().clone()))),
            Self::VecU64(r) => Self::VecU64(Rc::new(RefCell::new(r.borrow().clone()))),
            Self::VecU128(r) => Self::VecU128(Rc::new(RefCell::new(r.borrow().clone()))),
            Self::VecU256(r) => Self::VecU256(Rc::new(RefCell::new(r.borrow().clone()))),
            Self::VecBool(r) => Self::VecBool(Rc::new(RefCell::new(r.borrow().clone()))),
            Self::VecAddress(r) => Self::VecAddress(Rc::new(RefCell::new(r.borrow().clone()))),

            Self::Locals(_) => {
                return Err(
                    PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                        .with_message("cannot copy a Locals container".to_string()),
                );
            }
        })
    }

    fn copy_by_ref(&self) -> Self {
        match self {
            Self::Vec(r) => Self::Vec(Rc::clone(r)),
            Self::Struct(r) => Self::Struct(Rc::clone(r)),
            Self::VecU8(r) => Self::VecU8(Rc::clone(r)),
            Self::VecU16(r) => Self::VecU16(Rc::clone(r)),
            Self::VecU32(r) => Self::VecU32(Rc::clone(r)),
            Self::VecU64(r) => Self::VecU64(Rc::clone(r)),
            Self::VecU128(r) => Self::VecU128(Rc::clone(r)),
            Self::VecU256(r) => Self::VecU256(Rc::clone(r)),
            Self::VecBool(r) => Self::VecBool(Rc::clone(r)),
            Self::VecAddress(r) => Self::VecAddress(Rc::clone(r)),
            Self::Locals(r) => Self::Locals(Rc::clone(r)),
            Self::Variant(r) => Self::Variant(Rc::clone(r)),
        }
    }
}

impl IndexedRef {
    fn copy_value(&self) -> Self {
        Self {
            idx: self.idx,
            container_ref: self.container_ref.copy_value(),
        }
    }
}

impl ContainerRef {
    fn copy_value(&self) -> Self {
        match self {
            Self::Local(container) => Self::Local(container.copy_by_ref()),
            Self::Global { status, container } => Self::Global {
                status: Rc::clone(status),
                container: container.copy_by_ref(),
            },
        }
    }
}

impl Value {
    pub fn copy_value(&self) -> PartialVMResult<Self> {
        Ok(Self(self.0.copy_value()?))
    }
}

/***************************************************************************************
 *
 * Equality
 *
 *   Equality tests of Move values. Errors are raised when types mismatch.
 *
 *   It is intended to NOT use or even implement the standard library traits Eq and
 *   Partial Eq due to:
 *     1. They do not allow errors to be returned.
 *     2. They can be invoked without the user being noticed thanks to operator
 *        overloading.
 *
 *   Eq and Partial Eq must also NOT be derived for the reasons above plus that the
 *   derived implementation differs from the semantics we want.
 *
 **************************************************************************************/

impl ValueImpl {
    fn equals(&self, other: &Self) -> PartialVMResult<bool> {
        use ValueImpl::*;

        let res = match (self, other) {
            (U8(l), U8(r)) => l == r,
            (U16(l), U16(r)) => l == r,
            (U32(l), U32(r)) => l == r,
            (U64(l), U64(r)) => l == r,
            (U128(l), U128(r)) => l == r,
            (U256(l), U256(r)) => l == r,
            (Bool(l), Bool(r)) => l == r,
            (Address(l), Address(r)) => l == r,

            (Container(l), Container(r)) => l.equals(r)?,

            (ContainerRef(l), ContainerRef(r)) => l.equals(r)?,
            (IndexedRef(l), IndexedRef(r)) => l.equals(r)?,
            (Invalid, _)
            | (U8(_), _)
            | (U16(_), _)
            | (U32(_), _)
            | (U64(_), _)
            | (U128(_), _)
            | (U256(_), _)
            | (Bool(_), _)
            | (Address(_), _)
            | (Container(_), _)
            | (ContainerRef(_), _)
            | (IndexedRef(_), _) => {
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                    .with_message(format!("cannot compare values: {:?}, {:?}", self, other)));
            }
        };

        Ok(res)
    }
}

impl Container {
    fn equals(&self, other: &Self) -> PartialVMResult<bool> {
        use Container::*;

        let res = match (self, other) {
            (Vec(l), Vec(r)) | (Struct(l), Struct(r)) => {
                let l = &l.borrow();
                let r = &r.borrow();

                if l.len() != r.len() {
                    return Ok(false);
                }
                for (v1, v2) in l.iter().zip(r.iter()) {
                    if !v1.equals(v2)? {
                        return Ok(false);
                    }
                }
                true
            }
            (VecU8(l), VecU8(r)) => l.borrow().eq(&*r.borrow()),
            (VecU16(l), VecU16(r)) => l.borrow().eq(&*r.borrow()),
            (VecU32(l), VecU32(r)) => l.borrow().eq(&*r.borrow()),
            (VecU64(l), VecU64(r)) => l.borrow().eq(&*r.borrow()),
            (VecU128(l), VecU128(r)) => l.borrow().eq(&*r.borrow()),
            (VecU256(l), VecU256(r)) => l.borrow().eq(&*r.borrow()),
            (VecBool(l), VecBool(r)) => l.borrow().eq(&*r.borrow()),
            (VecAddress(l), VecAddress(r)) => l.borrow().eq(&*r.borrow()),
            (Variant(l), Variant(r)) => {
                let (tagl, valuesl) = &*l.borrow();
                let (tagr, valuesr) = &*r.borrow();
                if tagl != tagr {
                    return Ok(false);
                }

                if valuesl.len() != valuesr.len() {
                    return Ok(false);
                }
                for (v1, v2) in valuesl.iter().zip(valuesr) {
                    if !v1.equals(v2)? {
                        return Ok(false);
                    }
                }
                true
            }

            (Locals(_), _)
            | (Vec(_), _)
            | (Struct(_), _)
            | (VecU8(_), _)
            | (VecU16(_), _)
            | (VecU32(_), _)
            | (VecU64(_), _)
            | (VecU128(_), _)
            | (VecU256(_), _)
            | (VecBool(_), _)
            | (VecAddress(_), _)
            | (Variant { .. }, _) => {
                return Err(
                    PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(format!(
                        "cannot compare container values: {:?}, {:?}",
                        self, other
                    )),
                );
            }
        };

        Ok(res)
    }
}

impl ContainerRef {
    fn equals(&self, other: &Self) -> PartialVMResult<bool> {
        self.container().equals(other.container())
    }
}

impl IndexedRef {
    fn equals(&self, other: &Self) -> PartialVMResult<bool> {
        use Container::*;

        let res = match (
            self.container_ref.container(),
            other.container_ref.container(),
        ) {
            // VecC <=> VecR impossible
            (Vec(r1), Vec(r2))
            | (Vec(r1), Struct(r2))
            | (Vec(r1), Locals(r2))
            | (Struct(r1), Vec(r2))
            | (Struct(r1), Struct(r2))
            | (Struct(r1), Locals(r2))
            | (Locals(r1), Vec(r2))
            | (Locals(r1), Struct(r2))
            | (Locals(r1), Locals(r2)) => r1.borrow()[self.idx].equals(&r2.borrow()[other.idx])?,
            (Struct(r1), Variant(r2)) | (Locals(r1), Variant(r2)) => {
                r1.borrow()[self.idx].equals(&r2.borrow().1[other.idx])?
            }
            (Variant(r1), Vec(r2)) | (Variant(r1), Struct(r2)) | (Variant(r1), Locals(r2)) => {
                r1.borrow().1[self.idx].equals(&r2.borrow()[other.idx])?
            }
            (Variant(r1), Variant(r2)) => {
                r1.borrow().1[self.idx].equals(&r2.borrow().1[other.idx])?
            }

            (VecU8(r1), VecU8(r2)) => r1.borrow()[self.idx] == r2.borrow()[other.idx],
            (VecU16(r1), VecU16(r2)) => r1.borrow()[self.idx] == r2.borrow()[other.idx],
            (VecU32(r1), VecU32(r2)) => r1.borrow()[self.idx] == r2.borrow()[other.idx],
            (VecU64(r1), VecU64(r2)) => r1.borrow()[self.idx] == r2.borrow()[other.idx],
            (VecU128(r1), VecU128(r2)) => r1.borrow()[self.idx] == r2.borrow()[other.idx],
            (VecU256(r1), VecU256(r2)) => r1.borrow()[self.idx] == r2.borrow()[other.idx],
            (VecBool(r1), VecBool(r2)) => r1.borrow()[self.idx] == r2.borrow()[other.idx],
            (VecAddress(r1), VecAddress(r2)) => r1.borrow()[self.idx] == r2.borrow()[other.idx],

            // Equality between a generic and a specialized container.
            (Locals(r1), VecU8(r2)) | (Struct(r1), VecU8(r2)) => {
                *r1.borrow()[self.idx].as_value_ref::<u8>()? == r2.borrow()[other.idx]
            }
            (VecU8(r1), Locals(r2)) | (VecU8(r1), Struct(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow()[other.idx].as_value_ref::<u8>()?
            }
            (Variant(r1), VecU8(r2)) => {
                *r1.borrow().1[self.idx].as_value_ref::<u8>()? == r2.borrow()[other.idx]
            }
            (VecU8(r1), Variant(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow().1[other.idx].as_value_ref::<u8>()?
            }

            (Locals(r1), VecU16(r2)) | (Struct(r1), VecU16(r2)) => {
                *r1.borrow()[self.idx].as_value_ref::<u16>()? == r2.borrow()[other.idx]
            }
            (VecU16(r1), Locals(r2)) | (VecU16(r1), Struct(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow()[other.idx].as_value_ref::<u16>()?
            }
            (Variant(r1), VecU16(r2)) => {
                *r1.borrow().1[self.idx].as_value_ref::<u16>()? == r2.borrow()[other.idx]
            }
            (VecU16(r1), Variant(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow().1[other.idx].as_value_ref::<u16>()?
            }

            (Locals(r1), VecU32(r2)) | (Struct(r1), VecU32(r2)) => {
                *r1.borrow()[self.idx].as_value_ref::<u32>()? == r2.borrow()[other.idx]
            }
            (VecU32(r1), Locals(r2)) | (VecU32(r1), Struct(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow()[other.idx].as_value_ref::<u32>()?
            }
            (Variant(r1), VecU32(r2)) => {
                *r1.borrow().1[self.idx].as_value_ref::<u32>()? == r2.borrow()[other.idx]
            }
            (VecU32(r1), Variant(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow().1[other.idx].as_value_ref::<u32>()?
            }

            (Locals(r1), VecU64(r2)) | (Struct(r1), VecU64(r2)) => {
                *r1.borrow()[self.idx].as_value_ref::<u64>()? == r2.borrow()[other.idx]
            }
            (VecU64(r1), Locals(r2)) | (VecU64(r1), Struct(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow()[other.idx].as_value_ref::<u64>()?
            }
            (Variant(r1), VecU64(r2)) => {
                *r1.borrow().1[self.idx].as_value_ref::<u64>()? == r2.borrow()[other.idx]
            }
            (VecU64(r1), Variant(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow().1[other.idx].as_value_ref::<u64>()?
            }

            (Locals(r1), VecU128(r2)) | (Struct(r1), VecU128(r2)) => {
                *r1.borrow()[self.idx].as_value_ref::<u128>()? == r2.borrow()[other.idx]
            }
            (VecU128(r1), Locals(r2)) | (VecU128(r1), Struct(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow()[other.idx].as_value_ref::<u128>()?
            }
            (Variant(r1), VecU128(r2)) => {
                *r1.borrow().1[self.idx].as_value_ref::<u128>()? == r2.borrow()[other.idx]
            }
            (VecU128(r1), Variant(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow().1[other.idx].as_value_ref::<u128>()?
            }

            (Locals(r1), VecU256(r2)) | (Struct(r1), VecU256(r2)) => {
                *r1.borrow()[self.idx].as_value_ref::<u256::U256>()? == r2.borrow()[other.idx]
            }
            (VecU256(r1), Locals(r2)) | (VecU256(r1), Struct(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow()[other.idx].as_value_ref::<u256::U256>()?
            }
            (Variant(r1), VecU256(r2)) => {
                *r1.borrow().1[self.idx].as_value_ref::<u256::U256>()? == r2.borrow()[other.idx]
            }
            (VecU256(r1), Variant(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow().1[other.idx].as_value_ref::<u256::U256>()?
            }

            (Locals(r1), VecBool(r2)) | (Struct(r1), VecBool(r2)) => {
                *r1.borrow()[self.idx].as_value_ref::<bool>()? == r2.borrow()[other.idx]
            }
            (VecBool(r1), Locals(r2)) | (VecBool(r1), Struct(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow()[other.idx].as_value_ref::<bool>()?
            }
            (Variant(r1), VecBool(r2)) => {
                *r1.borrow().1[self.idx].as_value_ref::<bool>()? == r2.borrow()[other.idx]
            }
            (VecBool(r1), Variant(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow().1[other.idx].as_value_ref::<bool>()?
            }

            (Locals(r1), VecAddress(r2)) | (Struct(r1), VecAddress(r2)) => {
                *r1.borrow()[self.idx].as_value_ref::<AccountAddress>()? == r2.borrow()[other.idx]
            }
            (VecAddress(r1), Locals(r2)) | (VecAddress(r1), Struct(r2)) => {
                r1.borrow()[self.idx] == *r2.borrow()[other.idx].as_value_ref::<AccountAddress>()?
            }
            (Variant(r1), VecAddress(r2)) => {
                *r1.borrow().1[self.idx].as_value_ref::<AccountAddress>()? == r2.borrow()[other.idx]
            }
            (VecAddress(r1), Variant(r2)) => {
                r1.borrow()[self.idx]
                    == *r2.borrow().1[other.idx].as_value_ref::<AccountAddress>()?
            }

            // All other combinations are illegal.
            (Vec(_), _)
            | (VecU8(_), _)
            | (VecU16(_), _)
            | (VecU32(_), _)
            | (VecU64(_), _)
            | (VecU128(_), _)
            | (VecU256(_), _)
            | (VecBool(_), _)
            | (VecAddress(_), _) => {
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                    .with_message(format!("cannot compare references {:?}, {:?}", self, other)));
            }
        };
        Ok(res)
    }
}

impl Value {
    pub fn equals(&self, other: &Self) -> PartialVMResult<bool> {
        self.0.equals(&other.0)
    }
}

/***************************************************************************************
 *
 * Read Ref
 *
 *   Implementation of the Move operation read ref.
 *
 **************************************************************************************/

impl ContainerRef {
    fn read_ref(self) -> PartialVMResult<Value> {
        Ok(Value(ValueImpl::Container(self.container().copy_value()?)))
    }
}

impl IndexedRef {
    fn read_ref(self) -> PartialVMResult<Value> {
        use Container::*;

        let res = match self.container_ref.container() {
            Locals(r) | Vec(r) | Struct(r) => r.borrow()[self.idx].copy_value()?,
            Variant(r) => r.borrow().1[self.idx].copy_value()?,
            VecU8(r) => ValueImpl::U8(r.borrow()[self.idx]),
            VecU16(r) => ValueImpl::U16(r.borrow()[self.idx]),
            VecU32(r) => ValueImpl::U32(r.borrow()[self.idx]),
            VecU64(r) => ValueImpl::U64(r.borrow()[self.idx]),
            VecU128(r) => ValueImpl::U128(Box::new(r.borrow()[self.idx])),
            VecU256(r) => ValueImpl::U256(Box::new(r.borrow()[self.idx])),
            VecBool(r) => ValueImpl::Bool(r.borrow()[self.idx]),
            VecAddress(r) => ValueImpl::Address(Box::new(r.borrow()[self.idx])),
        };

        Ok(Value(res))
    }
}

impl ReferenceImpl {
    fn read_ref(self) -> PartialVMResult<Value> {
        match self {
            Self::ContainerRef(r) => r.read_ref(),
            Self::IndexedRef(r) => r.read_ref(),
        }
    }
}

impl StructRef {
    pub fn read_ref(self) -> PartialVMResult<Value> {
        self.0.read_ref()
    }
}

impl Reference {
    pub fn read_ref(self) -> PartialVMResult<Value> {
        self.0.read_ref()
    }
}

/***************************************************************************************
 *
 * Write Ref
 *
 *   Implementation of the Move operation write ref.
 *
 **************************************************************************************/

impl ContainerRef {
    fn write_ref(self, v: Value) -> PartialVMResult<()> {
        match v.0 {
            ValueImpl::Container(c) => {
                macro_rules! assign {
                    ($r1: expr, $tc: ident) => {{
                        let r = match c {
                            Container::$tc(v) => v,
                            _ => {
                                return Err(PartialVMError::new(
                                    StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                                )
                                .with_message(
                                    "failed to write_ref: container type mismatch".to_string(),
                                ));
                            }
                        };
                        *$r1.borrow_mut() = take_unique_ownership(r)?;
                    }};
                }

                match self.container() {
                    Container::Struct(r) => assign!(r, Struct),
                    Container::Variant(r) => match c {
                        Container::Variant(new) => {
                            *r.borrow_mut() = take_unique_ownership(new)?;
                        }
                        _ => {
                            return Err(PartialVMError::new(
                                StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                            )
                            .with_message(
                                "failed to write_ref: container type mismatch".to_string(),
                            ));
                        }
                    },
                    Container::Vec(r) => assign!(r, Vec),
                    Container::VecU8(r) => assign!(r, VecU8),
                    Container::VecU16(r) => assign!(r, VecU16),
                    Container::VecU32(r) => assign!(r, VecU32),
                    Container::VecU64(r) => assign!(r, VecU64),
                    Container::VecU128(r) => assign!(r, VecU128),
                    Container::VecU256(r) => assign!(r, VecU256),
                    Container::VecBool(r) => assign!(r, VecBool),
                    Container::VecAddress(r) => assign!(r, VecAddress),
                    Container::Locals(_) => {
                        return Err(PartialVMError::new(
                            StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                        )
                        .with_message("cannot overwrite Container::Locals".to_string()));
                    }
                }
                self.mark_dirty();
            }
            _ => {
                return Err(
                    PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                        .with_message(format!(
                            "cannot write value {:?} to container ref {:?}",
                            v, self
                        )),
                );
            }
        }
        Ok(())
    }
}

impl IndexedRef {
    fn write_ref(self, x: Value) -> PartialVMResult<()> {
        match &x.0 {
            ValueImpl::IndexedRef(_)
            | ValueImpl::ContainerRef(_)
            | ValueImpl::Invalid
            | ValueImpl::Container(_) => {
                return Err(
                    PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                        .with_message(format!(
                            "cannot write value {:?} to indexed ref {:?}",
                            x, self
                        )),
                );
            }
            _ => (),
        }

        match (self.container_ref.container(), &x.0) {
            (Container::Locals(r), _) | (Container::Vec(r), _) | (Container::Struct(r), _) => {
                let mut v = r.borrow_mut();
                v[self.idx] = x.0;
            }
            (Container::Variant(r), _) => {
                r.borrow_mut().1[self.idx] = x.0;
            }
            (Container::VecU8(r), ValueImpl::U8(x)) => r.borrow_mut()[self.idx] = *x,
            (Container::VecU16(r), ValueImpl::U16(x)) => r.borrow_mut()[self.idx] = *x,
            (Container::VecU32(r), ValueImpl::U32(x)) => r.borrow_mut()[self.idx] = *x,
            (Container::VecU64(r), ValueImpl::U64(x)) => r.borrow_mut()[self.idx] = *x,
            (Container::VecU128(r), ValueImpl::U128(x)) => r.borrow_mut()[self.idx] = **x,
            (Container::VecU256(r), ValueImpl::U256(x)) => r.borrow_mut()[self.idx] = **x,
            (Container::VecBool(r), ValueImpl::Bool(x)) => r.borrow_mut()[self.idx] = *x,
            (Container::VecAddress(r), ValueImpl::Address(x)) => r.borrow_mut()[self.idx] = **x,

            (Container::VecU8(_), _)
            | (Container::VecU16(_), _)
            | (Container::VecU32(_), _)
            | (Container::VecU64(_), _)
            | (Container::VecU128(_), _)
            | (Container::VecU256(_), _)
            | (Container::VecBool(_), _)
            | (Container::VecAddress(_), _) => {
                return Err(
                    PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(format!(
                        "cannot write value {:?} to indexed ref {:?}",
                        x, self
                    )),
                );
            }
        }
        self.container_ref.mark_dirty();
        Ok(())
    }
}

impl ReferenceImpl {
    fn write_ref(self, x: Value) -> PartialVMResult<()> {
        match self {
            Self::ContainerRef(r) => r.write_ref(x),
            Self::IndexedRef(r) => r.write_ref(x),
        }
    }
}

impl Reference {
    pub fn write_ref(self, x: Value) -> PartialVMResult<()> {
        self.0.write_ref(x)
    }
}

/***************************************************************************************
 *
 * Borrows (Move)
 *
 *   Implementation of borrowing in Move: borrow field, borrow local and infrastructure
 *   to support borrowing an element from a vector.
 *
 **************************************************************************************/

impl ContainerRef {
    fn borrow_elem(&self, idx: usize) -> PartialVMResult<ValueImpl> {
        let len = self.container().len();
        if idx >= len {
            return Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR).with_message(
                    format!(
                        "index out of bounds when borrowing container element: got: {}, len: {}",
                        idx, len
                    ),
                ),
            );
        }

        let res = match self.container() {
            c @ Container::Locals(_)
            | c @ Container::Vec(_)
            | c @ Container::Struct(_)
            | c @ Container::Variant(_) => {
                let rc_r;
                let rc_values;
                let v = match c {
                    Container::Locals(r) | Container::Vec(r) | Container::Struct(r) => {
                        rc_r = r.borrow();
                        &*rc_r
                    }
                    Container::Variant(r) => {
                        rc_values = r.borrow();
                        &*rc_values.1
                    }
                    _ => unreachable!(),
                };
                match &v[idx] {
                    // TODO: check for the impossible combinations.
                    ValueImpl::Container(container) => {
                        let r = match self {
                            Self::Local(_) => Self::Local(container.copy_by_ref()),
                            Self::Global { status, .. } => Self::Global {
                                status: Rc::clone(status),
                                container: container.copy_by_ref(),
                            },
                        };
                        ValueImpl::ContainerRef(Box::new(r))
                    }
                    _ => ValueImpl::IndexedRef(Box::new(IndexedRef {
                        idx,
                        container_ref: self.copy_value(),
                    })),
                }
            }

            Container::VecU8(_)
            | Container::VecU16(_)
            | Container::VecU32(_)
            | Container::VecU64(_)
            | Container::VecU128(_)
            | Container::VecU256(_)
            | Container::VecAddress(_)
            | Container::VecBool(_) => ValueImpl::IndexedRef(Box::new(IndexedRef {
                idx,
                container_ref: self.copy_value(),
            })),
        };

        Ok(res)
    }
}

impl StructRef {
    pub fn borrow_field(&self, idx: usize) -> PartialVMResult<Value> {
        Ok(Value(self.0.borrow_elem(idx)?))
    }
}

impl VariantRef {
    pub fn get_tag(&self) -> PartialVMResult<VariantTag> {
        match self.0.container() {
            Container::Variant(r) => Ok(r.borrow().0),
            Container::Locals(_)
            | Container::Vec(_)
            | Container::Struct(_)
            | Container::VecU8(_)
            | Container::VecU64(_)
            | Container::VecU128(_)
            | Container::VecBool(_)
            | Container::VecAddress(_)
            | Container::VecU16(_)
            | Container::VecU32(_)
            | Container::VecU256(_) => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!(
                    "expected variant container, got {:?}",
                    self.0.container()
                ))),
        }
    }
    pub fn check_tag(&self, expected_tag: VariantTag) -> PartialVMResult<()> {
        let tag = self.get_tag()?;
        if tag != expected_tag {
            Err(
                PartialVMError::new(StatusCode::VARIANT_TAG_MISMATCH).with_message(format!(
                    "Variant tag mismatch: expected {}, got {}",
                    expected_tag, tag
                )),
            )
        } else {
            Ok(())
        }
    }

    pub fn unpack_variant(&self) -> PartialVMResult<Vec<Value>> {
        match self.0.container() {
            Container::Variant(r) => {
                let values = &*r.borrow().1;
                let mut res = vec![];
                for (idx, v) in values.iter().enumerate() {
                    let ref_ = match v {
                        ValueImpl::Container(container) => {
                            let r = match &self.0 {
                                ContainerRef::Local(_) => {
                                    ContainerRef::Local(container.copy_by_ref())
                                }
                                ContainerRef::Global { status, .. } => ContainerRef::Global {
                                    status: Rc::clone(status),
                                    container: container.copy_by_ref(),
                                },
                            };
                            ValueImpl::ContainerRef(Box::new(r))
                        }
                        ValueImpl::U8(_)
                        | ValueImpl::U16(_)
                        | ValueImpl::U32(_)
                        | ValueImpl::U64(_)
                        | ValueImpl::U128(_)
                        | ValueImpl::U256(_)
                        | ValueImpl::Bool(_)
                        | ValueImpl::Address(_) => ValueImpl::IndexedRef(Box::new(IndexedRef {
                            idx,
                            container_ref: self.0.copy_value(),
                        })),
                        x @ (ValueImpl::ContainerRef(_)
                        | ValueImpl::IndexedRef(_)
                        | ValueImpl::Invalid) => return Err(PartialVMError::new(
                            StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                        )
                        .with_message(format!(
                            "cannot unpack a reference value {:?} held inside a variant ref {:?}",
                            x, self
                        ))),
                    };
                    res.push(Value(ref_));
                }
                Ok(res)
            }
            Container::Locals(_)
            | Container::Vec(_)
            | Container::Struct(_)
            | Container::VecU8(_)
            | Container::VecU64(_)
            | Container::VecU128(_)
            | Container::VecBool(_)
            | Container::VecAddress(_)
            | Container::VecU16(_)
            | Container::VecU32(_)
            | Container::VecU256(_) => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!(
                    "expected variant container, got {:?}",
                    self.0.container()
                ))),
        }
    }
}

impl Locals {
    pub fn borrow_loc(&self, idx: usize) -> PartialVMResult<Value> {
        // TODO: this is very similar to SharedContainer::borrow_elem. Find a way to
        // reuse that code?

        let v = self.0.borrow();
        if idx >= v.len() {
            return Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR).with_message(
                    format!(
                        "index out of bounds when borrowing local: got: {}, len: {}",
                        idx,
                        v.len()
                    ),
                ),
            );
        }

        match &v[idx] {
            ValueImpl::Container(c) => Ok(Value(ValueImpl::ContainerRef(Box::new(
                ContainerRef::Local(c.copy_by_ref()),
            )))),

            ValueImpl::U8(_)
            | ValueImpl::U16(_)
            | ValueImpl::U32(_)
            | ValueImpl::U64(_)
            | ValueImpl::U128(_)
            | ValueImpl::U256(_)
            | ValueImpl::Bool(_)
            | ValueImpl::Address(_) => Ok(Value(ValueImpl::IndexedRef(Box::new(IndexedRef {
                container_ref: ContainerRef::Local(Container::Locals(Rc::clone(&self.0))),
                idx,
            })))),

            ValueImpl::ContainerRef(_) | ValueImpl::Invalid | ValueImpl::IndexedRef(_) => Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                    .with_message(format!("cannot borrow local {:?}", &v[idx])),
            ),
        }
    }
}

impl SignerRef {
    pub fn borrow_signer(&self) -> PartialVMResult<Value> {
        Ok(Value(self.0.borrow_elem(0)?))
    }
}

/***************************************************************************************
 *
 * Locals
 *
 *   Public APIs for Locals to support reading, writing and moving of values.
 *
 **************************************************************************************/
impl Locals {
    pub fn new(n: usize) -> Self {
        Self(Rc::new(RefCell::new(
            iter::repeat_with(|| ValueImpl::Invalid).take(n).collect(),
        )))
    }

    pub fn copy_loc(&self, idx: usize) -> PartialVMResult<Value> {
        let v = self.0.borrow();
        match v.get(idx) {
            Some(ValueImpl::Invalid) => Err(PartialVMError::new(
                StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
            )
            .with_message(format!("cannot copy invalid value at index {}", idx))),
            Some(v) => Ok(Value(v.copy_value()?)),
            None => Err(
                PartialVMError::new(StatusCode::VERIFIER_INVARIANT_VIOLATION).with_message(
                    format!("local index out of bounds: got {}, len: {}", idx, v.len()),
                ),
            ),
        }
    }

    fn swap_loc(&mut self, idx: usize, x: Value, violation_check: bool) -> PartialVMResult<Value> {
        let mut v = self.0.borrow_mut();
        match v.get_mut(idx) {
            Some(v) => {
                if violation_check {
                    if let ValueImpl::Container(c) = v {
                        if c.rc_count() > 1 {
                            return Err(PartialVMError::new(
                                StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                            )
                            .with_message(
                                "moving container with dangling references".to_string(),
                            ));
                        }
                    }
                }
                Ok(Value(std::mem::replace(v, x.0)))
            }
            None => Err(
                PartialVMError::new(StatusCode::VERIFIER_INVARIANT_VIOLATION).with_message(
                    format!("local index out of bounds: got {}, len: {}", idx, v.len()),
                ),
            ),
        }
    }

    pub fn move_loc(&mut self, idx: usize, violation_check: bool) -> PartialVMResult<Value> {
        match self.swap_loc(idx, Value(ValueImpl::Invalid), violation_check)? {
            Value(ValueImpl::Invalid) => Err(PartialVMError::new(
                StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
            )
            .with_message(format!("cannot move invalid value at index {}", idx))),
            v => Ok(v),
        }
    }

    pub fn store_loc(
        &mut self,
        idx: usize,
        x: Value,
        violation_check: bool,
    ) -> PartialVMResult<()> {
        self.swap_loc(idx, x, violation_check)?;
        Ok(())
    }

    /// Drop all Move values onto a different Vec to avoid leaking memory.
    /// References are excluded since they may point to invalid data.
    pub fn drop_all_values(&mut self) -> impl Iterator<Item = (usize, Value)> {
        let mut locals = self.0.borrow_mut();
        let mut res = vec![];

        for idx in 0..locals.len() {
            match &locals[idx] {
                ValueImpl::Invalid => (),
                ValueImpl::ContainerRef(_) | ValueImpl::IndexedRef(_) => {
                    locals[idx] = ValueImpl::Invalid;
                }
                _ => res.push((
                    idx,
                    Value(std::mem::replace(&mut locals[idx], ValueImpl::Invalid)),
                )),
            }
        }

        res.into_iter()
    }

    pub fn is_invalid(&self, idx: usize) -> PartialVMResult<bool> {
        let v = self.0.borrow();
        match v.get(idx) {
            Some(ValueImpl::Invalid) => Ok(true),
            Some(_) => Ok(false),
            None => Err(
                PartialVMError::new(StatusCode::VERIFIER_INVARIANT_VIOLATION).with_message(
                    format!("local index out of bounds: got {}, len: {}", idx, v.len()),
                ),
            ),
        }
    }
}

/***************************************************************************************
 *
 * Public Value Constructors
 *
 *   Constructors to allow values to be created outside this module.
 *
 **************************************************************************************/
impl Value {
    pub fn u8(x: u8) -> Self {
        Self(ValueImpl::U8(x))
    }

    pub fn u16(x: u16) -> Self {
        Self(ValueImpl::U16(x))
    }

    pub fn u32(x: u32) -> Self {
        Self(ValueImpl::U32(x))
    }

    pub fn u64(x: u64) -> Self {
        Self(ValueImpl::U64(x))
    }

    pub fn u128(x: u128) -> Self {
        Self(ValueImpl::U128(Box::new(x)))
    }

    pub fn u256(x: u256::U256) -> Self {
        Self(ValueImpl::U256(Box::new(x)))
    }

    pub fn bool(x: bool) -> Self {
        Self(ValueImpl::Bool(x))
    }

    pub fn address(x: AccountAddress) -> Self {
        Self(ValueImpl::Address(Box::new(x)))
    }

    pub fn signer(x: AccountAddress) -> Self {
        Self(ValueImpl::Container(Container::signer(x)))
    }

    /// Create a "unowned" reference to a signer value (&signer) for populating the &signer in
    /// execute function
    pub fn signer_reference(x: AccountAddress) -> Self {
        Self(ValueImpl::ContainerRef(Box::new(ContainerRef::Local(
            Container::signer(x),
        ))))
    }

    pub fn struct_(s: Struct) -> Self {
        Self(ValueImpl::Container(Container::Struct(Rc::new(
            RefCell::new(s.fields),
        ))))
    }

    pub fn variant(s: Variant) -> Self {
        Self(ValueImpl::Container(Container::Variant(Rc::new(
            RefCell::new((s.tag, s.fields)),
        ))))
    }

    pub fn vector_u8(it: impl IntoIterator<Item = u8>) -> Self {
        Self(ValueImpl::Container(Container::VecU8(Rc::new(
            RefCell::new(it.into_iter().collect()),
        ))))
    }

    pub fn vector_u16(it: impl IntoIterator<Item = u16>) -> Self {
        Self(ValueImpl::Container(Container::VecU16(Rc::new(
            RefCell::new(it.into_iter().collect()),
        ))))
    }

    pub fn vector_u32(it: impl IntoIterator<Item = u32>) -> Self {
        Self(ValueImpl::Container(Container::VecU32(Rc::new(
            RefCell::new(it.into_iter().collect()),
        ))))
    }

    pub fn vector_u64(it: impl IntoIterator<Item = u64>) -> Self {
        Self(ValueImpl::Container(Container::VecU64(Rc::new(
            RefCell::new(it.into_iter().collect()),
        ))))
    }

    pub fn vector_u128(it: impl IntoIterator<Item = u128>) -> Self {
        Self(ValueImpl::Container(Container::VecU128(Rc::new(
            RefCell::new(it.into_iter().collect()),
        ))))
    }

    pub fn vector_u256(it: impl IntoIterator<Item = u256::U256>) -> Self {
        Self(ValueImpl::Container(Container::VecU256(Rc::new(
            RefCell::new(it.into_iter().collect()),
        ))))
    }

    pub fn vector_bool(it: impl IntoIterator<Item = bool>) -> Self {
        Self(ValueImpl::Container(Container::VecBool(Rc::new(
            RefCell::new(it.into_iter().collect()),
        ))))
    }

    pub fn vector_address(it: impl IntoIterator<Item = AccountAddress>) -> Self {
        Self(ValueImpl::Container(Container::VecAddress(Rc::new(
            RefCell::new(it.into_iter().collect()),
        ))))
    }
}

/***************************************************************************************
 *
 * Casting
 *
 *   Due to the public value types being opaque to an external user, the following
 *   public APIs are required to enable conversion between types in order to gain access
 *   to specific operations certain more refined types offer.
 *   For example, one must convert a `Value` to a `Struct` before unpack can be called.
 *
 *   It is expected that the caller will keep track of the invariants and guarantee
 *   the conversion will succeed. An error will be raised in case of a violation.
 *
 **************************************************************************************/
pub trait VMValueCast<T> {
    fn cast(self) -> PartialVMResult<T>;
}

macro_rules! impl_vm_value_cast {
    ($ty: ty, $tc: ident) => {
        impl VMValueCast<$ty> for Value {
            fn cast(self) -> PartialVMResult<$ty> {
                match self.0 {
                    ValueImpl::$tc(x) => Ok(x),
                    v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                        .with_message(format!("cannot cast {:?} to {}", v, stringify!($ty)))),
                }
            }
        }
    };
}

macro_rules! impl_vm_value_cast_boxed {
    ($ty: ty, $tc: ident) => {
        impl VMValueCast<$ty> for Value {
            fn cast(self) -> PartialVMResult<$ty> {
                match self.0 {
                    ValueImpl::$tc(x) => Ok(*x),
                    v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                        .with_message(format!("cannot cast {:?} to {}", v, stringify!($ty)))),
                }
            }
        }
    };
}

impl VMValueCast<Value> for Value {
    fn cast(self) -> PartialVMResult<Value> {
        Ok(self)
    }
}

impl_vm_value_cast!(u8, U8);
impl_vm_value_cast!(u16, U16);
impl_vm_value_cast!(u32, U32);
impl_vm_value_cast!(u64, U64);
impl_vm_value_cast_boxed!(u128, U128);
impl_vm_value_cast_boxed!(u256::U256, U256);
impl_vm_value_cast!(bool, Bool);
impl_vm_value_cast_boxed!(AccountAddress, Address);
impl_vm_value_cast_boxed!(ContainerRef, ContainerRef);
impl_vm_value_cast_boxed!(IndexedRef, IndexedRef);

impl VMValueCast<IntegerValue> for Value {
    fn cast(self) -> PartialVMResult<IntegerValue> {
        match self.0 {
            ValueImpl::U8(x) => Ok(IntegerValue::U8(x)),
            ValueImpl::U16(x) => Ok(IntegerValue::U16(x)),
            ValueImpl::U32(x) => Ok(IntegerValue::U32(x)),
            ValueImpl::U64(x) => Ok(IntegerValue::U64(x)),
            ValueImpl::U128(x) => Ok(IntegerValue::U128(*x)),
            ValueImpl::U256(x) => Ok(IntegerValue::U256(*x)),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to integer", v,))),
        }
    }
}

impl VMValueCast<Reference> for Value {
    fn cast(self) -> PartialVMResult<Reference> {
        match self.0 {
            ValueImpl::ContainerRef(r) => Ok(Reference(ReferenceImpl::ContainerRef(*r))),
            ValueImpl::IndexedRef(r) => Ok(Reference(ReferenceImpl::IndexedRef(*r))),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to reference", v,))),
        }
    }
}

impl VMValueCast<Container> for Value {
    fn cast(self) -> PartialVMResult<Container> {
        match self.0 {
            ValueImpl::Container(c) => Ok(c),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to container", v,))),
        }
    }
}

impl VMValueCast<Struct> for Value {
    fn cast(self) -> PartialVMResult<Struct> {
        match self.0 {
            ValueImpl::Container(Container::Struct(r)) => Ok(Struct {
                fields: take_unique_ownership(r)?,
            }),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to struct", v,))),
        }
    }
}

impl VMValueCast<Variant> for Value {
    fn cast(self) -> PartialVMResult<Variant> {
        match self.0 {
            ValueImpl::Container(Container::Variant(r)) => {
                let (tag, fields) = take_unique_ownership(r)?;
                Ok(Variant { tag, fields })
            }
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to enum variant", v,))),
        }
    }
}

impl VMValueCast<StructRef> for Value {
    fn cast(self) -> PartialVMResult<StructRef> {
        Ok(StructRef(VMValueCast::cast(self)?))
    }
}

impl VMValueCast<VariantRef> for Value {
    fn cast(self) -> PartialVMResult<VariantRef> {
        Ok(VariantRef(VMValueCast::cast(self)?))
    }
}

impl VMValueCast<Vec<u8>> for Value {
    fn cast(self) -> PartialVMResult<Vec<u8>> {
        match self.0 {
            ValueImpl::Container(Container::VecU8(r)) => take_unique_ownership(r),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to vector<u8>", v,))),
        }
    }
}

impl VMValueCast<Vec<u16>> for Value {
    fn cast(self) -> PartialVMResult<Vec<u16>> {
        match self.0 {
            ValueImpl::Container(Container::VecU16(r)) => take_unique_ownership(r),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to vector<u16>", v,))),
        }
    }
}

impl VMValueCast<Vec<u32>> for Value {
    fn cast(self) -> PartialVMResult<Vec<u32>> {
        match self.0 {
            ValueImpl::Container(Container::VecU32(r)) => take_unique_ownership(r),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to vector<u32>", v,))),
        }
    }
}

impl VMValueCast<Vec<u64>> for Value {
    fn cast(self) -> PartialVMResult<Vec<u64>> {
        match self.0 {
            ValueImpl::Container(Container::VecU64(r)) => take_unique_ownership(r),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to vector<u64>", v,))),
        }
    }
}

impl VMValueCast<Vec<u128>> for Value {
    fn cast(self) -> PartialVMResult<Vec<u128>> {
        match self.0 {
            ValueImpl::Container(Container::VecU128(r)) => take_unique_ownership(r),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to vector<u128>", v,))),
        }
    }
}

impl VMValueCast<Vec<u256::U256>> for Value {
    fn cast(self) -> PartialVMResult<Vec<u256::U256>> {
        match self.0 {
            ValueImpl::Container(Container::VecU256(r)) => take_unique_ownership(r),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to vector<u256>", v,))),
        }
    }
}

impl VMValueCast<Vec<AccountAddress>> for Value {
    fn cast(self) -> PartialVMResult<Vec<AccountAddress>> {
        match self.0 {
            ValueImpl::Container(Container::VecAddress(r)) => take_unique_ownership(r),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to vector<address>", v,))),
        }
    }
}

impl VMValueCast<Vec<Value>> for Value {
    fn cast(self) -> PartialVMResult<Vec<Value>> {
        match self.0 {
            ValueImpl::Container(Container::Vec(c)) => {
                Ok(take_unique_ownership(c)?.into_iter().map(Value).collect())
            }
            ValueImpl::Address(_)
            | ValueImpl::Bool(_)
            | ValueImpl::U8(_)
            | ValueImpl::U16(_)
            | ValueImpl::U32(_)
            | ValueImpl::U64(_)
            | ValueImpl::U128(_)
            | ValueImpl::U256(_) => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(
                    "cannot cast a specialized vector into a non-specialized one".to_string(),
                )),
            v => Err(
                PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(format!(
                    "cannot cast {:?} to vector<non-specialized-type>",
                    v,
                )),
            ),
        }
    }
}

impl VMValueCast<SignerRef> for Value {
    fn cast(self) -> PartialVMResult<SignerRef> {
        match self.0 {
            ValueImpl::ContainerRef(r) => Ok(SignerRef(*r)),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to Signer reference", v,))),
        }
    }
}

impl VMValueCast<VectorRef> for Value {
    fn cast(self) -> PartialVMResult<VectorRef> {
        match self.0 {
            ValueImpl::ContainerRef(r) => Ok(VectorRef(*r)),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to vector reference", v,))),
        }
    }
}

impl VMValueCast<Vector> for Value {
    fn cast(self) -> PartialVMResult<Vector> {
        match self.0 {
            ValueImpl::Container(c) => Ok(Vector(c)),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to vector", v,))),
        }
    }
}

impl Value {
    pub fn value_as<T>(self) -> PartialVMResult<T>
    where
        Self: VMValueCast<T>,
    {
        VMValueCast::cast(self)
    }
}

impl VMValueCast<u8> for IntegerValue {
    fn cast(self) -> PartialVMResult<u8> {
        match self {
            Self::U8(x) => Ok(x),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to u8", v,))),
        }
    }
}

impl VMValueCast<u16> for IntegerValue {
    fn cast(self) -> PartialVMResult<u16> {
        match self {
            Self::U16(x) => Ok(x),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to u16", v,))),
        }
    }
}

impl VMValueCast<u32> for IntegerValue {
    fn cast(self) -> PartialVMResult<u32> {
        match self {
            Self::U32(x) => Ok(x),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to u32", v,))),
        }
    }
}

impl VMValueCast<u64> for IntegerValue {
    fn cast(self) -> PartialVMResult<u64> {
        match self {
            Self::U64(x) => Ok(x),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to u64", v,))),
        }
    }
}

impl VMValueCast<u128> for IntegerValue {
    fn cast(self) -> PartialVMResult<u128> {
        match self {
            Self::U128(x) => Ok(x),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to u128", v,))),
        }
    }
}

impl VMValueCast<u256::U256> for IntegerValue {
    fn cast(self) -> PartialVMResult<u256::U256> {
        match self {
            Self::U256(x) => Ok(x),
            v => Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                .with_message(format!("cannot cast {:?} to u256", v,))),
        }
    }
}

impl IntegerValue {
    pub fn value_as<T>(self) -> PartialVMResult<T>
    where
        Self: VMValueCast<T>,
    {
        VMValueCast::cast(self)
    }
}

/***************************************************************************************
 *
 * Integer Operations
 *
 *   Arithmetic operations and conversions for integer values.
 *
 **************************************************************************************/
impl IntegerValue {
    pub fn add_checked(self, other: Self) -> PartialVMResult<Self> {
        use IntegerValue::*;
        let res = match (self, other) {
            (U8(l), U8(r)) => u8::checked_add(l, r).map(IntegerValue::U8),
            (U16(l), U16(r)) => u16::checked_add(l, r).map(IntegerValue::U16),
            (U32(l), U32(r)) => u32::checked_add(l, r).map(IntegerValue::U32),
            (U64(l), U64(r)) => u64::checked_add(l, r).map(IntegerValue::U64),
            (U128(l), U128(r)) => u128::checked_add(l, r).map(IntegerValue::U128),
            (U256(l), U256(r)) => u256::U256::checked_add(l, r).map(IntegerValue::U256),
            (l, r) => {
                let msg = format!("Cannot add {:?} and {:?}", l, r);
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        };
        res.ok_or_else(|| PartialVMError::new(StatusCode::ARITHMETIC_ERROR))
    }

    pub fn sub_checked(self, other: Self) -> PartialVMResult<Self> {
        use IntegerValue::*;
        let res = match (self, other) {
            (U8(l), U8(r)) => u8::checked_sub(l, r).map(IntegerValue::U8),
            (U16(l), U16(r)) => u16::checked_sub(l, r).map(IntegerValue::U16),
            (U32(l), U32(r)) => u32::checked_sub(l, r).map(IntegerValue::U32),
            (U64(l), U64(r)) => u64::checked_sub(l, r).map(IntegerValue::U64),
            (U128(l), U128(r)) => u128::checked_sub(l, r).map(IntegerValue::U128),
            (U256(l), U256(r)) => u256::U256::checked_sub(l, r).map(IntegerValue::U256),
            (l, r) => {
                let msg = format!("Cannot sub {:?} from {:?}", r, l);
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        };
        res.ok_or_else(|| PartialVMError::new(StatusCode::ARITHMETIC_ERROR))
    }

    pub fn mul_checked(self, other: Self) -> PartialVMResult<Self> {
        use IntegerValue::*;
        let res = match (self, other) {
            (U8(l), U8(r)) => u8::checked_mul(l, r).map(IntegerValue::U8),
            (U16(l), U16(r)) => u16::checked_mul(l, r).map(IntegerValue::U16),
            (U32(l), U32(r)) => u32::checked_mul(l, r).map(IntegerValue::U32),
            (U64(l), U64(r)) => u64::checked_mul(l, r).map(IntegerValue::U64),
            (U128(l), U128(r)) => u128::checked_mul(l, r).map(IntegerValue::U128),
            (U256(l), U256(r)) => u256::U256::checked_mul(l, r).map(IntegerValue::U256),
            (l, r) => {
                let msg = format!("Cannot mul {:?} and {:?}", l, r);
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        };
        res.ok_or_else(|| PartialVMError::new(StatusCode::ARITHMETIC_ERROR))
    }

    pub fn div_checked(self, other: Self) -> PartialVMResult<Self> {
        use IntegerValue::*;
        let res = match (self, other) {
            (U8(l), U8(r)) => u8::checked_div(l, r).map(IntegerValue::U8),
            (U16(l), U16(r)) => u16::checked_div(l, r).map(IntegerValue::U16),
            (U32(l), U32(r)) => u32::checked_div(l, r).map(IntegerValue::U32),
            (U64(l), U64(r)) => u64::checked_div(l, r).map(IntegerValue::U64),
            (U128(l), U128(r)) => u128::checked_div(l, r).map(IntegerValue::U128),
            (U256(l), U256(r)) => u256::U256::checked_div(l, r).map(IntegerValue::U256),
            (l, r) => {
                let msg = format!("Cannot div {:?} by {:?}", l, r);
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        };
        res.ok_or_else(|| PartialVMError::new(StatusCode::ARITHMETIC_ERROR))
    }

    pub fn rem_checked(self, other: Self) -> PartialVMResult<Self> {
        use IntegerValue::*;
        let res = match (self, other) {
            (U8(l), U8(r)) => u8::checked_rem(l, r).map(IntegerValue::U8),
            (U16(l), U16(r)) => u16::checked_rem(l, r).map(IntegerValue::U16),
            (U32(l), U32(r)) => u32::checked_rem(l, r).map(IntegerValue::U32),
            (U64(l), U64(r)) => u64::checked_rem(l, r).map(IntegerValue::U64),
            (U128(l), U128(r)) => u128::checked_rem(l, r).map(IntegerValue::U128),
            (U256(l), U256(r)) => u256::U256::checked_rem(l, r).map(IntegerValue::U256),
            (l, r) => {
                let msg = format!("Cannot rem {:?} by {:?}", l, r);
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        };
        res.ok_or_else(|| PartialVMError::new(StatusCode::ARITHMETIC_ERROR))
    }

    pub fn bit_or(self, other: Self) -> PartialVMResult<Self> {
        use IntegerValue::*;
        Ok(match (self, other) {
            (U8(l), U8(r)) => IntegerValue::U8(l | r),
            (U16(l), U16(r)) => IntegerValue::U16(l | r),
            (U32(l), U32(r)) => IntegerValue::U32(l | r),
            (U64(l), U64(r)) => IntegerValue::U64(l | r),
            (U128(l), U128(r)) => IntegerValue::U128(l | r),
            (U256(l), U256(r)) => IntegerValue::U256(l | r),
            (l, r) => {
                let msg = format!("Cannot bit_or {:?} and {:?}", l, r);
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        })
    }

    pub fn bit_and(self, other: Self) -> PartialVMResult<Self> {
        use IntegerValue::*;
        Ok(match (self, other) {
            (U8(l), U8(r)) => IntegerValue::U8(l & r),
            (U16(l), U16(r)) => IntegerValue::U16(l & r),
            (U32(l), U32(r)) => IntegerValue::U32(l & r),
            (U64(l), U64(r)) => IntegerValue::U64(l & r),
            (U128(l), U128(r)) => IntegerValue::U128(l & r),
            (U256(l), U256(r)) => IntegerValue::U256(l & r),
            (l, r) => {
                let msg = format!("Cannot bit_and {:?} and {:?}", l, r);
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        })
    }

    pub fn bit_xor(self, other: Self) -> PartialVMResult<Self> {
        use IntegerValue::*;
        Ok(match (self, other) {
            (U8(l), U8(r)) => IntegerValue::U8(l ^ r),
            (U16(l), U16(r)) => IntegerValue::U16(l ^ r),
            (U32(l), U32(r)) => IntegerValue::U32(l ^ r),
            (U64(l), U64(r)) => IntegerValue::U64(l ^ r),
            (U128(l), U128(r)) => IntegerValue::U128(l ^ r),
            (U256(l), U256(r)) => IntegerValue::U256(l ^ r),
            (l, r) => {
                let msg = format!("Cannot bit_xor {:?} and {:?}", l, r);
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        })
    }

    pub fn shl_checked(self, n_bits: u8) -> PartialVMResult<Self> {
        use IntegerValue::*;

        Ok(match self {
            U8(x) => {
                if n_bits >= 8 {
                    return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR));
                }
                IntegerValue::U8(x << n_bits)
            }
            U16(x) => {
                if n_bits >= 16 {
                    return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR));
                }
                IntegerValue::U16(x << n_bits)
            }
            U32(x) => {
                if n_bits >= 32 {
                    return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR));
                }
                IntegerValue::U32(x << n_bits)
            }
            U64(x) => {
                if n_bits >= 64 {
                    return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR));
                }
                IntegerValue::U64(x << n_bits)
            }
            U128(x) => {
                if n_bits >= 128 {
                    return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR));
                }
                IntegerValue::U128(x << n_bits)
            }
            U256(x) => IntegerValue::U256(x << n_bits),
        })
    }

    pub fn shr_checked(self, n_bits: u8) -> PartialVMResult<Self> {
        use IntegerValue::*;

        Ok(match self {
            U8(x) => {
                if n_bits >= 8 {
                    return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR));
                }
                IntegerValue::U8(x >> n_bits)
            }
            U16(x) => {
                if n_bits >= 16 {
                    return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR));
                }
                IntegerValue::U16(x >> n_bits)
            }
            U32(x) => {
                if n_bits >= 32 {
                    return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR));
                }
                IntegerValue::U32(x >> n_bits)
            }
            U64(x) => {
                if n_bits >= 64 {
                    return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR));
                }
                IntegerValue::U64(x >> n_bits)
            }
            U128(x) => {
                if n_bits >= 128 {
                    return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR));
                }
                IntegerValue::U128(x >> n_bits)
            }
            U256(x) => IntegerValue::U256(x >> n_bits),
        })
    }

    pub fn lt(self, other: Self) -> PartialVMResult<bool> {
        use IntegerValue::*;

        Ok(match (self, other) {
            (U8(l), U8(r)) => l < r,
            (U16(l), U16(r)) => l < r,
            (U32(l), U32(r)) => l < r,
            (U64(l), U64(r)) => l < r,
            (U128(l), U128(r)) => l < r,
            (U256(l), U256(r)) => l < r,
            (l, r) => {
                let msg = format!(
                    "Cannot compare {:?} and {:?}: incompatible integer types",
                    l, r
                );
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        })
    }

    pub fn le(self, other: Self) -> PartialVMResult<bool> {
        use IntegerValue::*;

        Ok(match (self, other) {
            (U8(l), U8(r)) => l <= r,
            (U16(l), U16(r)) => l <= r,
            (U32(l), U32(r)) => l <= r,
            (U64(l), U64(r)) => l <= r,
            (U128(l), U128(r)) => l <= r,
            (U256(l), U256(r)) => l <= r,
            (l, r) => {
                let msg = format!(
                    "Cannot compare {:?} and {:?}: incompatible integer types",
                    l, r
                );
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        })
    }

    pub fn gt(self, other: Self) -> PartialVMResult<bool> {
        use IntegerValue::*;

        Ok(match (self, other) {
            (U8(l), U8(r)) => l > r,
            (U16(l), U16(r)) => l > r,
            (U32(l), U32(r)) => l > r,
            (U64(l), U64(r)) => l > r,
            (U128(l), U128(r)) => l > r,
            (U256(l), U256(r)) => l > r,
            (l, r) => {
                let msg = format!(
                    "Cannot compare {:?} and {:?}: incompatible integer types",
                    l, r
                );
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        })
    }

    pub fn ge(self, other: Self) -> PartialVMResult<bool> {
        use IntegerValue::*;

        Ok(match (self, other) {
            (U8(l), U8(r)) => l >= r,
            (U16(l), U16(r)) => l >= r,
            (U32(l), U32(r)) => l >= r,
            (U64(l), U64(r)) => l >= r,
            (U128(l), U128(r)) => l >= r,
            (U256(l), U256(r)) => l >= r,
            (l, r) => {
                let msg = format!(
                    "Cannot compare {:?} and {:?}: incompatible integer types",
                    l, r
                );
                return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR).with_message(msg));
            }
        })
    }

    pub fn into_value(self) -> Value {
        use IntegerValue::*;

        match self {
            U8(x) => Value::u8(x),
            U16(x) => Value::u16(x),
            U32(x) => Value::u32(x),
            U64(x) => Value::u64(x),
            U128(x) => Value::u128(x),
            U256(x) => Value::u256(x),
        }
    }
}

impl IntegerValue {
    pub fn cast_u8(self) -> PartialVMResult<u8> {
        use IntegerValue::*;

        match self {
            U8(x) => Ok(x),
            U16(x) => {
                if x > (u8::MAX as u16) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u16({}) to u8", x)))
                } else {
                    Ok(x as u8)
                }
            }
            U32(x) => {
                if x > (u8::MAX as u32) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u32({}) to u8", x)))
                } else {
                    Ok(x as u8)
                }
            }
            U64(x) => {
                if x > (u8::MAX as u64) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u64({}) to u8", x)))
                } else {
                    Ok(x as u8)
                }
            }
            U128(x) => {
                if x > (u8::MAX as u128) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u128({}) to u8", x)))
                } else {
                    Ok(x as u8)
                }
            }
            U256(x) => {
                if x > (u256::U256::from(u8::MAX)) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u256({}) to u8", x)))
                } else {
                    Ok(x.unchecked_as_u8())
                }
            }
        }
    }

    pub fn cast_u16(self) -> PartialVMResult<u16> {
        use IntegerValue::*;

        match self {
            U8(x) => Ok(x as u16),
            U16(x) => Ok(x),
            U32(x) => {
                if x > (u16::MAX as u32) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u32({}) to u16", x)))
                } else {
                    Ok(x as u16)
                }
            }
            U64(x) => {
                if x > (u16::MAX as u64) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u64({}) to u16", x)))
                } else {
                    Ok(x as u16)
                }
            }
            U128(x) => {
                if x > (u16::MAX as u128) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u128({}) to u16", x)))
                } else {
                    Ok(x as u16)
                }
            }
            U256(x) => {
                if x > (u256::U256::from(u16::MAX)) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u256({}) to u16", x)))
                } else {
                    Ok(x.unchecked_as_u16())
                }
            }
        }
    }

    pub fn cast_u32(self) -> PartialVMResult<u32> {
        use IntegerValue::*;

        match self {
            U8(x) => Ok(x as u32),
            U16(x) => Ok(x as u32),
            U32(x) => Ok(x),
            U64(x) => {
                if x > (u32::MAX as u64) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u64({}) to u32", x)))
                } else {
                    Ok(x as u32)
                }
            }
            U128(x) => {
                if x > (u32::MAX as u128) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u128({}) to u32", x)))
                } else {
                    Ok(x as u32)
                }
            }
            U256(x) => {
                if x > (u256::U256::from(u32::MAX)) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u128({}) to u32", x)))
                } else {
                    Ok(x.unchecked_as_u32())
                }
            }
        }
    }

    pub fn cast_u64(self) -> PartialVMResult<u64> {
        use IntegerValue::*;

        match self {
            U8(x) => Ok(x as u64),
            U16(x) => Ok(x as u64),
            U32(x) => Ok(x as u64),
            U64(x) => Ok(x),
            U128(x) => {
                if x > (u64::MAX as u128) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u128({}) to u64", x)))
                } else {
                    Ok(x as u64)
                }
            }
            U256(x) => {
                if x > (u256::U256::from(u64::MAX)) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u256({}) to u64", x)))
                } else {
                    Ok(x.unchecked_as_u64())
                }
            }
        }
    }

    pub fn cast_u128(self) -> PartialVMResult<u128> {
        use IntegerValue::*;

        match self {
            U8(x) => Ok(x as u128),
            U16(x) => Ok(x as u128),
            U32(x) => Ok(x as u128),
            U64(x) => Ok(x as u128),
            U128(x) => Ok(x),
            U256(x) => {
                if x > (u256::U256::from(u128::MAX)) {
                    Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                        .with_message(format!("Cannot cast u256({}) to u128", x)))
                } else {
                    Ok(x.unchecked_as_u128())
                }
            }
        }
    }

    pub fn cast_u256(self) -> PartialVMResult<u256::U256> {
        use IntegerValue::*;

        Ok(match self {
            U8(x) => u256::U256::from(x),
            U16(x) => u256::U256::from(x),
            U32(x) => u256::U256::from(x),
            U64(x) => u256::U256::from(x),
            U128(x) => u256::U256::from(x),
            U256(x) => x,
        })
    }
}

/***************************************************************************************
*
* Vector
*
*   Implemented as a built-in data type.
*
**************************************************************************************/

pub const INDEX_OUT_OF_BOUNDS: u64 = NFE_VECTOR_ERROR_BASE + 1;
pub const POP_EMPTY_VEC: u64 = NFE_VECTOR_ERROR_BASE + 2;
pub const VEC_UNPACK_PARITY_MISMATCH: u64 = NFE_VECTOR_ERROR_BASE + 3;
pub const VEC_SIZE_LIMIT_REACHED: u64 = NFE_VECTOR_ERROR_BASE + 4;

fn check_elem_layout(ty: &Type, v: &Container) -> PartialVMResult<()> {
    match (ty, v) {
        (Type::U8, Container::VecU8(_))
        | (Type::U64, Container::VecU64(_))
        | (Type::U16, Container::VecU16(_))
        | (Type::U32, Container::VecU32(_))
        | (Type::U128, Container::VecU128(_))
        | (Type::U256, Container::VecU256(_))
        | (Type::Bool, Container::VecBool(_))
        | (Type::Address, Container::VecAddress(_))
        | (Type::Signer, Container::Struct(_)) => Ok(()),

        (Type::Vector(_), Container::Vec(_)) => Ok(()),

        (Type::Datatype(_), Container::Vec(_))
        | (Type::Signer, Container::Vec(_))
        | (Type::DatatypeInstantiation(_), Container::Vec(_)) => Ok(()),

        (Type::Reference(_), _) | (Type::MutableReference(_), _) | (Type::TyParam(_), _) => Err(
            PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                .with_message(format!("invalid type param for vector: {:?}", ty)),
        ),

        (Type::U8, _)
        | (Type::U64, _)
        | (Type::U16, _)
        | (Type::U32, _)
        | (Type::U128, _)
        | (Type::U256, _)
        | (Type::Bool, _)
        | (Type::Address, _)
        | (Type::Signer, _)
        | (Type::Vector(_), _)
        | (Type::Datatype(_), _)
        | (Type::DatatypeInstantiation(_), _) => Err(PartialVMError::new(
            StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
        )
        .with_message(format!(
            "vector elem layout mismatch, expected {:?}, got {:?}",
            ty, v
        ))),
    }
}

impl VectorRef {
    pub fn len(&self, type_param: &Type) -> PartialVMResult<Value> {
        let c = self.0.container();
        check_elem_layout(type_param, c)?;

        let len = match c {
            Container::VecU8(r) => r.borrow().len(),
            Container::VecU16(r) => r.borrow().len(),
            Container::VecU32(r) => r.borrow().len(),
            Container::VecU64(r) => r.borrow().len(),
            Container::VecU128(r) => r.borrow().len(),
            Container::VecU256(r) => r.borrow().len(),
            Container::VecBool(r) => r.borrow().len(),
            Container::VecAddress(r) => r.borrow().len(),
            Container::Vec(r) => r.borrow().len(),
            Container::Locals(_) | Container::Struct(_) | Container::Variant { .. } => {
                unreachable!()
            }
        };
        Ok(Value::u64(len as u64))
    }

    pub fn push_back(&self, e: Value, type_param: &Type, capacity: u64) -> PartialVMResult<()> {
        let c = self.0.container();
        check_elem_layout(type_param, c)?;

        let size = match c {
            Container::VecU8(r) => r.borrow().len(),
            Container::VecU16(r) => r.borrow().len(),
            Container::VecU32(r) => r.borrow().len(),
            Container::VecU64(r) => r.borrow().len(),
            Container::VecU128(r) => r.borrow().len(),
            Container::VecU256(r) => r.borrow().len(),
            Container::VecBool(r) => r.borrow().len(),
            Container::VecAddress(r) => r.borrow().len(),
            Container::Vec(r) => r.borrow().len(),
            Container::Locals(_) | Container::Struct(_) | Container::Variant { .. } => {
                unreachable!()
            }
        };
        if size >= (capacity as usize) {
            return Err(PartialVMError::new(StatusCode::VECTOR_OPERATION_ERROR)
                .with_sub_status(VEC_SIZE_LIMIT_REACHED)
                .with_message(format!("vector size limit is {capacity}",)));
        }

        match c {
            Container::VecU8(r) => r.borrow_mut().push(e.value_as()?),
            Container::VecU16(r) => r.borrow_mut().push(e.value_as()?),
            Container::VecU32(r) => r.borrow_mut().push(e.value_as()?),
            Container::VecU64(r) => r.borrow_mut().push(e.value_as()?),
            Container::VecU128(r) => r.borrow_mut().push(e.value_as()?),
            Container::VecU256(r) => r.borrow_mut().push(e.value_as()?),
            Container::VecBool(r) => r.borrow_mut().push(e.value_as()?),
            Container::VecAddress(r) => r.borrow_mut().push(e.value_as()?),
            Container::Vec(r) => r.borrow_mut().push(e.0),
            Container::Locals(_) | Container::Struct(_) | Container::Variant { .. } => {
                unreachable!()
            }
        }

        self.0.mark_dirty();
        Ok(())
    }

    pub fn borrow_elem(&self, idx: usize, type_param: &Type) -> PartialVMResult<Value> {
        let c = self.0.container();
        check_elem_layout(type_param, c)?;
        if idx >= c.len() {
            return Err(PartialVMError::new(StatusCode::VECTOR_OPERATION_ERROR)
                .with_sub_status(INDEX_OUT_OF_BOUNDS));
        }
        Ok(Value(self.0.borrow_elem(idx)?))
    }

    /// Returns a RefCell reference to the underlying vector of a `&vector<u8>` value.
    pub fn as_bytes_ref(&self) -> std::cell::Ref<'_, Vec<u8>> {
        let c = self.0.container();
        match c {
            Container::VecU8(r) => r.borrow(),
            _ => panic!("can only be called on vector<u8>"),
        }
    }

    pub fn pop(&self, type_param: &Type) -> PartialVMResult<Value> {
        let c = self.0.container();
        check_elem_layout(type_param, c)?;

        macro_rules! err_pop_empty_vec {
            () => {
                return Err(PartialVMError::new(StatusCode::VECTOR_OPERATION_ERROR)
                    .with_sub_status(POP_EMPTY_VEC))
            };
        }

        let res = match c {
            Container::VecU8(r) => match r.borrow_mut().pop() {
                Some(x) => Value::u8(x),
                None => err_pop_empty_vec!(),
            },
            Container::VecU16(r) => match r.borrow_mut().pop() {
                Some(x) => Value::u16(x),
                None => err_pop_empty_vec!(),
            },
            Container::VecU32(r) => match r.borrow_mut().pop() {
                Some(x) => Value::u32(x),
                None => err_pop_empty_vec!(),
            },
            Container::VecU64(r) => match r.borrow_mut().pop() {
                Some(x) => Value::u64(x),
                None => err_pop_empty_vec!(),
            },
            Container::VecU128(r) => match r.borrow_mut().pop() {
                Some(x) => Value::u128(x),
                None => err_pop_empty_vec!(),
            },
            Container::VecU256(r) => match r.borrow_mut().pop() {
                Some(x) => Value::u256(x),
                None => err_pop_empty_vec!(),
            },
            Container::VecBool(r) => match r.borrow_mut().pop() {
                Some(x) => Value::bool(x),
                None => err_pop_empty_vec!(),
            },
            Container::VecAddress(r) => match r.borrow_mut().pop() {
                Some(x) => Value::address(x),
                None => err_pop_empty_vec!(),
            },
            Container::Vec(r) => match r.borrow_mut().pop() {
                Some(x) => Value(x),
                None => err_pop_empty_vec!(),
            },
            Container::Locals(_) | Container::Struct(_) | Container::Variant { .. } => {
                unreachable!()
            }
        };

        self.0.mark_dirty();
        Ok(res)
    }

    pub fn swap(&self, idx1: usize, idx2: usize, type_param: &Type) -> PartialVMResult<()> {
        let c = self.0.container();
        check_elem_layout(type_param, c)?;

        macro_rules! swap {
            ($v: expr) => {{
                let mut v = $v.borrow_mut();
                if idx1 >= v.len() || idx2 >= v.len() {
                    return Err(PartialVMError::new(StatusCode::VECTOR_OPERATION_ERROR)
                        .with_sub_status(INDEX_OUT_OF_BOUNDS));
                }
                v.swap(idx1, idx2);
            }};
        }

        match c {
            Container::VecU8(r) => swap!(r),
            Container::VecU16(r) => swap!(r),
            Container::VecU32(r) => swap!(r),
            Container::VecU64(r) => swap!(r),
            Container::VecU128(r) => swap!(r),
            Container::VecU256(r) => swap!(r),
            Container::VecBool(r) => swap!(r),
            Container::VecAddress(r) => swap!(r),
            Container::Vec(r) => swap!(r),
            Container::Locals(_) | Container::Struct(_) | Container::Variant { .. } => {
                unreachable!()
            }
        }

        self.0.mark_dirty();
        Ok(())
    }
}

pub enum VectorSpecialization {
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Bool,
    Address,
    Container,
}

impl TryFrom<&Type> for VectorSpecialization {
    type Error = PartialVMError;

    fn try_from(ty: &Type) -> Result<Self, Self::Error> {
        Ok(match ty {
            Type::U8 => VectorSpecialization::U8,
            Type::U16 => VectorSpecialization::U16,
            Type::U32 => VectorSpecialization::U32,
            Type::U64 => VectorSpecialization::U64,
            Type::U128 => VectorSpecialization::U128,
            Type::U256 => VectorSpecialization::U256,
            Type::Bool => VectorSpecialization::Bool,
            Type::Address => VectorSpecialization::Address,
            Type::Vector(_) | Type::Signer | Type::Datatype(_) | Type::DatatypeInstantiation(_) => {
                VectorSpecialization::Container
            }
            Type::Reference(_) | Type::MutableReference(_) | Type::TyParam(_) => {
                return Err(
                    PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                        .with_message(format!("invalid type param for vector: {:?}", ty)),
                );
            }
        })
    }
}

impl Vector {
    pub fn pack(
        specialization: VectorSpecialization,
        elements: impl IntoIterator<Item = Value>,
    ) -> PartialVMResult<Value> {
        let container = match specialization {
            VectorSpecialization::U8 => Value::vector_u8(
                elements
                    .into_iter()
                    .map(|v| v.value_as())
                    .collect::<PartialVMResult<Vec<_>>>()?,
            ),
            VectorSpecialization::U16 => Value::vector_u16(
                elements
                    .into_iter()
                    .map(|v| v.value_as())
                    .collect::<PartialVMResult<Vec<_>>>()?,
            ),
            VectorSpecialization::U32 => Value::vector_u32(
                elements
                    .into_iter()
                    .map(|v| v.value_as())
                    .collect::<PartialVMResult<Vec<_>>>()?,
            ),
            VectorSpecialization::U64 => Value::vector_u64(
                elements
                    .into_iter()
                    .map(|v| v.value_as())
                    .collect::<PartialVMResult<Vec<_>>>()?,
            ),
            VectorSpecialization::U128 => Value::vector_u128(
                elements
                    .into_iter()
                    .map(|v| v.value_as())
                    .collect::<PartialVMResult<Vec<_>>>()?,
            ),
            VectorSpecialization::U256 => Value::vector_u256(
                elements
                    .into_iter()
                    .map(|v| v.value_as())
                    .collect::<PartialVMResult<Vec<_>>>()?,
            ),
            VectorSpecialization::Bool => Value::vector_bool(
                elements
                    .into_iter()
                    .map(|v| v.value_as())
                    .collect::<PartialVMResult<Vec<_>>>()?,
            ),
            VectorSpecialization::Address => Value::vector_address(
                elements
                    .into_iter()
                    .map(|v| v.value_as())
                    .collect::<PartialVMResult<Vec<_>>>()?,
            ),

            VectorSpecialization::Container => Value(ValueImpl::Container(Container::Vec(
                Rc::new(RefCell::new(elements.into_iter().map(|v| v.0).collect())),
            ))),
        };

        Ok(container)
    }

    pub fn empty(specialization: VectorSpecialization) -> PartialVMResult<Value> {
        Self::pack(specialization, vec![])
    }

    pub fn unpack(self, type_param: &Type, expected_num: u64) -> PartialVMResult<Vec<Value>> {
        check_elem_layout(type_param, &self.0)?;
        let elements: Vec<_> = match self.0 {
            Container::VecU8(r) => take_unique_ownership(r)?
                .into_iter()
                .map(Value::u8)
                .collect(),
            Container::VecU16(r) => take_unique_ownership(r)?
                .into_iter()
                .map(Value::u16)
                .collect(),
            Container::VecU32(r) => take_unique_ownership(r)?
                .into_iter()
                .map(Value::u32)
                .collect(),
            Container::VecU64(r) => take_unique_ownership(r)?
                .into_iter()
                .map(Value::u64)
                .collect(),
            Container::VecU128(r) => take_unique_ownership(r)?
                .into_iter()
                .map(Value::u128)
                .collect(),
            Container::VecU256(r) => take_unique_ownership(r)?
                .into_iter()
                .map(Value::u256)
                .collect(),
            Container::VecBool(r) => take_unique_ownership(r)?
                .into_iter()
                .map(Value::bool)
                .collect(),
            Container::VecAddress(r) => take_unique_ownership(r)?
                .into_iter()
                .map(Value::address)
                .collect(),
            Container::Vec(r) => take_unique_ownership(r)?.into_iter().map(Value).collect(),
            Container::Locals(_) | Container::Struct(_) | Container::Variant { .. } => {
                unreachable!()
            }
        };
        if expected_num as usize == elements.len() {
            Ok(elements)
        } else {
            Err(PartialVMError::new(StatusCode::VECTOR_OPERATION_ERROR)
                .with_sub_status(VEC_UNPACK_PARITY_MISMATCH))
        }
    }

    pub fn destroy_empty(self, type_param: &Type) -> PartialVMResult<()> {
        self.unpack(type_param, 0)?;
        Ok(())
    }

    pub fn to_vec_u8(self) -> PartialVMResult<Vec<u8>> {
        check_elem_layout(&Type::U8, &self.0)?;
        if let Container::VecU8(r) = self.0 {
            Ok(take_unique_ownership(r)?.into_iter().collect())
        } else {
            Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                    .with_message("expected vector<u8>".to_string()),
            )
        }
    }
}

/***************************************************************************************
 *
 * Abstract Memory Size
 *
 *   TODO(Gas): This is the oldest implementation of abstract memory size.
 *              It is now kept only as a reference impl, which is used to ensure
 *              the new implementation is fully backward compatible.
 *              We should be able to get this removed after we use the new impl
 *              for a while and gain enough confidence in that.
 *
 **************************************************************************************/

/// The size in bytes for a non-string or address constant on the stack
pub(crate) const LEGACY_CONST_SIZE: AbstractMemorySize = AbstractMemorySize::new(16);

/// The size in bytes for a reference on the stack
pub(crate) const LEGACY_REFERENCE_SIZE: AbstractMemorySize = AbstractMemorySize::new(8);

/// The size of a struct in bytes
pub(crate) const LEGACY_STRUCT_SIZE: AbstractMemorySize = AbstractMemorySize::new(2);

impl Container {
    fn legacy_size(&self) -> AbstractMemorySize {
        match self {
            Self::Locals(r) | Self::Vec(r) | Self::Struct(r) => {
                Struct::legacy_size_impl(&r.borrow())
            }
            Self::Variant(r) => Variant::legacy_size_impl(&r.borrow().1),
            Self::VecU8(r) => {
                AbstractMemorySize::new((r.borrow().len() * std::mem::size_of::<u8>()) as u64)
            }
            Self::VecU16(r) => {
                AbstractMemorySize::new((r.borrow().len() * std::mem::size_of::<u16>()) as u64)
            }
            Self::VecU32(r) => {
                AbstractMemorySize::new((r.borrow().len() * std::mem::size_of::<u32>()) as u64)
            }
            Self::VecU64(r) => {
                AbstractMemorySize::new((r.borrow().len() * std::mem::size_of::<u64>()) as u64)
            }
            Self::VecU128(r) => {
                AbstractMemorySize::new((r.borrow().len() * std::mem::size_of::<u128>()) as u64)
            }
            Self::VecU256(r) => AbstractMemorySize::new(
                (r.borrow().len() * std::mem::size_of::<u256::U256>()) as u64,
            ),
            Self::VecBool(r) => {
                AbstractMemorySize::new((r.borrow().len() * std::mem::size_of::<bool>()) as u64)
            }
            Self::VecAddress(r) => AbstractMemorySize::new(
                (r.borrow().len() * std::mem::size_of::<AccountAddress>()) as u64,
            ),
        }
    }
}

impl ContainerRef {
    fn legacy_size(&self) -> AbstractMemorySize {
        LEGACY_REFERENCE_SIZE
    }
}

impl IndexedRef {
    fn legacy_size(&self) -> AbstractMemorySize {
        LEGACY_REFERENCE_SIZE
    }
}

impl ValueImpl {
    fn legacy_size(&self) -> AbstractMemorySize {
        use ValueImpl::*;

        match self {
            Invalid | U8(_) | U16(_) | U32(_) | U64(_) | U128(_) | U256(_) | Bool(_) => {
                LEGACY_CONST_SIZE
            }
            Address(_) => AbstractMemorySize::new(AccountAddress::LENGTH as u64),
            ContainerRef(r) => r.legacy_size(),
            IndexedRef(r) => r.legacy_size(),
            // TODO: in case the borrow fails the VM will panic.
            Container(c) => c.legacy_size(),
        }
    }
}

impl Variant {
    const TAG_SIZE: AbstractMemorySize = AbstractMemorySize::new(std::mem::size_of::<u16>() as u64);

    fn legacy_size_impl(fields: &[ValueImpl]) -> AbstractMemorySize {
        fields
            .iter()
            .fold(LEGACY_STRUCT_SIZE.add(Self::TAG_SIZE), |acc, v| {
                acc + v.legacy_size()
            })
    }
}

impl Struct {
    fn legacy_size_impl(fields: &[ValueImpl]) -> AbstractMemorySize {
        fields
            .iter()
            .fold(LEGACY_STRUCT_SIZE, |acc, v| acc + v.legacy_size())
    }

    #[cfg(test)]
    pub(crate) fn legacy_size(&self) -> AbstractMemorySize {
        Self::legacy_size_impl(&self.fields)
    }
}

impl Value {
    pub fn legacy_size(&self) -> AbstractMemorySize {
        self.0.legacy_size()
    }
}

#[cfg(test)]
impl ReferenceImpl {
    fn legacy_size(&self) -> AbstractMemorySize {
        match self {
            Self::ContainerRef(r) => r.legacy_size(),
            Self::IndexedRef(r) => r.legacy_size(),
        }
    }
}

#[cfg(test)]
impl Reference {
    pub(crate) fn legacy_size(&self) -> AbstractMemorySize {
        self.0.legacy_size()
    }
}

/***************************************************************************************
 *
 * Struct Operations
 *
 *   Public APIs for Struct.
 *
 **************************************************************************************/
impl Struct {
    pub fn pack<I: IntoIterator<Item = Value>>(vals: I) -> Self {
        Self {
            fields: vals.into_iter().map(|v| v.0).collect(),
        }
    }

    pub fn unpack(self) -> PartialVMResult<impl Iterator<Item = Value>> {
        Ok(self.fields.into_iter().map(Value))
    }
}

/***************************************************************************************
 *
 * Variant Operations
 *
 *   Public APIs for Enums.
 *
 **************************************************************************************/
impl Variant {
    pub fn pack<I: IntoIterator<Item = Value>>(tag: VariantTag, vals: I) -> Self {
        Self {
            tag,
            fields: vals.into_iter().map(|v| v.0).collect(),
        }
    }

    pub fn unpack(self) -> PartialVMResult<impl Iterator<Item = Value>> {
        Ok(self.fields.into_iter().map(Value))
    }

    pub fn check_tag(&self, tag: VariantTag) -> PartialVMResult<()> {
        if tag != self.tag {
            Err(PartialVMError::new(StatusCode::VARIANT_TAG_MISMATCH)
                .with_message(format!("tag mismatch: expected {}, got {}", tag, self.tag)))
        } else {
            Ok(())
        }
    }
}

/***************************************************************************************
 *
 * Global Value Operations
 *
 *   Public APIs for GlobalValue. They allow global values to be created from external
 *   source (a.k.a. storage), and references to be taken from them. At the end of the
 *   transaction execution the dirty ones can be identified and wrote back to storage.
 *
 **************************************************************************************/
#[allow(clippy::unnecessary_wraps)]
impl GlobalValueImpl {
    fn cached(
        val: ValueImpl,
        status: GlobalDataStatus,
    ) -> Result<Self, (PartialVMError, ValueImpl)> {
        match val {
            ValueImpl::Container(Container::Struct(fields)) => {
                let status = Rc::new(RefCell::new(status));
                Ok(Self::Cached { fields, status })
            }
            val => Err((
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                    .with_message("failed to publish cached: not a resource".to_string()),
                val,
            )),
        }
    }

    fn fresh(val: ValueImpl) -> Result<Self, (PartialVMError, ValueImpl)> {
        match val {
            ValueImpl::Container(Container::Struct(fields)) => Ok(Self::Fresh { fields }),
            val => Err((
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                    .with_message("failed to publish fresh: not a resource".to_string()),
                val,
            )),
        }
    }

    fn move_from(&mut self) -> PartialVMResult<ValueImpl> {
        let fields = match self {
            Self::None | Self::Deleted => {
                return Err(PartialVMError::new(StatusCode::MISSING_DATA));
            }
            Self::Fresh { .. } => match std::mem::replace(self, Self::None) {
                Self::Fresh { fields } => fields,
                _ => unreachable!(),
            },
            Self::Cached { .. } => match std::mem::replace(self, Self::Deleted) {
                Self::Cached { fields, .. } => fields,
                _ => unreachable!(),
            },
        };
        if Rc::strong_count(&fields) != 1 {
            return Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                    .with_message("moving global resource with dangling reference".to_string()),
            );
        }
        Ok(ValueImpl::Container(Container::Struct(fields)))
    }

    fn move_to(&mut self, val: ValueImpl) -> Result<(), (PartialVMError, ValueImpl)> {
        match self {
            Self::Fresh { .. } | Self::Cached { .. } => {
                return Err((
                    PartialVMError::new(StatusCode::RESOURCE_ALREADY_EXISTS),
                    val,
                ));
            }
            Self::None => *self = Self::fresh(val)?,
            Self::Deleted => *self = Self::cached(val, GlobalDataStatus::Dirty)?,
        }
        Ok(())
    }

    fn exists(&self) -> PartialVMResult<bool> {
        match self {
            Self::Fresh { .. } | Self::Cached { .. } => Ok(true),
            Self::None | Self::Deleted => Ok(false),
        }
    }

    fn borrow_global(&self) -> PartialVMResult<ValueImpl> {
        match self {
            Self::None | Self::Deleted => Err(PartialVMError::new(StatusCode::MISSING_DATA)),
            Self::Fresh { fields } => Ok(ValueImpl::ContainerRef(Box::new(ContainerRef::Local(
                Container::Struct(Rc::clone(fields)),
            )))),
            Self::Cached { fields, status } => {
                Ok(ValueImpl::ContainerRef(Box::new(ContainerRef::Global {
                    container: Container::Struct(Rc::clone(fields)),
                    status: Rc::clone(status),
                })))
            }
        }
    }

    fn into_effect(self) -> Option<Op<ValueImpl>> {
        match self {
            Self::None => None,
            Self::Deleted => Some(Op::Delete),
            Self::Fresh { fields } => {
                Some(Op::New(ValueImpl::Container(Container::Struct(fields))))
            }
            Self::Cached { fields, status } => match &*status.borrow() {
                GlobalDataStatus::Dirty => {
                    Some(Op::Modify(ValueImpl::Container(Container::Struct(fields))))
                }
                GlobalDataStatus::Clean => None,
            },
        }
    }

    fn is_mutated(&self) -> bool {
        match self {
            Self::None => false,
            Self::Deleted => true,
            Self::Fresh { fields: _ } => true,
            Self::Cached { fields: _, status } => match &*status.borrow() {
                GlobalDataStatus::Dirty => true,
                GlobalDataStatus::Clean => false,
            },
        }
    }

    fn into_value(self) -> Option<ValueImpl> {
        match self {
            Self::None | Self::Deleted => None,
            Self::Fresh { fields } | Self::Cached { fields, .. } => {
                Some(ValueImpl::Container(Container::Struct(fields)))
            }
        }
    }
}

impl GlobalValue {
    pub fn none() -> Self {
        Self(GlobalValueImpl::None)
    }

    pub fn cached(val: Value) -> PartialVMResult<Self> {
        Ok(Self(
            GlobalValueImpl::cached(val.0, GlobalDataStatus::Clean).map_err(|(err, _val)| err)?,
        ))
    }

    pub fn move_from(&mut self) -> PartialVMResult<Value> {
        Ok(Value(self.0.move_from()?))
    }

    pub fn move_to(&mut self, val: Value) -> Result<(), (PartialVMError, Value)> {
        self.0
            .move_to(val.0)
            .map_err(|(err, val)| (err, Value(val)))
    }

    pub fn borrow_global(&self) -> PartialVMResult<Value> {
        Ok(Value(self.0.borrow_global()?))
    }

    pub fn exists(&self) -> PartialVMResult<bool> {
        self.0.exists()
    }

    pub fn into_effect(self) -> Option<Op<Value>> {
        self.0.into_effect().map(|op| op.map(Value))
    }

    pub fn is_mutated(&self) -> bool {
        self.0.is_mutated()
    }

    pub fn into_value(self) -> Option<Value> {
        self.0.into_value().map(Value)
    }
}

/***************************************************************************************
*
* Display
*
*   Implementation of the Display trait for VM Values. These are supposed to be more
*   friendly & readable than the generated Debug dump.
*
**************************************************************************************/

impl Display for ValueImpl {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Invalid => write!(f, "Invalid"),

            Self::U8(x) => write!(f, "U8({})", x),
            Self::U16(x) => write!(f, "U16({})", x),
            Self::U32(x) => write!(f, "U32({})", x),
            Self::U64(x) => write!(f, "U64({})", x),
            Self::U128(x) => write!(f, "U128({})", x),
            Self::U256(x) => write!(f, "U256({})", x),
            Self::Bool(x) => write!(f, "{}", x),
            Self::Address(addr) => write!(f, "Address({})", addr.short_str_lossless()),

            Self::Container(r) => write!(f, "{}", r),

            Self::ContainerRef(r) => write!(f, "{}", r),
            Self::IndexedRef(r) => write!(f, "{}", r),
        }
    }
}

fn display_list_of_items<T, I>(items: I, f: &mut fmt::Formatter) -> fmt::Result
where
    T: Display,
    I: IntoIterator<Item = T>,
{
    write!(f, "[")?;
    let mut items = items.into_iter();
    if let Some(x) = items.next() {
        write!(f, "{}", x)?;
        for x in items {
            write!(f, ", {}", x)?;
        }
    }
    write!(f, "]")
}

impl Container {
    fn raw_address(&self) -> usize {
        use Container::*;

        match self {
            Locals(r) => r.as_ptr() as usize,
            Vec(r) => r.as_ptr() as usize,
            Struct(r) => r.as_ptr() as usize,
            VecU8(r) => r.as_ptr() as usize,
            VecU16(r) => r.as_ptr() as usize,
            VecU32(r) => r.as_ptr() as usize,
            VecU64(r) => r.as_ptr() as usize,
            VecU128(r) => r.as_ptr() as usize,
            VecU256(r) => r.as_ptr() as usize,
            VecBool(r) => r.as_ptr() as usize,
            VecAddress(r) => r.as_ptr() as usize,
            Variant(r) => r.as_ptr() as usize,
        }
    }
}

impl Locals {
    pub fn raw_address(&self) -> usize {
        self.0.as_ptr() as usize
    }
}

impl Display for ContainerRef {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Local(c) => write!(f, "(&container {:x})", c.raw_address()),
            Self::Global { status, container } => write!(
                f,
                "(&container {:x} -- {:?})",
                container.raw_address(),
                &*status.borrow(),
            ),
        }
    }
}

impl Display for IndexedRef {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}[{}]", self.container_ref, self.idx)
    }
}

impl Display for Container {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "(container {:x}: ", self.raw_address())?;

        match self {
            Self::Locals(r) | Self::Vec(r) | Self::Struct(r) => {
                display_list_of_items(r.borrow().iter(), f)
            }
            Self::Variant(r) => {
                let (tag, values) = &*r.borrow();
                write!(f, "|tag: {}|", tag)?;
                display_list_of_items(values, f)
            }
            Self::VecU8(r) => display_list_of_items(r.borrow().iter(), f),
            Self::VecU16(r) => display_list_of_items(r.borrow().iter(), f),
            Self::VecU32(r) => display_list_of_items(r.borrow().iter(), f),
            Self::VecU64(r) => display_list_of_items(r.borrow().iter(), f),
            Self::VecU128(r) => display_list_of_items(r.borrow().iter(), f),
            Self::VecU256(r) => display_list_of_items(r.borrow().iter(), f),
            Self::VecBool(r) => display_list_of_items(r.borrow().iter(), f),
            Self::VecAddress(r) => display_list_of_items(r.borrow().iter(), f),
        }?;

        write!(f, ")")
    }
}

impl Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Display for Locals {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            self.0
                .borrow()
                .iter()
                .enumerate()
                .map(|(idx, val)| format!("[{}] {}", idx, val))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

#[allow(dead_code)]
pub mod debug {
    use super::*;
    use std::fmt::Write;

    fn print_invalid<B: Write>(buf: &mut B) -> PartialVMResult<()> {
        debug_write!(buf, "-")
    }

    fn print_u8<B: Write>(buf: &mut B, x: &u8) -> PartialVMResult<()> {
        debug_write!(buf, "{}", x)
    }

    fn print_u16<B: Write>(buf: &mut B, x: &u16) -> PartialVMResult<()> {
        debug_write!(buf, "{}", x)
    }

    fn print_u32<B: Write>(buf: &mut B, x: &u32) -> PartialVMResult<()> {
        debug_write!(buf, "{}", x)
    }

    fn print_u64<B: Write>(buf: &mut B, x: &u64) -> PartialVMResult<()> {
        debug_write!(buf, "{}", x)
    }

    fn print_u128<B: Write>(buf: &mut B, x: &u128) -> PartialVMResult<()> {
        debug_write!(buf, "{}", x)
    }

    fn print_u256<B: Write>(buf: &mut B, x: &u256::U256) -> PartialVMResult<()> {
        debug_write!(buf, "{}", x)
    }

    fn print_bool<B: Write>(buf: &mut B, x: &bool) -> PartialVMResult<()> {
        debug_write!(buf, "{}", x)
    }

    fn print_address<B: Write>(buf: &mut B, x: &AccountAddress) -> PartialVMResult<()> {
        debug_write!(buf, "{}", x)
    }

    fn print_value_impl<B: Write>(buf: &mut B, val: &ValueImpl) -> PartialVMResult<()> {
        match val {
            ValueImpl::Invalid => print_invalid(buf),

            ValueImpl::U8(x) => print_u8(buf, x),
            ValueImpl::U16(x) => print_u16(buf, x),
            ValueImpl::U32(x) => print_u32(buf, x),
            ValueImpl::U64(x) => print_u64(buf, x),
            ValueImpl::U128(x) => print_u128(buf, x),
            ValueImpl::U256(x) => print_u256(buf, x),
            ValueImpl::Bool(x) => print_bool(buf, x),
            ValueImpl::Address(x) => print_address(buf, x),

            ValueImpl::Container(c) => print_container(buf, c),

            ValueImpl::ContainerRef(r) => print_container_ref(buf, r),
            ValueImpl::IndexedRef(r) => print_indexed_ref(buf, r),
        }
    }

    fn print_list<'a, B, I, X, F>(
        buf: &mut B,
        begin: &str,
        items: I,
        print: F,
        end: &str,
    ) -> PartialVMResult<()>
    where
        B: Write,
        X: 'a,
        I: IntoIterator<Item = &'a X>,
        F: Fn(&mut B, &X) -> PartialVMResult<()>,
    {
        debug_write!(buf, "{}", begin)?;
        let mut it = items.into_iter();
        if let Some(x) = it.next() {
            print(buf, x)?;
            for x in it {
                debug_write!(buf, ", ")?;
                print(buf, x)?;
            }
        }
        debug_write!(buf, "{}", end)?;
        Ok(())
    }

    fn print_container<B: Write>(buf: &mut B, c: &Container) -> PartialVMResult<()> {
        match c {
            Container::Vec(r) => print_list(buf, "[", r.borrow().iter(), print_value_impl, "]"),

            Container::Struct(r) => {
                print_list(buf, "{ ", r.borrow().iter(), print_value_impl, " }")
            }

            Container::Variant(r) => {
                let (tag, values) = &*r.borrow();
                print_list(
                    buf,
                    &format!("|{}|{{ ", tag),
                    values,
                    print_value_impl,
                    " }",
                )
            }

            Container::VecU8(r) => print_list(buf, "[", r.borrow().iter(), print_u8, "]"),
            Container::VecU16(r) => print_list(buf, "[", r.borrow().iter(), print_u16, "]"),
            Container::VecU32(r) => print_list(buf, "[", r.borrow().iter(), print_u32, "]"),
            Container::VecU64(r) => print_list(buf, "[", r.borrow().iter(), print_u64, "]"),
            Container::VecU128(r) => print_list(buf, "[", r.borrow().iter(), print_u128, "]"),
            Container::VecU256(r) => print_list(buf, "[", r.borrow().iter(), print_u256, "]"),
            Container::VecBool(r) => print_list(buf, "[", r.borrow().iter(), print_bool, "]"),
            Container::VecAddress(r) => print_list(buf, "[", r.borrow().iter(), print_address, "]"),

            Container::Locals(_) => Err(PartialVMError::new(
                StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
            )
            .with_message("debug print - invalid container: Locals".to_string())),
        }
    }

    fn print_container_ref<B: Write>(buf: &mut B, r: &ContainerRef) -> PartialVMResult<()> {
        debug_write!(buf, "(&) ")?;
        print_container(buf, r.container())
    }

    fn print_slice_elem<B, X, F>(buf: &mut B, v: &[X], idx: usize, print: F) -> PartialVMResult<()>
    where
        B: Write,
        F: FnOnce(&mut B, &X) -> PartialVMResult<()>,
    {
        match v.get(idx) {
            Some(x) => print(buf, x),
            None => Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                    .with_message("ref index out of bounds".to_string()),
            ),
        }
    }

    fn print_indexed_ref<B: Write>(buf: &mut B, r: &IndexedRef) -> PartialVMResult<()> {
        let idx = r.idx;
        match r.container_ref.container() {
            Container::Locals(r) | Container::Vec(r) | Container::Struct(r) => {
                print_slice_elem(buf, &r.borrow(), idx, print_value_impl)
            }
            Container::Variant(r) => {
                let (tag, values) = &*r.borrow();
                debug_write!(buf, "|{}|", tag)?;
                print_slice_elem(buf, values, idx, print_value_impl)
            }

            Container::VecU8(r) => print_slice_elem(buf, &r.borrow(), idx, print_u8),
            Container::VecU16(r) => print_slice_elem(buf, &r.borrow(), idx, print_u16),
            Container::VecU32(r) => print_slice_elem(buf, &r.borrow(), idx, print_u32),
            Container::VecU64(r) => print_slice_elem(buf, &r.borrow(), idx, print_u64),
            Container::VecU128(r) => print_slice_elem(buf, &r.borrow(), idx, print_u128),
            Container::VecU256(r) => print_slice_elem(buf, &r.borrow(), idx, print_u256),
            Container::VecBool(r) => print_slice_elem(buf, &r.borrow(), idx, print_bool),
            Container::VecAddress(r) => print_slice_elem(buf, &r.borrow(), idx, print_address),
        }
    }

    // TODO: This function was used in an old implementation of std::debug::print, and can probably be removed.
    pub fn print_reference<B: Write>(buf: &mut B, r: &Reference) -> PartialVMResult<()> {
        match &r.0 {
            ReferenceImpl::ContainerRef(r) => print_container_ref(buf, r),
            ReferenceImpl::IndexedRef(r) => print_indexed_ref(buf, r),
        }
    }

    pub fn print_locals<B: Write>(buf: &mut B, locals: &Locals) -> PartialVMResult<()> {
        // REVIEW: The number of spaces in the indent is currently hard coded.
        for (idx, val) in locals.0.borrow().iter().enumerate() {
            debug_write!(buf, "            [{}] ", idx)?;
            print_value_impl(buf, val)?;
            debug_writeln!(buf)?;
        }
        Ok(())
    }

    pub fn print_value<B: Write>(buf: &mut B, val: &Value) -> PartialVMResult<()> {
        print_value_impl(buf, &val.0)
    }
}

/***************************************************************************************
 *
 * Serialization & Deserialization
 *
 *   BCS implementation for VM values. Note although values are represented as Rust
 *   enums that carry type info in the tags, we should NOT rely on them for
 *   serialization:
 *     1) Depending on the specific internal representation, it may be impossible to
 *        reconstruct the layout from a value. For example, one cannot tell if a general
 *        container is a struct or a value.
 *     2) Even if 1) is not a problem at a certain time, we may change to a different
 *        internal representation that breaks the 1-1 mapping. Extremely speaking, if
 *        we switch to untagged unions one day, none of the type info will be carried
 *        by the value.
 *
 *   Therefore the appropriate & robust way to implement serialization & deserialization
 *   is to involve an explicit representation of the type layout.
 *
 **************************************************************************************/
use serde::{
    Deserialize,
    de::Error as DeError,
    ser::{Error as SerError, SerializeSeq, SerializeTuple},
};

impl Value {
    pub fn simple_deserialize(blob: &[u8], layout: &MoveTypeLayout) -> Option<Value> {
        bcs::from_bytes_seed(SeedWrapper { layout }, blob).ok()
    }

    pub fn typed_serialize(&self, layout: &MoveTypeLayout) -> Option<Vec<u8>> {
        bcs::to_bytes(&AnnotatedValue {
            layout,
            val: &self.0,
        })
        .ok()
    }

    pub fn serialize(&self) -> Option<Vec<u8>> {
        bcs::to_bytes(&self.0).ok()
    }
}

impl serde::Serialize for ValueImpl {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ValueImpl::U8(u) => serializer.serialize_u8(*u),
            ValueImpl::U16(u) => serializer.serialize_u16(*u),
            ValueImpl::U32(u) => serializer.serialize_u32(*u),
            ValueImpl::U64(u) => serializer.serialize_u64(*u),
            ValueImpl::U128(u) => serializer.serialize_u128(**u),
            ValueImpl::U256(u) => u.serialize(serializer),
            ValueImpl::Bool(b) => serializer.serialize_bool(*b),
            ValueImpl::Address(a) => a.serialize(serializer),
            ValueImpl::Container(container) => container.serialize(serializer),
            v @ (ValueImpl::ContainerRef(_) | ValueImpl::IndexedRef(_) | ValueImpl::Invalid) => {
                Err(invariant_violation::<S>(format!(
                    "cannot serialize value {:?}",
                    v
                )))
            }
        }
    }
}

impl serde::Serialize for Container {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Container::VecU8(r) => r.borrow().serialize(serializer),
            Container::VecU16(r) => r.borrow().serialize(serializer),
            Container::VecU32(r) => r.borrow().serialize(serializer),
            Container::VecU64(r) => r.borrow().serialize(serializer),
            Container::VecU128(r) => r.borrow().serialize(serializer),
            Container::VecU256(r) => r.borrow().serialize(serializer),
            Container::VecBool(r) => r.borrow().serialize(serializer),
            Container::VecAddress(r) => r.borrow().serialize(serializer),
            Container::Vec(r) => {
                let v = r.borrow();
                let mut s = serializer.serialize_seq(Some(v.len()))?;
                for v in v.iter() {
                    s.serialize_element(v)?;
                }
                s.end()
            }
            Container::Struct(r) => {
                let v = r.borrow();
                let mut t = serializer.serialize_tuple(v.len())?;
                for val in v.iter() {
                    t.serialize_element(&val)?;
                }
                t.end()
            }
            Container::Variant(r) => {
                let (tag, values) = &*r.borrow();
                let tag = if *tag as u64 > VARIANT_TAG_MAX_VALUE {
                    return Err(serde::ser::Error::custom(format!(
                        "Variant tag {} is greater than the maximum allowed value of {}",
                        tag, VARIANT_TAG_MAX_VALUE
                    )));
                } else {
                    *tag as u8
                };

                struct VariantsSerializer<'a>(&'a [ValueImpl]);
                impl serde::Serialize for VariantsSerializer<'_> {
                    fn serialize<S: serde::Serializer>(
                        &self,
                        serializer: S,
                    ) -> Result<S::Ok, S::Error> {
                        let mut t = serializer.serialize_tuple(self.0.len())?;
                        for val in self.0.iter() {
                            t.serialize_element(val)?;
                        }
                        t.end()
                    }
                }

                let mut t = serializer.serialize_tuple(2)?;
                t.serialize_element(&tag)?;
                t.serialize_element(&VariantsSerializer(values))?;
                t.end()
            }
            Container::Locals(_) => Err(invariant_violation::<S>(format!(
                "Tried to serialize a locals container {:?}",
                self
            ))),
        }
    }
}

struct AnnotatedValue<'a, 'b, T1, T2> {
    layout: &'a T1,
    val: &'b T2,
}

fn invariant_violation<S: serde::Serializer>(message: String) -> S::Error {
    S::Error::custom(
        PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR).with_message(message),
    )
}

impl serde::Serialize for AnnotatedValue<'_, '_, MoveTypeLayout, ValueImpl> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match (self.layout, self.val) {
            (MoveTypeLayout::U8, ValueImpl::U8(x)) => serializer.serialize_u8(*x),
            (MoveTypeLayout::U16, ValueImpl::U16(x)) => serializer.serialize_u16(*x),
            (MoveTypeLayout::U32, ValueImpl::U32(x)) => serializer.serialize_u32(*x),
            (MoveTypeLayout::U64, ValueImpl::U64(x)) => serializer.serialize_u64(*x),
            (MoveTypeLayout::U128, ValueImpl::U128(x)) => serializer.serialize_u128(**x),
            (MoveTypeLayout::U256, ValueImpl::U256(x)) => x.serialize(serializer),
            (MoveTypeLayout::Bool, ValueImpl::Bool(x)) => serializer.serialize_bool(*x),
            (MoveTypeLayout::Address, ValueImpl::Address(x)) => x.serialize(serializer),

            (MoveTypeLayout::Struct(struct_layout), ValueImpl::Container(Container::Struct(r))) => {
                (AnnotatedValue {
                    layout: &**struct_layout,
                    val: &*r.borrow(),
                })
                .serialize(serializer)
            }

            (MoveTypeLayout::Enum(enum_layout), ValueImpl::Container(Container::Variant(r))) => {
                (AnnotatedValue {
                    layout: &**enum_layout,
                    val: &*r.borrow(),
                })
                .serialize(serializer)
            }

            (MoveTypeLayout::Vector(layout), ValueImpl::Container(c)) => {
                let layout = &**layout;
                match (layout, c) {
                    (MoveTypeLayout::U8, Container::VecU8(r)) => r.borrow().serialize(serializer),
                    (MoveTypeLayout::U16, Container::VecU16(r)) => r.borrow().serialize(serializer),
                    (MoveTypeLayout::U32, Container::VecU32(r)) => r.borrow().serialize(serializer),
                    (MoveTypeLayout::U64, Container::VecU64(r)) => r.borrow().serialize(serializer),
                    (MoveTypeLayout::U128, Container::VecU128(r)) => {
                        r.borrow().serialize(serializer)
                    }
                    (MoveTypeLayout::U256, Container::VecU256(r)) => {
                        r.borrow().serialize(serializer)
                    }
                    (MoveTypeLayout::Bool, Container::VecBool(r)) => {
                        r.borrow().serialize(serializer)
                    }
                    (MoveTypeLayout::Address, Container::VecAddress(r)) => {
                        r.borrow().serialize(serializer)
                    }

                    (_, Container::Vec(r)) => {
                        let v = r.borrow();
                        let mut t = serializer.serialize_seq(Some(v.len()))?;
                        for val in v.iter() {
                            t.serialize_element(&AnnotatedValue { layout, val })?;
                        }
                        t.end()
                    }

                    (layout, container) => Err(invariant_violation::<S>(format!(
                        "cannot serialize container {:?} as {:?}",
                        container, layout
                    ))),
                }
            }

            (MoveTypeLayout::Signer, ValueImpl::Container(Container::Struct(r))) => {
                let v = r.borrow();
                if v.len() != 1 {
                    return Err(invariant_violation::<S>(format!(
                        "cannot serialize container as a signer -- expected 1 field got {}",
                        v.len()
                    )));
                }
                (AnnotatedValue {
                    layout: &MoveTypeLayout::Address,
                    val: &v[0],
                })
                .serialize(serializer)
            }

            (ty, val) => Err(invariant_violation::<S>(format!(
                "cannot serialize value {:?} as {:?}",
                val, ty
            ))),
        }
    }
}

impl serde::Serialize for AnnotatedValue<'_, '_, MoveStructLayout, Vec<ValueImpl>> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let values = &self.val;
        let fields = self.layout.fields();
        if fields.len() != values.len() {
            return Err(invariant_violation::<S>(format!(
                "cannot serialize struct value {:?} as {:?} -- number of fields mismatch",
                self.val, self.layout
            )));
        }
        let mut t = serializer.serialize_tuple(values.len())?;
        for (field_layout, val) in fields.iter().zip(values.iter()) {
            t.serialize_element(&AnnotatedValue {
                layout: field_layout,
                val,
            })?;
        }
        t.end()
    }
}

impl serde::Serialize for AnnotatedValue<'_, '_, MoveEnumLayout, (VariantTag, Vec<ValueImpl>)> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (tag, values) = &self.val;
        let tag = if *tag as u64 > VARIANT_TAG_MAX_VALUE {
            return Err(serde::ser::Error::custom(format!(
                "Variant tag {} is greater than the maximum allowed value of {}",
                tag, VARIANT_TAG_MAX_VALUE
            )));
        } else {
            *tag as u8
        };

        let fields = &self.layout.0[tag as usize];
        if fields.len() != values.len() {
            return Err(invariant_violation::<S>(format!(
                "cannot serialize variant value {:?} as {:?} -- number of fields mismatch",
                self.val, self.layout
            )));
        }

        let mut t = serializer.serialize_tuple(2)?;
        t.serialize_element(&tag)?;

        t.serialize_element(&AnnotatedValue {
            layout: &VariantFields(fields),
            val: values,
        })?;

        t.end()
    }
}

struct VariantFields<'a>(&'a [MoveTypeLayout]);

impl<'a> serde::Serialize for AnnotatedValue<'a, '_, VariantFields<'a>, Vec<ValueImpl>> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let values = self.val;
        let types = self.layout.0;
        if types.len() != values.len() {
            return Err(invariant_violation::<S>(format!(
                "cannot serialize variant value {:?} as {:?} -- number of fields mismatch",
                self.val, self.layout.0
            )));
        }
        let mut t = serializer.serialize_tuple(values.len())?;
        for (field_layout, val) in types.iter().zip(values.iter()) {
            t.serialize_element(&AnnotatedValue {
                layout: field_layout,
                val,
            })?;
        }
        t.end()
    }
}

#[derive(Clone)]
struct SeedWrapper<L> {
    layout: L,
}

impl<'d> serde::de::DeserializeSeed<'d> for SeedWrapper<&MoveTypeLayout> {
    type Value = Value;

    fn deserialize<D: serde::de::Deserializer<'d>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        use MoveTypeLayout as L;

        match self.layout {
            L::Bool => bool::deserialize(deserializer).map(Value::bool),
            L::U8 => u8::deserialize(deserializer).map(Value::u8),
            L::U16 => u16::deserialize(deserializer).map(Value::u16),
            L::U32 => u32::deserialize(deserializer).map(Value::u32),
            L::U64 => u64::deserialize(deserializer).map(Value::u64),
            L::U128 => u128::deserialize(deserializer).map(Value::u128),
            L::U256 => u256::U256::deserialize(deserializer).map(Value::u256),
            L::Address => AccountAddress::deserialize(deserializer).map(Value::address),
            L::Signer => AccountAddress::deserialize(deserializer).map(Value::signer),

            L::Struct(struct_layout) => Ok(SeedWrapper {
                layout: &**struct_layout,
            }
            .deserialize(deserializer)?),

            L::Enum(enum_layout) => Ok(SeedWrapper {
                layout: &**enum_layout,
            }
            .deserialize(deserializer)?),

            L::Vector(layout) => {
                let container = match &**layout {
                    L::U8 => {
                        Container::VecU8(Rc::new(RefCell::new(Vec::deserialize(deserializer)?)))
                    }
                    L::U16 => {
                        Container::VecU16(Rc::new(RefCell::new(Vec::deserialize(deserializer)?)))
                    }
                    L::U32 => {
                        Container::VecU32(Rc::new(RefCell::new(Vec::deserialize(deserializer)?)))
                    }
                    L::U64 => {
                        Container::VecU64(Rc::new(RefCell::new(Vec::deserialize(deserializer)?)))
                    }
                    L::U128 => {
                        Container::VecU128(Rc::new(RefCell::new(Vec::deserialize(deserializer)?)))
                    }
                    L::U256 => {
                        Container::VecU256(Rc::new(RefCell::new(Vec::deserialize(deserializer)?)))
                    }
                    L::Bool => {
                        Container::VecBool(Rc::new(RefCell::new(Vec::deserialize(deserializer)?)))
                    }
                    L::Address => Container::VecAddress(Rc::new(RefCell::new(Vec::deserialize(
                        deserializer,
                    )?))),
                    layout => {
                        let v = deserializer
                            .deserialize_seq(VectorElementVisitor(SeedWrapper { layout }))?;
                        Container::Vec(Rc::new(RefCell::new(v)))
                    }
                };
                Ok(Value(ValueImpl::Container(container)))
            }
        }
    }
}

impl<'d> serde::de::DeserializeSeed<'d> for SeedWrapper<&MoveStructLayout> {
    type Value = Value;

    fn deserialize<D: serde::de::Deserializer<'d>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        let fields = deserializer
            .deserialize_tuple(self.layout.0.len(), StructFieldVisitor(&self.layout.0))?;
        Ok(Value::struct_(Struct::pack(fields)))
    }
}

impl<'d> serde::de::DeserializeSeed<'d> for SeedWrapper<&MoveEnumLayout> {
    type Value = Value;

    fn deserialize<D: serde::de::Deserializer<'d>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        let variant = deserializer.deserialize_tuple(2, EnumFieldVisitor(&self.layout.0))?;
        Ok(Value::variant(variant))
    }
}

struct VectorElementVisitor<'a>(SeedWrapper<&'a MoveTypeLayout>);

impl<'d> serde::de::Visitor<'d> for VectorElementVisitor<'_> {
    type Value = Vec<ValueImpl>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Vector")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'d>,
    {
        let mut vals = Vec::new();
        while let Some(elem) = seq.next_element_seed(self.0.clone())? {
            vals.push(elem.0)
        }
        Ok(vals)
    }
}

struct StructFieldVisitor<'a>(&'a [MoveTypeLayout]);

impl<'d> serde::de::Visitor<'d> for StructFieldVisitor<'_> {
    type Value = Vec<Value>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Struct")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'d>,
    {
        let mut val = Vec::new();
        for (i, field_layout) in self.0.iter().enumerate() {
            if let Some(elem) = seq.next_element_seed(SeedWrapper {
                layout: field_layout,
            })? {
                val.push(elem)
            } else {
                return Err(A::Error::invalid_length(i, &self));
            }
        }
        Ok(val)
    }
}

struct EnumFieldVisitor<'a>(&'a Vec<Vec<MoveTypeLayout>>);

impl<'d> serde::de::Visitor<'d> for EnumFieldVisitor<'_> {
    type Value = Variant;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Enum")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'d>,
    {
        let tag = match seq.next_element_seed(&MoveTypeLayout::U8)? {
            Some(MoveValue::U8(tag)) if tag as u64 <= VARIANT_TAG_MAX_VALUE => tag as u16,
            Some(MoveValue::U8(tag)) => return Err(A::Error::invalid_length(tag as usize, &self)),
            Some(val) => {
                return Err(A::Error::invalid_type(
                    serde::de::Unexpected::Other(&format!("{val:?}")),
                    &self,
                ));
            }
            None => return Err(A::Error::invalid_length(0, &self)),
        };

        let Some(variant_layout) = self.0.get(tag as usize) else {
            return Err(A::Error::invalid_length(tag as usize, &self));
        };

        let Some(fields) = seq.next_element_seed(&MoveRuntimeVariantFieldLayout(variant_layout))?
        else {
            return Err(A::Error::invalid_length(1, &self));
        };

        Ok(Variant::pack(tag, fields))
    }
}

struct MoveRuntimeVariantFieldLayout<'a>(&'a Vec<MoveTypeLayout>);

impl<'d> serde::de::DeserializeSeed<'d> for &MoveRuntimeVariantFieldLayout<'_> {
    type Value = Vec<Value>;

    fn deserialize<D: serde::de::Deserializer<'d>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_tuple(self.0.len(), StructFieldVisitor(self.0))
    }
}

/***************************************************************************************
*
* Constants
*
*   Implementation of deserialization of constant data into a runtime value
*
**************************************************************************************/

impl Value {
    fn constant_sig_token_to_layout(constant_signature: &SignatureToken) -> Option<MoveTypeLayout> {
        use MoveTypeLayout as L;
        use SignatureToken as S;

        Some(match constant_signature {
            S::Bool => L::Bool,
            S::U8 => L::U8,
            S::U16 => L::U16,
            S::U32 => L::U32,
            S::U64 => L::U64,
            S::U128 => L::U128,
            S::U256 => L::U256,
            S::Address => L::Address,
            S::Signer => return None,
            S::Vector(inner) => L::Vector(Box::new(Self::constant_sig_token_to_layout(inner)?)),
            // Not yet supported
            S::Datatype(_) | S::DatatypeInstantiation(_) => return None,
            // Not allowed/Not meaningful
            S::TypeParameter(_) | S::Reference(_) | S::MutableReference(_) => return None,
        })
    }

    pub fn deserialize_constant(constant: &Constant) -> Option<Value> {
        let layout = Self::constant_sig_token_to_layout(&constant.type_)?;
        Value::simple_deserialize(&constant.data, &layout)
    }
}

/***************************************************************************************
*
* Destructors
*
**************************************************************************************/
// Locals may contain reference values that points to the same cotnainer through Rc, hencing forming
// a cycle. Therefore values need to be manually taken out of the Locals in order to not leak memory.
impl Drop for Locals {
    fn drop(&mut self) {
        _ = self.drop_all_values();
    }
}

/***************************************************************************************
*
* Views
*
**************************************************************************************/
impl Container {
    fn visit_impl(&self, visitor: &mut impl ValueVisitor, depth: usize) {
        use Container::*;

        match self {
            Locals(_) => unreachable!("Should not ba able to visit a Locals container directly"),
            Vec(r) => {
                let r = r.borrow();
                if visitor.visit_vec(depth, r.len()) {
                    for val in r.iter() {
                        val.visit_impl(visitor, depth + 1);
                    }
                }
            }
            Struct(r) => {
                let r = r.borrow();
                if visitor.visit_struct(depth, r.len()) {
                    for val in r.iter() {
                        val.visit_impl(visitor, depth + 1);
                    }
                }
            }
            Variant(r) => {
                let r = &*r.borrow().1;
                if visitor.visit_variant(depth, r.len()) {
                    for val in r.iter() {
                        val.visit_impl(visitor, depth + 1);
                    }
                }
            }
            VecU8(r) => visitor.visit_vec_u8(depth, &r.borrow()),
            VecU16(r) => visitor.visit_vec_u16(depth, &r.borrow()),
            VecU32(r) => visitor.visit_vec_u32(depth, &r.borrow()),
            VecU64(r) => visitor.visit_vec_u64(depth, &r.borrow()),
            VecU128(r) => visitor.visit_vec_u128(depth, &r.borrow()),
            VecU256(r) => visitor.visit_vec_u256(depth, &r.borrow()),
            VecBool(r) => visitor.visit_vec_bool(depth, &r.borrow()),
            VecAddress(r) => visitor.visit_vec_address(depth, &r.borrow()),
        }
    }

    fn visit_indexed(&self, visitor: &mut impl ValueVisitor, depth: usize, idx: usize) {
        use Container::*;

        match self {
            Locals(r) | Vec(r) | Struct(r) => r.borrow()[idx].visit_impl(visitor, depth + 1),
            Variant(r) => r.borrow().1[idx].visit_impl(visitor, depth + 1),
            VecU8(vals) => visitor.visit_u8(depth + 1, vals.borrow()[idx]),
            VecU16(vals) => visitor.visit_u16(depth + 1, vals.borrow()[idx]),
            VecU32(vals) => visitor.visit_u32(depth + 1, vals.borrow()[idx]),
            VecU64(vals) => visitor.visit_u64(depth + 1, vals.borrow()[idx]),
            VecU128(vals) => visitor.visit_u128(depth + 1, vals.borrow()[idx]),
            VecU256(vals) => visitor.visit_u256(depth + 1, vals.borrow()[idx]),
            VecBool(vals) => visitor.visit_bool(depth + 1, vals.borrow()[idx]),
            VecAddress(vals) => visitor.visit_address(depth + 1, vals.borrow()[idx]),
        }
    }
}

impl ContainerRef {
    fn visit_impl(&self, visitor: &mut impl ValueVisitor, depth: usize) {
        use ContainerRef::*;

        let (container, is_global) = match self {
            Local(container) => (container, false),
            Global { container, .. } => (container, false),
        };

        if visitor.visit_ref(depth, is_global) {
            container.visit_impl(visitor, depth + 1);
        }
    }
}

impl IndexedRef {
    fn visit_impl(&self, visitor: &mut impl ValueVisitor, depth: usize) {
        use ContainerRef::*;

        let (container, is_global) = match &self.container_ref {
            Local(container) => (container, false),
            Global { container, .. } => (container, false),
        };

        if visitor.visit_ref(depth, is_global) {
            container.visit_indexed(visitor, depth, self.idx)
        }
    }
}

impl ValueImpl {
    fn visit_impl(&self, visitor: &mut impl ValueVisitor, depth: usize) {
        use ValueImpl::*;

        match self {
            Invalid => unreachable!("Should not be able to visit an invalid value"),

            U8(val) => visitor.visit_u8(depth, *val),
            U16(val) => visitor.visit_u16(depth, *val),
            U32(val) => visitor.visit_u32(depth, *val),
            U64(val) => visitor.visit_u64(depth, *val),
            U128(val) => visitor.visit_u128(depth, **val),
            U256(val) => visitor.visit_u256(depth, **val),
            Bool(val) => visitor.visit_bool(depth, *val),
            Address(val) => visitor.visit_address(depth, **val),

            Container(c) => c.visit_impl(visitor, depth),

            ContainerRef(r) => r.visit_impl(visitor, depth),
            IndexedRef(r) => r.visit_impl(visitor, depth),
        }
    }
}

impl ValueView for ValueImpl {
    fn visit(&self, visitor: &mut impl ValueVisitor) {
        self.visit_impl(visitor, 0)
    }
}

impl ValueView for Value {
    fn visit(&self, visitor: &mut impl ValueVisitor) {
        self.0.visit(visitor)
    }
}

impl ValueView for Struct {
    fn visit(&self, visitor: &mut impl ValueVisitor) {
        if visitor.visit_struct(0, self.fields.len()) {
            for val in self.fields.iter() {
                val.visit_impl(visitor, 1);
            }
        }
    }
}

impl ValueView for Vector {
    fn visit(&self, visitor: &mut impl ValueVisitor) {
        self.0.visit_impl(visitor, 0)
    }
}

impl ValueView for IntegerValue {
    fn visit(&self, visitor: &mut impl ValueVisitor) {
        use IntegerValue::*;

        match self {
            U8(val) => visitor.visit_u8(0, *val),
            U16(val) => visitor.visit_u16(0, *val),
            U32(val) => visitor.visit_u32(0, *val),
            U64(val) => visitor.visit_u64(0, *val),
            U128(val) => visitor.visit_u128(0, *val),
            U256(val) => visitor.visit_u256(0, *val),
        }
    }
}

impl ValueView for Reference {
    fn visit(&self, visitor: &mut impl ValueVisitor) {
        use ReferenceImpl::*;

        match &self.0 {
            ContainerRef(r) => r.visit_impl(visitor, 0),
            IndexedRef(r) => r.visit_impl(visitor, 0),
        }
    }
}

impl ValueView for VectorRef {
    fn visit(&self, visitor: &mut impl ValueVisitor) {
        self.0.visit_impl(visitor, 0)
    }
}

impl ValueView for StructRef {
    fn visit(&self, visitor: &mut impl ValueVisitor) {
        self.0.visit_impl(visitor, 0)
    }
}

impl ValueView for SignerRef {
    fn visit(&self, visitor: &mut impl ValueVisitor) {
        self.0.visit_impl(visitor, 0)
    }
}

impl ValueView for VariantRef {
    fn visit(&self, visitor: &mut impl ValueVisitor) {
        self.0.visit_impl(visitor, 0)
    }
}

// Note: We may want to add more helpers to retrieve value views behind references here.

impl Struct {
    #[allow(clippy::needless_lifetimes)]
    pub fn field_views<'a>(&'a self) -> impl ExactSizeIterator<Item = impl ValueView + 'a> {
        self.fields.iter()
    }
}

impl Variant {
    #[allow(clippy::needless_lifetimes)]
    pub fn field_views<'a>(&'a self) -> impl ExactSizeIterator<Item = impl ValueView + 'a> {
        self.fields.iter()
    }
}

impl Vector {
    pub fn elem_len(&self) -> usize {
        self.0.len()
    }

    #[allow(clippy::needless_lifetimes)]
    pub fn elem_views<'a>(&'a self) -> impl ExactSizeIterator<Item = impl ValueView + 'a> {
        struct ElemView<'b> {
            container: &'b Container,
            idx: usize,
        }

        impl<'b> ValueView for ElemView<'b> {
            fn visit(&self, visitor: &mut impl ValueVisitor) {
                self.container.visit_indexed(visitor, 0, self.idx)
            }
        }

        let len = self.0.len();

        (0..len).map(|idx| ElemView {
            container: &self.0,
            idx,
        })
    }
}

impl Reference {
    #[allow(clippy::needless_lifetimes)]
    pub fn value_view<'a>(&'a self) -> impl ValueView + 'a {
        struct ValueBehindRef<'b>(&'b ReferenceImpl);

        impl<'b> ValueView for ValueBehindRef<'b> {
            fn visit(&self, visitor: &mut impl ValueVisitor) {
                use ReferenceImpl::*;

                match self.0 {
                    ContainerRef(r) => r.container().visit_impl(visitor, 0),
                    IndexedRef(r) => r.container_ref.container().visit_indexed(visitor, 0, r.idx),
                }
            }
        }

        ValueBehindRef(&self.0)
    }
}

impl GlobalValue {
    #[allow(clippy::needless_lifetimes)]
    pub fn view<'a>(&'a self) -> Option<impl ValueView + 'a> {
        use GlobalValueImpl as G;

        struct Wrapper<'b>(&'b Rc<RefCell<Vec<ValueImpl>>>);

        impl<'b> ValueView for Wrapper<'b> {
            fn visit(&self, visitor: &mut impl ValueVisitor) {
                let r = self.0.borrow();
                if visitor.visit_struct(0, r.len()) {
                    for val in r.iter() {
                        val.visit_impl(visitor, 1);
                    }
                }
            }
        }

        match &self.0 {
            G::None | G::Deleted => None,
            G::Cached { fields, .. } | G::Fresh { fields } => Some(Wrapper(fields)),
        }
    }
}

/***************************************************************************************
 *
 * Prop Testing
 *
 *   Random generation of values that fit into a given layout.
 *
 **************************************************************************************/
#[cfg(any(test, feature = "fuzzing"))]
pub mod prop {
    use super::*;
    use proptest::{collection::vec, prelude::*};

    pub fn value_strategy_with_layout(layout: MoveTypeLayout) -> impl Strategy<Value = Value> {
        use MoveTypeLayout as L;

        match layout {
            L::U8 => any::<u8>().prop_map(Value::u8).boxed(),
            L::U16 => any::<u16>().prop_map(Value::u16).boxed(),
            L::U32 => any::<u32>().prop_map(Value::u32).boxed(),
            L::U64 => any::<u64>().prop_map(Value::u64).boxed(),
            L::U128 => any::<u128>().prop_map(Value::u128).boxed(),
            L::U256 => any::<u256::U256>().prop_map(Value::u256).boxed(),
            L::Bool => any::<bool>().prop_map(Value::bool).boxed(),
            L::Address => any::<AccountAddress>().prop_map(Value::address).boxed(),
            L::Signer => any::<AccountAddress>().prop_map(Value::signer).boxed(),

            L::Vector(layout) => match *layout {
                L::U8 => vec(any::<u8>(), 0..10)
                    .prop_map(|vals| {
                        Value(ValueImpl::Container(Container::VecU8(Rc::new(
                            RefCell::new(vals),
                        ))))
                    })
                    .boxed(),
                L::U16 => vec(any::<u16>(), 0..10)
                    .prop_map(|vals| {
                        Value(ValueImpl::Container(Container::VecU16(Rc::new(
                            RefCell::new(vals),
                        ))))
                    })
                    .boxed(),
                L::U32 => vec(any::<u32>(), 0..10)
                    .prop_map(|vals| {
                        Value(ValueImpl::Container(Container::VecU32(Rc::new(
                            RefCell::new(vals),
                        ))))
                    })
                    .boxed(),
                L::U64 => vec(any::<u64>(), 0..10)
                    .prop_map(|vals| {
                        Value(ValueImpl::Container(Container::VecU64(Rc::new(
                            RefCell::new(vals),
                        ))))
                    })
                    .boxed(),
                L::U128 => vec(any::<u128>(), 0..10)
                    .prop_map(|vals| {
                        Value(ValueImpl::Container(Container::VecU128(Rc::new(
                            RefCell::new(vals),
                        ))))
                    })
                    .boxed(),
                L::U256 => vec(any::<u256::U256>(), 0..10)
                    .prop_map(|vals| {
                        Value(ValueImpl::Container(Container::VecU256(Rc::new(
                            RefCell::new(vals),
                        ))))
                    })
                    .boxed(),
                L::Bool => vec(any::<bool>(), 0..10)
                    .prop_map(|vals| {
                        Value(ValueImpl::Container(Container::VecBool(Rc::new(
                            RefCell::new(vals),
                        ))))
                    })
                    .boxed(),
                L::Address => vec(any::<AccountAddress>(), 0..10)
                    .prop_map(|vals| {
                        Value(ValueImpl::Container(Container::VecAddress(Rc::new(
                            RefCell::new(vals),
                        ))))
                    })
                    .boxed(),
                layout => vec(value_strategy_with_layout(layout), 0..10)
                    .prop_map(|vals| {
                        Value(ValueImpl::Container(Container::Vec(Rc::new(RefCell::new(
                            vals.into_iter().map(|val| val.0).collect(),
                        )))))
                    })
                    .boxed(),
            },

            L::Struct(struct_layout) => struct_layout
                .into_fields()
                .into_iter()
                .map(value_strategy_with_layout)
                .collect::<Vec<_>>()
                .prop_map(move |vals| Value::struct_(Struct::pack(vals)))
                .boxed(),

            L::Enum(enum_layout) => {
                let enum_layouts = enum_layout
                    .clone()
                    .0
                    .into_iter()
                    .enumerate()
                    .collect::<Vec<_>>();
                proptest::sample::select(enum_layouts)
                    .prop_flat_map(move |(tag, layout)| {
                        layout
                            .into_iter()
                            .map(value_strategy_with_layout)
                            .collect::<Vec<_>>()
                            .prop_map(move |v| Value::variant(Variant::pack(tag as u16, v)))
                    })
                    .boxed()
            }
        }
    }

    pub fn layout_strategy() -> impl Strategy<Value = MoveTypeLayout> {
        use MoveTypeLayout as L;

        let leaf = prop_oneof![
            1 => Just(L::U8),
            1 => Just(L::U16),
            1 => Just(L::U32),
            1 => Just(L::U64),
            1 => Just(L::U128),
            1 => Just(L::U256),
            1 => Just(L::Bool),
            1 => Just(L::Address),
            1 => Just(L::Signer),
        ];

        leaf.prop_recursive(8, 32, 2, |inner| {
            prop_oneof![
                1 => inner.clone().prop_map(|layout| L::Vector(Box::new(layout))),
                1 => vec(inner, 0..1).prop_map(|f_layouts| {
                     L::Struct(Box::new(MoveStructLayout::new(f_layouts)))}),
            ]
        })
    }

    pub fn layout_and_value_strategy() -> impl Strategy<Value = (MoveTypeLayout, Value)> {
        layout_strategy().no_shrink().prop_flat_map(|layout| {
            let value_strategy = value_strategy_with_layout(layout.clone());
            (Just(layout), value_strategy)
        })
    }
}

use move_core_types::runtime_value::{MoveStruct, MoveValue, MoveVariant};

impl ValueImpl {
    pub fn as_move_value(&self, layout: &MoveTypeLayout) -> MoveValue {
        use MoveTypeLayout as L;

        match (layout, &self) {
            (L::U8, ValueImpl::U8(x)) => MoveValue::U8(*x),
            (L::U16, ValueImpl::U16(x)) => MoveValue::U16(*x),
            (L::U32, ValueImpl::U32(x)) => MoveValue::U32(*x),
            (L::U64, ValueImpl::U64(x)) => MoveValue::U64(*x),
            (L::U128, ValueImpl::U128(x)) => MoveValue::U128(**x),
            (L::U256, ValueImpl::U256(x)) => MoveValue::U256(**x),
            (L::Bool, ValueImpl::Bool(x)) => MoveValue::Bool(*x),
            (L::Address, ValueImpl::Address(x)) => MoveValue::Address(**x),

            (L::Enum(enum_layout), ValueImpl::Container(Container::Variant(r))) => {
                let MoveEnumLayout(variants) = &**enum_layout;
                let (tag, values) = &*r.borrow();
                let tag = *tag;
                let field_layouts = &variants.as_slice()[tag as usize];
                let mut fields = vec![];
                for (v, field_layout) in values.iter().zip(field_layouts) {
                    fields.push(v.as_move_value(field_layout));
                }
                MoveValue::Variant(MoveVariant { tag, fields })
            }

            (L::Struct(struct_layout), ValueImpl::Container(Container::Struct(r))) => {
                let mut fields = vec![];
                for (v, field_layout) in r.borrow().iter().zip(struct_layout.fields().iter()) {
                    fields.push(v.as_move_value(field_layout));
                }
                MoveValue::Struct(MoveStruct::new(fields))
            }

            (L::Vector(inner_layout), ValueImpl::Container(c)) => MoveValue::Vector(match c {
                Container::VecU8(r) => r.borrow().iter().map(|u| MoveValue::U8(*u)).collect(),
                Container::VecU16(r) => r.borrow().iter().map(|u| MoveValue::U16(*u)).collect(),
                Container::VecU32(r) => r.borrow().iter().map(|u| MoveValue::U32(*u)).collect(),
                Container::VecU64(r) => r.borrow().iter().map(|u| MoveValue::U64(*u)).collect(),
                Container::VecU128(r) => r.borrow().iter().map(|u| MoveValue::U128(*u)).collect(),
                Container::VecU256(r) => r.borrow().iter().map(|u| MoveValue::U256(*u)).collect(),
                Container::VecBool(r) => r.borrow().iter().map(|u| MoveValue::Bool(*u)).collect(),
                Container::VecAddress(r) => {
                    r.borrow().iter().map(|u| MoveValue::Address(*u)).collect()
                }
                Container::Vec(r) => r
                    .borrow()
                    .iter()
                    .map(|v| v.as_move_value(inner_layout.as_ref()))
                    .collect(),
                Container::Struct(_) => {
                    panic!("got struct container when converting vec")
                }
                Container::Variant { .. } => {
                    panic!("got variant container when converting vec")
                }
                Container::Locals(_) => panic!("got locals container when converting vec"),
            }),

            (L::Signer, ValueImpl::Container(Container::Struct(r))) => {
                let v = r.borrow();
                if v.len() != 1 {
                    panic!("Unexpected signer layout: {:?}", v);
                }
                match &v[0] {
                    ValueImpl::Address(a) => MoveValue::Signer(**a),
                    v => panic!("Unexpected non-address while converting signer: {:?}", v),
                }
            }

            (layout, val) => panic!("Cannot convert value {:?} as {:?}", val, layout),
        }
    }
}

impl Value {
    pub fn as_move_value(&self, layout: &MoveTypeLayout) -> MoveValue {
        self.0.as_move_value(layout)
    }

    pub fn as_annotated_move_value_for_tracing_only(
        &self,
        layout: &A::MoveTypeLayout,
    ) -> Option<A::MoveValue> {
        self.0.as_annotated_move_value(layout)
    }
}

impl ValueImpl {
    /// Converts the value to an annotated move value. This is only needed for tracing and care
    /// should be taken when using this function as it can possibly inflate the size of the value.
    fn as_annotated_move_value(&self, layout: &A::MoveTypeLayout) -> Option<A::MoveValue> {
        use move_core_types::annotated_value::MoveTypeLayout as L;
        use move_core_types::annotated_value::MoveValue;
        Some(match (layout, self) {
            (L::U8, ValueImpl::U8(x)) => MoveValue::U8(*x),
            (L::U16, ValueImpl::U16(x)) => MoveValue::U16(*x),
            (L::U32, ValueImpl::U32(x)) => MoveValue::U32(*x),
            (L::U64, ValueImpl::U64(x)) => MoveValue::U64(*x),
            (L::U128, ValueImpl::U128(x)) => MoveValue::U128(**x),
            (L::U256, ValueImpl::U256(x)) => MoveValue::U256(**x),
            (L::Bool, ValueImpl::Bool(x)) => MoveValue::Bool(*x),
            (L::Address, ValueImpl::Address(x)) => MoveValue::Address(**x),
            (l, ValueImpl::Container(c)) => return c.as_annotated_move_value(l),
            (_, ValueImpl::ContainerRef(container_ref)) => {
                let (ContainerRef::Local(c) | ContainerRef::Global { container: c, .. }) =
                    &**container_ref;
                return c.as_annotated_move_value(layout);
            }
            (_, ValueImpl::IndexedRef(index_ref)) => {
                let IndexedRef {
                    container_ref:
                        ContainerRef::Local(c) | ContainerRef::Global { container: c, .. },
                    idx,
                } = &**index_ref;
                use Container::*;
                let idx = *idx;
                let res = match c {
                    Locals(r) | Vec(r) | Struct(r) => r.borrow()[idx].copy_value().unwrap(),
                    Variant(r) => r.borrow().1[idx].copy_value().unwrap(),
                    VecU8(r) => ValueImpl::U8(r.borrow()[idx]),
                    VecU16(r) => ValueImpl::U16(r.borrow()[idx]),
                    VecU32(r) => ValueImpl::U32(r.borrow()[idx]),
                    VecU64(r) => ValueImpl::U64(r.borrow()[idx]),
                    VecU128(r) => ValueImpl::U128(Box::new(r.borrow()[idx])),
                    VecU256(r) => ValueImpl::U256(Box::new(r.borrow()[idx])),
                    VecBool(r) => ValueImpl::Bool(r.borrow()[idx]),
                    VecAddress(r) => ValueImpl::Address(Box::new(r.borrow()[idx])),
                };
                return res.as_annotated_move_value(layout);
            }
            (_layout, _val) => return None,
        })
    }
}

impl Container {
    fn as_annotated_move_value(&self, layout: &A::MoveTypeLayout) -> Option<A::MoveValue> {
        use move_core_types::annotated_value::MoveTypeLayout as L;
        Some(match (layout, self) {
            (L::Enum(e_layout), Container::Variant(r)) => {
                let A::MoveEnumLayout { type_, variants } = e_layout.as_ref();
                let (tag, values) = &*r.borrow();
                let tag = *tag;
                let ((name, _), field_layouts) =
                    variants.iter().find(|((_, t), _)| *t == tag).unwrap();
                let mut fields = vec![];
                for (v, field_layout) in values.iter().zip(field_layouts) {
                    fields.push((
                        field_layout.name.clone(),
                        v.as_annotated_move_value(&field_layout.layout)?,
                    ));
                }
                A::MoveValue::Variant(A::MoveVariant {
                    tag,
                    fields,
                    type_: type_.clone(),
                    variant_name: name.clone(),
                })
            }
            (L::Struct(struct_layout), Container::Struct(r)) => {
                let mut fields = vec![];
                for (v, field_layout) in r.borrow().iter().zip(struct_layout.fields.iter()) {
                    fields.push((
                        field_layout.name.clone(),
                        v.as_annotated_move_value(&field_layout.layout)?,
                    ));
                }
                A::MoveValue::Struct(A::MoveStruct::new(struct_layout.type_.clone(), fields))
            }

            (L::Vector(inner_layout), c) => A::MoveValue::Vector(match c {
                Container::VecU8(r) => r.borrow().iter().map(|u| A::MoveValue::U8(*u)).collect(),
                Container::VecU16(r) => r.borrow().iter().map(|u| A::MoveValue::U16(*u)).collect(),
                Container::VecU32(r) => r.borrow().iter().map(|u| A::MoveValue::U32(*u)).collect(),
                Container::VecU64(r) => r.borrow().iter().map(|u| A::MoveValue::U64(*u)).collect(),
                Container::VecU128(r) => {
                    r.borrow().iter().map(|u| A::MoveValue::U128(*u)).collect()
                }
                Container::VecU256(r) => {
                    r.borrow().iter().map(|u| A::MoveValue::U256(*u)).collect()
                }
                Container::VecBool(r) => {
                    r.borrow().iter().map(|u| A::MoveValue::Bool(*u)).collect()
                }
                Container::VecAddress(r) => r
                    .borrow()
                    .iter()
                    .map(|u| A::MoveValue::Address(*u))
                    .collect(),
                Container::Vec(r) => r
                    .borrow()
                    .iter()
                    .map(|v| v.as_annotated_move_value(inner_layout))
                    .collect::<Option<_>>()?,
                Container::Struct(_) | Container::Variant { .. } | Container::Locals(_) => {
                    return None;
                }
            }),

            (L::Signer, Container::Struct(r)) => {
                let v = r.borrow();
                if v.len() != 1 {
                    return None;
                }
                match &v[0] {
                    ValueImpl::Address(a) => A::MoveValue::Signer(**a),
                    _ => return None,
                }
            }
            (_layout, _val) => return None,
        })
    }
}

#[test]
fn assert_sizes() {
    assert_eq!(size_of::<ValueImpl>(), 16);
}
