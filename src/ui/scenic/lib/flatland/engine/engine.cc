// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/engine/engine.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <lib/async/cpp/time.h>
#include <lib/syslog/cpp/macros.h>

#include <sstream>
#include <string>

#include "src/ui/scenic/lib/flatland/global_image_data.h"
#include "src/ui/scenic/lib/flatland/global_matrix_data.h"
#include "src/ui/scenic/lib/flatland/global_topology_data.h"
#include "src/ui/scenic/lib/flatland/scene_dumper.h"
#include "src/ui/scenic/lib/scheduling/frame_scheduler.h"
#include "src/ui/scenic/lib/utils/logging.h"

// Hardcoded double buffering.
// TODO(https://fxbug.dev/42156567): make this configurable.  Even fancier: is it worth considering
// sharing a pool of framebuffers between multiple displays?  (assuming that their dimensions are
// similar, etc.)
static constexpr uint32_t kNumDisplayFramebuffers = 2;

namespace flatland {

namespace {

void SignalAll(const std::vector<zx::event>& events) {
  for (auto& e : events) {
    e.signal(0u, ZX_EVENT_SIGNALED);
  }
}

}  // namespace

Engine::Engine(std::shared_ptr<DisplayCompositor> flatland_compositor,
               std::shared_ptr<FlatlandPresenterImpl> flatland_presenter,
               std::shared_ptr<UberStructSystem> uber_struct_system,
               std::shared_ptr<LinkSystem> link_system, inspect::Node inspect_node,
               GetRootTransformFunc get_root_transform)
    : flatland_compositor_(std::move(flatland_compositor)),
      flatland_presenter_(std::move(flatland_presenter)),
      uber_struct_system_(std::move(uber_struct_system)),
      link_system_(std::move(link_system)),
      inspect_node_(std::move(inspect_node)),
      get_root_transform_(std::move(get_root_transform)) {
  FX_DCHECK(flatland_compositor_);
  FX_DCHECK(flatland_presenter_);
  FX_DCHECK(uber_struct_system_);
  FX_DCHECK(link_system_);
  InitializeInspectObjects();
}

constexpr char kSceneDump[] = "scene_dump";

void Engine::InitializeInspectObjects() {
  inspect_scene_dump_ = inspect_node_.CreateLazyValues(kSceneDump, [this] {
    inspect::Inspector inspector;
    const auto root_transform = get_root_transform_();
    if (!root_transform) {
      inspector.GetRoot().CreateString(kSceneDump, "(No Root Transform)", &inspector);
      return fpromise::make_ok_promise(std::move(inspector));
    }

    const SceneState scene_state(*this, *root_transform);
    std::ostringstream output;
    DumpScene(scene_state.snapshot, scene_state.topology_data, scene_state.images,
              scene_state.image_indices, scene_state.image_rectangles, output);
    inspector.GetRoot().CreateString(kSceneDump, output.str(), &inspector);
    return fpromise::make_ok_promise(std::move(inspector));
  });

  inspect_frame_results_ = inspect_node_.CreateChild("Frame result counts");
  inspect_direct_display_frame_count_ = inspect_frame_results_.CreateUint("Direct to display", 0);
  inspect_gpu_composition_frame_count_ = inspect_frame_results_.CreateUint("GPU composition", 0);
  inspect_failed_frame_count_ = inspect_frame_results_.CreateUint("Failed", 0);
}

void Engine::RenderScheduledFrame(uint64_t frame_number, zx::time presentation_time,
                                  const FlatlandDisplay& display,
                                  scheduling::FramePresentedCallback callback) {
  // Emit a counter called "ScenicRender" for visualization in the Trace Viewer.
  //
  // This counter is flipped between 0 and 1 and back on each frame, and is
  // used to visually delineate successive frames in the sometimes busy trace
  // view.
  static bool render_edge_flag = false;
  TRACE_COUNTER("gfx", "ScenicRender", 0, "", TA_UINT32(render_edge_flag = !render_edge_flag));
  // NOTE: this name is important for benchmarking.  Do not remove or modify it
  // without also updating the "process_gfx_trace.go" script.
  TRACE_DURATION("gfx", "RenderFrame", "frame_number", frame_number, "time",
                 presentation_time.get());
  TRACE_FLOW_STEP("gfx", "scenic_frame", frame_number);

  SceneState scene_state(*this, display.root_transform());
  scenic_impl::display::Display* const hw_display = display.display();

#if defined(USE_FLATLAND_VERBOSE_LOGGING)
  std::ostringstream str;
  str << "Engine::RenderScheduledFrame() frame_number=" << frame_number
      << "\nRoot transform of global topology: " << scene_state.topology_data.topology_vector[0]
      << "\nTopologically-sorted transforms and their corresponding parent transforms:";
  for (size_t i = 1; i < scene_state.topology_data.topology_vector.size(); ++i) {
    str << "\n        " << scene_state.topology_data.topology_vector[i] << " -> "
        << scene_state.topology_data.topology_vector[scene_state.topology_data.parent_indices[i]];
  }
  str << "\nFrame display-list contains " << scene_state.image_rectangles.size()
      << " image-rectangles and " << scene_state.images.size() << " images.";
  for (auto& r : scene_state.image_rectangles) {
    str << "\n        rect: " << r;
  }
  for (size_t i = 0; i < scene_state.image_indices.size(); ++i) {
    str << "\n        image: "
        << scene_state.topology_data.topology_vector[scene_state.image_indices[i]] << " "
        << scene_state.images[i];
  }
  FLATLAND_VERBOSE_LOG << str.str();
#endif

  link_system_->UpdateLinkWatchers(scene_state.topology_data.topology_vector,
                                   scene_state.topology_data.live_handles,
                                   scene_state.global_matrices, scene_state.snapshot);
  link_system_->UpdateDevicePixelRatio(hw_display->device_pixel_ratio());

  // TODO(https://fxbug.dev/42156567): hack!  need a better place to call AddDisplay().
  if (hack_seen_display_id_values_.find(hw_display->display_id().value) ==
      hack_seen_display_id_values_.end()) {
    // This display hasn't been added to the DisplayCompositor yet.
    hack_seen_display_id_values_.insert(hw_display->display_id().value);

    DisplayInfo display_info{
        .dimensions = glm::uvec2{hw_display->width_in_px(), hw_display->height_in_px()},
        .formats = display.display()->pixel_formats()};

    fuchsia::sysmem2::BufferCollectionInfo render_target_info;
    flatland_compositor_->AddDisplay(hw_display, display_info,
                                     /*num_vmos*/ kNumDisplayFramebuffers, &render_target_info);
  }

  CullRectanglesInPlace(&scene_state.image_rectangles, &scene_state.images,
                        hw_display->width_in_px(), hw_display->height_in_px());

  {
    TRACE_DURATION("gfx", "flatland::Engine::RenderScheduledFrame[move topology_data]");
    last_global_topology_data_ = std::move(scene_state.topology_data);
  }

  // Don't render any initial frames if there is no image that could actually be rendered. We do
  // this to avoid triggering any changes in the display until we have content ready to render. We
  // invoke |callback| to continue the render loop.
  if (!first_frame_with_image_is_rendered_) {
    if (scene_state.images.empty()) {
      SkipRender(std::move(callback));
      return;
    }
    first_frame_with_image_is_rendered_ = true;
  }

  auto frame_result = flatland_compositor_->RenderFrame(
      frame_number, presentation_time,
      {{.rectangles = std::move(scene_state.image_rectangles),
        .images = std::move(scene_state.images),
        .display_id = hw_display->display_id()}},
      flatland_presenter_->TakeReleaseFences(), std::move(callback));
  RecordFrameResult(frame_result);
}

void Engine::RecordFrameResult(DisplayCompositor::RenderFrameResult result) {
  switch (result) {
    case DisplayCompositor::RenderFrameResult::kDirectToDisplay:
      inspect_direct_display_frame_count_.Add(1);
      break;
    case DisplayCompositor::RenderFrameResult::kGpuComposition:
      inspect_gpu_composition_frame_count_.Add(1);
      break;
    case DisplayCompositor::RenderFrameResult::kFailure:
      inspect_failed_frame_count_.Add(1);
      break;
  }
}

view_tree::SubtreeSnapshot Engine::GenerateViewTreeSnapshot(
    const TransformHandle& root_transform) const {
  TRACE_DURATION("gfx", "flatland::Engine::GenerateViewTreeSnapshot");
  const auto& uber_struct_snapshot = uber_struct_system_->Snapshot();
  const auto link_child_to_parent_transform_map = link_system_->GetLinkChildToParentTransformMap();
  const auto& topology_data = last_global_topology_data_;

  const auto matrix_vector = ComputeGlobalMatrices(
      topology_data.topology_vector, topology_data.parent_indices, uber_struct_snapshot);
  auto global_clip_regions =
      ComputeGlobalTransformClipRegions(topology_data.topology_vector, topology_data.parent_indices,
                                        matrix_vector, uber_struct_snapshot);
  auto hit_regions =
      ComputeGlobalHitRegions(topology_data.topology_vector, topology_data.parent_indices,
                              matrix_vector, uber_struct_snapshot);

  return flatland::GlobalTopologyData::GenerateViewTreeSnapshot(
      topology_data, std::move(hit_regions), std::move(global_clip_regions), matrix_vector,
      link_child_to_parent_transform_map);
}

// TODO(https://fxbug.dev/42162342) If we put Screenshot on its own thread, we should make this call
// thread safe.
Renderables Engine::GetRenderables(const FlatlandDisplay& display) {
  TransformHandle root = display.root_transform();

  SceneState scene_state(*this, root);
  const auto hw_display = display.display();
  CullRectanglesInPlace(&scene_state.image_rectangles, &scene_state.images,
                        hw_display->width_in_px(), hw_display->height_in_px());

  return std::make_pair(std::move(scene_state.image_rectangles), std::move(scene_state.images));
}

Engine::SceneState::SceneState(Engine& engine, TransformHandle root_transform) {
  TRACE_DURATION("gfx", "flatland::Engine::SceneState");
  snapshot = engine.uber_struct_system_->Snapshot();

  const auto links = engine.link_system_->GetResolvedTopologyLinks();
  const auto link_system_id = engine.link_system_->GetInstanceId();

  topology_data = GlobalTopologyData::ComputeGlobalTopologyData(snapshot, links, link_system_id,
                                                                root_transform);
  global_matrices =
      ComputeGlobalMatrices(topology_data.topology_vector, topology_data.parent_indices, snapshot);

  auto [indices, im] =
      ComputeGlobalImageData(topology_data.topology_vector, topology_data.parent_indices, snapshot);
  this->image_indices = std::move(indices);
  this->images = std::move(im);

  const auto global_image_sample_regions = ComputeGlobalImageSampleRegions(
      topology_data.topology_vector, topology_data.parent_indices, snapshot);

  const auto global_clip_regions = ComputeGlobalTransformClipRegions(
      topology_data.topology_vector, topology_data.parent_indices, global_matrices, snapshot);

  image_rectangles =
      ComputeGlobalRectangles(FilterByIndices(global_matrices, image_indices),
                              FilterByIndices(global_image_sample_regions, image_indices),
                              FilterByIndices(global_clip_regions, image_indices), images);
}

void Engine::SkipRender(scheduling::FramePresentedCallback callback) {
  SignalAll(flatland_presenter_->TakeReleaseFences());
  const zx::time now = async::Now(async_get_default_dispatcher());
  callback({.render_done_time = now, .actual_presentation_time = now});
}

}  // namespace flatland
