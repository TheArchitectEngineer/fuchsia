// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_BOOTSTRAP16_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_BOOTSTRAP16_H_

#include <arch/x86/mmu.h>

#define BCD_PHYS_BOOTSTRAP_PML4_OFFSET 0
#define BCD_PHYS_KERNEL_PML4_OFFSET 4
#define BCD_PHYS_GDTR_OFFSET 10
#define BCD_PHYS_LM_ENTRY_OFFSET 16
#define BCD_LM_CS_OFFSET 20
#define BCD_VIRT_LM_HIGH_ENTRY_OFFSET 24
#define BCD_CPU_COUNTER_OFFSET 32
#define BCD_CPU_WAITING_OFFSET 36
#define BCD_PER_CPU_BASE_OFFSET 44

#define RED_REGISTERS_OFFSET 32

#ifndef __ASSEMBLER__
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <vm/vm_aspace.h>

struct x86_bootstrap16_data {
  // Physical address of identity PML4
  uint32_t phys_bootstrap_pml4;
  // Physical address of the kernel PML4
  uint32_t phys_kernel_pml4;
  // Physical address of GDT, aligned such that it can be treated as a LGDT descriptor.
  uint16_t padding1;
  uint16_t phys_gdtr_limit;
  uint32_t phys_gdtr_base;

  // Ordering of these two matter; they should be usable by retfl
  // Physical address of long mode entry point
  uint32_t phys_long_mode_entry;
  // 64-bit code segment to use
  uint32_t long_mode_cs;

  // The virtual address of the high-addressed entry point after the long mode
  // entry point.
  uint64_t virt_long_mode_high_entry;
};

struct __PACKED x86_realmode_entry_data {
  struct x86_bootstrap16_data hdr;

  // Virtual address of the register dump (expected to be in
  // the form of x86_realmode_entry_data_registers)
  uint64_t registers_ptr;
};

struct x86_realmode_entry_data_registers {
  uint64_t rdi, rsi, rbp, rbx, rdx, rcx, rax;
  uint64_t r8, r9, r10, r11, r12, r13, r14, r15;
  uint64_t rsp, rip;
};

struct __PACKED x86_ap_bootstrap_data {
  struct x86_bootstrap16_data hdr;

  // Counter for APs to use to determine which stack to take
  uint32_t cpu_id_counter;
  // Pointer to value to use to determine when APs are done with boot
  ktl::atomic<unsigned int>* cpu_waiting_mask;

  // Per-cpu data
  struct __PACKED {
    // Virtual address of the top of initial kstack
    vaddr_t kstack_top;
    // Virtual address of initial Thread
    Thread* thread;
  } per_cpu[SMP_MAX_CPUS - 1];
};

// Initialize the bootstrap16 subsystem by giving it pages to work with.
// |bootstrap_base| must refer to k_x86_boostrap16_buffer_size bytes of ram aligned
// on a page boundary less than 1M that are available for the OS to use.
constexpr size_t k_x86_bootstrap16_buffer_size = 3UL * PAGE_SIZE;
void x86_bootstrap16_init(paddr_t bootstrap_base);

// Upon success, returns a pointer to the virtual address of the bootstrap data, and the physical
// address of the first instruction that should be executed in 16-bit mode.
//
// If this function returns success, x86_bootstrap16_release() must be called
// later, to allow the bootstrap16 module to be reused.
zx_status_t x86_bootstrap16_acquire(uintptr_t entry64, void** bootstrap_aperture,
                                    paddr_t* instr_ptr);

// To be called once the caller is done using the bootstrap16 module
void x86_bootstrap16_release(void* bootstrap_aperture);

static_assert(sizeof(struct x86_ap_bootstrap_data) <= PAGE_SIZE);
static_assert(sizeof(struct x86_realmode_entry_data) <= PAGE_SIZE);

static_assert(__offsetof(struct x86_bootstrap16_data, phys_bootstrap_pml4) ==
              BCD_PHYS_BOOTSTRAP_PML4_OFFSET);
static_assert(__offsetof(struct x86_bootstrap16_data, phys_kernel_pml4) ==
              BCD_PHYS_KERNEL_PML4_OFFSET);
static_assert(__offsetof(struct x86_bootstrap16_data, phys_gdtr_limit) == BCD_PHYS_GDTR_OFFSET);
static_assert(__offsetof(struct x86_bootstrap16_data, phys_gdtr_base) == BCD_PHYS_GDTR_OFFSET + 2);
static_assert(__offsetof(struct x86_bootstrap16_data, phys_long_mode_entry) ==
              BCD_PHYS_LM_ENTRY_OFFSET);
static_assert(__offsetof(struct x86_bootstrap16_data, long_mode_cs) == BCD_LM_CS_OFFSET);

static_assert(offsetof(struct x86_bootstrap16_data, virt_long_mode_high_entry) ==
              BCD_VIRT_LM_HIGH_ENTRY_OFFSET);

static_assert(__offsetof(struct x86_ap_bootstrap_data, hdr) == 0);
static_assert(__offsetof(struct x86_ap_bootstrap_data, cpu_id_counter) == BCD_CPU_COUNTER_OFFSET);
static_assert(__offsetof(struct x86_ap_bootstrap_data, cpu_waiting_mask) == BCD_CPU_WAITING_OFFSET);
static_assert(__offsetof(struct x86_ap_bootstrap_data, per_cpu) == BCD_PER_CPU_BASE_OFFSET);

static_assert(__offsetof(struct x86_realmode_entry_data, hdr) == 0);
static_assert(__offsetof(struct x86_realmode_entry_data, registers_ptr) == RED_REGISTERS_OFFSET);

#endif  // __ASSEMBLER__

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_BOOTSTRAP16_H_
