// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_H_

#include <assert.h>
#include <lib/crypto/prng.h>
#include <lib/fit/function.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <stdint.h>
#include <zircon/types.h>

#include <fbl/canary.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/intrusive_wavl_tree.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <ffl/saturating_arithmetic.h>
#include <ktl/limits.h>
#include <ktl/optional.h>
#include <vm/vm_address_region_subtree_state.h>
#include <vm/vm_aspace.h>
#include <vm/vm_object.h>
#include <vm/vm_page_list.h>

// Creation flags for VmAddressRegion and VmMappings

// When randomly allocating subregions, reduce sprawl by placing allocations
// near each other.
#define VMAR_FLAG_COMPACT (1 << 0)
// Request that the new region be at the specified offset in its parent region.
#define VMAR_FLAG_SPECIFIC (1 << 1)
// Like VMAR_FLAG_SPECIFIC, but permits overwriting existing mappings.  This
// flag will not overwrite through a subregion.
#define VMAR_FLAG_SPECIFIC_OVERWRITE (1 << 2)
// Allow VmMappings to be created inside the new region with the SPECIFIC or
// OFFSET_IS_UPPER_LIMIT flag.
#define VMAR_FLAG_CAN_MAP_SPECIFIC (1 << 3)
// When on a VmAddressRegion, allow VmMappings to be created inside the region
// with read permissions.  When on a VmMapping, controls whether or not the
// mapping can gain this permission.
#define VMAR_FLAG_CAN_MAP_READ (1 << 4)
// When on a VmAddressRegion, allow VmMappings to be created inside the region
// with write permissions.  When on a VmMapping, controls whether or not the
// mapping can gain this permission.
#define VMAR_FLAG_CAN_MAP_WRITE (1 << 5)
// When on a VmAddressRegion, allow VmMappings to be created inside the region
// with execute permissions.  When on a VmMapping, controls whether or not the
// mapping can gain this permission.
#define VMAR_FLAG_CAN_MAP_EXECUTE (1 << 6)
// Require that VMO backing the mapping is non-resizable.
#define VMAR_FLAG_REQUIRE_NON_RESIZABLE (1 << 7)
// Allow VMO backings that could result in faults.
#define VMAR_FLAG_ALLOW_FAULTS (1 << 8)
// Treat the offset as an upper limit when allocating a VMO or child VMAR.
#define VMAR_FLAG_OFFSET_IS_UPPER_LIMIT (1 << 9)
// Opt this VMAR out of certain debugging checks. This allows for kernel mappings that have a more
// dynamic management strategy, that the regular checks would otherwise spuriously trip on.
#define VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING (1 << 10)
// Memory accesses past the stream size rounded up to the page boundary will fault.
#define VMAR_FLAG_FAULT_BEYOND_STREAM_SIZE (1 << 11)

#define VMAR_CAN_RWX_FLAGS \
  (VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE | VMAR_FLAG_CAN_MAP_EXECUTE)

enum class VmAddressRegionOpChildren : bool {
  Yes,
  No,
};

// forward declarations
class VmAddressRegion;
class VmMapping;
class VmEnumerator;
enum class VmAddressRegionEnumeratorType : bool;
template <VmAddressRegionEnumeratorType>
class VmAddressRegionEnumerator;

class MultiPageRequest;

// A VmAddressRegion represents a contiguous region of the virtual address
// space.  It is partitioned by non-overlapping children of the following types:
// 1) child VmAddressRegion
// 2) child VmMapping (leafs that map VmObjects into the address space)
// 3) gaps (logical, not actually objects).
//
// VmAddressRegionOrMapping represents a tagged union of the two types.
//
// A VmAddressRegion/VmMapping may be in one of two states: ALIVE or DEAD.  If
// it is ALIVE, then the VmAddressRegion is a description of the virtual memory
// mappings of the address range it represents in its parent VmAspace.  If it is
// DEAD, then the VmAddressRegion is invalid and has no meaning.
//
// All VmAddressRegion and VmMapping state is protected by the aspace lock.
class VmAddressRegionOrMapping
    : public fbl::WAVLTreeContainable<fbl::RefPtr<VmAddressRegionOrMapping>>,
      public fbl::RefCounted<VmAddressRegionOrMapping> {
 public:
  // If a VMO-mapping, unmap all pages and remove dependency on vm object it has a ref to.
  // Otherwise recursively destroy child VMARs and transition to the DEAD state.
  //
  // Returns ZX_OK on success, ZX_ERR_BAD_STATE if already dead, and other
  // values on error (typically unmap failure).
  virtual zx_status_t Destroy();

  // accessors
  vaddr_t base_locked() const TA_REQ(lock()) { return base_; }
  size_t size_locked() const TA_REQ(lock()) { return size_; }
  vaddr_t base_locking() const TA_EXCL(lock()) {
    Guard<CriticalMutex> guard{lock()};
    return base_;
  }
  size_t size_locking() const TA_EXCL(lock()) {
    Guard<CriticalMutex> guard{lock()};
    return size_;
  }
  uint32_t flags() const { return flags_; }
  const fbl::RefPtr<VmAspace>& aspace() const { return aspace_; }

  // Recursively compute the amount of attributed memory within this region
  using AttributionCounts = VmObject::AttributionCounts;
  virtual AttributionCounts GetAttributedMemory();

  // Subtype information and safe down-casting
  bool is_mapping() const { return is_mapping_; }
  fbl::RefPtr<VmAddressRegion> as_vm_address_region();
  fbl::RefPtr<VmMapping> as_vm_mapping();
  VmAddressRegion* as_vm_address_region_ptr();
  VmMapping* as_vm_mapping_ptr();
  static fbl::RefPtr<VmAddressRegion> downcast_as_vm_address_region(
      fbl::RefPtr<VmAddressRegionOrMapping>* region_or_map);
  static fbl::RefPtr<VmMapping> downcast_as_vm_mapping(
      fbl::RefPtr<VmAddressRegionOrMapping>* region_or_map);

  // WAVL tree key function
  // For use in WAVL tree code only.
  // base_ access is safe as WAVL tree is guarded by aspace lock.
  vaddr_t GetKey() const TA_NO_THREAD_SAFETY_ANALYSIS { return base_; }

  // Dump debug info
  virtual void DumpLocked(uint depth, bool verbose) const TA_REQ(lock()) = 0;

  // Expose our backing lock for annotation purposes.
  Lock<CriticalMutex>* lock() const TA_RET_CAP(aspace_->lock()) { return aspace_->lock(); }
  Lock<CriticalMutex>& lock_ref() const TA_RET_CAP(aspace_->lock()) { return aspace_->lock_ref(); }

  bool is_in_range_locked(vaddr_t base, size_t size) const TA_REQ(lock()) {
    const size_t offset = base - base_;
    return base >= base_ && offset < size_ && size_ - offset >= size;
  }

  // Memory priorities that can be applied to VMARs and mappings to propagate to VMOs and page
  // tables.
  enum class MemoryPriority : bool {
    // Default overcommit priority where reclamation is allowed.
    DEFAULT,
    // High priority prevents all reclamation.
    HIGH,
  };

  // Subtree state for augmented binary search tree operations.
  VmAddressRegionSubtreeState& subtree_state_locked() TA_REQ(lock()) { return subtree_state_; }
  const VmAddressRegionSubtreeState& subtree_state_locked() const TA_REQ(lock()) {
    return subtree_state_;
  }

 private:
  fbl::Canary<fbl::magic("VMRM")> canary_;
  VmAddressRegionSubtreeState subtree_state_ TA_GUARDED(lock());
  const bool is_mapping_;

 protected:
  // friend VmAddressRegion so it can access DestroyLocked
  friend VmAddressRegion;
  template <VmAddressRegionEnumeratorType>
  friend class VmAddressRegionEnumerator;

  // destructor, should only be invoked from RefPtr
  virtual ~VmAddressRegionOrMapping();
  friend fbl::RefPtr<VmAddressRegionOrMapping>;

  bool in_subregion_tree() const {
    return fbl::WAVLTreeContainable<fbl::RefPtr<VmAddressRegionOrMapping>>::InContainer();
  }

  enum class LifeCycleState : uint8_t {
    // Initial state: if NOT_READY, then do not invoke Destroy() in the
    // destructor
    NOT_READY,
    // Usual state: information is representative of the address space layout
    ALIVE,
    // Object is invalid
    DEAD
  };

  VmAddressRegionOrMapping(vaddr_t base, size_t size, uint32_t flags, VmAspace* aspace,
                           VmAddressRegion* parent, bool is_mapping);

  // Check if the given *arch_mmu_flags* are allowed under this
  // regions *flags_*
  bool is_valid_mapping_flags(uint arch_mmu_flags) {
    // Work out what flags we must support for these arch_mmu_flags
    uint32_t needed = 0;
    if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_READ) {
      needed |= VMAR_FLAG_CAN_MAP_READ;
    }
    if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) {
      needed |= VMAR_FLAG_CAN_MAP_WRITE;
    }
    if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) {
      needed |= VMAR_FLAG_CAN_MAP_EXECUTE;
    }
    // Mask out the actual relevant mappings flags we have.
    const uint32_t actual =
        flags_ & (VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE | VMAR_FLAG_CAN_MAP_EXECUTE);
    // Validate that every |needed| occurs in |actual|
    return (needed & actual) == needed;
  }

  // Returns true if the instance is alive and reporting information that
  // reflects the address space layout. |aspace()->lock()| must be held.
  bool IsAliveLocked() const TA_REQ(lock()) {
    canary_.Assert();
    return state_ == LifeCycleState::ALIVE;
  }

  virtual zx_status_t DestroyLocked() TA_REQ(lock()) = 0;

  virtual AttributionCounts GetAttributedMemoryLocked() TA_REQ(lock()) = 0;

  // Applies the given memory priority to this VMAR, which may or may not result in a change. Up to
  // the derived type to know how to apply and update the |memory_priority_| field.
  virtual zx_status_t SetMemoryPriorityLocked(MemoryPriority priority) TA_REQ(lock()) = 0;

  // Performs any actions necessary to apply a high memory priority over the given range.
  // This method is always safe to call as it will internally check the memory priority status and
  // skip if necessary, so the caller does not need to worry about races with a different memory
  // priority being applied.
  // As this may need to acquire the lock even to check the memory priority, if the caller knows
  // they have not caused this to become high priority (i.e. they have called
  // SetMemoryPriorityLocked with MemoryPriority::DEFAULT), then calling this should be skipped for
  // performance.
  // Memory that needs to be committed for a high memory priority are user pager backed pages and
  // any compressed or loaned pages. Anonymous pages and copy-on-write pages do not allocated /
  // committed.
  // This method has no return value as it is entirely best effort and no part of its operation is
  // needed for correctness.
  virtual void CommitHighMemoryPriority() TA_EXCL(lock()) = 0;

  // Transition from NOT_READY to READY, and add references to self to related
  // structures.
  virtual void Activate() TA_REQ(lock()) = 0;

  // current state of the VMAR.  If LifeCycleState::DEAD, then all other
  // fields are invalid.
  LifeCycleState state_ TA_GUARDED(lock()) = LifeCycleState::ALIVE;

  // Priority of the VMAR. This starts at DEFAULT and must be reset back to default as part of the
  // destroy path to ensure any propagation is undone correctly.
  MemoryPriority memory_priority_ TA_GUARDED(lock()) = MemoryPriority::DEFAULT;

  // flags from VMAR creation time
  const uint32_t flags_;

  // address/size within the container address space
  vaddr_t base_ TA_GUARDED(lock());
  size_t size_ TA_GUARDED(lock());

  // pointer back to our member address space.  The aspace's lock is used
  // to serialize all modifications.
  const fbl::RefPtr<VmAspace> aspace_;

  // pointer back to our parent region (nullptr if root or destroyed)
  VmAddressRegion* parent_ TA_GUARDED(lock());
};

// A list of regions ordered by virtual address. Templated to allow for test code to avoid needing
// to instantiate 'real' VmAddressRegionOrMapping instances.
template <typename T = VmAddressRegionOrMapping>
class RegionList final {
 public:
  using KeyType = vaddr_t;
  using PtrType = fbl::RefPtr<T>;
  using KeyTraits =
      fbl::DefaultKeyedObjectTraits<vaddr_t,
                                    typename fbl::internal::ContainerPtrTraits<PtrType>::ValueType>;
  using TagType = fbl::DefaultObjectTag;
  using NodeTraits = fbl::DefaultWAVLTreeTraits<PtrType, TagType>;
  using Observer = VmAddressRegionSubtreeState::Observer<T>;
  using ChildList = fbl::WAVLTree<KeyType, PtrType, KeyTraits, TagType, NodeTraits, Observer>;

  // Remove *region* from the list, returns the removed region.
  fbl::RefPtr<T> RemoveRegion(T* region) { return regions_.erase(*region); }

  // Request the region to the left or right of the given region.
  typename ChildList::iterator LeftOf(T* region) { return --regions_.make_iterator(*region); }
  typename ChildList::iterator RightOf(T* region) { return ++regions_.make_iterator(*region); }
  typename ChildList::const_iterator Root() const { return regions_.root(); }

  // Insert *region* to the region list.
  void InsertRegion(fbl::RefPtr<T> region) { regions_.insert(region); }

  // Use a static template to allow for returning a const and non-const pointer depending on the
  // constness of self.
  template <typename S, typename R>
  static R* FindRegion(S self, vaddr_t addr) {
    // Find the first region with a base greater than *addr*.  If a region
    // exists for *addr*, it will be immediately before it.
    auto itr = --self->regions_.upper_bound(addr);
    if (!itr.IsValid()) {
      return nullptr;
    }
    // Subregion size should never be zero unless during unmapping which should never overlap with
    // this operation.
    AssertHeld(itr->lock_ref());
    DEBUG_ASSERT(itr->size_locked() > 0);
    vaddr_t region_end;
    bool overflowed = add_overflow(itr->base_locked(), itr->size_locked() - 1, &region_end);
    ASSERT(!overflowed);
    if (itr->base_locked() > addr || addr > region_end) {
      return nullptr;
    }

    return &*itr;
  }

  // Find the region that covers addr, returns nullptr if not found.
  const T* FindRegion(vaddr_t addr) const {
    return FindRegion<const RegionList<T>*, T>(this, addr);
  }
  T* FindRegion(vaddr_t addr) { return FindRegion<RegionList<T>*, T>(this, addr); }

  // Find the region that contains |base|, or if that doesn't exist, the first region that contains
  // an address greater than |base|.
  typename ChildList::iterator IncludeOrHigher(vaddr_t base) {
    // Find the first region with a base greater than *base*.  If a region
    // exists for *base*, it will be immediately before it.
    auto itr = regions_.upper_bound(base);
    itr--;
    if (!itr.IsValid()) {
      itr = regions_.begin();
    } else {
      AssertHeld(itr->lock_ref());
      if (base >= itr->base_locked() && base - itr->base_locked() >= itr->size_locked()) {
        // If *base* isn't in this region, ignore it.
        ++itr;
      }
    }
    return itr;
  }

  typename ChildList::iterator UpperBound(vaddr_t base) { return regions_.upper_bound(base); }

  // Check whether it would be valid to create a child in the range [base, base+size).
  bool IsRangeAvailable(vaddr_t base, size_t size) const {
    DEBUG_ASSERT(size > 0);

    // Find the first region with base > *base*.  Since subregions_ has no
    // overlapping elements, we just need to check this one and the prior
    // child.

    auto prev = regions_.upper_bound(base);
    auto next = prev--;

    if (prev.IsValid()) {
      vaddr_t prev_last_byte;
      AssertHeld(prev->lock_ref());
      if (add_overflow(prev->base_locked(), prev->size_locked() - 1, &prev_last_byte)) {
        return false;
      }
      if (prev_last_byte >= base) {
        return false;
      }
    }

    if (next.IsValid() && next != regions_.end()) {
      vaddr_t last_byte;
      if (add_overflow(base, size - 1, &last_byte)) {
        return false;
      }
      AssertHeld(next->lock_ref());
      if (next->base_locked() <= last_byte) {
        return false;
      }
    }
    return true;
  }

  // Returns the base address of an available spot in the address range that satisfies the given
  // entropy, alignment, size, and upper limit requirements. If no spot is found that satisfies the
  // given entropy (i.e. target_index), the number of candidate spots encountered is returned.
  //
  // See vm/vm_address_region_subtree_state.h for an explanation of the augmented state used by this
  // method to perform efficient tree traversal.
  struct FindSpotAtIndexFailed {
    size_t candidate_spot_count;
  };
  fit::result<FindSpotAtIndexFailed, vaddr_t> FindSpotAtIndex(vaddr_t target_index,
                                                              uint8_t align_pow2, size_t size,
                                                              vaddr_t parent_base,
                                                              size_t parent_size,
                                                              vaddr_t upper_limit) const {
    // Returns the number of addresses that satisfy the size and alignment in the given range,
    // accounting for ranges that overlap the upper limit.
    const auto spots_in_range = [align_pow2, size, upper_limit](vaddr_t aligned_base,
                                                                size_t aligned_size) -> size_t {
      DEBUG_ASSERT(aligned_base < upper_limit);

      const size_t range_limit = ffl::SaturateAddAs<size_t>(aligned_base, aligned_size);
      const size_t clamped_range_size =
          range_limit < upper_limit ? aligned_size : aligned_size - (range_limit - upper_limit);

      if (clamped_range_size >= size) {
        return ((clamped_range_size - size) >> align_pow2) + 1;
      }
      return 0;
    };

    // Returns the given range with the base aligned and the size adjusted to maintain the same end
    // address. If the aligned base address is greater than the end address, the returned size is
    // zero.
    struct AlignedRange {
      vaddr_t base;
      size_t size;
    };
    const auto align_range = [align_pow2](vaddr_t range_base, size_t range_size) -> AlignedRange {
      const vaddr_t aligned_base = ALIGN(range_base, 1UL << align_pow2);
      const size_t base_delta = aligned_base - range_base;
      const size_t aligned_size = ffl::SaturateSubtractAs<size_t>(range_size, base_delta);
      return {.base = aligned_base, .size = aligned_size};
    };

    // Track the number of candidate spots encountered.
    size_t candidate_spot_count = 0;

    // See if there is a suitable gap between the start of the parent region and the first
    // subregion, or within the range of the parent region if there are no subregions.
    {
      const size_t gap_size =
          regions_.is_empty() ? parent_size : Observer::MinFirstByte(regions_.root()) - parent_base;
      const AlignedRange aligned_gap = align_range(parent_base, gap_size);
      if (aligned_gap.base >= upper_limit) {
        return fit::error(FindSpotAtIndexFailed{candidate_spot_count});
      }
      const size_t spot_count = spots_in_range(aligned_gap.base, aligned_gap.size);
      candidate_spot_count += spot_count;
      if (target_index < spot_count) {
        return fit::ok(aligned_gap.base + (target_index << align_pow2));
      }
      target_index -= spot_count;
    }

    // Traverse the tree to the leftmost gap that satisfies the required entropy, alignment, size,
    // and upper limit, skipping over gaps that are too small to consider. Keep track of the highest
    // address already visited to prune paths during traversal.
    vaddr_t already_visited = 0;
    auto node = regions_.root();
    while (node) {
      // Consider this node if there is a suitable gap in the left or right subtrees, including the
      // gaps between this node and its subtrees.
      if (Observer::MaxGap(node) >= size) {
        // First consider the left subtree, considering earlier addresses first to maximize page
        // table compactness. When entropy is zero (i.e. target_index is 0) this results in a first
        // fit search.
        if (auto left = node.left(); left) {
          //  Descend to the left subtree if it has a sufficient gap and its range has not been
          //  visited.
          if (Observer::MaxGap(left) >= size && Observer::MaxLastByte(left) > already_visited) {
            node = left;
            continue;
          }

          // The left subtree doesn't contain a sufficent gap. See if the gap between the current
          // node and the end of the left subtree is sufficient.
          const vaddr_t gap_base = Observer::MaxLastByte(left) + 1;
          const size_t gap_size =
              Observer::Gap(Observer::MaxLastByte(left), Observer::FirstByte(node));
          const AlignedRange aligned_gap = align_range(gap_base, gap_size);
          if (aligned_gap.base >= upper_limit) {
            return fit::error(FindSpotAtIndexFailed{candidate_spot_count});
          }
          const size_t spot_count = spots_in_range(aligned_gap.base, aligned_gap.size);
          candidate_spot_count += spot_count;
          if (target_index < spot_count) {
            return fit::ok(aligned_gap.base + (target_index << align_pow2));
          }
          target_index -= spot_count;
        }

        // If a sufficient gap is not found in the left subtree, consider the right subtree.
        if (auto right = node.right(); right) {
          // See if the gap between the current node and the start of the right subtree is
          // sufficient.
          const vaddr_t gap_base = Observer::LastByte(node) + 1;
          const size_t gap_size =
              Observer::Gap(Observer::LastByte(node), Observer::MinFirstByte(right));
          const AlignedRange aligned_gap = align_range(gap_base, gap_size);
          if (aligned_gap.base >= upper_limit) {
            return fit::error(FindSpotAtIndexFailed{candidate_spot_count});
          }
          const size_t spot_count = spots_in_range(aligned_gap.base, aligned_gap.size);
          candidate_spot_count += spot_count;
          if (target_index < spot_count) {
            return fit::ok(aligned_gap.base + (target_index << align_pow2));
          }
          target_index -= spot_count;

          // The gap with the current node is not sufficient. Descend to the right if it has a
          // sufficient gap and its range has not been visited.
          if (Observer::MaxGap(right) >= size && Observer::MaxLastByte(right) > already_visited) {
            node = right;
            continue;
          }
        }
      }

      // This subtree has been fully visited. Set the partition point to the end of this subtree and
      // ascend to the parent node to continue traversal. If this was the left child of the parent,
      // only the right child will be considered. If this was the right child, visiting the parent
      // is done and will proceed to its parent and so forth. If this node was the root, the
      // traversal is complete and a spot at the target index was not found.
      already_visited = Observer::MaxLastByte(node);
      node = node.parent();
    }

    // See if there is a suitable gap between the end of the last subregion and the end of the
    // parent.
    if (auto root = regions_.root()) {
      const vaddr_t gap_base = ffl::SaturateAddAs<vaddr_t>(Observer::MaxLastByte(root), 1);
      const size_t gap_size = parent_size - (gap_base - parent_base);
      const AlignedRange aligned_gap = align_range(gap_base, gap_size);
      if (aligned_gap.base >= upper_limit) {
        return fit::error(FindSpotAtIndexFailed{candidate_spot_count});
      }
      const size_t spot_count = spots_in_range(aligned_gap.base, aligned_gap.size);
      candidate_spot_count += spot_count;
      if (target_index < spot_count) {
        return fit::ok(aligned_gap.base + (target_index << align_pow2));
      }
      target_index -= spot_count;
    }

    return fit::error(FindSpotAtIndexFailed{candidate_spot_count});
  }

  // Get the allocation spot that is free and large enough for the aligned size.
  zx_status_t GetAllocSpot(vaddr_t* alloc_spot, uint8_t align_pow2, uint8_t entropy, size_t size,
                           vaddr_t parent_base, size_t parent_size, crypto::Prng* prng,
                           vaddr_t upper_limit = ktl::numeric_limits<vaddr_t>::max()) const {
    DEBUG_ASSERT(entropy < sizeof(size_t) * 8);

    // The number of addresses to consider based on the configured entropy.
    const size_t max_candidate_spaces = 1ul << entropy;

    // We first pick an index in [0, max_candidate_spaces] and hope to find a spot there. If the
    // number of available spots is less than the selected index, the attempt fails, returning the
    // actual number of candidate spots found, and we try again in this smaller range.
    //
    // This is mathematically equivalent to randomly picking a spot within [0, candidate_spot_count]
    // when selected_index <= candidate_spot_count.
    //
    // Prove as following:
    // Define M = candidate_spot_count
    // Define N = max_candidate_spaces (M < N, otherwise we can randomly allocate any spot from
    // [0, max_candidate_spaces], thus allocate a specific slot has (1 / N) probability).
    // Define slot X0 where X0 belongs to [1, M].
    // Define event A: randomly pick a slot X in [1, N], N = X0.
    // Define event B: randomly pick a slot X in [1, N], N belongs to [1, M].
    // Define event C: randomly pick a slot X in [1, N], N = X0 when N belongs to [1, M].
    // P(C) = P(A | B)
    // Since when A happens, B definitely happens, so P(AB) = P(A)
    // P(C) = P(A) / P(B) = (1 / N) / (M / N) = (1 / M)
    // which is equal to the probability of picking a specific spot in [0, M].
    vaddr_t selected_index = prng != nullptr ? prng->RandInt(max_candidate_spaces) : 0;

    fit::result allocation_result =
        FindSpotAtIndex(selected_index, align_pow2, size, parent_base, parent_size, upper_limit);
    if (allocation_result.is_error()) {
      const size_t candidate_spot_count = allocation_result.error_value().candidate_spot_count;
      if (candidate_spot_count == 0) {
        return ZX_ERR_NO_RESOURCES;
      }

      // If the number of available spaces is smaller than the selected index, pick again from the
      // available range.
      DEBUG_ASSERT(candidate_spot_count < max_candidate_spaces);
      DEBUG_ASSERT(prng);
      selected_index = prng->RandInt(candidate_spot_count);
      allocation_result =
          FindSpotAtIndex(selected_index, align_pow2, size, parent_base, parent_size, upper_limit);
    }

    DEBUG_ASSERT(allocation_result.is_ok());
    *alloc_spot = allocation_result.value();
    ASSERT_MSG(IS_ALIGNED(*alloc_spot, 1UL << align_pow2), "size=%zu align_pow2=%u alloc_spot=%zx",
               size, align_pow2, *alloc_spot);
    return ZX_OK;
  }

  // Returns whether the region list is empty.
  bool IsEmpty() const { return regions_.is_empty(); }

  // Returns the iterator points to the first element of the list.
  T& front() { return regions_.front(); }

  typename ChildList::iterator begin() { return regions_.begin(); }

  typename ChildList::const_iterator begin() const { return regions_.begin(); }

  typename ChildList::const_iterator cbegin() const { return regions_.cbegin(); }

  typename ChildList::iterator end() { return regions_.end(); }

  typename ChildList::const_iterator end() const { return regions_.end(); }

  typename ChildList::const_iterator cend() const { return regions_.cend(); }

  size_t size() const { return regions_.size(); }

 private:
  // list of memory regions, indexed by base address.
  ChildList regions_;
};

// A representation of a contiguous range of virtual address space
class VmAddressRegion final : public VmAddressRegionOrMapping {
 public:
  // Creates a root region.  This will span the entire aspace
  static zx_status_t CreateRootLocked(VmAspace& aspace, uint32_t vmar_flags,
                                      fbl::RefPtr<VmAddressRegion>* out) TA_REQ(aspace.lock());
  // Creates a subregion of this region
  zx_status_t CreateSubVmar(size_t offset, size_t size, uint8_t align_pow2, uint32_t vmar_flags,
                            const char* name, fbl::RefPtr<VmAddressRegion>* out);
  // Creates a VmMapping within this region. To avoid leaks, this should be paired with a call to
  // VmMapping::Destroy if desired; dropping `MapResult::mapping` will *not* destroy the mapping.
  struct MapResult {
    // This will never be null
    fbl::RefPtr<VmMapping> mapping;
    // Represents the virtual address of |mapping| at the time of creation, which is equivalent to
    // |mapping->base_locking()|.
    vaddr_t base;
  };
  zx::result<MapResult> CreateVmMapping(size_t mapping_offset, size_t size, uint8_t align_pow2,
                                        uint32_t vmar_flags, fbl::RefPtr<VmObject> vmo,
                                        uint64_t vmo_offset, uint arch_mmu_flags, const char* name);

  // Finds the child region that contains the given addr.  If addr is in a gap,
  // returns nullptr.  This is a non-recursive search.
  fbl::RefPtr<VmAddressRegionOrMapping> FindRegion(vaddr_t addr);
  fbl::RefPtr<VmAddressRegionOrMapping> FindRegionLocked(vaddr_t addr) TA_REQ(lock());

  // Base & size accessors
  // Lock not required as base & size will never change in VmAddressRegion
  vaddr_t base() const TA_NO_THREAD_SAFETY_ANALYSIS { return base_; }
  size_t size() const TA_NO_THREAD_SAFETY_ANALYSIS { return size_; }

  enum class RangeOpType {
    Commit,
    Decommit,
    MapRange,
    DontNeed,
    AlwaysNeed,
    Prefetch,
  };

  // Apply |op| to VMO mappings in the specified range of pages.
  zx_status_t RangeOp(RangeOpType op, vaddr_t base, size_t len,
                      VmAddressRegionOpChildren op_children, user_inout_ptr<void> buffer,
                      size_t buffer_size);

  // Unmap a subset of the region of memory in the containing address space,
  // returning it to this region to allocate.  If a subregion is entirely in
  // the range, and op_children is Yes, that subregion is destroyed. If a subregion is partially in
  // the range, Unmap() will fail.
  zx_status_t Unmap(vaddr_t base, size_t size, VmAddressRegionOpChildren op_children);

  // Same as Unmap, but allows for subregions that are partially in the range.
  // Additionally, sub-VMARs that are completely within the range will not be
  // destroyed.
  zx_status_t UnmapAllowPartial(vaddr_t base, size_t size);

  // Change protections on a subset of the region of memory in the containing
  // address space. If the requested range overlaps with a subregion and op_children is No,
  // Protect() will fail, otherwise the mapping permissions in the sub-region may only be reduced.
  zx_status_t Protect(vaddr_t base, size_t size, uint new_arch_mmu_flags,
                      VmAddressRegionOpChildren op_children);

  // Reserve a memory region within this VMAR. This region is already mapped in the page table with
  // |arch_mmu_flags|. VMAR should create a VmMapping for this region even though no physical pages
  // need to be allocated for this region.
  zx_status_t ReserveSpace(const char* name, size_t base, size_t size, uint arch_mmu_flags);

  const char* name() const { return name_; }
  bool has_parent() const;

  void DumpLocked(uint depth, bool verbose) const TA_REQ(lock()) override;

  // Recursively traverses the regions for a given virtual address and returns a raw pointer to a
  // mapping if one is found. The returned pointer is only valid as long as the aspace lock remains
  // held.
  VmMapping* FindMappingLocked(vaddr_t va) TA_REQ(lock());

  // Apply a memory priority to this VMAR and all of its subregions.
  zx_status_t SetMemoryPriority(MemoryPriority priority);

  // Constructors are public as LazyInit cannot use them otherwise, even if friended, but
  // otherwise should be considered private and Create...() should be used instead.
  VmAddressRegion(VmAspace& aspace, vaddr_t base, size_t size, uint32_t vmar_flags);
  VmAddressRegion(VmAddressRegion& parent, vaddr_t base, size_t size, uint32_t vmar_flags,
                  const char* name);

  // Lock not required as base & size values won't change in region.
  bool is_in_range(vaddr_t base, size_t size) const TA_NO_THREAD_SAFETY_ANALYSIS {
    const size_t offset = base - base_;
    return base >= base_ && offset < size_ && size_ - offset >= size;
  }

  // Traverses this vmar (and any sub-vmars) starting at this node, in depth-first pre-order. See
  // VmEnumerator for more details. If this vmar is not alive (in the LifeCycleState sense) or
  // otherwise not enumerable this returns ZX_ERR_BAD_STATE, otherwise the result of enumeration is
  // returned.
  zx_status_t EnumerateChildren(VmEnumerator* ve) TA_EXCL(lock());

 protected:
  friend class VmAspace;
  friend lazy_init::Access;

  // constructor for use in creating the kernel aspace singleton
  explicit VmAddressRegion(VmAspace& kernel_aspace);
  // Count the allocated pages, caller must be holding the aspace lock
  AttributionCounts GetAttributedMemoryLocked() TA_REQ(lock()) override;

  zx_status_t SetMemoryPriorityLocked(MemoryPriority priority) override TA_REQ(lock());
  void CommitHighMemoryPriority() override TA_EXCL(lock());

  friend class VmMapping;
  template <VmAddressRegionEnumeratorType>
  friend class VmAddressRegionEnumerator;

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(VmAddressRegion);

  fbl::Canary<fbl::magic("VMAR")> canary_;

  zx_status_t DestroyLocked() TA_REQ(lock()) override;

  void Activate() TA_REQ(lock()) override;

  // Helpers to share code between CreateSubVmar and CreateVmMapping
  zx_status_t CreateSubVmarInternal(size_t offset, size_t size, uint8_t align_pow2,
                                    uint32_t vmar_flags, fbl::RefPtr<VmObject> vmo,
                                    uint64_t vmo_offset, uint arch_mmu_flags, const char* name,
                                    vaddr_t* base_out, fbl::RefPtr<VmAddressRegionOrMapping>* out);
  zx_status_t CreateSubVmarInner(size_t offset, size_t size, uint8_t align_pow2,
                                 uint32_t vmar_flags, fbl::RefPtr<VmObject> vmo,
                                 uint64_t vmo_offset, uint arch_mmu_flags, const char* name,
                                 vaddr_t* base_out, fbl::RefPtr<VmAddressRegionOrMapping>* out);

  // Create a new VmMapping within this region, overwriting any existing
  // mappings that are in the way.  If the range crosses a subregion, the call
  // fails.
  zx_status_t OverwriteVmMappingLocked(vaddr_t base, size_t size, uint32_t vmar_flags,
                                       fbl::RefPtr<VmObject> vmo, uint64_t vmo_offset,
                                       uint arch_mmu_flags,
                                       fbl::RefPtr<VmAddressRegionOrMapping>* out) TA_REQ(lock());

  // Implementation for Unmap() and OverwriteVmMapping() that does not hold
  // the aspace lock. If |can_destroy_regions| is true, then this may destroy
  // VMARs that it completely covers. If |allow_partial_vmar| is true, then
  // this can handle the situation where only part of the VMAR is contained
  // within the region and will not destroy any VMARs.
  zx_status_t UnmapInternalLocked(vaddr_t base, size_t size, bool can_destroy_regions,
                                  bool allow_partial_vmar) TA_REQ(lock());

  // If the allocation between the given children can be met this returns a virtual address of the
  // base address of that allocation, otherwise a nullopt is returned.
  ktl::optional<vaddr_t> CheckGapLocked(VmAddressRegionOrMapping* prev,
                                        VmAddressRegionOrMapping* next, vaddr_t search_base,
                                        vaddr_t align, size_t region_size, size_t min_gap,
                                        uint arch_mmu_flags) TA_REQ(lock());

  // search for a spot to allocate for a region of a given size
  zx_status_t AllocSpotLocked(size_t size, uint8_t align_pow2, uint arch_mmu_flags, vaddr_t* spot,
                              vaddr_t upper_limit = ktl::numeric_limits<vaddr_t>::max())
      TA_REQ(lock());

  RegionList<VmAddressRegionOrMapping> subregions_ TA_GUARDED(lock());

  const char name_[ZX_MAX_NAME_LEN] = {};
};

// Helper object for managing a WAVL tree of protection ranges inside a VmMapping. For efficiency
// this object does not duplicate the base_ and size_ of the mapping, and so these values must be
// passed into most methods as |mapping_base| and |mapping_size|.
// This object is thread-compatible
// TODO: This object could be generalized into a dense range tracker as it is not really doing
// anything mapping specific.
class MappingProtectionRanges {
 public:
  explicit MappingProtectionRanges(uint arch_mmu_flags)
      : first_region_arch_mmu_flags_(arch_mmu_flags) {}
  MappingProtectionRanges(MappingProtectionRanges&&) = default;
  ~MappingProtectionRanges() = default;

  // Helper struct for FlagsRangeAtAddr
  struct FlagsRange {
    uint mmu_flags;
    uint64_t region_top;
  };
  // Returns both the flags for the specified vaddr, as well as the end of the range those flags are
  // valid for.
  FlagsRange FlagsRangeAtAddr(vaddr_t mapping_base, size_t mapping_size, vaddr_t vaddr) const {
    if (protect_region_list_rest_.is_empty()) {
      return FlagsRange{first_region_arch_mmu_flags_, mapping_base + mapping_size};
    } else {
      auto region = protect_region_list_rest_.upper_bound(vaddr);
      const vaddr_t region_top =
          region.IsValid() ? region->region_start : (mapping_base + mapping_size);
      const uint mmu_flags = FlagsForPreviousRegion(region);
      return FlagsRange{mmu_flags, region_top};
    }
  }

  // Updates the specified inclusive sub range to have the given flags. On error state is unchanged.
  // When updating the provided callback is invoked for every old range and value that is being
  // modified.
  template <typename F>
  zx_status_t UpdateProtectionRange(vaddr_t mapping_base, size_t mapping_size, vaddr_t base,
                                    size_t size, uint new_arch_mmu_flags, F callback);

  // Returns the precise mmu flags for the given vaddr. The vaddr is assumed to be within the range
  // of this mapping.
  uint MmuFlagsForRegion(vaddr_t vaddr) const {
    // Check the common case here inline since it doesn't generate much code. The full lookup
    // requires wavl tree traversal, and so we want to avoid inlining that.
    if (protect_region_list_rest_.is_empty()) {
      return first_region_arch_mmu_flags_;
    }
    return MmuFlagsForWavlRegion(vaddr);
  }

  // Enumerates any different protection ranges that exist inside this mapping. The virtual range
  // specified by range_base and range_size must be within this mappings base_ and size_. The
  // provided callback is called in virtual address order for each protection type. ZX_ERR_NEXT
  // and ZX_ERR_STOP can be used to control iteration, with any other status becoming the return
  // value of this method. The callback |func| is assumed to have a type signature of:
  // |zx_status_t(vaddr_t region_base, size_t region_size, uint mmu_flags)|
  template <typename F>
  zx_status_t EnumerateProtectionRanges(vaddr_t mapping_base, size_t mapping_size, vaddr_t base,
                                        size_t size, F func) const {
    DEBUG_ASSERT(size > 0);

    // Have a short circuit for the single protect region case to avoid wavl tree processing in the
    // common case.
    if (protect_region_list_rest_.is_empty()) {
      zx_status_t result = func(base, size, first_region_arch_mmu_flags_);
      if (result == ZX_ERR_NEXT || result == ZX_ERR_STOP) {
        return ZX_OK;
      }
      return result;
    }

    // See comments in the loop that explain what next and current represent.
    auto next = protect_region_list_rest_.upper_bound(base);
    auto current = next;
    current--;
    const vaddr_t range_top = base + (size - 1);
    do {
      // The region starting from 'current' and ending at 'next' represents a single protection
      // domain. We first work that, remembering that either of these could be an invalid node,
      // meaning the start or end of the mapping respectively.
      const vaddr_t protect_region_base = current.IsValid() ? current->region_start : mapping_base;
      const vaddr_t protect_region_top =
          next.IsValid() ? (next->region_start - 1) : (mapping_base + (mapping_size - 1));
      // We should only be iterating nodes that are actually part of the requested range.
      DEBUG_ASSERT(base <= protect_region_top);
      DEBUG_ASSERT(range_top >= protect_region_base);
      // The region found is of an entire protection block, and could extend outside the requested
      // range, so trim if necessary.
      const vaddr_t region_base = ktl::max(protect_region_base, base);
      const size_t region_len = ktl::min(protect_region_top, range_top) - region_base + 1;
      zx_status_t result =
          func(region_base, region_len,
               current.IsValid() ? current->arch_mmu_flags : first_region_arch_mmu_flags_);
      if (result != ZX_ERR_NEXT) {
        if (result == ZX_ERR_STOP) {
          return ZX_OK;
        }
        return result;
      }
      // Move to the next block.
      current = next;
      next++;
      // Continue looping as long we operating on nodes that overlap with the requested range.
    } while (current.IsValid() && current->region_start <= range_top);

    return ZX_OK;
  }

  // Merges protection ranges such that |right| is left cleared, and |this| contains the information
  // of both ranges. It is an error to call this if |this| and |right| are not virtually contiguous.
  zx_status_t MergeRightNeighbor(MappingProtectionRanges& right, vaddr_t merge_addr);

  // Splits this protection range into two ranges around the specified split point. |this| becomes
  // the left range and the right range is returned.
  MappingProtectionRanges SplitAt(vaddr_t split);

  // Discard any protection information below the given address.
  void DiscardBelow(vaddr_t addr);

  // Discard any protection information above the given address.
  void DiscardAbove(vaddr_t addr);

  // Returns whether all the protection nodes are within the given range. Intended for asserts.
  bool DebugNodesWithinRange(vaddr_t mapping_base, size_t mapping_size);

  // Clears all protection information and sets the size to 0.
  void clear() { protect_region_list_rest_.clear(); }

  // Flags for the first protection region.
  uint FirstRegionMmuFlags() const { return first_region_arch_mmu_flags_; }

  // Returns whether there is only a single protection region, that being the first region.
  bool IsSingleRegion() const { return protect_region_list_rest_.is_empty(); }

  // Sets the flags for the first region
  void SetFirstRegionMmuFlags(uint32_t new_flags) { first_region_arch_mmu_flags_ = new_flags; }

 private:
  // If a mapping is protected so that parts of it are different types then we need to track this
  // information. The ProtectNode represents the additional metadata that we need to allocate to
  // track this, and these nodes get placed in the protect_region_list_rest_.
  struct ProtectNode : public fbl::WAVLTreeContainable<ktl::unique_ptr<ProtectNode>> {
    ProtectNode(vaddr_t start, uint flags) : region_start(start), arch_mmu_flags(flags) {}
    ProtectNode() = default;
    ~ProtectNode() = default;

    vaddr_t GetKey() const { return region_start; }

    // Defines the start of the region that the flags apply to. The end of the region is determined
    // implicitly by either the next region in the tree, or the end of the mapping.
    vaddr_t region_start = 0;
    // The mapping flags (read/write/user/etc) for this region.
    uint arch_mmu_flags = 0;
  };
  using RegionList = fbl::WAVLTree<vaddr_t, ktl::unique_ptr<ProtectNode>>;

  // Internal helper that returns the flags for the region before the given node. Templated to work
  // on both iterator and const_iterator.
  template <typename T>
  uint FlagsForPreviousRegion(T node) const {
    node--;
    return node.IsValid() ? node->arch_mmu_flags : first_region_arch_mmu_flags_;
  }

  // Counts how many nodes would need to be allocated for a protection range. This calculation is
  // based of whether there are actually changes in the protection type that require a node to be
  // added.
  uint NodeAllocationsForRange(vaddr_t mapping_base, size_t mapping_size, vaddr_t base, size_t size,
                               RegionList::iterator removal_start, RegionList::iterator removal_end,
                               uint new_mmu_flags) const;

  // Helper method for MmuFlagsForRegionLocked that does the wavl tree lookup. Defined this way so
  // that the common case can inline efficiently, and the wavl tree traversal can stay behind a
  // function call.
  uint MmuFlagsForWavlRegion(vaddr_t vaddr) const;

  // To efficiently track the current protection/arch mmu flags of the mapping we want to avoid
  // allocating ProtectNode's as much as possible. For this the following scheme is used:
  // * The first_region_arch_mmu_flags_ represent the mmu flags from the start of the mapping (that
  //   is base_) up to the first node in the protect_region_list_rest_. Should
  //   protect_region_list_rest_ be empty then the region extends all the way to base_+size_. This
  //   means that when a mapping is first created no nodes need to be allocated and inserted into
  //   protect_region_list_rest_, we can simply set first_region_arch_mmu_flags_ to the initial
  //   protection flags.
  // * Should ::Protect need to 'split' a region, then nodes can be added to the
  // protect_region_list_rest_
  //   such that the mapping base_+first_region-arch_mmu_flags_ always represent the start of the
  //   first region, and the last region is implicitly ended by the end of the mapping.
  // As we want to avoid having redundant nodes, we can apply the following invariants to
  // protect_region_list_rest_
  // * No node region_start==base_
  // * No node with region_start==(base_+size_-1)
  // * First node in the tree cannot have arch_mmu_flags == first_region_arch_mmu_flags_
  // * No two adjacent nodes in the tree can have the same arch_mmu_flags.
  // To give an example. If there was a mapping with base_ = 0x1000, size_ = 0x5000,
  // first_region_arch_mmu_flags_ = READ and a single ProtectNode with region_start = 0x3000,
  // arch_mmu_flags = READ_WRITE. Then would determine there to be the regions
  // 0x1000-0x3000: READ (start comes from base_, the end comes from the start of the first node)
  // 0x3000-0x6000: READ_WRITE (start from node start, end comes from the end of the mapping as
  // there is no next node.
  uint first_region_arch_mmu_flags_;
  RegionList protect_region_list_rest_;
};

// A representation of the mapping of a VMO into the address space
class VmMapping final : public VmAddressRegionOrMapping {
 public:
  // Accessors for VMO-mapping state
  // These can be read under either lock (both locks being held for writing), so we provide two
  // different accessors, one for each lock.
  uint arch_mmu_flags_locked(vaddr_t offset) const TA_REQ(lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    return protection_ranges_.MmuFlagsForRegion(offset);
  }
  uint arch_mmu_flags_locked_object(vaddr_t offset) const
      TA_REQ(object_->lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    return protection_ranges_.MmuFlagsForRegion(offset);
  }
  uint64_t object_offset_locked() const TA_REQ(lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    return object_offset_;
  }
  uint64_t object_offset_locked_object() const
      TA_REQ(object_->lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    return object_offset_;
  }
  vaddr_t base_locked_object() const TA_REQ(object_->lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    return base_;
  }
  size_t size_locked_object() const TA_REQ(object_->lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    return size_;
  }

  Lock<CriticalMutex>* object_lock() const TA_RET_CAP(object_->lock()) TA_REQ(lock()) {
    return object_->lock();
  }
  Lock<CriticalMutex>& object_lock_ref() const TA_RET_CAP(object_->lock()) TA_REQ(lock()) {
    return object_->lock_ref();
  }

  // Intended to be used from VmEnumerator callbacks where the aspace_->lock() will be held.
  fbl::RefPtr<VmObject> vmo_locked() const TA_REQ(lock()) { return object_; }
  fbl::RefPtr<VmObject> vmo() const TA_EXCL(lock());

  // Convenience wrapper for vmo()->DecommitRange() with the necessary
  // offset modification and locking.
  zx_status_t DecommitRange(size_t offset, size_t len) TA_EXCL(lock());

  // Map in pages from the underlying vm object, optionally committing pages as it goes.
  // |ignore_existing| controls whether existing hardware mappings in the specified range should be
  // ignored or treated as an error. |ignore_existing| should only be set to true for user mappings
  // where populating mappings may already be racy with multiple threads, and where we are already
  // tolerant of mappings being arbitrarily created and destroyed.
  zx_status_t MapRange(size_t offset, size_t len, bool commit, bool ignore_existing = false)
      TA_EXCL(lock());

  // Unmap a subset of the region of memory in the containing address space,
  // returning it to the parent region to allocate.  If all of the memory is unmapped,
  // Destroy()s this mapping.  If a subrange of the mapping is specified, the
  // mapping may be split.
  zx_status_t Unmap(vaddr_t base, size_t size);

  // Change access permissions for this mapping.  It is an error to specify a
  // caching mode in the flags.  This will persist the caching mode the
  // mapping was created with.  If a subrange of the mapping is specified, the
  // mapping may be split.
  zx_status_t Protect(vaddr_t base, size_t size, uint new_arch_mmu_flags);

  void DumpLocked(uint depth, bool verbose) const TA_REQ(lock()) override;

  // Page fault in an address within the mapping. The requested address must be paged aligned. If
  // |additional_pages| is non-zero, then up to that many additional pages may be resolved using the
  // same |pf_flags|. It is not an error for the |additional_pages| to span beyond the mapping or
  // underlying VMO, although the range will get truncated internally. As such only the page
  // containing va is required to be resolved, and this method may return ZX_OK if any number,
  // including zero, of the additional pages are resolved.
  // As the |additional_pages| are resolved with the same |pf_flags| they may trigger copy-on-write
  // or other allocations in the underlying VMO.
  // If this returns ZX_ERR_SHOULD_WAIT, then the caller should wait on |page_request|
  // and try again. In addition to a status this returns how many pages got mapped in.
  // If ZX_OK is returned then the number of pages mapped in is guaranteed to be >0.
  // If |additional_pages| was non-zero, then the maximum number of pages that will be mapped is
  // |additional_pages + 1|. Otherwise the maximum number of pages that will be mapped is
  // kPageFaultMaxOptimisticPages.
  ktl::pair<zx_status_t, uint32_t> PageFaultLocked(vaddr_t va, uint pf_flags,
                                                   size_t additional_pages,
                                                   MultiPageRequest* page_request) TA_REQ(lock());

  // Apis intended for use by VmObject

  // |assert_object_lock| exists to satisfy clang capability analysis since there are circumstances
  // when the object_->lock() is actually being held, but it was not acquired by dereferencing
  // object_. In this scenario we need to explain to the analysis that the lock held is actually the
  // same as object_->lock(), and even though we otherwise have no intention of using object_, the
  // only way to do this is to notionally dereferencing object_ to compare the lock.
  // Since this is asserting that the lock is held, and not just returning a reference to the lock,
  // this method is logically correct since object_ itself is only modified if object_->lock() is
  // held.
  void assert_object_lock() TA_ASSERT(object_->lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    AssertHeld(object_->lock_ref());
  }

  enum UnmapOptions : uint8_t {
    kNone = 0u,
    OnlyHasZeroPages = (1u << 0),
    Harvest = (1u << 1),
  };

  // Unmap any pages that map the passed in vmo range from the arch aspace.
  // May not intersect with this range.
  // If |only_has_zero_pages| is true then the caller is asserting that it knows that any mappings
  // in the region will only be for the shared zero page.
  void AspaceUnmapLockedObject(uint64_t offset, uint64_t len, UnmapOptions options) const
      TA_REQ(object_->lock());

  // Removes any writeable mappings for the passed in vmo range from the arch aspace.
  // May fall back to unmapping pages from the arch aspace if necessary.
  void AspaceRemoveWriteLockedObject(uint64_t offset, uint64_t len) const TA_REQ(object_->lock());

  // Checks if this is a kernel mapping within the given VMO range, which would be an error to be
  // unpinning.
  void AspaceDebugUnpinLockedObject(uint64_t offset, uint64_t len) const TA_REQ(object_->lock());

  // Marks this mapping as being a candidate for merging, and will immediately attempt to merge with
  // any neighboring mappings. Making a mapping mergeable essentially indicates that you will no
  // longer use this specific VmMapping instance to refer to the referenced region, and will access
  // the region via the parent vmar in the future, and so the region merely needs to remain valid
  // through some VmMapping.
  // For this the function requires you to hand in your last remaining refptr to the mapping.
  static void MarkMergeable(fbl::RefPtr<VmMapping>&& mapping);

  // Used to cache the memory attribution counts for this vmo range. Also tracks the vmo hierarchy
  // generation count and the mapping generation count at the time of caching the attribution
  // counts.
  struct CachedMemoryAttribution {
    uint64_t mapping_generation_count = 0;
    uint64_t vmo_generation_count = 0;
    AttributionCounts attribution_counts;
  };

  // Enumerates any different protection ranges that exist inside this mapping. The virtual range
  // specified by range_base and range_size must be within this mappings base_ and size_. The
  // provided callback is called in virtual address order for each protection type. ZX_ERR_NEXT
  // and ZX_ERR_STOP can be used to control iteration, with any other status becoming the return
  // value of this method.
  template <typename F>
  zx_status_t EnumerateProtectionRangesLocked(vaddr_t base, size_t size, F func) const
      TA_REQ(lock()) {
    DEBUG_ASSERT(is_in_range_locked(base, size));
    return ProtectRangesLocked().EnumerateProtectionRanges(base_, size_, base, size, func);
  }

  // The maximum number of pages that a page fault can optimistically extend the fault to include.
  // This is defined and exposed here for the purposes of unittests.
  static constexpr uint64_t kPageFaultMaxOptimisticPages = 16;

  // WAVL tree key function
  // For use in WAVL tree code only.
  VmObject::MappingTreeTraits::Key GetKey() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return VmObject::MappingTreeTraits::Key{
        .offset = object_offset_locked_object(),
        .object = reinterpret_cast<uint64_t>(this),
    };
  }

  // TODO(https://fxbug.dev/42106188): Informs the mapping that a write is going to be performed to
  // the backing VMO, even if the VMO is not writable. This gives the mapping an opportunity to
  // create a private clone of the VMO if necessary and use that to back the mapping instead,
  // providing a way to 'safely' perform the write.
  // This may change the underlying VMO and invalidates any previous calls to |vmo| or |vmo_locked|.
  zx_status_t ForceWritableLocked() TA_REQ(lock());

 protected:
  ~VmMapping() override;
  friend fbl::RefPtr<VmMapping>;

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(VmMapping);

  fbl::Canary<fbl::magic("VMAP")> canary_;

  enum class Mergeable : bool { YES = true, NO = false };

  // allow VmAddressRegion to manipulate VmMapping internals for construction
  // and bookkeeping
  friend class VmAddressRegion;

  // private constructors, use VmAddressRegion::Create...() instead
  VmMapping(VmAddressRegion& parent, vaddr_t base, size_t size, uint32_t vmar_flags,
            fbl::RefPtr<VmObject> vmo, uint64_t vmo_offset, uint arch_mmu_flags,
            Mergeable mergeable);
  VmMapping(VmAddressRegion& parent, vaddr_t base, size_t size, uint32_t vmar_flags,
            fbl::RefPtr<VmObject> vmo, uint64_t vmo_offset, MappingProtectionRanges&& ranges,
            Mergeable mergeable);

  zx_status_t DestroyLocked() TA_REQ(lock()) override;

  // Implementation for Unmap().  This supports partial unmapping.
  zx_status_t UnmapLocked(vaddr_t base, size_t size) TA_REQ(lock());

  // Implementation for Protect().
  zx_status_t ProtectLocked(vaddr_t base, size_t size, uint new_arch_mmu_flags) TA_REQ(lock());

  // Helper for protect and unmap.
  static zx_status_t ProtectOrUnmap(const fbl::RefPtr<VmAspace>& aspace, vaddr_t base, size_t size,
                                    uint new_arch_mmu_flags);

  AttributionCounts GetAttributedMemoryLocked() TA_REQ(lock()) override;

  zx_status_t SetMemoryPriorityLocked(VmAddressRegion::MemoryPriority priority) override
      TA_REQ(lock());

  void CommitHighMemoryPriority() override TA_EXCL(lock());

  void Activate() TA_REQ(lock()) override;

  void ActivateLocked() TA_REQ(lock()) TA_REQ(object_->lock());

  // Takes a range relative to the vmo object_ and converts it into a virtual address range relative
  // to aspace_. Returns true if a non zero sized intersection was found, false otherwise. If false
  // is returned |base| and |virtual_len| hold undefined contents.
  bool ObjectRangeToVaddrRange(uint64_t offset, uint64_t len, vaddr_t* base,
                               uint64_t* virtual_len) const TA_REQ(object_->lock());

  // Attempts to merge this mapping with any neighbors. It is the responsibility of the caller to
  // ensure a refptr to this is being held, as on return |this| may be in the dead state and have
  // removed itself from the hierarchy, dropping a refptr.
  void TryMergeNeighborsLocked() TA_REQ(lock());

  // Attempts to merge the given mapping into this one. This only succeeds if the candidate is
  // placed just after |this|, both in the aspace and the vmo. See implementation for the full
  // requirements for merging to succeed.
  // The candidate must be held as a RefPtr by the caller so that this function does not trigger
  // any VmMapping destructor by dropping the last reference when removing from the parent vmar.
  void TryMergeRightNeighborLocked(VmMapping* right_candidate) TA_REQ(lock());

  // Helper function that updates the |size_| to |new_size| and also increments the mapping
  // generation count. Requires both the aspace lock and the object lock to be held, since |size_|
  // can be read under either of those locks.
  void set_size_locked(size_t new_size) TA_REQ(lock()) TA_REQ(object_->lock()) {
    // Mappings cannot be zero sized while the mapping is in the region list.
    DEBUG_ASSERT(new_size > 0 || !in_subregion_tree());
    // Check that if we have additional protection regions that they have already been constrained
    // to the range of the new size.
    DEBUG_ASSERT(protection_ranges_.DebugNodesWithinRange(base_, new_size));

    const bool size_changed = size_ != new_size;
    size_ = new_size;

    // Restore the invalidated subtree invariants when the size changes while the node is in the
    // subregion tree.
    if (size_changed && in_subregion_tree()) {
      auto iter = RegionList<>::ChildList::materialize_iterator(*this);
      RegionList<>::Observer::RestoreInvariants(iter);
    }
    if (size_changed && vmo_mapping_node_.InContainer()) {
      auto iter = VmObject::MappingTree::materialize_iterator(*this);
      VmMappingSubtreeState::Observer<VmMapping>::RestoreInvariants(iter);
    }
  }

  // For a VmMapping |state_| is only modified either with the object_ lock held, or if there is no
  // |object_|. Therefore it is safe to read state if just the object lock is held.
  LifeCycleState get_state_locked_object() const
      TA_REQ(object_->lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    return state_;
  }

  // Returns the minimum of the requested map length, the size of the VMO or, if
  // FAULT_BEYOND_STREAM_SIZE is set, the  page containing the stream size. MapRange can be trimmed
  // to these lengths as it should not be considered an error to call MapRange past the VMO size in
  // a resizable VMO or past the page containing the stream size in a FAULT_BEYOND_STREAM_SIZE VMO.
  uint64_t TrimmedObjectRangeLocked(uint64_t offset, uint64_t len) const TA_REQ(lock())
      TA_REQ(object_->lock());

  // Whether this mapping may be merged with other adjacent mappings. A mergeable mapping is just a
  // region that can be represented by any VmMapping object, not specifically this one.
  Mergeable mergeable_ TA_GUARDED(lock()) = Mergeable::NO;

  // TODO(https://fxbug.dev/42106188): Tracks whether this mapping has been transitioned into a
  // private clone to allow for writes to safely be done without modifying a VMO that the mapping
  // does not have permission to.
  bool private_clone_ TA_GUARDED(lock()) = false;

  fbl::WAVLTreeNodeState<VmMapping*> vmo_mapping_node_ TA_GUARDED(object_->lock());
  VmMappingSubtreeState mapping_subtree_state_ TA_GUARDED(object_->lock());

  friend VmObject::MappingTreeTraits;
  friend VmMappingSubtreeState;

  // pointer and region of the object we are mapping
  fbl::RefPtr<VmObject> object_ TA_GUARDED(lock());
  // This can be read with either lock hold, but requires both locks to write it.
  uint64_t object_offset_ TA_GUARDED(object_->lock()) TA_GUARDED(lock()) = 0;

  // This can be read with either lock hold, but requires both locks to write it.
  MappingProtectionRanges protection_ranges_ TA_GUARDED(object_->lock()) TA_GUARDED(lock());

  class CurrentlyFaulting;
  // Pointer to a CurrentlyFaulting object if the mapping is presently handling a page fault. This
  // is protected specifically by the object lock so that AspaceUnmapLockedObject can inspect it.
  CurrentlyFaulting* currently_faulting_ TA_GUARDED(object_->lock()) = nullptr;

  // Helpers for gaining read access to the protection information when only one of the locks is
  // held.
  const MappingProtectionRanges& ProtectRangesLocked() const
      TA_REQ(lock()) __TA_NO_THREAD_SAFETY_ANALYSIS {
    return protection_ranges_;
  }
  const MappingProtectionRanges& ProtectRangesLockedObject() const
      TA_REQ(object_->lock()) __TA_NO_THREAD_SAFETY_ANALYSIS {
    return protection_ranges_;
  }
};

// Interface for walking a VmAspace-rooted VmAddressRegion/VmMapping tree.
// Override this class and pass an instance to VmAddressRegion::EnumerateChildren().
// VmAddressRegion::EnumerateChildren() will call the On* methods in depth-first pre-order.
// ZX_ERR_NEXT and ZX_ERR_STOP can be used to control iteration, with any other status becoming the
// return value of this method. The root VmAspace's lock is held during the traversal and passed in
// to the callbacks as |guard|. A callback is permitted to temporarily drop the lock, using
// |CallUnlocked|, although doing so invalidates the pointers and to use them without the lock held,
// of after it is reacquired, they should first be turned into a RefPtr, with the caveat that they
// might now refer to a dead, aka unmapped, object.
class VmEnumerator {
 public:
  // |depth| will be 0 for the root VmAddressRegion.
  virtual zx_status_t OnVmAddressRegion(VmAddressRegion* vmar, uint depth,
                                        Guard<CriticalMutex>& guard) TA_REQ(vmar->lock()) {
    return ZX_ERR_NEXT;
  }

  // |vmar| is the parent of |map|.
  virtual zx_status_t OnVmMapping(VmMapping* map, VmAddressRegion* vmar, uint depth,
                                  Guard<CriticalMutex>& guard) TA_REQ(map->lock())
      TA_REQ(vmar->lock()) {
    return ZX_ERR_NEXT;
  }

 protected:
  VmEnumerator() = default;
  ~VmEnumerator() = default;
};

// Now that all the sub-classes are defined finish declaring some inline VmAddressRegionOrMapping
// methods.
inline fbl::RefPtr<VmAddressRegion> VmAddressRegionOrMapping::as_vm_address_region() {
  canary_.Assert();
  if (is_mapping()) {
    return nullptr;
  }
  return fbl::RefPtr<VmAddressRegion>(static_cast<VmAddressRegion*>(this));
}

inline VmAddressRegion* VmAddressRegionOrMapping::as_vm_address_region_ptr() {
  canary_.Assert();
  if (unlikely(is_mapping())) {
    return nullptr;
  }
  return static_cast<VmAddressRegion*>(this);
}

inline fbl::RefPtr<VmAddressRegion> VmAddressRegionOrMapping::downcast_as_vm_address_region(
    fbl::RefPtr<VmAddressRegionOrMapping>* region_or_map) {
  DEBUG_ASSERT(region_or_map);
  if ((*region_or_map)->is_mapping()) {
    return nullptr;
  }
  return fbl::RefPtr<VmAddressRegion>::Downcast(ktl::move(*region_or_map));
}

inline fbl::RefPtr<VmMapping> VmAddressRegionOrMapping::as_vm_mapping() {
  canary_.Assert();
  if (!is_mapping()) {
    return nullptr;
  }
  return fbl::RefPtr<VmMapping>(static_cast<VmMapping*>(this));
}

inline VmMapping* VmAddressRegionOrMapping::as_vm_mapping_ptr() {
  canary_.Assert();
  if (unlikely(!is_mapping())) {
    return nullptr;
  }
  return static_cast<VmMapping*>(this);
}

inline fbl::RefPtr<VmMapping> VmAddressRegionOrMapping::downcast_as_vm_mapping(
    fbl::RefPtr<VmAddressRegionOrMapping>* region_or_map) {
  DEBUG_ASSERT(region_or_map);
  if (!(*region_or_map)->is_mapping()) {
    return nullptr;
  }
  return fbl::RefPtr<VmMapping>::Downcast(ktl::move(*region_or_map));
}

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_H_
