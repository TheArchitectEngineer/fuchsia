// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>
#include <zircon/tls.h>

#include "asm-linkage.h"
#include "../jmp_buf.h"


// CFI to find regno at 8*index(%rdi).
.macro jb_cfi regno, index
  .if 8 * \index < 0x7f
    .cfi_escape DW_CFA_expression, \regno, 2, DW_OP_breg(5), 8 * \index
  .else
    .error "offset too large for one-byte SLEB128"
  .endif
.endm

.macro restore reg, index
  mov 8*\index(%rdi), \reg
  .cfi_same_value \reg
.endm

.macro demangle index, reg
  xor LIBC_ASM_LINKAGE(gJmpBufManglers)+8*\index(%rip), \reg
.endm

jmp_buf.llvm_libc_function longjmp

  // The sanitizer runtime wants to be informed of non-local exits.
  // Call __asan_handle_no_return() before doing the actual longjmp.
#if __has_feature(address_sanitizer)
  // Save our incoming argument registers on the stack around calling
  // __asan_handle_no_return.  The incoming stack is misaligned by one
  // word, so we need to move the stack by an odd number of words,
  // hence the unnecessary save/restore of %rax.
  push.spill %rax
  push.spill %rsi
  push.spill %rdi
  call __asan_handle_no_return@PLT
  pop.reload %rdi
  pop.reload %rsi
  pop.reload %rax
#endif

  // Calculate return value: %r11d = %esi ?: 1
  mov $1, %r11d
  test %esi, %esi
  cmovnz %esi, %r11d

  // Load the mangled values into temporaries.
  mov 8*JB_PC(%rdi), %rax
  .cfi_undefined %rax
  mov 8*JB_SP(%rdi), %rcx
  mov 8*JB_FP(%rdi), %rdx
  mov 8*JB_USP(%rdi), %rsi
  .cfi_undefined %rsi

  // Demangle each temporary.
  demangle JB_PC, %rax
  demangle JB_SP, %rcx
  demangle JB_FP, %rdx
  demangle JB_USP, %rsi

  // The next instruction clobbers the state of longjmp's caller.
  // So from here on, we'll use CFI that unwinds to setjmp's caller instead.
  // Both callers have the same %rdi value, which we're still using.
  .cfi_register %rip, %rax
  .cfi_register %rsp, %rcx
  .cfi_register %rbp, %rdx
  //.cfi_register %unsafe_sp, %rsi  -- No DWARF register number for it!
  jb_cfi 3, JB_RBX
  jb_cfi 12, JB_R12
  jb_cfi 13, JB_R13
  jb_cfi 14, JB_R14
  jb_cfi 15, JB_R15

  // Restore all the vanilla callee-saves registers.
  restore %rbx, JB_RBX
  restore %r12, JB_R12
  restore %r13, JB_R13
  restore %r14, JB_R14
  restore %r15, JB_R15

  // Restore the demangled values.
  mov %rdx, %rbp
  .cfi_same_value %rbp
  mov %rsi, %fs:ZX_TLS_UNSAFE_SP_OFFSET
  //.cfi_same_value %unsafe_sp  -- No DWARF register number for it!
  mov %rax, (%rcx)
  .cfi_escape DW_CFA_expression, 16, 3, DW_OP_breg(2), 0, DW_OP_deref

  // Restore SP last.
  // After this, our CFA is setjmp's CFA rather than longjmp's CFA.
  mov %rcx, %rsp
  .cfi_same_value %rsp
  .cfi_escape DW_CFA_expression, 16, 3, DW_OP_breg(7), 0, DW_OP_deref

  // Don't leak the demangled values.
  xor %ecx, %ecx
  xor %edx, %edx
  xor %esi, %esi

  mov %r11d, %eax
  ret

.end_function
