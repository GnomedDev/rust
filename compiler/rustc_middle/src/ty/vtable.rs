use std::fmt;

use rustc_ast::Mutability;
use rustc_macros::HashStable;

use crate::mir::interpret::{alloc_range, AllocId, Allocation, Pointer, Scalar, CTFE_ALLOC_SALT};
use crate::ty::{self, Instance, PolyTraitRef, Ty, TyCtxt};

#[derive(Clone, Copy, PartialEq, HashStable)]
pub enum VtblEntry<'tcx> {
    /// destructor of this type (used in vtable header)
    MetadataDropInPlace,
    /// size and align of this type (used in vtable header)
    MetadataTyLayout,
    /// non-dispatchable associated function that is excluded from trait object
    Vacant,
    /// dispatchable associated function
    Method(Instance<'tcx>),
    /// pointer to a separate supertrait vtable, can be used by trait upcasting coercion
    TraitVPtr(PolyTraitRef<'tcx>),
}

impl<'tcx> fmt::Debug for VtblEntry<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // We want to call `Display` on `Instance` and `PolyTraitRef`,
        // so we implement this manually.
        match self {
            VtblEntry::MetadataDropInPlace => write!(f, "MetadataDropInPlace"),
            VtblEntry::MetadataTyLayout => write!(f, "MetadataTyLayout"),
            VtblEntry::Vacant => write!(f, "Vacant"),
            VtblEntry::Method(instance) => write!(f, "Method({instance})"),
            VtblEntry::TraitVPtr(trait_ref) => write!(f, "TraitVPtr({trait_ref})"),
        }
    }
}

// Needs to be associated with the `'tcx` lifetime
impl<'tcx> TyCtxt<'tcx> {
    pub const COMMON_VTABLE_ENTRIES: &'tcx [VtblEntry<'tcx>] =
        &[VtblEntry::MetadataDropInPlace, VtblEntry::MetadataTyLayout];
}

pub const VTABLE_DROPINPLACE_OFFSET: usize = 0;
pub const VTABLE_LAYOUT_OFFSET: usize = 1;

pub fn get_vtable_metadata_index<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_ref: Option<ty::PolyExistentialTraitRef<'tcx>>,
) -> usize {
    count_vtable_entries(tcx, trait_ref) - TyCtxt::COMMON_VTABLE_ENTRIES.len()
}

pub(crate) fn count_vtable_entries<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_ref: Option<ty::PolyExistentialTraitRef<'tcx>>,
) -> usize {
    match trait_ref {
        Some(trait_ref) => tcx.count_vtable_entries(trait_ref),
        None => TyCtxt::COMMON_VTABLE_ENTRIES.len(),
    }
}

/// Retrieves an allocation that represents the contents of a vtable.
/// Since this is a query, allocations are cached and not duplicated.
///
/// This is an "internal" `AllocId` that should never be used as a value in the interpreted program.
/// The interpreter should use `AllocId` that refer to a `GlobalAlloc::VTable` instead.
/// (This is similar to statics, which also have a similar "internal" `AllocId` storing their
/// initial contents.)
pub(super) fn vtable_allocation_provider<'tcx>(
    tcx: TyCtxt<'tcx>,
    key: (Ty<'tcx>, Option<ty::PolyExistentialTraitRef<'tcx>>),
) -> AllocId {
    let (ty, poly_trait_ref) = key;

    let vtable_entries = if let Some(poly_trait_ref) = poly_trait_ref {
        let trait_ref = poly_trait_ref.with_self_ty(tcx, ty);
        let trait_ref = tcx.erase_regions(trait_ref);

        tcx.vtable_entries(trait_ref)
    } else {
        TyCtxt::COMMON_VTABLE_ENTRIES
    };

    // This confirms that both the layout computation for &dyn Trait and
    // the offset computation for vtable metadata is correct.
    assert_eq!(vtable_entries.len(), count_vtable_entries(tcx, poly_trait_ref));

    let layout = tcx
        .layout_of(ty::ParamEnv::reveal_all().and(ty))
        .expect("failed to build vtable representation");
    assert!(layout.is_sized(), "can't create a vtable for an unsized type");
    let size = layout.size.bytes();
    let align = layout.align.abi.bytes();

    let ptr_size = tcx.data_layout.pointer_size;
    let ptr_align = tcx.data_layout.pointer_align.abi;

    let vtable_size = ptr_size * u64::try_from(vtable_entries.len()).unwrap();
    let mut vtable = Allocation::uninit(vtable_size, ptr_align);

    // No need to do any alignment checks on the memory accesses below, because we know the
    // allocation is correctly aligned as we created it above. Also we're only offsetting by
    // multiples of `ptr_align`, which means that it will stay aligned to `ptr_align`.

    for (idx, entry) in vtable_entries.iter().enumerate() {
        let idx: u64 = u64::try_from(idx).unwrap();
        let scalar = match entry {
            VtblEntry::MetadataDropInPlace => {
                if ty.needs_drop(tcx, ty::ParamEnv::reveal_all()) {
                    let instance = ty::Instance::resolve_drop_in_place(tcx, ty);
                    let fn_alloc_id = tcx.reserve_and_set_fn_alloc(instance, CTFE_ALLOC_SALT);
                    let fn_ptr = Pointer::from(fn_alloc_id);
                    Scalar::from_pointer(fn_ptr, &tcx)
                } else {
                    Scalar::from_maybe_pointer(Pointer::null(), &tcx)
                }
            }
            VtblEntry::MetadataTyLayout => {
                // Pack size and alignment into a single usize
                let layout = size << 1 | align;
                Scalar::from_uint(layout, ptr_size)
            }
            VtblEntry::Vacant => continue,
            VtblEntry::Method(instance) => {
                // Prepare the fn ptr we write into the vtable.
                let instance = instance.polymorphize(tcx);
                let fn_alloc_id = tcx.reserve_and_set_fn_alloc(instance, CTFE_ALLOC_SALT);
                let fn_ptr = Pointer::from(fn_alloc_id);
                Scalar::from_pointer(fn_ptr, &tcx)
            }
            VtblEntry::TraitVPtr(trait_ref) => {
                let super_trait_ref = trait_ref
                    .map_bound(|trait_ref| ty::ExistentialTraitRef::erase_self_ty(tcx, trait_ref));
                let supertrait_alloc_id = tcx.vtable_allocation((ty, Some(super_trait_ref)));
                let vptr = Pointer::from(supertrait_alloc_id);
                Scalar::from_pointer(vptr, &tcx)
            }
        };
        vtable
            .write_scalar(&tcx, alloc_range(ptr_size * idx, ptr_size), scalar)
            .expect("failed to build vtable representation");
    }

    vtable.mutability = Mutability::Not;
    tcx.reserve_and_set_memory_alloc(tcx.mk_const_alloc(vtable))
}
