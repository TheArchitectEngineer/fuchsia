// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_TESTING_UI_TEST_MANAGER_UI_TEST_MANAGER_H_
#define SRC_UI_TESTING_UI_TEST_MANAGER_UI_TEST_MANAGER_H_

#include <fuchsia/session/scene/cpp/fidl.h>
#include <fuchsia/ui/composition/cpp/fidl.h>
#include <fuchsia/ui/display/singleton/cpp/fidl.h>
#include <fuchsia/ui/focus/cpp/fidl.h>
#include <fuchsia/ui/observation/test/cpp/fidl.h>
#include <fuchsia/ui/test/scene/cpp/fidl.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/sys/cpp/service_directory.h>

#include <memory>
#include <optional>

#include "src/lib/fxl/macros.h"
#include "src/ui/testing/ui_test_realm/ui_test_realm.h"
#include "src/ui/testing/util/screenshot_helper.h"

namespace ui_testing {

using fuchsia::ui::composition::ScreenshotFormat;

// Library class to manage test realm and scene setup on behalf of UI
// integration test clients.
class UITestManager : public fuchsia::ui::focus::FocusChainListener {
 public:
  explicit UITestManager(UITestRealm::Config config);
  ~UITestManager() override = default;

  // Adds a child to the realm under construction, and returns the new child.
  // Must NOT be called after BuildRealm().
  component_testing::Realm AddSubrealm();

  // Calls realm_builder_.Build();
  void BuildRealm();

  // Calls realm_.Teardown();
  void TeardownRealm(component_testing::ScopedChild::TeardownCallback on_teardown_complete);

  // Returns a clone of the realm's exposed services directory.
  // Clients should call this method once, and retain the handle returned.
  //
  // MUST be called AFTER BuildRealm().
  std::unique_ptr<sys::ServiceDirectory> CloneExposedServicesDirectory();

  // Creates the root of the scene (either via scene manager or by direct
  // construction), and attaches the client view via
  // fuchsia.ui.app.ViewProvider.
  //
  // MUST be called AFTER BuildRealm().
  //
  // `use_scene_provider` indicates whether UITestManager should use
  // `fuchsia.ui.test.scene.Controller` to initialize the scene, or use the raw
  // scene manager APIs.
  void InitializeScene();

  // Returns the view ref koid of the client view if it's available, and false
  // otherwise.
  //
  // NOTE: Different scene owners have different policies about client view
  // refs, so users should NOT use this method as a proxy for determining that
  // the client view is attached to the scene. Use |ClientViewIsRendering| for
  // that purpose.
  std::optional<zx_koid_t> ClientViewRefKoid();

  // Convenience method to inform the client if its view is rendering.
  // Syntactic sugar for `ViewIsRendering(ClientViewRefKoid())`.
  //
  // Returns true if the client's view ref koid is present in the most recent
  // view tree snapshot received from scenic.
  bool ClientViewIsRendering();

  // Convenience method to inform the client if its view is focused.
  bool ClientViewIsFocused();

  // Convenience method to inform if a view is focused by its koid.
  bool ViewIsFocused(zx_koid_t view_ref_koid);

  // Convenience method that returns the scale factor applied to the client view.
  float ClientViewScaleFactor();

  // Convenience method to inform the client if the view specified by
  // `view_ref_koid` is rendering content.
  //
  // Returns true if `view_ref_koid` is present in the most recent view tree
  // snapshot received from scenic.
  bool ViewIsRendering(zx_koid_t view_ref_koid);

  // Attempts to find the `ViewDescriptor` for a view with `view_ref_koid` in the most recent
  // `ViewTreeSnapshot`.
  //
  // Returns the descriptor if it is found, or `std::nullopt` if no view with the given
  // `view_ref_koid` could be found.
  std::optional<fuchsia::ui::observation::geometry::ViewDescriptor> FindViewFromSnapshotByKoid(
      zx_koid_t view_ref_koid);

  // Returns the width and height of the display in pixels as returned by
  // |fuchsia.ui.display.singleton| protocol.
  std::pair<uint64_t, uint64_t> GetDisplayDimensions() const;

  // Takes a screenshot using the |fuchsia.ui.composition.Screenshot| protocol, converts it into
  // |ui_testing::Screenshot| and returns it. Note that this is a blocking call i.e the client has
  // to wait until |fuchsia.ui.composition.Screenshot.Take| finishes execution.
  Screenshot TakeScreenshot(ScreenshotFormat format = ScreenshotFormat::BGRA_RAW) const;

 private:
  // Helper method to monitor the state of the view tree continuously.
  void Watch();

  // |fuchsia::ui::focus::FocusChainListener|
  void OnFocusChange(fuchsia::ui::focus::FocusChain focus_chain,
                     OnFocusChangeCallback callback) override;

  // Manages test realm configuration.
  UITestRealm realm_;

  // FIDL endpoints used to drive scene business logic.
  fuchsia::ui::observation::test::RegistrySyncPtr observer_registry_;
  fuchsia::ui::observation::geometry::ViewTreeWatcherPtr view_tree_watcher_;
  fidl::Binding<fuchsia::ui::focus::FocusChainListener> focus_chain_listener_binding_;
  fuchsia::ui::test::scene::ControllerPtr scene_controller_;
  fuchsia::ui::composition::ScreenshotSyncPtr screenshotter_;

  // Client view's `ViewRef` kernel object ID.
  std::optional<zx_koid_t> client_view_ref_koid_ = std::nullopt;

  // Holds the most recent view tree snapshot received from the view tree
  // watcher.
  //
  // From this snapshot, we can retrieve relevant view tree state on demand,
  // e.g. if the client view is rendering content.
  std::optional<fuchsia::ui::observation::geometry::ViewTreeSnapshot> last_view_tree_snapshot_;

  // Holds the most recent focus chain received from the view tree watcher.
  std::optional<fuchsia::ui::focus::FocusChain> last_focus_chain_;

  uint64_t display_width_ = 0;

  uint64_t display_height_ = 0;

  // |UITestRealm::Config::display_rotation|.
  int display_rotation_ = 0;

  FXL_DISALLOW_COPY_ASSIGN_AND_MOVE(UITestManager);
};

}  // namespace ui_testing

#endif  // SRC_UI_TESTING_UI_TEST_MANAGER_UI_TEST_MANAGER_H_
