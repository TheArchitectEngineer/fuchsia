// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013 Google, Inc.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <debug.h>
#include <lib/console.h>
#include <lib/version.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <ktl/byte.h>
#include <ktl/span.h>
#include <ktl/unique_ptr.h>
#include <lk/init.h>
#include <phys/handoff.h>
#include <vm/vm.h>

#include <ktl/enforce.h>

// Forward declaration so the type can be used in `extern "C"` first.  The
// `extern "C"` declarations have to stay outside the anonymous namespace.
namespace {
struct build_id_note;
}  // namespace

extern "C" const struct build_id_note __build_id_note_start;
extern "C" const ktl::byte __build_id_note_end[];

namespace {

// The kernel is linked at 0, making the bias the load address.
constexpr uintptr_t kKernelLoadBias = kArchHandoffVirtualAddress;

ktl::string_view gVersionString;

// If the build ID were SHA256, it would be 32 bytes.
// (The algorithms used for build IDs today actually produce fewer than that.)
// This string needs 2 bytes to print each byte in hex, plus a NUL terminator.
char gElfBuildIdString[65];

// Standard ELF note layout (Elf{32,64}_Nhdr in <elf.h>).
// The name and type fields' values are what GNU and GNU-compatible
// tools (i.e. everything in the Unix-like world in recent years)
// specify for build ID notes.
struct build_id_note {
  uint32_t namesz;
  uint32_t descsz;
  uint32_t type;
#define NT_GNU_BUILD_ID 3
#define NOTE_NAME "GNU"
  char name[(sizeof(NOTE_NAME) + 3) & -4];
  ktl::byte id[];
};

void init_build_id(uint level) {
  const build_id_note* const note = &__build_id_note_start;
  if (note->type != NT_GNU_BUILD_ID || note->namesz != sizeof(NOTE_NAME) ||
      memcmp(note->name, NOTE_NAME, sizeof(NOTE_NAME)) != 0 ||
      &note->id[note->descsz] != __build_id_note_end) {
    panic("ELF build ID note has bad format!\n");
  }
  ktl::span<const ktl::byte> id = ElfBuildId();
  if (id.size() * 2 >= sizeof(gElfBuildIdString)) {
    panic("ELF build ID is %zu bytes, expected %zu or fewer\n", id.size(),
          sizeof(gElfBuildIdString) / 2);
  }
  for (size_t i = 0; i < id.size(); ++i) {
    snprintf(&gElfBuildIdString[i * 2], 3, "%02x", static_cast<unsigned int>(id[i]));
  }
}

// This must happen before print_version below and should happen as early as possible to ensure we
// get useful backtraces when the kernel panics.
LK_INIT_HOOK(elf_build_id, &init_build_id, LK_INIT_LEVEL_EARLIEST)

void print_module(FILE* f, const char* build_id) {
  fprintf(f, "{{{module:0:kernel:elf:%s}}}\n", build_id);
}

// TODO(eieio): Consider whether it makes sense to locate the logic for printing
// mappings somewhere else (perhaps in vm/vm.cpp?).
void print_mmap(FILE* f, const void* begin, const void* end, const char* perm) {
  const uintptr_t runtime_start = reinterpret_cast<uintptr_t>(begin);
  const size_t size = reinterpret_cast<uintptr_t>(end) - runtime_start;
  const uintptr_t static_start = runtime_start - kKernelLoadBias;
  if (size != 0) {
    fprintf(f, "{{{mmap:%#lx:%#lx:load:0:%s:%#lx}}}\n", runtime_start, size, perm, static_start);
  }
}

void init_version(uint level) {
  gVersionString = gPhysHandoff->version_string.get();
  ASSERT(!gVersionString.empty());
  print_version();
}

LK_INIT_HOOK(version, init_version, LK_INIT_LEVEL_EARLIEST + 1)

}  // namespace

const char* elf_build_id_string() { return gElfBuildIdString; }

ktl::string_view VersionString() { return gVersionString; }

ktl::span<const ktl::byte> ElfBuildId() {
  const build_id_note* const note = &__build_id_note_start;
  return {note->id, note->descsz};
}

void print_version() {
  dprintf(ALWAYS, "version:\n");
  dprintf(ALWAYS, "\tarch:     %s\n", ARCH);
  dprintf(ALWAYS, "\tzx_system_get_version_string: %.*s\n", static_cast<int>(gVersionString.size()),
          gVersionString.data());
  dprintf(ALWAYS, "\tELF build ID: %s\n", gElfBuildIdString);
  dprintf(ALWAYS, "\tLK_DEBUGLEVEL: %d\n", LK_DEBUGLEVEL);
}

void PrintSymbolizerContext(FILE* f) {
  fprintf(f, "{{{reset}}}\n");
  print_module(f, gElfBuildIdString);
  // These four mappings match the mappings printed by vm_init().
  print_mmap(f, __code_start, __code_end, "rx");
  print_mmap(f, __rodata_start, __rodata_end, "r");
  print_mmap(f, __relro_start, __relro_end, "r");
  print_mmap(f, __data_start, __data_end, "rw");
  print_mmap(f, __bss_start, _end, "rw");
}

void print_backtrace_version_info(FILE* f) {
  fprintf(f, "zx_system_get_version_string %.*s\n\n", static_cast<int>(gVersionString.size()),
          gVersionString.data());

  // Log the ELF build ID in the format the symbolizer scripts understand.
  if (gElfBuildIdString[0] != '\0') {
    PrintSymbolizerContext(f);
    fprintf(f, "dso: id=%s base=%#lx name=physzircon\n", gElfBuildIdString,
            reinterpret_cast<uintptr_t>(__executable_start));
  }
}

static int cmd_version(int argc, const cmd_args* argv, uint32_t flags) {
  print_version();
  return 0;
}

STATIC_COMMAND_START
STATIC_COMMAND("version", "print version", &cmd_version)
STATIC_COMMAND_END(version)
