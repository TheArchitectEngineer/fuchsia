// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//

#include <asm.h>

.code64
.section .text
// zx_status_t x86_acpi_transition_s_state(struct x86_realmode_entry_data_registers* reg,
//                                         uint8_t target_s_state, uint8_t sleep_type_a,
//                                         uint8_t sleep_type_b)
FUNCTION_LABEL(x86_acpi_transition_s_state)
    // We do not need to save floating point registers, since this method must be called
    // by a kernel thread if the system is expected to return from the S-state.
    mov %rdi, (%rdi)
    mov %rsi, 8(%rdi)
    mov %rbp, 16(%rdi)
    mov %rbx, 24(%rdi)
    mov %rdx, 32(%rdi)
    mov %rcx, 40(%rdi)
    // Stash 0 as rax, so we return AE_OK if we do suspend
    movq $0, 48(%rdi)
    mov %r8, 56(%rdi)
    mov %r9, 64(%rdi)
    mov %r10, 72(%rdi)
    mov %r11, 80(%rdi)
    mov %r12, 88(%rdi)
    mov %r13, 96(%rdi)
    mov %r14, 104(%rdi)
    mov %r15, 112(%rdi)
    mov %rsp, 120(%rdi)

    // Set up our return IP, in case the S-state needs it
    leaq .Lafter_sleep(%rip), %rax
    movq %rax, 128(%rdi)

    // Enter the sleep state
    push %rbp
    mov %rsp, %rbp
    mov %rsi, %rdi
    mov %rdx, %rsi
    mov %rcx, %rdx
    call set_suspend_registers
    pop %rbp
.Lafter_sleep:
    ret
