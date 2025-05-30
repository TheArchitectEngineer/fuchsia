// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/lib/vfs/cpp/connection/file_connection.h"

#include <fidl/fuchsia.io/cpp/fidl.h>
#include <lib/zx/handle.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <zircon/assert.h>

#include <memory>
#include <utility>

#include <fbl/string_buffer.h>

#include "src/storage/lib/vfs/cpp/connection/advisory_lock.h"
#include "src/storage/lib/vfs/cpp/debug.h"
#include "src/storage/lib/vfs/cpp/vfs_types.h"
#include "src/storage/lib/vfs/cpp/vnode.h"

namespace fio = fuchsia_io;

namespace fs::internal {

FileConnection::FileConnection(fs::FuchsiaVfs* vfs, fbl::RefPtr<fs::Vnode> vnode,
                               fuchsia_io::Rights rights, zx_koid_t koid)
    : Connection(vfs, std::move(vnode), rights), koid_(koid) {
  // Ensure the VFS does not create connections that have privileges which cannot be used.
  ZX_DEBUG_ASSERT(internal::DownscopeRights(rights, VnodeProtocol::kFile) == rights);
}

void FileConnection::BindImpl(zx::channel channel, OnUnbound on_unbound) {
  ZX_DEBUG_ASSERT(!binding_);
  binding_.emplace(fidl::BindServer(
      vfs()->dispatcher(), fidl::ServerEnd<fuchsia_io::File>{std::move(channel)}, this,
      [on_unbound = std::move(on_unbound)](FileConnection* self, fidl::UnbindInfo,
                                           fidl::ServerEnd<fuchsia_io::File>) {
        [[maybe_unused]] zx::result result = self->CloseVnode(self->koid_);
        on_unbound(self);
      }));
}

void FileConnection::Unbind() {
  // NOTE: This needs to be thread-safe!
  if (binding_)
    binding_->Unbind();
}

#if FUCHSIA_API_LEVEL_AT_LEAST(26)
void FileConnection::DeprecatedClone(DeprecatedCloneRequestView request,
                                     DeprecatedCloneCompleter::Sync& completer) {
#else
void FileConnection::Clone(CloneRequestView request, CloneCompleter::Sync& completer) {
#endif
  fio::OpenFlags inherited_flags = {};
  // The APPEND flag should be preserved when cloning a file connection.
  if (GetAppend()) {
    inherited_flags |= fio::OpenFlags::kAppend;
  }
  Connection::NodeCloneDeprecated(request->flags | inherited_flags, VnodeProtocol::kFile,
                                  std::move(request->object));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(26)
void FileConnection::Clone(CloneRequestView request, CloneCompleter::Sync& completer) {
#else
void FileConnection::Clone2(Clone2RequestView request, Clone2Completer::Sync& completer) {
#endif
  const fio::Flags flags = fio::Flags::kProtocolFile | fs::internal::RightsToFlags(rights()) |
                           (GetAppend() ? fio::Flags::kFileAppend : fio::Flags());
  Connection::NodeClone(flags, request->request.TakeChannel());
}

void FileConnection::Close(CloseCompleter::Sync& completer) {
  completer.Reply(CloseVnode(koid_));
  Unbind();
}

void FileConnection::Query(QueryCompleter::Sync& completer) {
  std::string_view protocol = fio::kFileProtocolName;
  // TODO(https://fxbug.dev/42052765): avoid the const cast.
  uint8_t* data = reinterpret_cast<uint8_t*>(const_cast<char*>(protocol.data()));
  completer.Reply(fidl::VectorView<uint8_t>::FromExternal(data, protocol.size()));
}

zx_status_t FileConnection::WithNodeInfoDeprecated(
    fit::callback<zx_status_t(fuchsia_io::wire::NodeInfoDeprecated)> handler) const {
  fio::wire::FileObject file_object;
  zx::result<zx::event> observer = vnode()->GetObserver();
  if (observer.is_ok()) {
    file_object.event = std::move(*observer);
  } else if (observer.error_value() != ZX_ERR_NOT_SUPPORTED) {
    return observer.error_value();
  }
  if (stream()) {
    if (zx_status_t status = stream()->duplicate(ZX_RIGHT_SAME_RIGHTS, &file_object.stream);
        status != ZX_OK) {
      return status;
    }
  }
  return handler(fuchsia_io::wire::NodeInfoDeprecated::WithFile(
      fidl::ObjectView<fio::wire::FileObject>::FromExternal(&file_object)));
}

zx::result<> FileConnection::WithRepresentation(
    fit::callback<zx::result<>(fuchsia_io::wire::Representation)> handler,
    std::optional<fuchsia_io::NodeAttributesQuery> query) const {
  using FileRepresentation = fio::wire::FileInfo;
  fidl::WireTableFrame<FileRepresentation> representation_frame;
  auto builder = FileRepresentation::ExternalBuilder(
      fidl::ObjectView<fidl::WireTableFrame<FileRepresentation>>::FromExternal(
          &representation_frame));
#if FUCHSIA_API_LEVEL_AT_LEAST(18)
  std::optional<NodeAttributeBuilder> attributes_builder;
  if (query) {
    attributes_builder.emplace(vnode());
    zx::result<fio::wire::NodeAttributes2*> attributes;
    attributes = attributes_builder->Build(*query);
    if (attributes.is_error()) {
      return attributes.take_error();
    }
    builder.attributes(fidl::ObjectView<fio::wire::NodeAttributes2>::FromExternal(*attributes));
  }
#endif
  builder.is_append(GetAppend());
  if (zx::result observer = vnode()->GetObserver(); observer.is_ok()) {
    builder.observer(std::move(*observer));
  } else if (observer.error_value() != ZX_ERR_NOT_SUPPORTED) {
    return observer.take_error();
  }
  if (this->stream()) {
    zx::stream stream;
    if (zx_status_t status = this->stream()->duplicate(ZX_RIGHT_SAME_RIGHTS, &stream);
        status != ZX_OK) {
      return zx::error(status);
    }
    builder.stream(std::move(stream));
  }
  auto representation = builder.Build();
  return handler(fuchsia_io::wire::Representation::WithFile(
      fidl::ObjectView<FileRepresentation>::FromExternal(&representation)));
}

void FileConnection::Describe(DescribeCompleter::Sync& completer) {
  zx::result sent_describe = WithRepresentation(
      [&](fio::wire::Representation representation) -> zx::result<> {
        ZX_DEBUG_ASSERT(representation.is_file() && representation.file().has_is_append());
        completer.Reply(representation.file());
        return zx::ok();
      },
      std::nullopt);
  if (sent_describe.is_error()) {
    completer.Close(sent_describe.error_value());
    return;
  }
}

void FileConnection::GetConnectionInfo(GetConnectionInfoCompleter::Sync& completer) {
  fidl::Arena arena;
  completer.Reply(fio::wire::ConnectionInfo::Builder(arena).rights(rights()).Build());
}

void FileConnection::Sync(SyncCompleter::Sync& completer) {
  vnode()->Sync([completer = completer.ToAsync()](zx_status_t sync_status) mutable {
    if (sync_status != ZX_OK) {
      completer.ReplyError(sync_status);
    } else {
      completer.ReplySuccess();
    }
  });
}

void FileConnection::GetAttr(GetAttrCompleter::Sync& completer) {
  zx::result attrs = vnode()->GetAttributes();
  if (attrs.is_ok()) {
    completer.Reply(ZX_OK, attrs->ToIoV1NodeAttributes(*vnode()));
  } else {
    completer.Reply(attrs.error_value(), fio::wire::NodeAttributes());
  }
}

void FileConnection::SetAttr(SetAttrRequestView request, SetAttrCompleter::Sync& completer) {
  VnodeAttributesUpdate update =
      VnodeAttributesUpdate::FromIo1(request->attributes, request->flags);
  completer.Reply(Connection::NodeUpdateAttributes(update).status_value());
}

void FileConnection::GetAttributes(fio::wire::NodeGetAttributesRequest* request,
                                   GetAttributesCompleter::Sync& completer) {
  // TODO(https://fxbug.dev/346585458): This operation should require the GET_ATTRIBUTES right.
  internal::NodeAttributeBuilder builder(vnode());
  completer.Reply(builder.Build(request->query));
}

void FileConnection::UpdateAttributes(fio::wire::MutableNodeAttributes* request,
                                      UpdateAttributesCompleter::Sync& completer) {
  VnodeAttributesUpdate update = VnodeAttributesUpdate::FromIo2(*request);
  completer.Reply(Connection::NodeUpdateAttributes(update));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(27)
void FileConnection::DeprecatedGetFlags(DeprecatedGetFlagsCompleter::Sync& completer) {
#else
void FileConnection::GetFlags(GetFlagsCompleter::Sync& completer) {
#endif
  fio::OpenFlags flags = {};
  if (rights() & fio::Rights::kReadBytes) {
    flags |= fio::OpenFlags::kRightReadable;
  }
  if (rights() & fio::Rights::kWriteBytes) {
    flags |= fio::OpenFlags::kRightWritable;
  }
  if (rights() & fio::Rights::kExecute) {
    flags |= fio::OpenFlags::kRightExecutable;
  }
  if (GetAppend()) {
    flags |= fio::OpenFlags::kAppend;
  }
  completer.Reply(ZX_OK, flags);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(27)
void FileConnection::DeprecatedSetFlags(DeprecatedSetFlagsRequestView request,
                                        DeprecatedSetFlagsCompleter::Sync& completer) {
#else
void FileConnection::SetFlags(SetFlagsRequestView request, SetFlagsCompleter::Sync& completer) {
#endif
  const bool append = static_cast<bool>(request->flags & fio::OpenFlags::kAppend);
  completer.Reply(SetAppend(append).status_value());
}

#if FUCHSIA_API_LEVEL_AT_LEAST(27)
void FileConnection::GetFlags(GetFlagsCompleter::Sync& completer) {
  fio::Flags flags = fio::Flags::kProtocolFile | RightsToFlags(rights());
  if (GetAppend()) {
    flags |= fio::Flags::kFileAppend;
  }
  completer.ReplySuccess(flags);
}

void FileConnection::SetFlags(SetFlagsRequestView request, SetFlagsCompleter::Sync& completer) {
  // Only the APPEND flag is allowed.
  if (request->flags & ~fio::Flags::kFileAppend) {
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }
  const bool append = static_cast<bool>(request->flags & fio::Flags::kFileAppend);
  completer.Reply(SetAppend(append));
}
#endif

void FileConnection::QueryFilesystem(QueryFilesystemCompleter::Sync& completer) {
  zx::result result = Connection::NodeQueryFilesystem();
  completer.Reply(result.status_value(),
                  result.is_ok()
                      ? fidl::ObjectView<fio::wire::FilesystemInfo>::FromExternal(&result.value())
                      : nullptr);
}

zx_status_t FileConnection::ResizeInternal(uint64_t length) {
  FS_PRETTY_TRACE_DEBUG("[FileTruncate] rights: ", rights(), ", append: ", GetAppend());
  if (!(rights() & fuchsia_io::Rights::kWriteBytes)) {
    return ZX_ERR_BAD_HANDLE;
  }
  return vnode()->Truncate(length);
}

void FileConnection::Resize(ResizeRequestView request, ResizeCompleter::Sync& completer) {
  zx_status_t result = ResizeInternal(request->length);
  if (result != ZX_OK) {
    completer.ReplyError(result);
  } else {
    completer.ReplySuccess();
  }
}

zx_status_t FileConnection::GetBackingMemoryInternal(fio::wire::VmoFlags flags, zx::vmo* out_vmo) {
  if ((flags & fio::VmoFlags::kPrivateClone) && (flags & fio::VmoFlags::kSharedBuffer)) {
    return ZX_ERR_INVALID_ARGS;
  }
  if ((flags & fio::VmoFlags::kRead) && !(rights() & fio::Rights::kReadBytes)) {
    return ZX_ERR_ACCESS_DENIED;
  }
  if ((flags & fio::VmoFlags::kWrite) && !(rights() & fio::Rights::kWriteBytes)) {
    return ZX_ERR_ACCESS_DENIED;
  }
  if ((flags & fio::VmoFlags::kExecute) && !(rights() & fio::Rights::kExecute)) {
    return ZX_ERR_ACCESS_DENIED;
  }
  return vnode()->GetVmo(flags, out_vmo);
}

void FileConnection::GetBackingMemory(GetBackingMemoryRequestView request,
                                      GetBackingMemoryCompleter::Sync& completer) {
  zx::vmo vmo;
  zx_status_t status = GetBackingMemoryInternal(request->flags, &vmo);
  if (status != ZX_OK) {
    completer.ReplyError(status);
  } else {
    completer.ReplySuccess(std::move(vmo));
  }
}

void FileConnection::AdvisoryLock(fidl::WireServer<fio::File>::AdvisoryLockRequestView request,
                                  AdvisoryLockCompleter::Sync& completer) {
  // advisory_lock replies to the completer
  auto async_completer = completer.ToAsync();
  fit::callback<void(zx_status_t)> callback = file_lock::lock_completer_t(
      [lock_completer = std::move(async_completer)](zx_status_t status) mutable {
        lock_completer.ReplyError(status);
      });

  advisory_lock(koid_, vnode(), true, request->request, std::move(callback));
}

void FileConnection::handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_io::File>,
                                           fidl::UnknownMethodCompleter::Sync&) {}

}  // namespace fs::internal
