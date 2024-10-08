//! # Object model
//!
//!
//! Describes object representation to MMTk. We provide enough freedom to have almost any kind of objects: arrays, strings,
//! butterflies, etc.

use std::marker::PhantomData;

use crate::mm::slot::SlotExt;
use crate::{MMTKVMKit, Runtime, VTableOf};
use constants::{OBJECT_HASH_OFFSET, OBJECT_HASH_SIZE, OBJECT_REF_OFFSET};

use header::{HashState, HeapObjectHeader};
use mmtk::{
    util::{
        alloc::fill_alignment_gap,
        conversions::raw_align_up,
        copy::{CopySemantics, GCWorkerCopyContext},
        Address, ObjectReference,
    },
    vm::{
        slot::SimpleSlot, VMGlobalLogBitSpec, VMLocalForwardingBitsSpec,
        VMLocalForwardingPointerSpec, VMLocalLOSMarkNurserySpec, VMLocalMarkBitSpec,
    },
};
use vtable::*;

pub mod constants;
pub mod ephemeron;
pub mod header;
pub mod mark_word;
pub mod nanbox;
pub mod reference;
pub mod traits;
pub mod vtable;

pub struct ObjectModel<R: Runtime>(PhantomData<R>);

/// Used as a parameter of `move_object` to specify where to move an object to.
enum MoveTarget {
    /// Move an object to the address returned from `alloc_copy`.
    ToAddress(Address),
    /// Move an object to an `ObjectReference` pointing to an object previously computed from
    /// `get_reference_when_copied_to`.
    ToObject(ObjectReference),
}

impl<R: Runtime> ObjectModel<R> {
    pub(crate) fn get_alignment(object: ObjectReference) -> usize {
        let header = <&HeapObjectHeader<R>>::from(object);

        VTableOf::<R>::from_pointer(header.vtable())
            .gc()
            .alignment
            .get()
    }

    pub(crate) fn get_offset_for_alignment(object: ObjectReference) -> usize {
        let header = <&HeapObjectHeader<R>>::from(object);

        let hash_state = header.hash_state();

        size_of::<HeapObjectHeader<R>>()
            + (hash_state != HashState::Unhashed)
                .then_some(OBJECT_HASH_SIZE)
                .unwrap_or(0)
    }

    pub fn bytes_used(object: ObjectReference) -> usize {
        let header = <&HeapObjectHeader<R>>::from(object);
        let vt = VTableOf::<R>::from_pointer(header.vtable()).gc();
        let mut size = vt.size();

        if size == 0 {
            let compute_size = vt.compute_size.expect("Must be available");

            size += compute_size(object).get();
        }

        if header.hash_state() == HashState::HashedAndMoved {
            size += OBJECT_HASH_SIZE;
        }

        raw_align_up(size, size_of::<usize>())
    }

    pub fn bytes_required_when_copied(object: ObjectReference) -> usize {
        let header = <&HeapObjectHeader<R>>::from(object);
        let vt = VTableOf::<R>::from_pointer(header.vtable()).gc();
        let mut size = vt.size();

        if size == 0 {
            let compute_size = vt.compute_size.expect("Must be available");

            size += compute_size(object).get();
        }

        if header.hash_state() != HashState::Unhashed {
            size += OBJECT_HASH_SIZE;
        }

        raw_align_up(size, size_of::<usize>())
    }

    fn move_object(
        from_obj: ObjectReference,
        mut to: MoveTarget,
        num_bytes: usize,
    ) -> ObjectReference {
        let copy_bytes = num_bytes;

        let obj_ref_offset = OBJECT_REF_OFFSET as isize;

        /*if hash_state == HashState::Hashed {
            copy_bytes -= OBJECT_HASH_SIZE;

            if let MoveTarget::ToAddress(ref mut addr) = to {
                *addr += OBJECT_HASH_SIZE;
            }
        } else if hash_state == HashState::HashedAndMoved {
            obj_ref_offset += OBJECT_HASH_SIZE as isize;
        }*/

        let (to_address, to_obj) = match to {
            MoveTarget::ToAddress(addr) => {
                let obj =
                    unsafe { ObjectReference::from_raw_address_unchecked(addr + obj_ref_offset) };

                debug_assert!(obj.to_raw_address() == addr + obj_ref_offset);
                (addr, obj)
            }

            MoveTarget::ToObject(obj) => {
                let addr = obj.to_raw_address() + (-obj_ref_offset);
                debug_assert!(obj.to_raw_address() == addr + obj_ref_offset);
                (addr, obj)
            }
        };

        let from_address = from_obj.to_raw_address() + (-obj_ref_offset);
        if let MoveTarget::ToAddress(ref mut addr) = to {
            *addr += OBJECT_HASH_SIZE;
        }
        println!(
            "move_object: {}->{} ({} bytes)",
            from_address, to_address, copy_bytes
        );
        unsafe {
            std::ptr::copy_nonoverlapping(
                from_address.to_ptr::<u8>(),
                to_address.to_mut_ptr::<u8>(),
                copy_bytes,
            );
        }

        to_obj
    }

    fn copy_scalar(
        from: ObjectReference,
        copy: CopySemantics,
        copy_context: &mut GCWorkerCopyContext<MMTKVMKit<R>>,
    ) -> ObjectReference {
        let bytes = Self::bytes_required_when_copied(from);
        let align = Self::get_alignment(from);
        let offset = Self::get_offset_for_alignment(from);
        let region = copy_context.alloc_copy(from, bytes, align, offset, copy);

        let to_obj = Self::move_object(from, MoveTarget::ToAddress(region), bytes);

        copy_context.post_copy(to_obj, bytes, copy);
        to_obj
    }

    fn object_start_ref(object: ObjectReference) -> Address {
        let header = <&HeapObjectHeader<R>>::from(object);

        let hash_state = header.hash_state();

        if hash_state == HashState::HashedAndMoved {
            return object.to_raw_address()
                + (-(OBJECT_REF_OFFSET as isize + OBJECT_HASH_SIZE as isize));
        }

        object.to_raw_address() + (-(OBJECT_REF_OFFSET as isize))
    }
}

const LOGGING_SIDE_METADATA_SPEC: VMGlobalLogBitSpec = VMGlobalLogBitSpec::side_first();
const FORWARDING_POINTER_METADATA_SPEC: VMLocalForwardingPointerSpec =
    VMLocalForwardingPointerSpec::in_header(0);
const MARKING_METADATA_SPEC: VMLocalMarkBitSpec =
    VMLocalMarkBitSpec::side_after(LOS_METADATA_SPEC.as_spec());
const FORWARDING_BITS_METADATA_SPEC: VMLocalForwardingBitsSpec =
    VMLocalForwardingBitsSpec::in_header(62);
const LOS_METADATA_SPEC: VMLocalLOSMarkNurserySpec = VMLocalLOSMarkNurserySpec::side_first();

impl<R: Runtime> mmtk::vm::ObjectModel<MMTKVMKit<R>> for ObjectModel<R> {
    const OBJECT_REF_OFFSET_LOWER_BOUND: isize = OBJECT_HASH_OFFSET;
    const UNIFIED_OBJECT_REFERENCE_ADDRESS: bool = false;
    const LOCAL_MARK_BIT_SPEC: VMLocalMarkBitSpec = MARKING_METADATA_SPEC;
    const GLOBAL_LOG_BIT_SPEC: VMGlobalLogBitSpec = LOGGING_SIDE_METADATA_SPEC;
    const LOCAL_FORWARDING_BITS_SPEC: VMLocalForwardingBitsSpec = FORWARDING_BITS_METADATA_SPEC;
    const LOCAL_FORWARDING_POINTER_SPEC: VMLocalForwardingPointerSpec =
        FORWARDING_POINTER_METADATA_SPEC;
    const LOCAL_LOS_MARK_NURSERY_SPEC: VMLocalLOSMarkNurserySpec = LOS_METADATA_SPEC;
    #[cfg(feature = "vo-bit")]
    const NEED_VO_BITS_DURING_TRACING: bool = R::VO_BIT;

    fn ref_to_header(object: ObjectReference) -> Address {
        object.to_raw_address() + (-(OBJECT_REF_OFFSET as isize))
    }

    fn ref_to_object_start(object: ObjectReference) -> Address {
        Self::object_start_ref(object)
    }

    fn get_align_offset_when_copied(object: ObjectReference) -> usize {
        ObjectModel::<R>::get_alignment(object)
    }

    fn get_align_when_copied(object: ObjectReference) -> usize {
        ObjectModel::<R>::get_alignment(object)
    }

    fn get_current_size(object: ObjectReference) -> usize {
        ObjectModel::<R>::bytes_used(object)
    }

    fn get_size_when_copied(object: ObjectReference) -> usize {
        ObjectModel::<R>::bytes_required_when_copied(object)
    }

    fn get_reference_when_copied_to(from: ObjectReference, to: Address) -> ObjectReference {
        let mut res = to;
        let hash_state = <&HeapObjectHeader<R>>::from(from).hash_state();
        if hash_state != HashState::Unhashed {
            res += OBJECT_HASH_SIZE;
        }

        unsafe { ObjectReference::from_raw_address_unchecked(res + OBJECT_REF_OFFSET) }
    }

    fn copy_to(from: ObjectReference, to: ObjectReference, region: Address) -> Address {
        let copy = from != to;

        let bytes = if copy {
            let bytes = Self::bytes_required_when_copied(from);
            Self::move_object(from, MoveTarget::ToObject(to), bytes);
            bytes
        } else {
            Self::bytes_used(from)
        };

        let start = Self::object_start_ref(to);

        fill_alignment_gap::<MMTKVMKit<R>>(region, start);

        start + bytes
    }

    fn copy(
        from: ObjectReference,
        semantics: mmtk::util::copy::CopySemantics,
        copy_context: &mut mmtk::util::copy::GCWorkerCopyContext<MMTKVMKit<R>>,
    ) -> ObjectReference {
        Self::copy_scalar(from, semantics, copy_context)
    }

    fn get_type_descriptor(_: ObjectReference) -> &'static [i8] {
        unreachable!()
    }

    fn dump_object(_object: ObjectReference) {}
}

impl<R: Runtime> SlotExt<R> for SimpleSlot {
    fn from_member<T, Tag>(member: &reference::BasicMember<T, Tag>) -> Self {
        SimpleSlot::from_address(Address::from_ptr(member))
    }

    fn from_pointer(pointer: *mut ObjectReference) -> Self {
        SimpleSlot::from_address(Address::from_ptr(pointer))
    }

    fn from_vtable_slot(_slot: crate::mm::slot::VTableSlot<R>) -> Self {
        unimplemented!("SimpleSlot does not support enqueing vtable slot")
    }
}
