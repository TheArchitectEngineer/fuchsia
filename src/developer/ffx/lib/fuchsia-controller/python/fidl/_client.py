# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# TODO(https://fxbug.dev/346628306): Remove this comment to ignore mypy errors.
# mypy: ignore-errors

import asyncio
import logging
from abc import abstractmethod
from inspect import getframeinfo, stack
from typing import Any, Dict, Set

import fuchsia_controller_py as fc
from fidl_codec import decode_fidl_response, encode_fidl_message

from ._fidl_common import (
    FIDL_EPITAPH_ORDINAL,
    EpitaphError,
    FidlMessage,
    FidlMeta,
    StopEventHandler,
    TXID_Type,
    parse_epitaph_value,
    parse_ordinal,
    parse_txid,
)
from ._ipc import EventWrapper, GlobalHandleWaker

TXID: TXID_Type = 0
# Simple client ID. Monotonically increasing for each client.
_CLIENT_ID = 0
_LOGGER = logging.getLogger("fidl.client")


class FidlClient(metaclass=FidlMeta):
    @staticmethod
    @abstractmethod
    def construct_response_object(
        response_ident: str, response_obj: Any
    ) -> Any:
        ...

    def __init__(self, channel, channel_waker=None):
        global _CLIENT_ID
        self.id = _CLIENT_ID
        _CLIENT_ID += 1
        if type(channel) is int:
            self._channel = fc.Channel(channel)
        else:
            self._channel = channel
        if channel_waker is None:
            self._channel_waker = GlobalHandleWaker()
        else:
            self._channel_waker = channel_waker
        self.pending_txids: Set[TXID_Type] = set({})
        self.staged_messages: Dict[TXID_Type, asyncio.Queue[FidlMessage]] = {}
        self.epitaph_received: EpitaphError | None = None
        self.epitaph_event = EventWrapper()
        caller = getframeinfo(stack()[1][0])
        _LOGGER.debug(
            f"{self} instantiated from {caller.filename}:{caller.lineno}"
        )

    def close_cleanly(self):
        """Closes the underlying channel safely.

        This is so-named to avoid name conflicts with existing FIDL methods.
        A potential other method here is to make this appear private (leading underscore).
        """
        _LOGGER.debug(f"{self} closing from caller")
        self._close()

    def __str__(self):
        return f"client:{type(self).__name__}:{self.id}"

    def __del__(self):
        _LOGGER.debug(f"{self} closing from GC")
        self._close()

    def _close(self):
        if self._channel is not None:
            self._channel_waker.unregister(self._channel)
            self._channel.close()
            self._channel = None

    async def _get_staged_message(self, txid: TXID_Type):
        res = await self.staged_messages[txid].get()
        self.staged_messages[txid].task_done()
        return res

    def _stage_message(self, txid: TXID_Type, msg: FidlMessage):
        # This should only ever happen if we're a channel reading another channel's response before
        # it has ever made a request.
        if txid not in self.staged_messages:
            self.staged_messages[txid] = asyncio.Queue(1)
        self.staged_messages[txid].put_nowait(msg)

    def _clean_staging(self, txid: TXID_Type):
        self.staged_messages.pop(txid)
        # Events are never added to this set, since they're always pending.
        if txid != 0:
            self.pending_txids.remove(txid)

    def _decode(self, txid: TXID_Type, msg: FidlMessage) -> Dict[str, Any]:
        self._clean_staging(txid)
        handles = [m.take() for m in msg[1]]
        return decode_fidl_response(bytes=msg[0], handles=handles)

    async def next_event(self) -> FidlMessage:
        """Attempts to read the next FIDL event from this client.

        Returns:
            The next FIDL event. If ZX_ERR_PEER_CLOSED is received on the channel, will return None.
            Note: this does not check to see if the protocol supports any events, so if not this
            function could wait forever.

        Raises:
            Any exceptions other than ZX_ERR_PEER_CLOSED (fuchsia_controller_py.ZxStatus)
        """
        # TODO(awdavies): Raise an exception if there are no events supported for this client.
        try:
            self._channel_waker.register(self._channel)
            return await self._read_and_decode(0)
        except fc.ZxStatus as e:
            if e.args[0] != fc.ZxStatus.ZX_ERR_PEER_CLOSED:
                _LOGGER.warning(
                    f"{self} received error waiting for next event: {e}"
                )
                self._channel_waker.unregister(self._channel)
                raise e
        return None

    def _epitaph_check(self, msg: FidlMessage):
        # If the epitaph is already set, no need to continue with the remaining
        # work.
        if self.epitaph_received is not None:
            raise self.epitaph_received

        ordinal = parse_ordinal(msg)
        if ordinal == FIDL_EPITAPH_ORDINAL:
            if self.epitaph_received is None:
                self.epitaph_received = EpitaphError(parse_epitaph_value(msg))
                self.epitaph_event.set()
            raise self.epitaph_received

    async def _epitaph_event_wait(self):
        await self.epitaph_event.wait()
        raise self.epitaph_received

    async def _read_and_decode(self, txid: int):
        if txid not in self.staged_messages:
            self.staged_messages[txid] = asyncio.Queue(1)
        while True:
            # The main gist of this loop is:
            # 1.) Try to read from the channel.
            #   a.) If we read the message and it matches out TXID we're done.
            #   b.) If we read the message and it doesn't match our TXID, we "stage" the message for
            #       another task to read later, then we wait.
            #   c.) If we get a ZX_ERR_SHOULD_WAIT, we need to wait.
            # 2.) Once we're waiting, we select on either the handle being ready to read again, or
            #     on a staged message becoming available.
            # 3.) If the select returns something that isn't a staged message, continue the loop
            #     again.
            #
            # It's pretty straightforward on paper but requires a bit of bookkeeping for the corner
            # cases to prevent memory leaks.
            try:
                if self.epitaph_received is not None:
                    raise self.epitaph_received
                msg = self._channel.read()
                self._epitaph_check(msg)
                recvd_txid = parse_txid(msg)
                if recvd_txid == txid:
                    if txid != 0:
                        return self._decode(txid, msg)
                    else:
                        # There's additional message processing for events, so instead return the
                        # raw bytes/handles.
                        self._clean_staging(txid)
                        return msg
                if recvd_txid != 0 and recvd_txid not in self.pending_txids:
                    self._channel_waker.unregister(self._channel)
                    self._channel = None
                    _LOGGER.warning(
                        f"{self} received unexpected TXID: {recvd_txid}"
                    )
                    raise RuntimeError(
                        f"{self} received unexpected TXID. Channel closed and invalid. "
                        + "Continuing to use this FIDL client after this exception will result "
                        + "in undefined behavior"
                    )
                self._stage_message(recvd_txid, msg)
            except EpitaphError as ep:
                # This is to avoid some possible race conditions with the below
                # where unregistering can happen at the same time as receiving
                # an epitaph error. It should not unregister the channel.
                _LOGGER.warning(f"{self} received epitaph error: {ep}")
                raise ep
            except fc.ZxStatus as e:
                if e.args[0] != fc.ZxStatus.ZX_ERR_SHOULD_WAIT:
                    self._channel_waker.unregister(self._channel)
                    _LOGGER.warning(f"{self} received channel error: {e}")
                    raise e
            loop = asyncio.get_running_loop()
            channel_waker_task = loop.create_task(
                self._channel_waker.wait_ready(self._channel)
            )
            staged_msg_task = loop.create_task(self._get_staged_message(txid))
            epitaph_event_task = loop.create_task(self._epitaph_event_wait())
            done, pending = await asyncio.wait(
                [
                    channel_waker_task,
                    staged_msg_task,
                    epitaph_event_task,
                ],
                return_when=asyncio.FIRST_COMPLETED,
            )
            for p in pending:
                p.cancel()
            # Multiple notifications happened at the same time.
            if len(done) > 1:
                results = [r.result() for r in done]
                # Order of asyncio.wait is not guaranteed, so check all
                # results. If there's an epitaph, running the "result()"
                # function will raise an exception, so there are
                # only two values we can ever have here.
                first = results.pop()
                second = results.pop()
                if type(first) == int:
                    msg = second
                else:
                    msg = first

                # Since both the channel and the staged message were available, we've chosen to take
                # the staged message. To ensure another task can be awoken, we must post an event
                # saying the channel still needs to be read, since we've essentilly stolen it from
                # another task.
                self._channel_waker.post_ready(self._channel)
                return self._decode(txid, msg)
            # Only one notification came in.
            msg = done.pop().result()
            if type(msg) != int:  # Not a FIDL channel response
                return self._decode(txid, msg)

    def _send_two_way_fidl_request(
        self, ordinal, library, msg_obj, response_ident
    ):
        """Sends a two-way asynchronous FIDL request.

        Args:
            ordinal: The method ordinal (for encoding).
            library: The FIDL library from which this method ordinal exists.
            msg_obj: The object being sent.
            response_ident: The full FIDL identifier of the response object, e.g. foo.bar/Baz

        Returns:
            The object from the two-way function, as constructed from the response_ident type.
        """
        global TXID
        TXID += 1
        self._channel_waker.register(self._channel)
        self.pending_txids.add(TXID)
        self._send_one_way_fidl_request(TXID, ordinal, library, msg_obj)

        async def result(txid):
            # This is called a second time because the first attempt may have been in a sync context
            # and would not have added a reader.
            self._channel_waker.register(self._channel)
            res = await self._read_and_decode(txid)
            return self.construct_response_object(response_ident, res)

        return result(TXID)

    def _send_one_way_fidl_request(
        self, txid: int, ordinal: int, library: str, msg_obj
    ):
        """Sends a synchronous one-way FIDL request.

        Args:
            ordinal: The method ordinal (for encoding).
            library: The FIDL library from which this method ordinal exists.
            msg_obj: The object being sent.
        """
        type_name = None
        if msg_obj is not None:
            type_name = msg_obj.__fidl_raw_type__
        encoded_fidl_message = encode_fidl_message(
            ordinal=ordinal,
            object=msg_obj,
            library=library,
            txid=txid,
            type_name=type_name,
        )
        self._channel.write(encoded_fidl_message)


class EventHandlerBase(
    metaclass=FidlMeta,
    required_class_variables=[
        ("library", str),
        ("method_map", dict),
    ],
):
    """Base object for doing FIDL client event handling."""

    @staticmethod
    @abstractmethod
    def construct_response_object(
        response_ident: str, response_obj: Any
    ) -> Any:
        ...

    def __init__(self, client: FidlClient):
        self.client = client
        self.client._channel_waker.register(self.client._channel)

    def __str__(self):
        return f"event:{type(self.client).__name__}:{self.client.id}"

    async def serve(self):
        while True:
            msg = await self.client.next_event()
            # msg is None if the channel has been closed.
            if msg is None:
                break
            if not await self._handle_request(msg):
                break

    async def _handle_request(self, msg: FidlMessage):
        try:
            await self._handle_request_helper(msg)
            return True
        except StopEventHandler:
            return False

    async def _handle_request_helper(self, msg: FidlMessage):
        ordinal = parse_ordinal(msg)
        handles = [x.take() for x in msg[1]]
        decoded_msg = decode_fidl_response(bytes=msg[0], handles=handles)
        method = self.method_map[ordinal]
        request_ident = method.request_ident
        request_obj = self.construct_response_object(request_ident, decoded_msg)
        method_lambda = getattr(self, method.name)
        if request_obj is not None:
            res = method_lambda(request_obj)
        else:
            res = method_lambda()
        if asyncio.iscoroutine(res) or asyncio.isfuture(res):
            await res
