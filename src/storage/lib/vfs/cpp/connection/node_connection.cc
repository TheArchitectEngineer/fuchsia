// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/lib/vfs/cpp/connection/node_connection.h"

#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/zx/handle.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <zircon/assert.h>

#include <utility>

#include <fbl/string_buffer.h>

#include "src/storage/lib/vfs/cpp/vfs_types.h"
#include "src/storage/lib/vfs/cpp/vnode.h"

namespace fio = fuchsia_io;

namespace fs::internal {

NodeConnection::NodeConnection(fs::FuchsiaVfs* vfs, fbl::RefPtr<fs::Vnode> vnode,
                               fuchsia_io::Rights rights)
    : Connection(vfs, std::move(vnode), rights) {
  // Ensure the VFS does not create connections that have privileges which cannot be used.
  ZX_DEBUG_ASSERT(internal::DownscopeRights(rights, VnodeProtocol::kNode) == rights);
}

void NodeConnection::BindImpl(zx::channel channel, OnUnbound on_unbound) {
  ZX_DEBUG_ASSERT(!binding_);
  binding_.emplace(fidl::BindServer(vfs()->dispatcher(),
                                    fidl::ServerEnd<fuchsia_io::Node>{std::move(channel)}, this,
                                    [on_unbound = std::move(on_unbound)](
                                        NodeConnection* self, fidl::UnbindInfo,
                                        fidl::ServerEnd<fuchsia_io::Node>) { on_unbound(self); }));
}

void NodeConnection::Unbind() {
  // NOTE: This needs to be thread-safe!
  if (binding_)
    binding_->Unbind();
}

#if FUCHSIA_API_LEVEL_AT_LEAST(26)
void NodeConnection::DeprecatedClone(DeprecatedCloneRequestView request,
                                     DeprecatedCloneCompleter::Sync& completer) {
#else
void NodeConnection::Clone(CloneRequestView request, CloneCompleter::Sync& completer) {
#endif
  Connection::NodeCloneDeprecated(request->flags, VnodeProtocol::kNode, std::move(request->object));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(26)
void NodeConnection::Clone(CloneRequestView request, CloneCompleter::Sync& completer) {
#else
void NodeConnection::Clone2(Clone2RequestView request, Clone2Completer::Sync& completer) {
#endif
  Connection::NodeClone(fio::Flags::kProtocolNode | fs::internal::RightsToFlags(rights()),
                        request->request.TakeChannel());
}

void NodeConnection::Close(CloseCompleter::Sync& completer) {
  completer.Reply(zx::ok());
  Unbind();
}

void NodeConnection::Query(QueryCompleter::Sync& completer) {
  std::string_view protocol = fio::kNodeProtocolName;
  // TODO(https://fxbug.dev/42052765): avoid the const cast.
  uint8_t* data = reinterpret_cast<uint8_t*>(const_cast<char*>(protocol.data()));
  completer.Reply(fidl::VectorView<uint8_t>::FromExternal(data, protocol.size()));
}

void NodeConnection::GetConnectionInfo(GetConnectionInfoCompleter::Sync& completer) {
  fidl::Arena arena;
  completer.Reply(fio::wire::ConnectionInfo::Builder(arena).rights(rights()).Build());
}

void NodeConnection::Sync(SyncCompleter::Sync& completer) {
  completer.Reply(zx::make_result(ZX_ERR_BAD_HANDLE));
}

void NodeConnection::GetAttr(GetAttrCompleter::Sync& completer) {
  zx::result attrs = vnode()->GetAttributes();
  if (attrs.is_ok()) {
    completer.Reply(ZX_OK, attrs->ToIoV1NodeAttributes(*vnode()));
  } else {
    completer.Reply(attrs.error_value(), fio::wire::NodeAttributes());
  }
}

void NodeConnection::SetAttr(SetAttrRequestView request, SetAttrCompleter::Sync& completer) {
  completer.Reply(ZX_ERR_BAD_HANDLE);
}

void NodeConnection::GetAttributes(fio::wire::NodeGetAttributesRequest* request,
                                   GetAttributesCompleter::Sync& completer) {
  // TODO(https://fxbug.dev/346585458): This operation should require the GET_ATTRIBUTES right.
  internal::NodeAttributeBuilder builder(vnode());
  completer.Reply(builder.Build(request->query));
}

void NodeConnection::UpdateAttributes(fio::wire::MutableNodeAttributes* request,
                                      UpdateAttributesCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_BAD_HANDLE);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(27)
void NodeConnection::GetFlags(GetFlagsCompleter::Sync& completer) {
  completer.ReplySuccess(fio::Flags::kProtocolNode | RightsToFlags(rights()));
}
void NodeConnection::SetFlags(SetFlagsRequestView, SetFlagsCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}
#endif

#if FUCHSIA_API_LEVEL_AT_LEAST(27)
void NodeConnection::DeprecatedGetFlags(DeprecatedGetFlagsCompleter::Sync& completer) {
#else
void NodeConnection::GetFlags(GetFlagsCompleter::Sync& completer) {
#endif
  completer.Reply(ZX_OK, fio::OpenFlags::kNodeReference);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(27)
void NodeConnection::DeprecatedSetFlags(DeprecatedSetFlagsRequestView,
                                        DeprecatedSetFlagsCompleter::Sync& completer) {
#else
void NodeConnection::SetFlags(SetFlagsRequestView, SetFlagsCompleter::Sync& completer) {
#endif
  completer.Reply(ZX_ERR_BAD_HANDLE);
}

void NodeConnection::QueryFilesystem(QueryFilesystemCompleter::Sync& completer) {
  zx::result result = Connection::NodeQueryFilesystem();
  completer.Reply(result.status_value(),
                  result.is_ok()
                      ? fidl::ObjectView<fio::wire::FilesystemInfo>::FromExternal(&result.value())
                      : nullptr);
}

zx::result<> NodeConnection::WithRepresentation(
    fit::callback<zx::result<>(fio::wire::Representation)> handler,
    std::optional<fio::NodeAttributesQuery> query) const {
#if FUCHSIA_API_LEVEL_AT_LEAST(27)
  using NodeRepresentation = fio::wire::NodeInfo;
#else
  using NodeRepresentation = fio::wire::ConnectorInfo;
#endif
  fidl::WireTableFrame<NodeRepresentation> representation_frame;
  auto builder = NodeRepresentation::ExternalBuilder(
      fidl::ObjectView<fidl::WireTableFrame<NodeRepresentation>>::FromExternal(
          &representation_frame));
#if FUCHSIA_API_LEVEL_AT_LEAST(18)
  std::optional<NodeAttributeBuilder> attributes_builder;
  if (query) {
    attributes_builder.emplace(vnode());
    zx::result<fio::wire::NodeAttributes2*> attributes = attributes_builder->Build(*query);
    if (attributes.is_error()) {
      return attributes.take_error();
    }
    builder.attributes(fidl::ObjectView<fio::wire::NodeAttributes2>::FromExternal(*attributes));
  }
#endif
  auto representation = builder.Build();
#if FUCHSIA_API_LEVEL_AT_LEAST(27)
  return handler(fuchsia_io::wire::Representation::WithNode(
      fidl::ObjectView<NodeRepresentation>::FromExternal(&representation)));
#else
  return handler(fuchsia_io::wire::Representation::WithConnector(
      fidl::ObjectView<NodeRepresentation>::FromExternal(&representation)));
#endif
}

zx_status_t NodeConnection::WithNodeInfoDeprecated(
    fit::callback<zx_status_t(fuchsia_io::wire::NodeInfoDeprecated)> handler) const {
  // In io1, node reference connections are mapped to the service variant of NodeInfoDeprecated.
  return handler(fuchsia_io::wire::NodeInfoDeprecated::WithService({}));
}

}  // namespace fs::internal
