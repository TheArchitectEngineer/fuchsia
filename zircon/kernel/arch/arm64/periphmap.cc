// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include "arch/arm64/periphmap.h"

#include <align.h>
#include <lib/console.h>
#include <lib/instrumentation/asan.h>
#include <stdint.h>

#include <arch/arm64/mmu.h>
#include <arch/defines.h>
#include <ktl/bit.h>
#include <ktl/optional.h>
#include <phys/handoff.h>
#include <vm/vm.h>
#include <vm/vm_address_region.h>
#include <vm/vm_aspace.h>

#include <ktl/enforce.h>

#define PERIPH_RANGE_MAX 4

typedef struct {
  uint64_t base_phys;
  uint64_t base_virt;
  uint64_t length;
} periph_range_t;

static periph_range_t periph_ranges[PERIPH_RANGE_MAX] = {};

namespace {
struct Phys2VirtTrait {
  static uint64_t src(const periph_range_t& r) { return r.base_phys; }
  static uint64_t dst(const periph_range_t& r) { return r.base_virt; }
};

struct Virt2PhysTrait {
  static uint64_t src(const periph_range_t& r) { return r.base_virt; }
  static uint64_t dst(const periph_range_t& r) { return r.base_phys; }
};

template <typename Fetch>
struct PeriphUtil {
  // Translate (without range checking) the (virt|phys) peripheral provided to
  // its (phys|virt) counterpart using the provided range.
  static uint64_t Translate(const periph_range_t& range, uint64_t addr) {
    return addr - Fetch::src(range) + Fetch::dst(range);
  }

  // Find the index (if any) of the peripheral range which contains the
  // (virt|phys) address <addr>
  static ktl::optional<uint32_t> LookupNdx(uint64_t addr) {
    for (uint32_t i = 0; i < ktl::size(periph_ranges); ++i) {
      const auto& range = periph_ranges[i];
      if (range.length == 0) {
        break;
      } else if (addr >= Fetch::src(range)) {
        uint64_t offset = addr - Fetch::src(range);
        if (offset < range.length) {
          return {i};
        }
      }
    }
    return {};
  }

  // Map the (virt|phys) peripheral provided to its (phys|virt) counterpart (if
  // any)
  static ktl::optional<uint64_t> Map(uint64_t addr) {
    auto ndx = LookupNdx(addr);
    if (ndx.has_value()) {
      return Translate(periph_ranges[ndx.value()], addr);
    }
    return {};
  }
};

using Phys2Virt = PeriphUtil<Phys2VirtTrait>;
using Virt2Phys = PeriphUtil<Virt2PhysTrait>;

template <typename T>
uint64_t rd_reg(vaddr_t addr) {
  return static_cast<uint64_t>(reinterpret_cast<volatile T*>(addr)[0]);
}

template <typename T>
void wr_reg(vaddr_t addr, uint64_t val) {
  reinterpret_cast<volatile T*>(addr)[0] = static_cast<T>(val);
}

// Note; the choice of these values must also align with the definitions in the
// options array below.
enum class AccessWidth {
  Byte = 0,
  Halfword = 1,
  Word = 2,
  Doubleword = 3,
};
constexpr struct {
  const char* tag;
  void (*print)(uint64_t);
  uint64_t (*rd)(vaddr_t);
  void (*wr)(vaddr_t, uint64_t);
  uint32_t byte_width;
} kDumpModOptions[] = {
    {
        .tag = "byte",
        .print = [](uint64_t val) { printf(" %02" PRIx64, val); },
        .rd = rd_reg<uint8_t>,
        .wr = wr_reg<uint8_t>,
        .byte_width = 1,
    },
    {
        .tag = "halfword",
        .print = [](uint64_t val) { printf(" %04" PRIx64, val); },
        .rd = rd_reg<uint16_t>,
        .wr = wr_reg<uint16_t>,
        .byte_width = 2,
    },
    {
        .tag = "word",
        .print = [](uint64_t val) { printf(" %08" PRIx64, val); },
        .rd = rd_reg<uint32_t>,
        .wr = wr_reg<uint32_t>,
        .byte_width = 4,
    },
    {
        .tag = "doubleword",
        .print = [](uint64_t val) { printf(" %016" PRIx64, val); },
        .rd = rd_reg<uint64_t>,
        .wr = wr_reg<uint64_t>,
        .byte_width = 8,
    },
};

zx_status_t dump_periph(paddr_t phys, uint64_t count, AccessWidth width) {
  const auto& opt = kDumpModOptions[static_cast<uint32_t>(width)];

  // Sanity check count
  if (!count) {
    printf("Illegal count %lu\n", count);
    return ZX_ERR_INVALID_ARGS;
  }

  uint64_t byte_amt = count * opt.byte_width;
  paddr_t phys_end_addr = phys + byte_amt - 1;

  // Sanity check alignment.
  if (phys & (opt.byte_width - 1)) {
    printf("%016lx is not aligned to a %u byte boundary!\n", phys, opt.byte_width);
    return ZX_ERR_INVALID_ARGS;
  }

  // Validate that the entire requested range fits within a single mapping.
  auto start_ndx = Phys2Virt::LookupNdx(phys);
  auto end_ndx = Phys2Virt::LookupNdx(phys_end_addr);
  if (!start_ndx.has_value() || !end_ndx.has_value() || (start_ndx.value() != end_ndx.value())) {
    printf("Physical range [%016lx, %016lx] is not contained in a single mapping!\n", phys,
           phys_end_addr);
    return ZX_ERR_INVALID_ARGS;
  }

  // OK, all of our sanity checks are complete.  Time to start dumping data.
  constexpr uint32_t bytes_per_line = 16;
  const uint64_t count_per_line = bytes_per_line / opt.byte_width;
  vaddr_t virt = Phys2Virt::Translate(periph_ranges[start_ndx.value()], phys);
  vaddr_t virt_end_addr = virt + byte_amt;

  printf("Dumping %lu %s%s starting at phys 0x%016lx\n", count, opt.tag, count == 1 ? "" : "s",
         phys);
  while (virt < virt_end_addr) {
    printf("%016lx :", phys);
    for (uint64_t i = 0; (i < count_per_line) && (virt < virt_end_addr);
         ++i, virt += opt.byte_width) {
      opt.print(opt.rd(virt));
    }
    phys += bytes_per_line;
    printf("\n");
  }

  return ZX_OK;
}

zx_status_t mod_periph(paddr_t phys, uint64_t val, AccessWidth width) {
  const auto& opt = kDumpModOptions[static_cast<uint32_t>(width)];

  // Sanity check alignment.
  if (phys & (opt.byte_width - 1)) {
    printf("%016lx is not aligned to a %u byte boundary!\n", phys, opt.byte_width);
    return ZX_ERR_INVALID_ARGS;
  }

  // Translate address
  auto vaddr = Phys2Virt::Map(phys);
  if (!vaddr.has_value()) {
    printf("Physical addr %016lx in not in the peripheral mappings!\n", phys);
  }

  // Perform the write, then report what we did.
  opt.wr(vaddr.value(), val);
  printf("Wrote");
  opt.print(val);
  printf(" to phys addr %016lx\n", phys);

  return ZX_OK;
}

int cmd_peripheral_map(int argc, const cmd_args* argv, uint32_t flags) {
  auto usage = [cmd = argv[0].str](bool not_enough_args = false) -> zx_status_t {
    if (not_enough_args) {
      printf("not enough arguments\n");
    }

    printf("usage:\n");
    printf("%s dump\n", cmd);
    printf("%s phys2virt <addr>\n", cmd);
    printf("%s virt2phys <addr>\n", cmd);
    printf(
        "%s dd|dw|dh|db <phys_addr> [<count>] :: Dump <count> (double|word|half|byte) from "
        "<phys_addr> (count default = 1)\n",
        cmd);
    printf(
        "%s md|mw|mh|mb <phys_addr> <value> :: Write the contents of <value> to the "
        "(double|word|half|byte) at <phys_addr>\n",
        cmd);

    return ZX_ERR_INTERNAL;
  };

  if (argc < 2) {
    return usage(true);
  }

  if (!strcmp(argv[1].str, "dump")) {
    uint32_t i = 0;
    for (const auto& range : periph_ranges) {
      if (range.length) {
        printf("Phys [%016lx, %016lx] ==> Virt [%016lx, %016lx] (len 0x%08lx)\n", range.base_phys,
               range.base_phys + range.length - 1, range.base_virt,
               range.base_virt + range.length - 1, range.length);
        ++i;
      }
    }
    printf("Dumped %u defined peripheral map ranges\n", i);
  } else if (!strcmp(argv[1].str, "phys2virt") || !strcmp(argv[1].str, "virt2phys")) {
    if (argc < 3) {
      return usage(true);
    }

    bool phys_src = !strcmp(argv[1].str, "phys2virt");
    auto map_fn = phys_src ? Phys2Virt::Map : Virt2Phys::Map;
    auto res = map_fn(argv[2].u);
    if (res.has_value()) {
      printf("%016lx ==> %016lx\n", argv[2].u, res.value());
    } else {
      printf("Failed to find the %s address 0x%016lx in the peripheral mappings.\n",
             phys_src ? "physical" : "virtual", argv[2].u);
    }
  } else if ((argv[1].str[0] == 'd') || (argv[1].str[0] == 'm')) {
    // If this is a valid display or modify command, its length will be exactly 2.
    if (strlen(argv[1].str) != 2) {
      return usage();
    }

    // Parse the next letter to figure out the width of the operation.
    AccessWidth width;
    switch (argv[1].str[1]) {
      case 'd':
        width = AccessWidth::Doubleword;
        break;
      case 'w':
        width = AccessWidth::Word;
        break;
      case 'h':
        width = AccessWidth::Halfword;
        break;
      case 'b':
        width = AccessWidth::Byte;
        break;
      default:
        return usage();
    }

    paddr_t phys_addr = argv[2].u;
    if (argv[1].str[0] == 'd') {
      // Dump commands have a default count of 1
      return dump_periph(phys_addr, (argc < 4) ? 1 : argv[3].u, width);
    } else {
      // Modify commands are required to have a value.
      return (argc < 4) ? usage(true) : mod_periph(phys_addr, argv[3].u, width);
    }

  } else {
    return usage();
  }

  return ZX_OK;
}

}  // namespace

zx_status_t add_periph_range(paddr_t base_phys, size_t length) {
  // peripheral ranges are allocated below the temporary hand-off data, which
  // is itself located below the kernel image.
  //
  // TODO(https://fxbug.dev/42164859): This dependency on the location of the
  // temporary hand-off VMAR will soon go away once periphmap mappings are made
  // in physboot.
  uintptr_t base_virt = gPhysHandoff->temporary_vmar.get()->base;

  // give ourselves an extra gap of space to try to catch overruns
  base_virt -= 0x10000;

  DEBUG_ASSERT(IS_PAGE_ALIGNED(base_phys));
  DEBUG_ASSERT(IS_PAGE_ALIGNED(length));

  // Periph ranges is fixed size stack, where the first non allocated range
  // is represented by having 0 length.
  for (auto& range : periph_ranges) {
    // Finihsed iterating all allocated ranges, with no range already
    // containing this range.
    if (range.length == 0) {  // No range containing.
      base_virt -= length;

      // Round down to try to align the mappings to maximize usage of large pages
      uint64_t phys_log = ktl::countr_zero(base_phys);
      uint64_t len_log = log2_floor(length);

      // This is clamped to the minimal supported page size.
      uint64_t log2_align = ktl::min(ktl::min(phys_log, len_log), 30UL);  // No point aligning > 1GB
      if (log2_align < PAGE_SIZE_SHIFT) {
        log2_align = PAGE_SIZE_SHIFT;
      }
      base_virt = ROUNDDOWN(base_virt, 1UL << log2_align);

      auto status = arm64_boot_map_v(base_virt, base_phys, length, MMU_INITIAL_MAP_DEVICE, true);
      if (status == ZX_OK) {
        range.base_phys = base_phys;
        range.base_virt = base_virt;
        range.length = length;
      }
      return status;
    }

    // Mapping already covered.
    if (range.base_phys <= base_phys && range.length >= base_phys - range.base_phys + length) {
      return ZX_OK;
    }

    base_virt = range.base_virt;
  }
  return ZX_ERR_OUT_OF_RANGE;
}

void reserve_periph_ranges() {
  fbl::RefPtr<VmAddressRegion> vmar = VmAspace::kernel_aspace()->RootVmar();
  // Peripheral ranges are read/write device mappings.
  const uint arch_mmu_flags =
      ARCH_MMU_FLAG_UNCACHED_DEVICE | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;
  for (auto& range : periph_ranges) {
    if (range.length == 0) {
      break;
    }

    dprintf(INFO, "Periphmap: reserving physical %#lx virtual [%#lx, %#lx) flags %#x\n",
            range.base_phys, range.base_virt, range.base_virt + range.length, arch_mmu_flags);
    zx_status_t status =
        vmar->ReserveSpace("periph", range.base_virt, range.length, arch_mmu_flags);
    ASSERT_MSG(status == ZX_OK, "status %d\n", status);

#if __has_feature(address_sanitizer)
    asan_map_shadow_for(range.base_virt, range.length);
#endif  // __has_feature(address_sanitizer)
  }
}

vaddr_t periph_paddr_to_vaddr(paddr_t paddr) {
  auto ret = Phys2Virt::Map(paddr);
  return ret.has_value() ? ret.value() : 0;
}

STATIC_COMMAND_START
STATIC_COMMAND_MASKED("pm", "peripheral mapping commands", &cmd_peripheral_map, CMD_AVAIL_ALWAYS)
STATIC_COMMAND_END(pmap)
