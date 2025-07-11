// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_HYPERVISOR_EL2_STATE_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_HYPERVISOR_EL2_STATE_H_

#include <lib/arch/arm64/system-asm.h>
#include <zircon/compiler.h>

// clang-format off

#ifndef __ASSEMBLER__
#define BIT_32(bit)         (1u << bit)
#define BIT_64(bit)         (1ul << bit)
#else
#define BIT_32(bit)         (0x1 << bit)
#define BIT_64(bit)         (0x1 << bit)
#endif

#define SCTLR_ELX_M         BIT_32(0)
#define SCTLR_ELX_A         BIT_32(1)
#define SCTLR_ELX_C         BIT_32(2)
#define SCTLR_ELX_SA        BIT_32(3)
#define SCTLR_ELX_I         BIT_32(12)

#define SCTLR_EL1_RES1      0x00500800
#define SCTLR_EL2_RES1      0x30c50830

#define FS_Q0               0
#define FS_Q(num)           (FS_Q0 + ((num) * 16))
#define FS_NUM_REGS         32
#define FS_FPSR             FS_Q(FS_NUM_REGS)
#define FS_FPCR             (FS_FPSR + 8)

#define SS_SP_EL0           0
#define SS_TPIDR_EL0        (SS_SP_EL0 + 8)
#define SS_TPIDRRO_EL0      (SS_TPIDR_EL0 + 8)
#define SS_CNTKCTL_EL1      (SS_TPIDRRO_EL0 + 8)
#define SS_CONTEXTIDR_EL1   (SS_CNTKCTL_EL1 + 8)
#define SS_CPACR_EL1        (SS_CONTEXTIDR_EL1 + 8)
#define SS_CSSELR_EL1       (SS_CPACR_EL1 + 8)
#define SS_ELR_EL1          (SS_CSSELR_EL1 + 8)
#define SS_ESR_EL1          (SS_ELR_EL1 + 8)
#define SS_FAR_EL1          (SS_ESR_EL1 + 8)
#define SS_MAIR_EL1         (SS_FAR_EL1 + 8)
#define SS_MDSCR_EL1        (SS_MAIR_EL1 + 8)
#define SS_PAR_EL1          (SS_MDSCR_EL1 + 8)
#define SS_SCTLR_EL1        (SS_PAR_EL1 + 8)
#define SS_SP_EL1           (SS_SCTLR_EL1 + 8)
#define SS_SPSR_EL1         (SS_SP_EL1 + 8)
#define SS_TCR_EL1          (SS_SPSR_EL1 + 8)
#define SS_TPIDR_EL1        (SS_TCR_EL1 + 8)
#define SS_TTBR0_EL1        (SS_TPIDR_EL1 + 8)
#define SS_TTBR1_EL1        (SS_TTBR0_EL1 + 8)
#define SS_VBAR_EL1         (SS_TTBR1_EL1 + 8)
#define SS_ELR_EL2          (SS_VBAR_EL1 + 8)
#define SS_SPSR_EL2         (SS_ELR_EL2 + 8)

#define ES_RESUME           0

#define GS_X0               (ES_RESUME + 16)
#define GS_X(num)           (GS_X0 + ((num) * 8))
#define GS_NUM_REGS         31
#define GS_FP_STATE         (GS_X(GS_NUM_REGS) + 8)
#define GS_SYSTEM_STATE     (GS_FP_STATE + FS_FPCR + 8)
#define GS_CNTV_CTL_EL0     (GS_SYSTEM_STATE + SS_SPSR_EL2 + 8)
#define GS_CNTV_CVAL_EL0    (GS_CNTV_CTL_EL0 + 8)
#define GS_ESR_EL2          (GS_CNTV_CVAL_EL0 + 8)
#define GS_FAR_EL2          (GS_ESR_EL2 + 8)
#define GS_HPFAR_EL2        (GS_FAR_EL2 + 8)
#define GS_VMPIDR_EL2       (GS_HPFAR_EL2 + 8)

#define HS_XREGS            (GS_VMPIDR_EL2 + 16)
// NOTE(abdulla): This differs from GS_X in that it calculates a value relative
// to host_state.x, and not relative to El2State.
#define HS_X(num)           ((num) * 8)
#define HS_NUM_REGS         14
#define HS_FP_STATE         (HS_XREGS + HS_X(HS_NUM_REGS))
#define HS_SYSTEM_STATE     (HS_FP_STATE + FS_FPCR + 8)

#define IS_NUM_APRS         0
#define IS_NUM_LRS          (IS_NUM_APRS + 1)
#define IS_VMCR             (IS_NUM_LRS + 7)
#define IS_MISR             (IS_VMCR + 8)
#define IS_ELRSR            (IS_MISR + 8)
#define IS_AP0R0            (IS_ELRSR + 8)
#define IS_MAX_APRS         4
#define IS_APR(group, num)  (IS_AP0R0 + ((((group) * IS_MAX_APRS) + (num)) * 8))
#define IS_MAX_APR_GROUPS   2
#define IS_LR0              IS_APR(IS_MAX_APR_GROUPS - 1, IS_MAX_APRS)
#define IS_LR(num)          (IS_LR0 + ((num) * 8))
#define IS_MAX_LRS          64

// clang-format on

#ifndef __ASSEMBLER__

#include <bits.h>
#include <zircon/types.h>

#include <arch/defines.h>

typedef uint32_t __ALIGNED(8) algn32_t;

struct FpState {
  __uint128_t q[FS_NUM_REGS];
  algn32_t fpsr;
  algn32_t fpcr;
};

struct SystemState {
  uint64_t sp_el0;
  uint64_t tpidr_el0;
  uint64_t tpidrro_el0;

  algn32_t cntkctl_el1;
  algn32_t contextidr_el1;
  algn32_t cpacr_el1;
  algn32_t csselr_el1;
  uint64_t elr_el1;
  algn32_t esr_el1;
  uint64_t far_el1;
  uint64_t mair_el1;
  algn32_t mdscr_el1;
  uint64_t par_el1;
  algn32_t sctlr_el1;
  uint64_t sp_el1;
  algn32_t spsr_el1;
  uint64_t tcr_el1;
  uint64_t tpidr_el1;
  uint64_t ttbr0_el1;
  uint64_t ttbr1_el1;
  uint64_t vbar_el1;

  uint64_t elr_el2;
  algn32_t spsr_el2;
};

struct GuestState {
  uint64_t x[GS_NUM_REGS];
  FpState fp_state;
  SystemState system_state;

  // Exit state.
  algn32_t cntv_ctl_el0;
  uint64_t cntv_cval_el0;
  algn32_t esr_el2;
  uint64_t far_el2;
  uint64_t hpfar_el2;
  uint64_t vmpidr_el2;

  // Get the guest's exception level (EL).
  //
  // We only expect guests to be running in EL0 or EL1, though the
  // status bits support up to EL2.
  uint32_t el() const { return BITS_SHIFT(system_state.spsr_el2, 3, 2); }
};

struct HostState {
  // We only save X15, X18..X30 from the host, as the host is making an explicit
  // call into the hypervisor, and therefore is saving the rest of its state.
  uint64_t x[HS_NUM_REGS];
  FpState fp_state;
  SystemState system_state;
};

struct IchState {
  uint8_t num_aprs;
  uint8_t num_lrs;
  algn32_t vmcr;
  algn32_t misr;
  uint64_t elrsr;
  uint64_t apr[IS_MAX_APR_GROUPS][IS_MAX_APRS];
  uint64_t lr[IS_MAX_LRS];
};

struct El2State {
  bool resume;
  GuestState guest_state;
  HostState host_state;
  IchState ich_state;
};

static_assert(sizeof(El2State) <= PAGE_SIZE);

static_assert(offsetof(FpState, q) == FS_Q0);
static_assert(offsetof(FpState, q[FS_NUM_REGS - 1]) == FS_Q(FS_NUM_REGS - 1));
static_assert(offsetof(FpState, fpsr) == FS_FPSR);
static_assert(offsetof(FpState, fpcr) == FS_FPCR);

static_assert(offsetof(SystemState, sp_el0) == SS_SP_EL0);
static_assert(offsetof(SystemState, tpidr_el0) == SS_TPIDR_EL0);
static_assert(offsetof(SystemState, tpidrro_el0) == SS_TPIDRRO_EL0);
static_assert(offsetof(SystemState, cntkctl_el1) == SS_CNTKCTL_EL1);
static_assert(offsetof(SystemState, contextidr_el1) == SS_CONTEXTIDR_EL1);
static_assert(offsetof(SystemState, cpacr_el1) == SS_CPACR_EL1);
static_assert(offsetof(SystemState, csselr_el1) == SS_CSSELR_EL1);
static_assert(offsetof(SystemState, elr_el1) == SS_ELR_EL1);
static_assert(offsetof(SystemState, esr_el1) == SS_ESR_EL1);
static_assert(offsetof(SystemState, far_el1) == SS_FAR_EL1);
static_assert(offsetof(SystemState, mair_el1) == SS_MAIR_EL1);
static_assert(offsetof(SystemState, mdscr_el1) == SS_MDSCR_EL1);
static_assert(offsetof(SystemState, par_el1) == SS_PAR_EL1);
static_assert(offsetof(SystemState, sctlr_el1) == SS_SCTLR_EL1);
static_assert(offsetof(SystemState, sp_el1) == SS_SP_EL1);
static_assert(offsetof(SystemState, spsr_el1) == SS_SPSR_EL1);
static_assert(offsetof(SystemState, tcr_el1) == SS_TCR_EL1);
static_assert(offsetof(SystemState, tpidr_el1) == SS_TPIDR_EL1);
static_assert(offsetof(SystemState, ttbr0_el1) == SS_TTBR0_EL1);
static_assert(offsetof(SystemState, ttbr1_el1) == SS_TTBR1_EL1);
static_assert(offsetof(SystemState, vbar_el1) == SS_VBAR_EL1);
static_assert(offsetof(SystemState, elr_el2) == SS_ELR_EL2);
static_assert(offsetof(SystemState, spsr_el2) == SS_SPSR_EL2);

static_assert(offsetof(El2State, resume) == ES_RESUME);

static_assert(offsetof(El2State, guest_state.x) == GS_X0);
static_assert(offsetof(El2State, guest_state.x[GS_NUM_REGS - 1]) == GS_X(GS_NUM_REGS - 1));
static_assert(offsetof(El2State, guest_state.fp_state) == GS_FP_STATE);
static_assert(offsetof(El2State, guest_state.fp_state.q) == GS_FP_STATE + FS_Q0);
static_assert(offsetof(El2State, guest_state.system_state) == GS_SYSTEM_STATE);
static_assert(offsetof(El2State, guest_state.cntv_ctl_el0) == GS_CNTV_CTL_EL0);
static_assert(offsetof(El2State, guest_state.cntv_cval_el0) == GS_CNTV_CVAL_EL0);
static_assert(offsetof(El2State, guest_state.vmpidr_el2) == GS_VMPIDR_EL2);
static_assert(offsetof(El2State, guest_state.esr_el2) == GS_ESR_EL2);
static_assert(offsetof(El2State, guest_state.far_el2) == GS_FAR_EL2);
static_assert(offsetof(El2State, guest_state.hpfar_el2) == GS_HPFAR_EL2);

static_assert(offsetof(El2State, host_state.x) == HS_XREGS);
static_assert(offsetof(El2State, host_state.x[HS_NUM_REGS - 1]) ==
              HS_XREGS + HS_X(HS_NUM_REGS - 1));
static_assert(offsetof(El2State, host_state.fp_state) == HS_FP_STATE);
static_assert(offsetof(El2State, host_state.fp_state.q) == HS_FP_STATE + FS_Q0);
static_assert(offsetof(El2State, host_state.system_state) == HS_SYSTEM_STATE);

static_assert(offsetof(IchState, num_aprs) == IS_NUM_APRS);
static_assert(offsetof(IchState, num_lrs) == IS_NUM_LRS);
static_assert(offsetof(IchState, vmcr) == IS_VMCR);
static_assert(offsetof(IchState, misr) == IS_MISR);
static_assert(offsetof(IchState, elrsr) == IS_ELRSR);
static_assert(offsetof(IchState, apr) == IS_AP0R0);
static_assert(offsetof(IchState, apr[IS_MAX_APR_GROUPS - 1][IS_MAX_APRS - 1]) ==
              IS_APR(IS_MAX_APR_GROUPS - 1, IS_MAX_APRS - 1));
static_assert(offsetof(IchState, lr) == IS_LR0);
static_assert(offsetof(IchState, lr[IS_MAX_LRS - 1]) == IS_LR(IS_MAX_LRS - 1));

__BEGIN_CDECLS

extern zx_status_t arm64_el2_on(zx_paddr_t ttbr0, zx_paddr_t stack_top, uint64_t tcr,
                                uint64_t vtcr);
extern zx_status_t arm64_el2_off();
extern zx_status_t arm64_el2_tlbi_ipa(uint64_t vttbr, zx_vaddr_t addr, bool terminal);
extern zx_status_t arm64_el2_tlbi_vmid(uint64_t vttbr);
extern zx_status_t arm64_el2_enter(uint64_t vttbr, zx_paddr_t state, uint64_t hcr);

// Invalidate all TLB entries in EL2.
extern zx_status_t arm64_el2_tlbi_el2();

__END_CDECLS

#endif  // __ASSEMBLER__

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_HYPERVISOR_EL2_STATE_H_
