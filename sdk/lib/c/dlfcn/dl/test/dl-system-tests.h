// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DL_TEST_DL_SYSTEM_TESTS_H_
#define LIB_DL_TEST_DL_SYSTEM_TESTS_H_

#include <dlfcn.h>  // for dlinfo

#include "dl-load-tests-base.h"

#ifdef __Fuchsia__
#include "dl-load-zircon-tests-base.h"
#endif

namespace dl::testing {

#ifdef __Fuchsia__
using DlSystemLoadTestsBase = DlLoadZirconTestsBase;
#else
using DlSystemLoadTestsBase = DlLoadTestsBase;
#endif

class DlSystemTests : public DlSystemLoadTestsBase {
 public:
  // This test fixture does not need to match on exact error text, since the
  // error message can vary between different system implementations.
  static constexpr bool kCanMatchExactError = false;
#ifdef __Fuchsia__
  // Musl always prioritizes a loaded module for symbol lookup.
  static constexpr bool kStrictLoadOrderPriority = true;
  // Musl does not validate flag values for dlopen's mode argument.
  static constexpr bool kCanValidateMode = false;
  // Musl will emit a "symbol not found" error for scenarios where glibc or
  // libdl will emit an "undefined symbol" error.
  static constexpr bool kEmitsSymbolNotFound = true;
  // Fuchsia's dlclose is a no-op.
  static constexpr bool kDlCloseCanRunFinalizers = false;
  static constexpr bool kDlCloseUnloadsModules = false;
  // Musl will "double-count" in its `.dlpi_adds` counter a module that was
  // reused because of DT_SONAME match. For example, if a previously loaded
  // module had a DT_SONAME that matched the filename of a module that is about
  // to be loaded, Musl will reuse the previously loaded module, but it will
  // still increment the counter as if a new module was loaded.
  static constexpr bool kInaccurateLoadCountAfterSonameMatch = true;
  // Musl attempts to fetch the same shlib from the filesystem twice, when its
  // DT_SONAME is matched with another module in a linking session.
  static constexpr bool kSonameLookupInPendingDeps = false;
#endif

  fit::result<Error, void*> DlOpen(const char* file, int mode);

  fit::result<Error> DlClose(void* module);

  static fit::result<Error, void*> DlSym(void* module, const char* ref);

  static int DlIteratePhdr(DlIteratePhdrCallback* callback, void* data);

  // ExpectRootModule or Needed are called by tests when a file is expected to
  // be loaded from the file system for the first time. The following functions
  // will call DlOpen(file, RTLD_NOLOAD) to ensure that `file` is not already
  // loaded (e.g. by a previous test).
  void ExpectRootModule(std::string_view name);

  void Needed(std::initializer_list<std::string_view> names);

  void Needed(std::initializer_list<std::pair<std::string_view, bool>> name_found_pairs);

  void CleanUpOpenedFile(void* ptr) override { ASSERT_TRUE(DlClose(ptr).is_ok()); }

  // This function is a no-op for system tests, since they manage their own TLS
  // setup.
  void PrepareForTlsAccess() {}

  // Call the system's dlinfo to fill in the link map for the given handle, and
  // return it to the caller.
  static const link_map* ModuleLinkMap(void* handle) {
    struct link_map* info = nullptr;
    EXPECT_EQ(dlinfo(handle, RTLD_DI_LINKMAP, static_cast<void*>(&info)), 0) << dlerror();
    return info;
  }

 private:
  // This will call the system dlopen in an OS-specific context. This method is
  // defined directly on this test fixture rather than its OS-tailored base
  // classes because the logic it performs is only needed for testing the
  // system dlopen by this test fixture.
  void* CallDlOpen(const char* file, int mode);

  // DlOpen `name` with `RTLD_NOLOAD` to ensure this will be the first time the
  // file is loaded from the filesystem.
  void NoLoadCheck(std::string_view name);
};

}  // namespace dl::testing

#endif  // LIB_DL_TEST_DL_SYSTEM_TESTS_H_
