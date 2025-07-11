// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// DO NOT EDIT. Generated from FIDL library
//   zbi (//sdk/fidl/zbi/driver-config.fidl)
// by zither, a Fuchsia platform tool.

#ifndef LIB_ZBI_FORMAT_DRIVER_CONFIG_H_
#define LIB_ZBI_FORMAT_DRIVER_CONFIG_H_

#include <stdint.h>

#if defined(__cplusplus)
extern "C" {
#endif

// ZBI_TYPE_KERNEL_DRIVER item types (for zbi_header_t.extra)
typedef uint32_t zbi_kernel_driver_t;

// 'PSCI'
#define ZBI_KERNEL_DRIVER_ARM_PSCI ((zbi_kernel_driver_t)(0x49435350u))

// 'PSCS'
#define ZBI_KERNEL_DRIVER_ARM_PSCI_CPU_SUSPEND ((zbi_kernel_driver_t)(0x53435350u))

// 'GIC2'
#define ZBI_KERNEL_DRIVER_ARM_GIC_V2 ((zbi_kernel_driver_t)(0x32434947u))

// 'GIC3'
#define ZBI_KERNEL_DRIVER_ARM_GIC_V3 ((zbi_kernel_driver_t)(0x33434947u))

// 'ATIM'
#define ZBI_KERNEL_DRIVER_ARM_GENERIC_TIMER ((zbi_kernel_driver_t)(0x4d495441u))

// 'ATMM'
#define ZBI_KERNEL_DRIVER_ARM_GENERIC_TIMER_MMIO ((zbi_kernel_driver_t)(0x4d4d5441u))

// 'PL0U'
#define ZBI_KERNEL_DRIVER_PL011_UART ((zbi_kernel_driver_t)(0x55304c50u))

// 'AMLU'
#define ZBI_KERNEL_DRIVER_AMLOGIC_UART ((zbi_kernel_driver_t)(0x554c4d41u))

// 'AMLH'
#define ZBI_KERNEL_DRIVER_AMLOGIC_HDCP ((zbi_kernel_driver_t)(0x484c4d41u))

// 'DW8U'
#define ZBI_KERNEL_DRIVER_DW8250_UART ((zbi_kernel_driver_t)(0x44573855u))

// 'RMLH' (typoed, originally intended to by 'AMLR')
#define ZBI_KERNEL_DRIVER_AMLOGIC_RNG_V1 ((zbi_kernel_driver_t)(0x484c4d52u))

// 'AMLR'
#define ZBI_KERNEL_DRIVER_AMLOGIC_RNG_V2 ((zbi_kernel_driver_t)(0x524c4d41u))

// 'WD32'
#define ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG ((zbi_kernel_driver_t)(0x32334457u))

// 'GENI'
#define ZBI_KERNEL_DRIVER_GENI_UART ((zbi_kernel_driver_t)(0x494E4547u))

// '8250'
#define ZBI_KERNEL_DRIVER_I8250_PIO_UART ((zbi_kernel_driver_t)(0x30353238u))

// '825M'
#define ZBI_KERNEL_DRIVER_I8250_MMIO32_UART ((zbi_kernel_driver_t)(0x4d353238u))

// '825B'
#define ZBI_KERNEL_DRIVER_I8250_MMIO8_UART ((zbi_kernel_driver_t)(0x42353238u))

// 'MMTP'
#define ZBI_KERNEL_DRIVER_MOTMOT_POWER ((zbi_kernel_driver_t)(0x4d4d5450u))

// '370P'
#define ZBI_KERNEL_DRIVER_AS370_POWER ((zbi_kernel_driver_t)(0x50303733u))

// 'MNFP'
#define ZBI_KERNEL_DRIVER_MOONFLOWER_POWER ((zbi_kernel_driver_t)(0x4d4e4650u))

// 'IMXU'
#define ZBI_KERNEL_DRIVER_IMX_UART ((zbi_kernel_driver_t)(0x55584d49u))

// 'PLIC'
#define ZBI_KERNEL_DRIVER_RISCV_PLIC ((zbi_kernel_driver_t)(0x43494C50u))

// 'RTIM'
#define ZBI_KERNEL_DRIVER_RISCV_GENERIC_TIMER ((zbi_kernel_driver_t)(0x4D495452u))

// 'PXAU'
#define ZBI_KERNEL_DRIVER_PXA_UART ((zbi_kernel_driver_t)(0x50584155u))

// 'EXYU'
#define ZBI_KERNEL_DRIVER_EXYNOS_USI_UART ((zbi_kernel_driver_t)(0x45585955u))

// Kernel driver struct that can be used for simple drivers.
// Used by ZBI_KERNEL_DRIVER_PL011_UART, ZBI_KERNEL_DRIVER_AMLOGIC_UART, and
// ZBI_KERNEL_DRIVER_GENI_UART, ZBI_KERNEL_DRIVER_I8250_MMIO_UART.
typedef struct {
  uint64_t mmio_phys;
  uint32_t irq;
  uint32_t flags;
} zbi_dcfg_simple_t;

typedef uint32_t zbi_kernel_driver_irq_flags_t;

// When no flag is set, implies no information was obtained, and the
// kernel will apply default configuration as it sees fit.
#define ZBI_KERNEL_DRIVER_IRQ_FLAGS_EDGE_TRIGGERED ((zbi_kernel_driver_irq_flags_t)(1u << 0))
#define ZBI_KERNEL_DRIVER_IRQ_FLAGS_LEVEL_TRIGGERED ((zbi_kernel_driver_irq_flags_t)(1u << 1))

// Interpretation depends on whether is edge or level triggered.
// When `LEVEL_TRIGGERED` refers to `ACTIVE_LOW`.
// When `EDGE_TRIGGERED` refers to `HIGH_TO_LOW`.
#define ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_LOW ((zbi_kernel_driver_irq_flags_t)(1u << 2))

// Interpretation depends on whether is edge or level triggered.
// When `LEVEL_TRIGGERED` refers to `ACTIVE_HIGH`.
// When `EDGE_TRIGGERED` refers to `LOW_TO_HIGH`.
#define ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_HIGH ((zbi_kernel_driver_irq_flags_t)(1u << 3))

// Used by ZBI_KERNEL_DRIVER_I8250_PIO_UART.
typedef struct {
  uint16_t base;
  uint16_t reserved;
  uint32_t irq;
} zbi_dcfg_simple_pio_t;

// for ZBI_KERNEL_DRIVER_ARM_PSCI
typedef struct {
  uint8_t use_hvc;
  uint8_t reserved[7];
  uint64_t shutdown_args[3];
  uint64_t reboot_args[3];
  uint64_t reboot_bootloader_args[3];
  uint64_t reboot_recovery_args[3];
} zbi_dcfg_arm_psci_driver_t;

// for ZBI_KERNEL_DRIVER_ARM_GIC_V2
typedef struct {
  uint64_t mmio_phys;
  uint64_t msi_frame_phys;
  uint64_t gicd_offset;
  uint64_t gicc_offset;
  uint64_t gich_offset;
  uint64_t gicv_offset;
  uint32_t ipi_base;
  uint8_t optional;
  uint8_t use_msi;
  uint16_t reserved;
} zbi_dcfg_arm_gic_v2_driver_t;

// for ZBI_KERNEL_DRIVER_ARM_GIC_V3
typedef struct {
  uint64_t mmio_phys;
  uint64_t gicd_offset;
  uint64_t gicr_offset;
  uint64_t gicr_stride;
  uint64_t reserved0;
  uint32_t ipi_base;
  uint8_t optional;
  uint8_t reserved1[3];
} zbi_dcfg_arm_gic_v3_driver_t;

// for ZBI_KERNEL_DRIVER_ARM_GENERIC_TIMER
typedef struct {
  uint32_t irq_phys;
  uint32_t irq_virt;
  uint32_t irq_sphys;
  uint32_t freq_override;
} zbi_dcfg_arm_generic_timer_driver_t;

typedef struct {
  // Base address for the frame's EL1 view.
  uint64_t mmio_phys_el1;

  // Base address for the frame's EL0 view.
  // This is optional.
  uint64_t mmio_phys_el0;

  // IRQ information for physical timer. This is mandatory.
  uint32_t irq_phys;

  // Same scheme as `DcfgSimple::irq`. This is mandatory.
  uint32_t irq_phys_flags;

  // IRQ information for virtual timer.
  // This is optional.
  // When is not present both `irq_virt` and `irq_virt_flags` will be zero.
  uint32_t irq_virt;

  // Same scheme as `DcfgSimple::irq`.
  uint32_t irq_virt_flags;
} zbi_dcfg_arm_generic_timer_mmio_frame_t;

// for ZBI_KERNEL_DRIVER_ARM_GENERIC_TIMER_MMIO
typedef struct {
  // Base address of `CNTCTLBase` frame.
  uint64_t mmio_phys;

  // The frequency of the main counter for the timer.
  uint32_t frequency;

  // Bitmask containing the set of active frames.
  // The `i-th` frame is considered active iff the `i`-th bit is set.
  // Note: While there may be up to 8 frames, both missing and disabled frames are treated
  // as inactive. Disabled frame information will be present, while missing frames will be zeroed.
  uint8_t active_frames_mask;
  uint8_t reserved0[3];

  // Information for each individual frame.
  // Inactive frames must be zero-filled.
  zbi_dcfg_arm_generic_timer_mmio_frame_t frames[8];
} zbi_dcfg_arm_generic_timer_mmio_driver_t;

// for ZBI_KERNEL_DRIVER_AMLOGIC_HDCP
typedef struct {
  uint64_t preset_phys;
  uint64_t hiu_phys;
  uint64_t hdmitx_phys;
} zbi_dcfg_amlogic_hdcp_driver_t;

// for ZBI_KERNEL_DRIVER_AMLOGIC_RNG_V1
// for ZBI_KERNEL_DRIVER_AMLOGIC_RNG_V2
typedef struct {
  uint64_t rng_data_phys;
  uint64_t rng_status_phys;
  uint64_t rng_refresh_interval_usec;
} zbi_dcfg_amlogic_rng_driver_t;

// Defines a register write action for a generic kernel watchdog driver.  An
// action consists of the following steps.
//
// 1) Read from the register located a physical address |addr|
// 2) Clear all of the bits in the value which was read using the |clr_mask|
// 3) Set all of the bits in the value using the |set_mask|
// 4) Write this value back to the address located at addr
typedef struct {
  uint64_t addr;
  uint32_t clr_mask;
  uint32_t set_mask;
} zbi_dcfg_generic32_watchdog_action_t;

typedef uint32_t zbi_kernel_driver_generic32_watchdog_flags_t;

#define ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_FLAGS_ENABLED \
  ((zbi_kernel_driver_generic32_watchdog_flags_t)(1u << 0))

// 1ms
#define ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_MIN_PERIOD ((int64_t)(1000000u))

// Definitions of actions which may be taken by a generic 32 bit watchdog timer
// kernel driver which may be passed by a bootloader.  Field definitions are as
// follows.
typedef struct {
  // The address and masks needed to "pet" (aka, dismiss) a hardware watchdog timer.
  zbi_dcfg_generic32_watchdog_action_t pet_action;

  // The address and masks needed to enable a hardware watchdog timer.  If enable
  // is an unsupported operation, the addr of the |enable_action| shall be zero.
  zbi_dcfg_generic32_watchdog_action_t enable_action;

  // The address and masks needed to disable a hardware watchdog timer.  If
  // disable is an unsupported operation, the addr of the |disable_action| shall
  // be zero.
  zbi_dcfg_generic32_watchdog_action_t disable_action;

  // The period of the watchdog timer given in nanoseconds.  When enabled, the
  // watchdog timer driver must pet the watch dog at least this often.  The value
  // must be at least 1 mSec, typically much larger (on the order of a second or
  // two).
  int64_t watchdog_period_nsec;

  // Storage for additional flags.  Currently, only one flag is defined,
  // "FLAG_ENABLED".  When this flag is set, it indicates that the watchdog timer
  // was left enabled by the bootloader at startup.
  zbi_kernel_driver_generic32_watchdog_flags_t flags;
  uint32_t reserved;
} zbi_dcfg_generic32_watchdog_t;

// for ZBI_KERNEL_DRIVER_RISCV_PLIC
typedef struct {
  uint64_t mmio_phys;
  uint32_t num_irqs;
  uint32_t reserved;
} zbi_dcfg_riscv_plic_driver_t;

// for ZBI_KERNEL_DRIVER_RISCV_GENERIC_TIMER
typedef struct {
  uint32_t freq_hz;
  uint32_t reserved;
} zbi_dcfg_riscv_generic_timer_driver_t;

typedef uint32_t zbi_arm_psci_cpu_suspend_state_flags_t;

// If set, when entering the associated low power state the CPU's architectural timer will be
// turned off, making it an unsuitable source for exiting the low power state.
// A different source must be programmed.
#define ZBI_ARM_PSCI_CPU_SUSPEND_STATE_FLAGS_LOCAL_TIMER_STOPS \
  ((zbi_arm_psci_cpu_suspend_state_flags_t)(1u << 0))

// If set, the PSCI CPU Suspend operation will affect the entire power domain, implying all other
// CPUs of the power domain must be in a low power mode. That is, the last CPU in the power
// domain is the one to enter this power state.
#define ZBI_ARM_PSCI_CPU_SUSPEND_STATE_FLAGS_TARGETS_POWER_DOMAIN \
  ((zbi_arm_psci_cpu_suspend_state_flags_t)(1u << 1))

// The ZBI_KERNEL_DRIVER_ARM_PSCI_CPU_SUSPEND's payload consists on any number of
// `DcfgArmPsciCpuSuspendState` entries.
//
// The length of the item is `sizeof(zbi_dcfg_arm_psci_cou_suspend_state_t)` times the number of
// entries. Each entry describes an 'idle state' that can be entered through PSCI CPU Suspend call.
//
// Entries in the table may be in any order, and only a single item of type
// ZBI_KERNEL_DRIVER_ARM_PSCI_CPU_SUSPEND should be present in the ZBI.
typedef struct {
  // Unique identifier representing this suspend state.
  uint32_t id;

  // PSCI power_state as described in "Section 5.4.2. of Arm Power State Coordination Interface"
  // v1.3.
  uint32_t power_state;
  zbi_arm_psci_cpu_suspend_state_flags_t flags;

  // Latency in microseconds to enter the low power state.
  uint32_t entry_latency_us;

  // Latency in microseconds to exit the low power state.
  uint32_t exit_latency_us;

  // Minimum time in microseconds, including `entry_latency`, to stay in this low power state.
  // Spending less time would be inefficient energy-wise.
  uint32_t min_residency_us;
} zbi_dcfg_arm_psci_cpu_suspend_state_t;

#if defined(__cplusplus)
}
#endif

#endif  // LIB_ZBI_FORMAT_DRIVER_CONFIG_H_
