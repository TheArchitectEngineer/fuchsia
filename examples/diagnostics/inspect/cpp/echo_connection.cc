// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "echo_connection.h"

#include <lib/sys/cpp/component_context.h>

namespace example {

EchoConnection::EchoConnection(std::weak_ptr<EchoConnectionStats> stats)
    : stats_(std::move(stats)) {}

void EchoConnection::EchoString(EchoStringRequest& request, EchoStringCompleter::Sync& completer) {
  auto stats = stats_.lock();
  if (stats) {
    stats->total_requests.Add(1);
    stats->bytes_processed.Add(request.value()->size());
  }
  completer.Reply({{.response = request.value()}});
}

}  // namespace example
