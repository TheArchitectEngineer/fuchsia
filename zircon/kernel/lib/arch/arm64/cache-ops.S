// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/arm64/system-asm.h>
#include <lib/arch/asm.h>
#include <lib/arch/cache.h>

.text

// Scratch register, not saved across function calls.
tmp             .req x16

.function CleanDataCacheRange, global
  data_cache_range_op cvac

  ret
  speculation_postfence
.end_function

.function CleanInvalidateDataCacheRange, global
  data_cache_range_op civac

  ret
  speculation_postfence
.end_function

.function InvalidateDataCacheRange, global
  data_cache_range_op ivac

  ret
  speculation_postfence
.end_function

.function InvalidateInstructionCacheRange, global
  instruction_cache_range_invalidate

  ret
  speculation_postfence
.end_function

// void InvalidateLocalCaches()
.function InvalidateLocalCaches, global
   data_cache_way_set_op isw, invalidate
   ic  iallu
   isb

   ret
   speculation_postfence
.end_function

// void CleanLocalCaches()
.function CleanLocalCaches, global
   data_cache_way_set_op csw, clean
   ic  iallu
   isb

   ret
   speculation_postfence
.end_function

// void CleanAndInvalidateLocalCaches()
.function CleanAndInvalidateLocalCaches, global
   data_cache_way_set_op cisw, clean_and_invalidate
   ic  iallu
   isb

   ret
   speculation_postfence
.end_function

// void DisableLocalCachesAndMmu()
//
// This routine is purposefully written in assembly and only using registers;
// until the dirty cache lines in the disabled cache are written back, we must
// avoid accessing RAM and its possible stale values.
.function DisableLocalCachesAndMmu, global
  // Make sure the caches are flushed before we start bypassing them.
first_flush:
  data_cache_way_set_op cisw, flush1
  ic  iallu
  isb

  // What EL are we running at?
  mrs     tmp, CurrentEL
  cmp     tmp, #(1 << 2)
  beq     disable_el1

disable_el2:  // EL2 version
  mrs     tmp, sctlr_el2
  bic     tmp, tmp, #SCTLR_EL2_I  // Instruction caches
  bic     tmp, tmp, #SCTLR_EL2_C  // Data and unified caches
  bic     tmp, tmp, #SCTLR_EL2_M  // MMU
  msr     sctlr_el2, tmp
  b       second_flush

disable_el1:  // EL1 version
  mrs     tmp, sctlr_el1
  bic     tmp, tmp, #SCTLR_EL1_I
  bic     tmp, tmp, #SCTLR_EL1_C
  bic     tmp, tmp, #SCTLR_EL1_M
  msr     sctlr_el1, tmp

  // And flush again before returning to leave the caches and TLB in pristine shape.
second_flush:
  data_cache_way_set_op cisw, flush2
  ic  iallu
  isb
  tlbi vmalle1

  ret
  speculation_postfence
.end_function
