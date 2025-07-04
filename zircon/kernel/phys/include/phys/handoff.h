// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_HANDOFF_H_
#define ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_HANDOFF_H_

// `offsetof(PhysHandoff, kernel_physical_load_address)`, for use in assembly.
//
// TODO(https://fxbug.dev/379891035): We only need this for x86 kASan
// page-table set-up in start.S, which can go away with proper kASan support in
// physboot.
#define PHYS_HANDOFF_KERNEL_PHYSICAL_LOAD_ADDRESS 0x8

#ifndef __ASSEMBLER__

// Note: we refrain from using the ktl namespace as <phys/handoff.h> is
// expected to be compiled in the userboot toolchain.

#include <lib/arch/ticks.h>
#include <lib/crypto/entropy_pool.h>
#include <lib/memalloc/range.h>
#include <lib/uart/all.h>
#include <lib/zbi-format/board.h>
#include <lib/zbi-format/cpu.h>
#include <lib/zbi-format/memory.h>
#include <lib/zbi-format/reboot.h>
#include <lib/zbi-format/zbi.h>
#include <stddef.h>
#include <zircon/assert.h>
#include <zircon/types.h>

#include <array>
#include <bitset>
#include <optional>
#include <string_view>
#include <type_traits>

#include <phys/arch/arch-handoff.h>

#include "handoff-ptr.h"

struct BootOptions;

// This holds arch::EarlyTicks timestamps collected by physboot before the
// kernel proper is cognizant.  Once the platform timer hardware is set up for
// real, platform_convert_early_ticks translates these values into zx_instant_mono_ticks_t
// values that can be published as kcounters and then converted to actual time
// units in userland via zx_ticks_per_second().
//
// platform_convert_early_ticks returns zero if arch::EarlyTicks samples cannot
// be accurately converted to zx_instant_mono_ticks_t.  This can happen on suboptimal x86
// hardware, where the early samples are in TSC but the platform timer decides
// that a synchronized and monotonic TSC is not available on the machine.
class PhysBootTimes {
 public:
  // These are various time points sampled during physboot's work.
  // kernel/top/handoff.cc has a kcounter corresponding to each of these.
  // When a new time point is added here, a new kcounter must be added
  // there to make that sample visible anywhere.
  enum Index : size_t {
    kZbiEntry,         // ZBI entry from boot loader.
    kPhysSetup,        // Earliest/arch-specific phys setup (e.g. paging).
    kDecompressStart,  // Begin decompression.
    kDecompressEnd,    // STORAGE_KERNEL decompressed.
    kZbiDone,          // ZBI items have been ingested.
    kCount
  };

  constexpr arch::EarlyTicks Get(Index i) const { return timestamps_[i]; }

  constexpr void Set(Index i, arch::EarlyTicks ts) { timestamps_[i] = ts; }

  void SampleNow(Index i) { Set(i, arch::EarlyTicks::Get()); }

 private:
  arch::EarlyTicks timestamps_[kCount] = {};
};

// A base class for VM object descriptions.
struct PhysVmObject {
  using Name = std::array<char, ZX_MAX_NAME_LEN>;

  constexpr auto operator<=>(const PhysVmObject& other) const = default;

  constexpr void set_name(std::string_view new_name) {
    ZX_DEBUG_ASSERT(new_name.size() < name.size());
    new_name.copy(name.data(), name.size() - 1);
    name[new_name.size()] = '\0';
  }

  Name name{};
};
static_assert(std::is_default_constructible_v<PhysVmObject>);

// VMOs to publish as is.
struct PhysVmo : public PhysVmObject {
  // The maximum number of additional VMOs expected to be in the hand-off
  // beyond the special ones explicitly enumerated.
  static constexpr size_t kMaxExtraHandoffPhysVmos = 3;

  // It's useful to normalize VMO order on physical base address for more
  // readable kernel start-up logging.
  constexpr auto operator<=>(const PhysVmo& other) const { return addr <=> other.addr; }

  // The full page-aligned size of the memory.
  constexpr size_t size_bytes() const { return (content_size + ZX_PAGE_SIZE - 1) & -ZX_PAGE_SIZE; }

  // The physical address of the memory.
  uintptr_t addr = 0;
  size_t content_size = 0;
};
static_assert(std::is_default_constructible_v<PhysVmo>);

// Describes a virtual mapping present at the time of hand-off, the virtual
// address range of which should be reserved during VM initialization.
struct PhysMapping : public PhysVmObject {
  // The type of memory being mapped.
  enum class Type { kNormal, kMmio };

  class Permissions {
   public:
    static Permissions Ro() { return Permissions{}.set_readable(); }
    static Permissions Rw() { return Permissions{}.set_readable().set_writable(); }
    static Permissions Rx() { return Permissions{}.set_readable().set_executable(); }
    static Permissions Xom() { return Permissions{}.set_executable(); }

    // This works on anything with .readable(), .writable(), and .executable()
    // methods, which includes this class itself as well as elfldltl::LoadInfo
    // segment types.
    static Permissions FromSegment(const auto& segment) {
      return Permissions{}
          .set_readable(segment.readable())
          .set_writable(segment.writable())
          .set_executable(segment.executable());
    }

    constexpr Permissions() = default;

    bool operator==(const Permissions&) const = default;

    constexpr bool readable() const { return perms_[kReadable]; }
    constexpr bool writable() const { return perms_[kWritable]; }
    constexpr bool executable() const { return perms_[kExecutable]; }

    Permissions& set_readable(bool value = true) {
      perms_.set(kReadable, value);
      return *this;
    }

    Permissions& set_writable(bool value = true) {
      perms_.set(kWritable, value);
      return *this;
    }

    Permissions& set_executable(bool value = true) {
      perms_.set(kExecutable, value);
      return *this;
    }

    Permissions& operator|=(const Permissions& other) {
      perms_ |= other.perms_;
      return *this;
    }

   private:
    static constexpr size_t kReadable = 0;
    static constexpr size_t kWritable = 1;
    static constexpr size_t kExecutable = 2;

    std::bitset<3> perms_;
  };

  constexpr PhysMapping() = default;

  constexpr PhysMapping(std::string_view name, Type type, uintptr_t vaddr, size_t size,
                        uintptr_t paddr, Permissions perms, bool kasan_shadow = true)
      : type(type),
        vaddr(vaddr),
        size(size),
        paddr(paddr),
        perms(perms),
        kasan_shadow(kasan_shadow) {
    set_name(name);
  }

  // It's useful to normalize mapping order on virtual base addr for more
  // readable kernel start-up logging.
  constexpr auto operator<=>(const PhysMapping& other) const { return vaddr <=> other.vaddr; }

  constexpr uintptr_t vaddr_end() const { return vaddr + size; }
  constexpr uintptr_t paddr_end() const { return paddr + size; }

  Type type = Type::kNormal;
  uintptr_t vaddr = 0;
  size_t size = 0;
  uintptr_t paddr = 0;
  Permissions perms;

  // TODO(https://fxbug.dev/379891035): Revisit handing this information off -
  // once there is first-class kASan support in physboot.
  bool kasan_shadow = true;
};
static_assert(std::is_default_constructible_v<PhysMapping>);

// The virtual address range intended to be occupied only by an associated,
// logical grouping of mappings, to be realized as a proper VMAR during VM
// initialization.
struct PhysVmar : public PhysVmObject {
  constexpr bool operator==(const PhysVmar& other) const = default;

  // It's useful to normalize VMAR order on base address for more readable
  // kernel start-up logging.
  constexpr auto operator<=>(const PhysVmar& other) const { return base <=> other.base; }

  constexpr uintptr_t end() const { return base + size; }

#if HANDOFF_PTR_DEREF
  // The union/OR-ing of all associated mapping permissions.
  PhysMapping::Permissions permissions() const {
    PhysMapping::Permissions perms;
    for (const auto& mapping : mappings.get()) {
      perms |= mapping.perms;
    }
    return perms;
  }
#endif

  uintptr_t base = 0;
  size_t size = 0;
  PhysHandoffTemporarySpan<const PhysMapping> mappings;
};
static_assert(std::is_default_constructible_v<PhysVmar>);

// This combines a PhysVmo containing an ELF image with information on how to
// perform ELF loading for it.  The PhysVmar is repurposed to describe a VMAR
// that should be created at an arbitrary address (its .base is always 0).  The
// mappings within use vaddr relative to that base, and each PhysMapping::paddr
// is in fact an offset into the VMO rather than a physical address.
struct PhysElfImage {
  struct Info {
    uintptr_t relative_entry_point = 0;  // Add to VMAR base address.
    std::optional<size_t> stack_size;
  };

  // This value in .vmar.mappings[n].paddr indicates the mapping is for
  // zero-fill pages rather than pages from the PhysVmo.
  static constexpr uintptr_t kZeroFill = -1;

  PhysVmo vmo;
  PhysVmar vmar;
  Info info;
};

struct MappedMmioRange {
  volatile void* base = nullptr;
  size_t size = 0;
};

// This holds (or points to) everything that is handed off from physboot to the
// kernel proper at boot time.
struct PhysHandoff {
  // Whether the given type represents physical memory that should be turned
  // into a VMO.
  static bool IsPhysVmoType(memalloc::Type type) {
    switch (type) {
      case memalloc::Type::kDataZbi:
      case memalloc::Type::kPhysDebugdata:
      case memalloc::Type::kPhysLog:
      case memalloc::Type::kUserboot:
      case memalloc::Type::kVdso:
        return true;
      default:
        break;
    }
    return false;
  }

  constexpr bool Valid() const { return magic == kMagic; }

  static constexpr uint64_t kMagic = 0xfeedfaceb002da2a;

  const uint64_t magic = kMagic;

  // The physical address at which the kernel is to be loaded.
  uintptr_t kernel_physical_load_address = 0;

  PhysHandoffPermanentPtr<const BootOptions> boot_options;

  PhysBootTimes times;
  static_assert(std::is_default_constructible_v<PhysBootTimes>);

  PhysHandoffPermanentString version_string;

  // Permanent VMARs to construct along with mapped regions within. The VMARs
  // will be sorted by base address, and the mappings within each VMAR will
  // similarly be sorted by virtual address.
  PhysHandoffTemporarySpan<const PhysVmar> vmars;

  // A VMAR comprising all temporary hand-off mappings, including that of the
  // PhysHandoff itself.
  PhysHandoffTemporaryPtr<const PhysVmar> temporary_vmar;

  // The data ZBI.
  PhysVmo zbi;

  // The vDSO.
  PhysElfImage vdso;

  // Userboot.
  PhysElfImage userboot;

  // Additional VMOs to be published to userland as-is and not otherwise used by
  // the kernel proper.
  PhysHandoffTemporarySpan<const PhysVmo> extra_vmos;

  // Entropy gleaned from ZBI Items such as 'ZBI_TYPE_SECURE_ENTROPY' and/or command line.
  std::optional<crypto::EntropyPool> entropy_pool;

  // ZBI container of items to be propagated in mexec.
  PhysHandoffPermanentSpan<const std::byte> mexec_data;

  // Architecture-specific content.
  ArchPhysHandoff arch_handoff;
  static_assert(std::is_default_constructible_v<ArchPhysHandoff>);

  // A normalized accounting of RAM (and peripheral ranges). It consists of
  // ranges that are maximally contiguous and in sorted order, and features
  // allocations that are of interest to the kernel.
  PhysHandoffTemporarySpan<const memalloc::Range> memory;

  // ZBI_TYPE_CPU_TOPOLOGY payload (or translated legacy equivalents).
  PhysHandoffTemporarySpan<const zbi_topology_node_t> cpu_topology;

  // ZBI_TYPE_CRASHLOG payload.
  PhysHandoffTemporaryString crashlog;

  // ZBI_TYPE_HW_REBOOT_REASON payload.
  std::optional<zbi_hw_reboot_reason_t> reboot_reason;

  // ZBI_TYPE_NVRAM payload.
  // A physical memory region that will persist across warm boots.
  std::optional<zbi_nvram_t> nvram;

  // ZBI_TYPE_PLATFORM_ID payload.
  std::optional<zbi_platform_id_t> platform_id;

  // ZBI_TYPE_ACPI_RSDP payload.
  // Physical address of the ACPI RSDP (Root System Descriptor Pointer).
  std::optional<uint64_t> acpi_rsdp;

  // ZBI_TYPE_SMBIOS payload.
  // Physical address of the SMBIOS tables.
  std::optional<uint64_t> smbios_phys;

  // ZBI_TYPE_EFI_MEMORY_ATTRIBUTES_TABLE payload.
  // EFI memory attributes table.
  PhysHandoffTemporarySpan<const std::byte> efi_memory_attributes;

  // ZBI_TYPE_EFI_SYSTEM_TABLE payload.
  // Physical address of the EFI system table.
  std::optional<uint64_t> efi_system_table;

  // Initialized UART to be used by the kernel, if any.
  uart::all::Driver uart;

  // The UART's mapped MMIO range, if present and MMIO-based.
  MappedMmioRange uart_mmio;
};
static_assert(std::is_default_constructible_v<PhysHandoff>);

// PhysHandoff does not have a standard layout due to some non-standard
// members, but it's standard enough that we'd expect to be able to use
// offsetof() on its members, especially on early ones like this.
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Winvalid-offsetof"
static_assert(offsetof(PhysHandoff, kernel_physical_load_address) ==
              PHYS_HANDOFF_KERNEL_PHYSICAL_LOAD_ADDRESS);
#pragma GCC diagnostic pop

extern PhysHandoff* gPhysHandoff;

// This is the entry point function for the ELF kernel.
extern "C" [[noreturn]] void PhysbootHandoff(PhysHandoff* handoff);

#endif  // __ASSEMBLER__

#endif  // ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_HANDOFF_H_
