// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/flatland_manager.h"

#include <lib/fit/thread_checker.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/view_identity.h>

#include <gtest/gtest.h>

#include "fuchsia/ui/composition/cpp/fidl.h"
#include "src/ui/scenic/lib/allocation/mock_buffer_collection_importer.h"
#include "src/ui/scenic/lib/flatland/tests/logging_event_loop.h"
#include "src/ui/scenic/lib/flatland/tests/mock_flatland_presenter.h"
#include "src/ui/scenic/lib/scheduling/frame_scheduler.h"
#include "src/ui/scenic/lib/scheduling/id.h"

using ::testing::_;
using ::testing::AtLeast;
using ::testing::Return;

using flatland::FlatlandManager;
using flatland::FlatlandPresenter;
using flatland::LinkSystem;
using flatland::MockFlatlandPresenter;
using flatland::UberStructSystem;
using fuchsia::ui::composition::Flatland;
using fuchsia::ui::composition::FlatlandError;
using fuchsia::ui::composition::OnNextFrameBeginValues;

// These macros works like functions that check a variety of conditions, but if those conditions
// fail, the line number for the failure will appear in-line rather than in a function.

// This macro calls Present() on a Flatland object and immediately triggers the session update
// for all sessions so that changes from that Present() are visible in global systems. This is
// primarily useful for testing the user-facing Flatland API.
//
// This macro must be used within a test using the FlatlandManagerTest harness.
//
// |flatland| is a Flatland object constructed with the MockFlatlandPresenter owned by the
// FlatlandManagerTest harness. |session_id| is the SessionId for |flatland|. |expect_success|
// should be false if the call to Present() is expected to trigger an error.
#define PRESENT(flatland, session_id, expect_success)                               \
  {                                                                                 \
    const auto num_pending_sessions = GetNumPendingSessionUpdates(session_id);      \
    if (expect_success) {                                                           \
      EXPECT_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _, _)); \
    }                                                                               \
    fuchsia::ui::composition::PresentArgs present_args;                             \
    present_args.set_requested_presentation_time(0);                                \
    present_args.set_acquire_fences({});                                            \
    present_args.set_release_fences({});                                            \
    present_args.set_unsquashable(false);                                           \
    flatland->Present(std::move(present_args));                                     \
    /* If expecting success, wait for the worker thread to process the request. */  \
    if (expect_success) {                                                           \
      RunLoopUntil([this, session_id, num_pending_sessions] {                       \
        return GetNumPendingSessionUpdates(session_id) > num_pending_sessions;      \
      });                                                                           \
    }                                                                               \
  }

namespace {

class FlatlandManagerTest : public LoggingEventLoop, public ::testing::Test {
 public:
  FlatlandManagerTest()
      : uber_struct_system_(std::make_shared<UberStructSystem>()),
        link_system_(std::make_shared<LinkSystem>(uber_struct_system_->GetNextInstanceId())) {}

  void SetUp() override {
    ::testing::Test::SetUp();

    mock_flatland_presenter_ = std::make_shared<::testing::StrictMock<MockFlatlandPresenter>>();

    ON_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _, _))
        .WillByDefault(::testing::Invoke(
            [&](zx::time requested_presentation_time, scheduling::SchedulingIdPair id_pair,
                bool unsquashable, std::vector<zx::event> release_fences) {
              EXPECT_TRUE(release_fences.empty());

              // The ID pair must not be already registered.
              EXPECT_FALSE(pending_presents_.count(id_pair));
              pending_presents_.insert(id_pair);

              // Ensure present IDs are strictly increasing.
              auto& queue = pending_session_updates_[id_pair.session_id];
              EXPECT_TRUE(queue.empty() || queue.back() < id_pair.present_id);

              // Save the pending present ID.
              queue.push(id_pair.present_id);
            }));

    ON_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos())
        .WillByDefault(::testing::Invoke([&]() {
          // The requested_prediction_span should be at least one frame.

          // Give back at least one info.
          std::vector<scheduling::FuturePresentationInfo> presentation_infos;
          auto& info = presentation_infos.emplace_back();
          info.latch_point = zx::time(5);
          info.presentation_time = zx::time(10);

          return presentation_infos;
        }));

    ON_CALL(*mock_flatland_presenter_, RemoveSession(_, _))
        .WillByDefault(::testing::Invoke(
            [&](scheduling::SessionId session_id, std::optional<zx::event> release_fence) {
              async::PostTask(this->dispatcher(), [&, session_id]() {
                std::lock_guard lock(removed_session_thread_checker_);
                removed_sessions_.insert(session_id);
              });
            }));

    const fuchsia_hardware_display_types::wire::DisplayId kDisplayId = {.value = 1};
    constexpr uint32_t kDisplayWidth = 640;
    constexpr uint32_t kDisplayHeight = 480;
    std::vector<std::shared_ptr<allocation::BufferCollectionImporter>> importers;
    manager_ = std::make_unique<FlatlandManager>(
        dispatcher(), mock_flatland_presenter_, uber_struct_system_, link_system_,
        std::make_shared<scenic_impl::display::Display>(kDisplayId, kDisplayWidth, kDisplayHeight),
        importers,
        /*register_view_focuser*/ [this](auto...) { view_focuser_registered_ = true; },
        /*register_view_ref_focused*/ [this](auto...) { view_ref_focused_registered_ = true; },
        /*register_touch_source*/ [this](auto...) { touch_source_registered_ = true; },
        /*register_mouse_source*/ [this](auto...) { mouse_source_registered_ = true; });
  }

  void TearDown() override {
    // |manager_| may have been reset during the test. If not, run until all sessions have closed,
    // which depends on the worker threads receiving "peer closed" for the clients created in
    // the tests.
    {
      std::lock_guard lock(removed_session_thread_checker_);
      std::for_each(removed_sessions_.begin(), removed_sessions_.end(), [](auto session_id) {
        FX_LOGS(INFO) << "`removed_sessions_` includes " << session_id;
      });
      removed_sessions_.clear();
    }
    if (manager_) {
      const size_t initial_session_count = manager_->GetSessionCount();
      FX_LOGS(INFO) << "initial_session_count=" << initial_session_count;
      EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(_, _))
          .Times(AtLeast(initial_session_count));
      RunLoopUntil([this, initial_session_count] {
        std::lock_guard lock(removed_session_thread_checker_);
        // It could be tempting to only check a single condition here. However,
        // it won't work as expected. `FlatlandManager` posts the task
        // destroying `Flatland` instance on the Session's loop thread, and then
        // deletes the session from its session list immediately.
        //
        // The `Flatland` destructor later calls presenter API to remove the
        // session, which is handled on the presenter handler's FIDL loop.
        // There may be a critical section in between and we should make sure
        // both conditions are fulfilled before proceeding.
        auto current_session_count = manager_->GetSessionCount();
        std::for_each(removed_sessions_.begin(), removed_sessions_.end(), [](auto session_id) {
          FX_LOGS(INFO) << "`removed_sessions_` includes " << session_id;
        });
        FX_LOGS(INFO) << "current_session_count=" << current_session_count;

        // We can't test for equality between `removed_sessions_.size()` and `initial_session_count`
        // due to a race condition: the about-to-be-destroyed session might already have been
        // removed from the manager, but the presenter is not notified until the end of the Flatland
        // destructor, which occurs on a different thread.
        return current_session_count == 0 && removed_sessions_.size() >= initial_session_count;
      });
    }

    auto snapshot = uber_struct_system_->Snapshot();
    EXPECT_TRUE(snapshot.empty());

    manager_.reset();
    RunLoopUntilIdle();

    EXPECT_EQ(uber_struct_system_->GetSessionCount(), 0ul);

    pending_presents_.clear();
    pending_session_updates_.clear();
    mock_flatland_presenter_.reset();
    {
      std::lock_guard lock(removed_session_thread_checker_);
      removed_sessions_.clear();
    }

    ::testing::Test::TearDown();
  }

  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> CreateFlatland() {
    fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland;
    const scheduling::SessionId id = manager_->CreateFlatland(flatland.NewRequest(dispatcher()));
    FX_LOGS(INFO) << "Created flatland with ID " << id;
    return flatland;
  }

  // Returns the number of currently pending session updates for |session_id|.
  size_t GetNumPendingSessionUpdates(scheduling::SessionId session_id) {
    const auto& queue = pending_session_updates_[session_id];
    return queue.size();
  }

  // Returns the next pending PresentId for |session_id| and removes it from the list of pending
  // session updates. Fails if |session_id| has no pending presents.
  scheduling::PresentId PopPendingPresent(scheduling::SessionId session_id) {
    auto& queue = pending_session_updates_[session_id];
    EXPECT_FALSE(queue.empty());

    auto next_present_id = queue.front();
    queue.pop();
    return next_present_id;
  }

 protected:
  std::shared_ptr<::testing::StrictMock<MockFlatlandPresenter>> mock_flatland_presenter_;
  const std::shared_ptr<UberStructSystem> uber_struct_system_;

  std::unique_ptr<FlatlandManager> manager_;

  // Storage for |mock_flatland_presenter_|.
  std::set<scheduling::SchedulingIdPair> pending_presents_;
  std::unordered_map<scheduling::SessionId, std::queue<scheduling::PresentId>>
      pending_session_updates_;

  // std::unordered_set is not thread-safe. Here we add the thread checker
  // to make sure that it is only used on the test loop (which runs on the same
  // thread as test main thread).
  fit::thread_checker removed_session_thread_checker_;
  std::unordered_set<scheduling::SessionId> removed_sessions_
      FIT_GUARDED(removed_session_thread_checker_);

  const std::shared_ptr<LinkSystem> link_system_;

  bool view_focuser_registered_ = false;
  bool view_ref_focused_registered_ = false;
  bool touch_source_registered_ = false;
  bool mouse_source_registered_ = false;
};

}  // namespace

namespace flatland::test {

TEST_F(FlatlandManagerTest, CreateFlatlands) {
  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland1 = CreateFlatland();

  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland2 = CreateFlatland();

  RunLoopUntilIdle();

  EXPECT_TRUE(flatland1.is_bound());
  EXPECT_TRUE(flatland2.is_bound());
  EXPECT_EQ(manager_->GetSessionCount(), 2ul);
}

TEST_F(FlatlandManagerTest, CreateViewportedFlatlands) {
  fuchsia::ui::views::ViewportCreationToken parent_token;
  fuchsia::ui::views::ViewCreationToken child_token;
  ASSERT_EQ(ZX_OK, zx::channel::create(0, &parent_token.value, &child_token.value));

  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> parent = CreateFlatland();
  const fuchsia::ui::composition::ContentId kLinkId = {1};
  fidl::InterfacePtr<fuchsia::ui::composition::ChildViewWatcher> child_view_watcher;
  fuchsia::ui::composition::ViewportProperties properties;
  properties.set_logical_size({1, 2});
  parent->CreateViewport(kLinkId, std::move(parent_token), std::move(properties),
                         child_view_watcher.NewRequest());
  {
    fidl::InterfacePtr<fuchsia::ui::composition::Flatland> child = CreateFlatland();
    fidl::InterfacePtr<fuchsia::ui::composition::ParentViewportWatcher> parent_viewport_watcher;
    child->CreateView(std::move(child_token), parent_viewport_watcher.NewRequest());

    RunLoopUntilIdle();
    EXPECT_EQ(manager_->GetSessionCount(), 2ul);
    RunLoopUntil([this] { return !link_system_->GetResolvedTopologyLinks().empty(); });

    EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(_, _));
  }

  RunLoopUntil([this] { return link_system_->GetResolvedTopologyLinks().empty(); });
}

TEST_F(FlatlandManagerTest, ClientDiesBeforeManager) {
  scheduling::SessionId id;
  {
    fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland = CreateFlatland();
    id = uber_struct_system_->GetLatestInstanceId();

    RunLoopUntilIdle();

    EXPECT_TRUE(flatland.is_bound());

    // |flatland| falls out of scope, killing the session.
    EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(id, _));

    // FlatlandManager::RemoveFlatlandInstance() will be posted on main thread and may not be run
    // yet.
    RunLoopUntilIdle();
  }

  // The session should show up in the set of removed sessions.
  RunLoopUntil([this] {
    std::lock_guard lock(removed_session_thread_checker_);
    // It could be tempting to only check a single condition here, however,
    // this will cause a race and won't work as expected, since FlatlandManager
    // removing session from manager's session list and Flatland impl requests
    // presenter to remove the session occur on different threads.
    // See the comment at `TearDown()` for details.
    return manager_->GetSessionCount() == 0 && removed_sessions_.size() == 1;
  });
  {
    std::lock_guard lock(removed_session_thread_checker_);
    EXPECT_TRUE(removed_sessions_.count(id));
  }
}

TEST_F(FlatlandManagerTest, ManagerDiesBeforeClients) {
  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  RunLoopUntilIdle();

  EXPECT_TRUE(flatland.is_bound());
  EXPECT_EQ(manager_->GetSessionCount(), 1ul);

  // Explicitly kill the server.
  EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(id, _));
  manager_.reset();

  EXPECT_EQ(uber_struct_system_->GetSessionCount(), 0ul);

  RunLoopUntil([this] {
    std::lock_guard lock(removed_session_thread_checker_);
    return removed_sessions_.size() == 1ul;
  });

  {
    std::lock_guard lock(removed_session_thread_checker_);
    EXPECT_TRUE(removed_sessions_.count(id));
  }

  // Wait until unbound.
  RunLoopUntil([&flatland] { return !flatland.is_bound(); });

  // FlatlandManager::RemoveFlatlandInstance() will be posted on main thread and may not be run yet.
  RunLoopUntilIdle();
}

TEST_F(FlatlandManagerTest, FirstPresentReturnsMaxPresentCredits) {
  // Setup a Flatland instance with an OnNextFrameBegin() callback.
  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  uint32_t returned_tokens = 0;
  flatland.events().OnNextFrameBegin = [&returned_tokens](OnNextFrameBeginValues values) {
    returned_tokens += values.additional_present_credits();
    EXPECT_TRUE(returned_tokens > 0);
    EXPECT_FALSE(values.future_presentation_infos().empty());
  };

  // Present once, but don't update sessions.
  PRESENT(flatland, id, true);

  auto snapshot = uber_struct_system_->Snapshot();
  EXPECT_TRUE(snapshot.empty());

  EXPECT_EQ(GetNumPendingSessionUpdates(id), 1ul);

  // Update the session, this should return max tokens through OnNextFrameBegin().
  const auto next_present_id = PopPendingPresent(id);
  manager_->UpdateInstances({{id, next_present_id}});

  EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos());
  manager_->SendHintsToStartRendering();

  snapshot = uber_struct_system_->Snapshot();
  EXPECT_EQ(snapshot.size(), 1u);
  EXPECT_TRUE(snapshot.count(id));

  RunLoopUntil([&returned_tokens] { return returned_tokens != 0; });
  EXPECT_EQ(returned_tokens, scheduling::FrameScheduler::kMaxPresentsInFlight);
  EXPECT_EQ(GetNumPendingSessionUpdates(id), 0ul);
}

TEST_F(FlatlandManagerTest, UpdateInstancesReturnsPresentCredits) {
  // Setup two Flatland instances with OnNextFrameBegin() callbacks.
  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland1 = CreateFlatland();
  const scheduling::SessionId id1 = uber_struct_system_->GetLatestInstanceId();

  uint32_t returned_tokens1 = 0;
  flatland1.events().OnNextFrameBegin = [&returned_tokens1](OnNextFrameBeginValues values) {
    returned_tokens1 += values.additional_present_credits();
    EXPECT_TRUE(returned_tokens1 > 0);
    EXPECT_FALSE(values.future_presentation_infos().empty());
  };

  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland2 = CreateFlatland();
  const scheduling::SessionId id2 = uber_struct_system_->GetLatestInstanceId();

  uint32_t returned_tokens2 = 0;
  flatland2.events().OnNextFrameBegin = [&returned_tokens2](OnNextFrameBeginValues values) {
    returned_tokens2 += values.additional_present_credits();
    EXPECT_TRUE(returned_tokens2 > 0);
    EXPECT_FALSE(values.future_presentation_infos().empty());
  };

  {  // Go through the initial present so both instances have multiple credits.
    PRESENT(flatland1, id1, true);
    PRESENT(flatland2, id2, true);
    const auto next_present_id1 = PopPendingPresent(id1);
    const auto next_present_id2 = PopPendingPresent(id2);
    manager_->UpdateInstances({{id1, next_present_id1}, {id2, next_present_id2}});
    EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos());
    manager_->SendHintsToStartRendering();
    RunLoopUntil([&] {
      return returned_tokens1 == scheduling::FrameScheduler::kMaxPresentsInFlight &&
             returned_tokens2 == scheduling::FrameScheduler::kMaxPresentsInFlight;
    });
    EXPECT_EQ(GetNumPendingSessionUpdates(id1), 0ul);
    EXPECT_EQ(GetNumPendingSessionUpdates(id2), 0ul);
    // Now forget about the returned tokens.
    returned_tokens1 = 0;
    returned_tokens2 = 0;
  }

  // Present both instances twice, but don't update sessions.
  PRESENT(flatland1, id1, true);
  PRESENT(flatland1, id1, true);

  PRESENT(flatland2, id2, true);
  PRESENT(flatland2, id2, true);

  EXPECT_EQ(GetNumPendingSessionUpdates(id1), 2ul);
  EXPECT_EQ(GetNumPendingSessionUpdates(id2), 2ul);

  // Update the first session, but only with the first PresentId, which should push an UberStruct
  // and return one token to the first instance.
  auto next_present_id1 = PopPendingPresent(id1);
  manager_->UpdateInstances({{id1, next_present_id1}});

  EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos());
  manager_->SendHintsToStartRendering();

  RunLoopUntil([&returned_tokens1] { return returned_tokens1 != 0; });

  EXPECT_EQ(returned_tokens1, 1u);
  EXPECT_EQ(returned_tokens2, 0u);

  EXPECT_EQ(GetNumPendingSessionUpdates(id1), 1ul);
  EXPECT_EQ(GetNumPendingSessionUpdates(id2), 2ul);

  returned_tokens1 = 0;

  // Update only the second session and consume both PresentIds, which should push an UberStruct
  // and return two tokens to the second instance.
  PopPendingPresent(id2);
  const auto next_present_id2 = PopPendingPresent(id2);

  manager_->UpdateInstances({{id2, next_present_id2}});

  EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos());
  manager_->SendHintsToStartRendering();

  const auto snapshot = uber_struct_system_->Snapshot();
  EXPECT_EQ(snapshot.size(), 2u);
  EXPECT_TRUE(snapshot.count(id1));
  EXPECT_TRUE(snapshot.count(id2));

  RunLoopUntil([&returned_tokens2] { return returned_tokens2 != 0; });

  EXPECT_EQ(returned_tokens1, 0u);
  EXPECT_EQ(returned_tokens2, 2u);

  EXPECT_EQ(GetNumPendingSessionUpdates(id1), 1ul);
  EXPECT_EQ(GetNumPendingSessionUpdates(id2), 0ul);
}

// It is possible for the session to update multiple times in a row before
// SendHintsToStartRendering() is called. If that's the case, we need to ensure that present credits
// returned from the first update are not lost.
TEST_F(FlatlandManagerTest, ConsecutiveUpdateInstances_ReturnsCorrectPresentCredits) {
  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  uint32_t returned_tokens = 0;
  flatland.events().OnNextFrameBegin = [&returned_tokens](OnNextFrameBeginValues values) {
    returned_tokens = values.additional_present_credits();
    EXPECT_TRUE(returned_tokens > 0);
    EXPECT_FALSE(values.future_presentation_infos().empty());
  };

  {  // Receive the initial allotment of tokens, then forget those tokens.
    PRESENT(flatland, id, true);
    const auto next_present_id = PopPendingPresent(id);
    manager_->UpdateInstances({{id, next_present_id}});
    EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos());
    manager_->SendHintsToStartRendering();
    RunLoopUntil(
        [&] { return returned_tokens == scheduling::FrameScheduler::kMaxPresentsInFlight; });
    EXPECT_EQ(GetNumPendingSessionUpdates(id), 0ul);
    returned_tokens = 0;
  }

  // Present twice, but don't update the session yet.
  PRESENT(flatland, id, true);
  PRESENT(flatland, id, true);
  EXPECT_EQ(GetNumPendingSessionUpdates(id), 2ul);

  // Update the session, but only with the first PresentId, which should push an UberStruct
  // and return one token to the first instance.
  auto next_present_id = PopPendingPresent(id);
  manager_->UpdateInstances({{id, next_present_id}});

  // Update again.
  next_present_id = PopPendingPresent(id);
  manager_->UpdateInstances({{id, next_present_id}});

  // Finally, the work is done according to the frame scheduler.
  EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos());
  manager_->SendHintsToStartRendering();

  const auto snapshot = uber_struct_system_->Snapshot();
  EXPECT_EQ(snapshot.size(), 1u);
  EXPECT_TRUE(snapshot.count(id));

  RunLoopUntil([&returned_tokens] { return returned_tokens != 0; });

  EXPECT_EQ(returned_tokens, 2u);

  EXPECT_EQ(GetNumPendingSessionUpdates(id), 0ul);
}

TEST_F(FlatlandManagerTest, PresentWithoutTokensClosesSession) {
  // Setup a Flatland instance with an OnNextFrameBegin() callback.
  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  FlatlandError error_returned;
  uint32_t tokens_remaining = 1;
  flatland.events().OnError = [&error_returned](FlatlandError error) { error_returned = error; };

  // Present until no tokens remain.
  PRESENT(flatland, id, true);
  EXPECT_TRUE(flatland.is_bound());

  // Present one more time and ensure the session is closed.
  EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(id, _));
  PRESENT(flatland, id, false);

  // The instance will eventually be unbound, but it takes a pair of thread hops to complete since
  // the destroy_instance_function() posts a task from the worker to the main and that task
  // ultimately posts the destruction back onto the worker.
  RunLoopUntil([&flatland] { return !flatland.is_bound(); });
  EXPECT_EQ(error_returned, FlatlandError::NO_PRESENTS_REMAINING);

  // Wait until all Flatland threads are destroyed.
  RunLoopUntil([this] { return manager_->GetAliveSessionCount() == 0; });

  // FlatlandManager::RemoveFlatlandInstance() will be posted on main thread and may not be run yet.
  RunLoopUntilIdle();
}

TEST_F(FlatlandManagerTest, ErrorClosesSession) {
  // Setup a Flatland instance with an OnNextFrameBegin() callback.
  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  FlatlandError error_returned;
  flatland.events().OnError = [&error_returned](FlatlandError error) { error_returned = error; };
  EXPECT_TRUE(flatland.is_bound());

  // Queue a bad SetRootTransform call ensure the session is closed.
  EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(id, _));
  flatland->SetRootTransform({2});
  PRESENT(flatland, id, false);

  // The instance will eventually be unbound, but it takes a pair of thread hops to complete since
  // the destroy_instance_function() posts a task from the worker to the main and that task
  // ultimately posts the destruction back onto the worker.
  RunLoopUntil([&flatland] { return !flatland.is_bound(); });
  EXPECT_EQ(error_returned, FlatlandError::BAD_OPERATION);

  // Wait until all Flatland threads are destroyed.
  RunLoopUntil([this] { return manager_->GetAliveSessionCount() == 0; });

  // FlatlandManager::RemoveFlatlandInstance() will be posted on main thread and may not be run
  // yet.
  RunLoopUntilIdle();
}

TEST_F(FlatlandManagerTest, TokensAreReplenishedAfterRunningOut) {
  // Setup a Flatland instance with an OnNextFrameBegin() callback.
  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  uint32_t tokens_remaining = 0;
  flatland.events().OnNextFrameBegin = [&tokens_remaining](OnNextFrameBeginValues values) {
    tokens_remaining += values.additional_present_credits();
    EXPECT_TRUE(tokens_remaining > 0);
  };

  {  // Receive the initial allotment of tokens.
    PRESENT(flatland, id, true);
    const auto next_present_id = PopPendingPresent(id);
    manager_->UpdateInstances({{id, next_present_id}});
    EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos());
    manager_->SendHintsToStartRendering();
    RunLoopUntil(
        [&] { return tokens_remaining == scheduling::FrameScheduler::kMaxPresentsInFlight; });
  }

  // Present until no tokens remain.
  while (tokens_remaining > 0) {
    PRESENT(flatland, id, true);
    --tokens_remaining;
  }

  // Process the first present.
  auto next_present_id = PopPendingPresent(id);
  manager_->UpdateInstances({{id, next_present_id}});

  // Signal that the work is done, which should return present credits to the client.
  EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos());
  manager_->SendHintsToStartRendering();

  RunLoopUntil([&tokens_remaining] { return tokens_remaining != 0; });

  // Present once more which should succeed.
  PRESENT(flatland, id, true);
  EXPECT_TRUE(flatland.is_bound());
}

TEST_F(FlatlandManagerTest, OnFramePresentedEvent) {
  // Setup two Flatland instances with OnFramePresented() callbacks.
  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland1 = CreateFlatland();
  const scheduling::SessionId id1 = uber_struct_system_->GetLatestInstanceId();

  std::optional<fuchsia::scenic::scheduling::FramePresentedInfo> info1;
  flatland1.events().OnFramePresented =
      [&info1](fuchsia::scenic::scheduling::FramePresentedInfo info) { info1 = std::move(info); };

  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> flatland2 = CreateFlatland();
  const scheduling::SessionId id2 = uber_struct_system_->GetLatestInstanceId();

  std::optional<fuchsia::scenic::scheduling::FramePresentedInfo> info2;
  flatland2.events().OnFramePresented =
      [&info2](fuchsia::scenic::scheduling::FramePresentedInfo info) { info2 = std::move(info); };

  {  // Go through the initial present so both instances have multiple credits.
    uint32_t returned_tokens1 = 0;
    flatland1.events().OnNextFrameBegin = [&returned_tokens1](OnNextFrameBeginValues values) {
      returned_tokens1 += values.additional_present_credits();
    };
    uint32_t returned_tokens2 = 0;
    flatland2.events().OnNextFrameBegin = [&](OnNextFrameBeginValues values) {
      returned_tokens2 += values.additional_present_credits();
    };
    PRESENT(flatland1, id1, true);
    PRESENT(flatland2, id2, true);
    const auto next_present_id1 = PopPendingPresent(id1);
    const auto next_present_id2 = PopPendingPresent(id2);
    manager_->UpdateInstances({{id1, next_present_id1}, {id2, next_present_id2}});
    EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos());
    manager_->SendHintsToStartRendering();
    RunLoopUntil([&] {
      return returned_tokens1 == scheduling::FrameScheduler::kMaxPresentsInFlight &&
             returned_tokens2 == scheduling::FrameScheduler::kMaxPresentsInFlight;
    });
  }

  // Present both instances twice, but don't update sessions.
  PRESENT(flatland1, id1, true);
  PRESENT(flatland1, id1, true);

  PRESENT(flatland2, id2, true);
  PRESENT(flatland2, id2, true);

  // Call OnFramePresented() with a PresentId for the first session and ensure the event fires.
  scheduling::PresentTimestamps timestamps{
      .presented_time = zx::time(111),
      .vsync_interval = zx::duration(11),
  };
  zx::time latch_time1 = zx::time(123);
  auto next_present_id1 = PopPendingPresent(id1);

  std::unordered_map<scheduling::SessionId,
                     std::map<scheduling::PresentId, /*latched_time*/ zx::time>>
      latch_times;
  latch_times[id1] = {{next_present_id1, latch_time1}};

  manager_->OnFramePresented(latch_times, timestamps);

  // Wait until the event has fired.
  RunLoopUntil([&info1] { return info1.has_value(); });

  // Verify that info1 contains the expected data.
  EXPECT_EQ(zx::time(info1->actual_presentation_time), timestamps.presented_time);
  EXPECT_EQ(info1->num_presents_allowed, 0ul);
  EXPECT_EQ(info1->presentation_infos.size(), 1ul);
  EXPECT_EQ(zx::time(info1->presentation_infos[0].latched_time()), latch_time1);

  // Run the loop again to show that info2 hasn't been populated.
  RunLoopUntilIdle();
  EXPECT_FALSE(info2.has_value());

  // Call OnFramePresented with all the remaining PresentIds and ensure an event fires for both.
  info1.reset();
  latch_times.clear();

  timestamps = scheduling::PresentTimestamps({
      .presented_time = zx::time(222),
      .vsync_interval = zx::duration(22),
  });
  latch_time1 = zx::time(234);
  auto latch_time2_1 = zx::time(345);
  auto latch_time2_2 = zx::time(456);
  next_present_id1 = PopPendingPresent(id1);
  auto next_present_id2_1 = PopPendingPresent(id2);
  auto next_present_id2_2 = PopPendingPresent(id2);

  latch_times[id1] = {{next_present_id1, latch_time1}};
  latch_times[id2] = {{next_present_id2_1, latch_time2_1}, {next_present_id2_2, latch_time2_2}};

  manager_->OnFramePresented(latch_times, timestamps);

  // Wait until both events have fired.
  RunLoopUntil([&info1] { return info1.has_value(); });
  RunLoopUntil([&info2] { return info2.has_value(); });

  // Verify that both infos contain the expected data.
  EXPECT_EQ(zx::time(info1->actual_presentation_time), timestamps.presented_time);
  EXPECT_EQ(info1->num_presents_allowed, 0ul);
  EXPECT_EQ(info1->presentation_infos.size(), 1ul);
  EXPECT_EQ(zx::time(info1->presentation_infos[0].latched_time()), latch_time1);

  EXPECT_EQ(zx::time(info2->actual_presentation_time), timestamps.presented_time);
  EXPECT_EQ(info2->num_presents_allowed, 0ul);
  EXPECT_EQ(info2->presentation_infos.size(), 2ul);
  EXPECT_EQ(zx::time(info2->presentation_infos[0].latched_time()), latch_time2_1);
  EXPECT_EQ(zx::time(info2->presentation_infos[1].latched_time()), latch_time2_2);

  // Call `OnFramePresented()` after the first session has terminated.
  //
  // Verify that Scenic does not crash, and that the second session still gets its
  // `OnFramePresented` event.
  //
  // Note: The iteration order of sessions within the argument to `OnFramePresented()`
  // is dependent on a hash function. Hence: if the hash ordering varies from one test
  // run to another (for identical builds), and there is a bug in `OnFramePresented()`,
  // this test could flake.
  PRESENT(flatland1, id1, true);
  PRESENT(flatland2, id2, true);
  EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(id1, _));
  flatland1 = {};
  FX_LOGS(INFO) << "Waiting for removal of session " << id1;
  RunLoopUntil([this, id1] {
    std::lock_guard lock(removed_session_thread_checker_);
    return removed_sessions_.find(id1) != removed_sessions_.end();
  });
  info2 = {};
  manager_->OnFramePresented({{id1, {{PopPendingPresent(id1), zx::time(789)}}},
                              {id2, {{PopPendingPresent(id2), zx::time(789)}}}},
                             scheduling::PresentTimestamps({
                                 .presented_time = zx::time(777),
                                 .vsync_interval = zx::duration(16),
                             }));
  FX_LOGS(INFO) << "Waiting for event on session " << id2;
  RunLoopUntil([&info2] { return info2.has_value(); });
}

TEST_F(FlatlandManagerTest, ViewBoundProtocolsAreRegistered) {
  fuchsia::ui::views::ViewportCreationToken parent_token;
  fuchsia::ui::views::ViewCreationToken child_token;
  ASSERT_EQ(ZX_OK, zx::channel::create(0, &parent_token.value, &child_token.value));
  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> parent = CreateFlatland();
  const fuchsia::ui::composition::ContentId kLinkId = {1};
  fidl::InterfacePtr<fuchsia::ui::composition::ChildViewWatcher> child_view_watcher;
  fuchsia::ui::composition::ViewportProperties properties;
  properties.set_logical_size({1, 2});
  parent->CreateViewport(kLinkId, std::move(parent_token), std::move(properties),
                         child_view_watcher.NewRequest());

  fidl::InterfacePtr<fuchsia::ui::composition::Flatland> child = CreateFlatland();

  fidl::InterfacePtr<fuchsia::ui::views::Focuser> view_focuser_ptr;
  fidl::InterfacePtr<fuchsia::ui::views::ViewRefFocused> view_ref_focused_ptr;
  fidl::InterfacePtr<fuchsia::ui::pointer::TouchSource> touch_source_ptr;
  fidl::InterfacePtr<fuchsia::ui::pointer::MouseSource> mouse_source_ptr;

  fidl::InterfacePtr<fuchsia::ui::composition::ParentViewportWatcher> parent_viewport_watcher;
  fuchsia::ui::composition::ViewBoundProtocols protocols;
  protocols.set_view_focuser(view_focuser_ptr.NewRequest())
      .set_view_ref_focused(view_ref_focused_ptr.NewRequest())
      .set_touch_source(touch_source_ptr.NewRequest())
      .set_mouse_source(mouse_source_ptr.NewRequest());
  child->CreateView2(std::move(child_token), scenic::NewViewIdentityOnCreation(),
                     std::move(protocols), parent_viewport_watcher.NewRequest());

  RunLoopUntil([this] {
    return view_focuser_registered_ && view_ref_focused_registered_ && touch_source_registered_ &&
           mouse_source_registered_;
  });
}

#undef PRESENT

}  // namespace flatland::test
