// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "scene_dumper.h"

#include <lib/async/default.h>

#include <stack>

#include <sdk/lib/syslog/cpp/macros.h>

#include "src/lib/fostr/fidl/fuchsia.math/amendments.h"
#include "src/lib/fsl/handles/object_info.h"
#include "src/ui/scenic/lib/allocation/id.h"
#include "src/ui/scenic/lib/flatland/global_image_data.h"
#include "src/ui/scenic/lib/flatland/global_matrix_data.h"
#include "src/ui/scenic/lib/flatland/global_topology_data.h"

namespace {

constexpr char kIndentation[] = " | ";

inline void IndentLine(size_t current_indentation_level, std::ostream& output) {
  for (size_t i = 0; i < current_indentation_level; i++) {
    output << kIndentation;
  }
}

std::ostream& operator<<(std::ostream& str, const fuchsia::ui::composition::HitTestInteraction& h) {
  switch (h) {
    case fuchsia::ui::composition::HitTestInteraction::DEFAULT:
      return str << "default";
    case fuchsia::ui::composition::HitTestInteraction::SEMANTICALLY_INVISIBLE:
      return str << "semantically_invisible";
    default:
      return str << "unknown";
  }
}

// Dumps the connected topology by outputting information on the current node and then iteratively
// dumping for direct children nodes. The topology vector is organized in a preordered depth-first
// order.
void DumpTopology(const flatland::UberStruct::InstanceMap& snapshot,
                  const flatland::GlobalTopologyData& topology_data, std::ostream& output) {
  output << "Topology:\n";
  std::stack<size_t> indentation_levels;
  std::stack<uint64_t> parent_session_ids;
  for (size_t transform_index = 0; transform_index < topology_data.topology_vector.size();
       transform_index++) {
    auto& transform = topology_data.topology_vector[transform_index];
    const auto children = topology_data.child_counts[transform_index];
    auto current_indentation_level = indentation_levels.size();

    // Print indented, no-parentheses transform.
    IndentLine(current_indentation_level, output);
    output << transform.GetInstanceId() << ":" << transform.GetTransformId();

    // If the transform has children, print the pipe.
    if (children > 0) {
      output << "-|";
    }

    // Every time we cross a viewport/view boundary, print out the `debug_name` of the view's
    // Flatland session.
    const auto uber_struct_it = snapshot.find(transform.GetInstanceId());
    if (uber_struct_it != snapshot.end() && !parent_session_ids.empty() &&
        transform.GetInstanceId() != parent_session_ids.top() &&
        !uber_struct_it->second->debug_name.empty()) {
      const auto& view_ref = uber_struct_it->second->view_ref;
      output << " <-- (" << uber_struct_it->second->debug_name
             << " koid:" << (view_ref ? fsl::GetKoid(view_ref->reference.get()) : 0) << ")";
    }

    // Newline.
    output << '\n';

    // Adjust indentation for newline.
    if (children > 0) {
      indentation_levels.push(children);
      parent_session_ids.push(transform.GetInstanceId());
    } else {
      while (!indentation_levels.empty()) {
        auto& current_indentation_level_children = indentation_levels.top();
        current_indentation_level_children--;
        if (current_indentation_level_children == 0) {
          indentation_levels.pop();
          parent_session_ids.pop();
        } else {
          break;
        }
      }
    }
  }
}

// Dumps the complete topology by outputting information on the current node and then iteratively
// dumping for direct children nodes.
//
// Sessions which are not present in the main topology will still appear in this dump.
void DumpAllSessions(const flatland::UberStruct::InstanceMap& snapshot, std::ostream& output) {
  std::optional<zx::time_monotonic> now;
  if (auto* dispatcher = async_get_default_dispatcher()) {
    now = zx::time_monotonic(async_now(dispatcher));
  }

  output << "All Flatland Sessions:\n";
  for (auto& [session_id, uber_struct] : snapshot) {
    // Output session ID and the session debug name if available.
    output << "Session " << session_id;
    if (!uber_struct->debug_name.empty()) {
      output << " (" << uber_struct->debug_name << ")";
    }
    output << ":\nlast uberstruct ";
    if (now.has_value()) {
      output << "age: " << ((now.value() - uber_struct->creation_time) / 1000000).get() << "ms\n";
    } else {
      output << "creation time: " << uber_struct->creation_time.get() << "\n";
    }

    std::stack<size_t> indentation_levels;
    for (size_t transform_index = 0; transform_index < uber_struct->local_topology.size();
         transform_index++) {
      auto& transform = uber_struct->local_topology[transform_index];
      const auto children = transform.child_count;
      auto current_indentation_level = indentation_levels.size();

      // Print indented, no-parentheses transform.
      IndentLine(current_indentation_level, output);
      output << transform.handle.GetInstanceId() << ":" << transform.handle.GetTransformId();

      // If the transform has children, print the pipe.
      if (children > 0) {
        output << "-|";
      }

      // Newline.
      output << '\n';

      // Adjust indentation for newline.
      if (children > 0) {
        indentation_levels.push(children);
      } else {
        while (!indentation_levels.empty()) {
          auto& current_indentation_level_children = indentation_levels.top();
          current_indentation_level_children--;
          if (current_indentation_level_children == 0) {
            indentation_levels.pop();
          } else {
            break;
          }
        }
      }
    }
  }
}

void DumpImages(const flatland::GlobalTopologyData& topology_data,
                const flatland::GlobalImageVector& images,
                const flatland::GlobalIndexVector& image_indices,
                const flatland::GlobalRectangleVector& image_rectangles, std::ostream& output) {
  output << "\nFrame display-list contains " << images.size() << " images and image-rectangles.";
  FX_DCHECK(images.size() == image_rectangles.size());
  FX_DCHECK(images.size() == image_indices.size());
  for (size_t i = 0; i < images.size(); i++) {
    auto& image = images[i];
    output << "\n        image: " << image;
    output << "\n        transform: " << topology_data.topology_vector[image_indices[i]];
    output << "\n        rect: " << image_rectangles[i];
  }
}

void DumpHitRegions(const flatland::UberStruct::InstanceMap& snapshot, std::ostream& output) {
  output << "\nHit Regions:\n";
  for (const auto& [session_id, uber_struct] : snapshot) {
    for (const auto& [transform_handle, hit_regions] : uber_struct->local_hit_regions_map) {
      if (hit_regions.empty())
        continue;
      output << "        transform: " << transform_handle << "\n";
      for (const auto& hit_region : hit_regions) {
        if (!hit_region.is_finite()) {
          output << "        infinite";
        } else {
          output << "        region: " << hit_region.region();
        }
        output << " interaction: " << hit_region.interaction() << "\n";
      }
    }
  }
}

}  // namespace

namespace flatland {

void DumpScene(const UberStruct::InstanceMap& snapshot,
               const flatland::GlobalTopologyData& topology_data,
               const flatland::GlobalImageVector& images,
               const flatland::GlobalIndexVector& image_indices,
               const flatland::GlobalRectangleVector& image_rectangles, std::ostream& output) {
  output << "\n========== BEGIN SCENE DUMP ======================\n";
  DumpTopology(snapshot, topology_data, output);
  output << '\n';
  DumpAllSessions(snapshot, output);
  output << '\n';
  DumpImages(topology_data, images, image_indices, image_rectangles, output);
  output << '\n';
  DumpHitRegions(snapshot, output);
  output << "\n============ END SCENE DUMP ======================";
}

}  // namespace flatland
