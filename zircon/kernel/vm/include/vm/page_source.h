// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_H_

#include <zircon/types.h>

#include <fbl/intrusive_wavl_tree.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/event.h>
#include <kernel/lockdep.h>
#include <kernel/mutex.h>
#include <ktl/optional.h>
#include <ktl/unique_ptr.h>
#include <vm/anonymous_page_request.h>
#include <vm/page.h>
#include <vm/vm.h>

// At the high level the goal of the objects here is to
// 1. Trigger external entities to do work based on VMO operations, such as asking a pager to supply
//    a missing page of data.
// 2. Have a way for external entities to let the VMO system know these requests have been
//    fulfilled.
// 3. Provide a way for the high level caller, who may not know what actions are being performed on
//    what entities, to wait until their operation can be completed.
//
// The different objects can be summarized as:
//  * PageRequest: Caller allocated object that the caller uses to perform the Wait.
//  * PageRequestInterface: A reference to an object implementing this interface is held by the
//    PageRequest and provides a way for the PageRequest to interact with the underlying PageSource.
//  * PageSource: Performs request and overlap tracking, forwarding unique ranges of requests to the
//    underlying PageProvider.
//  * PageProvider: Asynchronously performs requests. Requests are completed by actions being
//    performed on the VMO.
//
// A typical flow would be
//  * User allocates PageRequest on the stack, and passes it in to some VMO operation
//  * VMO code needs something to happen and calls a PageSource method, passing in PageRequest it
//    had been given.
//  * PageSource populates fields of the PageRequest and adds it to the list of requests it is
//    tracking, and determines how this request overlaps with any others. Based on overlap, it may
//    or may not notify the underlying PageProvider that some work needs to be done (the page
//    provider will complete this asynchronously somehow).
//  * VMO returns ZX_ERR_SHOULD_WAIT and then the top level calls PageRequest::Wait
//  * PageRequest::Wait uses the PageRequestInterface to ask the underlying PageSource how to Wait
//    for the operation to complete
//  # As an optional path, if the PageRequest was not Waited on for some reason, the PageRequest
//    will also use the PageRequestInterface to inform the PageSource that this request is no longer
//    needed and can be canceled.
// For the other side, while the Wait is happening some other thread will
//  * Call a VMO operation, such as VmObject::SupplyPages
//  * VMO will perform the operation, and then let the PageSource know by the corresponding
//    interface method, such as OnPagesSupplied.
//  * PageSource will update request tracking, and notify any PageRequests that were waiting and can
//    be woken up.
//
// There is more complexity of implementation and API, largely to handle the fact that the
// PageRequest serves as the allocation of all data needed for all parties. Therefore every layer
// needs to be told when requests are coming and going to ensure they update any lists and do not
// refer to out of scope stack variables.

class PageRequest;
class PageSource;
class AnonymousPageRequester;

struct VmoDebugInfo {
  uint64_t vmo_id;
  char vmo_name[8];
};

// The different types of page requests that can exist.
enum page_request_type : uint32_t {
  READ = 0,   // Request to provide the initial contents for the page.
  DIRTY,      // Request to alter contents of the page, i.e. transition it from clean to dirty.
  WRITEBACK,  // Request to write back modified page contents back to the source.
  COUNT       // Number of page request types.
};

inline const char* PageRequestTypeToString(page_request_type type) {
  switch (type) {
    case page_request_type::READ:
      return "READ";
    case page_request_type::DIRTY:
      return "DIRTY";
    case page_request_type::WRITEBACK:
      return "WRITEBACK";
    default:
      return "UNKNOWN";
  }
}

// These properties are constant per PageProvider type, so a given VmCowPages can query and cache
// these properties once (if it has a PageSource) and know they won't change after that.  This also
// avoids per-property plumbing via PageSource.
//
// TODO(dustingreen): (or rashaeqbal) Migrate more const per-PageProvider-type properties to
// PageSourceProperties, after the initial round of merging is done.
struct PageSourceProperties {
  // We use PageSource for both user pager and contiguous page reclaim.  This is how we tell whether
  // the PageSource is really a user pager when reporting to user mode that a given VMO is/isn't
  // user pager backed.  This property should not be used for other purposes since we can use more
  // specific properties for any behavior differences.
  const bool is_user_pager;

  // Currently, this is always equal to is_user_pager, but per the comment on is_user_pager, we
  // prefer to use more specific behavior properties rather than lean on is_user_pager.
  //
  // True iff providing page content.  This can be immutable page content, or it can be page content
  // that was potentially modified and written back previously.
  //
  // If this is false, the provider will ensure (possibly with VmCowPages help) that pages are
  // zeroed by the time they are added to the VmCowPages.
  const bool is_preserving_page_content;

  // Iff true, the PageSource (and PageProvider) must be used to allocate all pages.  Pre-allocating
  // generic pages from the pmm won't work. These pages must be specifically returned via
  // PageSource::FreePages instead of pmm_free.
  const bool is_providing_specific_physical_pages;

  // For every entry, if true the PageSource supports the given |page_request_type|.
  const bool supports_request_type[page_request_type::COUNT];
};

// Interface for providing pages to a VMO through page requests.
class PageProvider : public fbl::RefCounted<PageProvider> {
 public:
  virtual ~PageProvider() = default;

 protected:
  // Methods a PageProvider implementation can use to retrieve fields from a PageRequest.
  static page_request_type GetRequestType(const PageRequest* request);
  static uint64_t GetRequestOffset(const PageRequest* request);
  static uint64_t GetRequestLen(const PageRequest* request);
  static uint64_t GetRequestVmoId(const PageRequest* request);

 private:
  // The returned properties can assumed to be const and never change. As such the caller may cache
  // them.
  virtual PageSourceProperties properties() const = 0;

  // Informs the backing source of a page request. The provider has ownership
  // of |request| until the async request is cancelled.
  virtual void SendAsyncRequest(PageRequest* request) = 0;
  // Informs the backing source that a page request has been fulfilled. This
  // must be called for all requests that are raised.
  virtual void ClearAsyncRequest(PageRequest* request) = 0;
  // Swaps the backing memory for a request. Assumes that |old|
  // and |new_request| have the same type, offset, and length.
  virtual void SwapAsyncRequest(PageRequest* old, PageRequest* new_req) = 0;
  // This will assert unless is_handling_free is true, in which case this will make the pages FREE.
  virtual void FreePages(list_node* pages) {
    // If is_handling_free true, must implement FreePages().
    ASSERT(false);
  }
  // For asserting purposes only.  This gives the PageProvider a chance to check that a page is
  // consistent with any rules the PageProvider has re. which pages can go where in the VmCowPages.
  // PhysicalPageProvider implements this to verify that page at offset makes sense with respect to
  // phys_base_, since VmCowPages can't do that on its own due to lack of knowledge of phys_base_
  // and lack of awareness of contiguous.
  virtual bool DebugIsPageOk(vm_page_t* page, uint64_t offset) = 0;

  // OnDetach is called once no more calls to SendAsyncRequest will be made. It will be called
  // before OnClose and will only be called once.
  virtual void OnDetach() = 0;
  // After OnClose is called, no more calls will be made except for ::WaitOnEvent.
  virtual void OnClose() = 0;

  // Waits on an |event| associated with a page request. The waiting thread can return early from
  // the wait due to a suspend signal only if |suspendable| is true.
  virtual zx_status_t WaitOnEvent(Event* event, bool suspendable) = 0;

  // Dumps relevant state for debugging purposes. The |max_items| parameter should be used to cap
  // the number of elements printed from any kind of variable sized list to prevent spam.
  virtual void Dump(uint depth, uint max_items) = 0;

  friend PageSource;
};

// Interface used by the page requests to communicate with the PageSource. Due to the nature of
// intrusive containers the RefCounted needs to be here and not on the PageSource to allow the
// PageRequest to hold a RefPtr just to this interface.
class PageRequestInterface : public fbl::RefCounted<PageRequestInterface> {
 public:
  virtual ~PageRequestInterface() = default;

 protected:
  PageRequestInterface() = default;

 private:
  friend PageRequest;
  // Instruct the page source that this request has been cancelled.
  virtual void CancelRequest(PageRequest* request) = 0;
  // Ask the page source to wait on this request, typically by forwarding to the page provider.
  // Note this gets called without a lock and so due to races the implementation needs to be
  // tolerant of having already been detached/closed. The waiting thread can return early from
  // the wait due to a suspend signal only if |suspendable| is true.
  virtual zx_status_t WaitOnRequest(PageRequest* request, bool suspendable) = 0;
};

// A page source is responsible for fulfilling page requests from a VMO with backing pages.
// The PageSource class mostly contains generic functionality around managing
// the lifecycle of VMO page requests. The PageSource contains an reference to a PageProvider
// implementation, which is responsible for actually providing the pages. (E.g. for VMOs backed by a
// userspace pager, the PageProvider is a PagerProxy instance which talks to the userspace pager
// service.)
//
// The synchronous fulfillment of requests is fairly straightforward, with direct calls
// from the vm object to the PageSource to the PageProvider.
//
// For asynchronous requests, the lifecycle is as follows:
//   1) A vm object requests a page with PageSource::GetPage.
//   2) PageSource starts tracking the request's PageRequest and then
//      forwards the request to PageProvider::SendAsyncRequest.
//   3) The caller waits for the request with PageRequest::Wait.
//   4) At some point, whatever is backing the PageProvider provides pages
//      to the vm object (e.g. with VmObjectPaged::SupplyPages).
//   5) The vm object calls PageSource::OnPagesSupplied, which signals
//      any PageRequests that have been fulfilled.
//   6) The caller wakes up and queries the vm object again, by which
//      point the requested page will be present.
//
// For a contiguous VMO requesting physical pages back, step 4 above just frees the pages from some
// other use, and step 6 finds the requested pages available, but not yet present in the VMO,
// similar to what can happen with a normal PageProvider where pages can be read and then
// decommitted before the caller queries the vm object again.

// Object which provides pages to a vm_object.
class PageSource final : public PageRequestInterface {
 public:
  PageSource() = delete;
  explicit PageSource(fbl::RefPtr<PageProvider>&& page_provider);

  // Sends a request to the backing source to provide the requested page at |offset|.
  //
  // Returns ZX_ERR_NOT_FOUND if the request cannot be fulfilled.
  // Returns ZX_ERR_SHOULD_WAIT if the request will be asynchronously fulfilled and the caller
  // should wait on |req|.
  zx_status_t GetPages(uint64_t offset, uint64_t len, PageRequest* req,
                       VmoDebugInfo vmo_debug_info) {
    return PopulateRequest(req, offset, len, vmo_debug_info, page_request_type::READ);
  }

  void FreePages(list_node* pages);

  // For asserting purposes only.  This gives the PageProvider a chance to check that a page is
  // consistent with any rules the PageProvider has re. which pages can go where in the VmCowPages.
  // PhysicalPageProvider implements this to verify that page at offset makes sense with respect to
  // phys_base_, since VmCowPages can't do that on its own due to lack of knowledge of phys_base_
  // and lack of awareness of contiguous.
  bool DebugIsPageOk(vm_page_t* page, uint64_t offset);

  // Updates the request tracking metadata to account for pages [offset, offset + len) having
  // been supplied to the owning vmo.
  //
  // Note that the range [offset, offset + len) should not have been previously supplied. The page
  // request tracking in PageSource works by tracking only a fulfilled length, and not exact
  // fulfilled offsets, to save on memory required for metadata. So in order to prevent
  // over-accounting errors, the caller must ensure that they are only calling OnPagesSupplied for
  // newly supplied ranges.
  // TODO(rashaeqbal): Consider relaxing this constraint by more precise tracking of fulfilled
  // offsets with a bitmap. Might require capping the max permissible length of a page request.
  void OnPagesSupplied(uint64_t offset, uint64_t len);

  // Fails outstanding page requests in the range [offset, offset + len). Events associated with the
  // failed page requests are signaled with the |error_status|, and any waiting threads are
  // unblocked.
  void OnPagesFailed(uint64_t offset, uint64_t len, zx_status_t error_status);

  // Returns true if |error_status| is a valid ZX_PAGER_OP_FAIL failure error code (input, specified
  // by user mode pager).  These codes can be used with |OnPagesFailed| (and so can any failure
  // codes for which IsValidInternalFailureCode() returns true).
  //
  // Not every error code is supported, since these errors can get returned via a zx_vmo_read() or a
  // zx_vmo_op_range(), if those calls resulted in a page fault.  So the |error_status| should be a
  // supported return error code for those syscalls _and_ be an error code that we want to be
  // supported for the user mode pager to specify via ZX_PAGER_OP_FAIL.  Currently,
  // IsValidExternalFailureCode(ZX_ERR_NO_MEMORY) returns false, as we don't want ZX_ERR_NO_MEMORY
  // to be specified via ZX_PAGER_OP_FAIL (at least so far).
  static bool IsValidExternalFailureCode(zx_status_t error_status);

  // Returns true if |error_status| is a valid provider failure error code, which can be used with
  // |OnPagesFailed|.
  //
  // This returns true for every error code that IsValidExternalFailureCode() returns true for, plus
  // any additional error codes that are valid as an internal PageProvider status but not valid for
  // ZX_PAGER_OP_FAIL.
  //
  // ZX_ERR_NO_MEMORY will return true, unlike IsValidExternalFailureCode(ZX_ERR_NO_MEMORY) which
  // returns false.
  //
  // Not every error code is supported, since these errors can get returned via a zx_vmo_read() or a
  // zx_vmo_op_range(), if those calls resulted in a page fault.  So the |error_status| should be a
  // supported return error code for those syscalls.  An error code need not be specifiable via
  // ZX_PAGER_OP_FAIL for this function to return true.
  static bool IsValidInternalFailureCode(zx_status_t error_status);

  bool SupportsPageRequestType(page_request_type type) const {
    return properties().supports_request_type[type];
  }

  // Whether transitions from clean to dirty should be trapped.
  bool ShouldTrapDirtyTransitions() const {
    return SupportsPageRequestType(page_request_type::DIRTY);
  }

  // Request the page provider for clean pages in the range [offset, offset + len) to become dirty,
  // in order for a write to proceed. Returns ZX_ERR_SHOULD_WAIT if the request will be
  // asynchronously fulfilled; the caller should wait on |request|. Depending on the state of pages
  // in the range, the |request| might be generated for a range that is a subset of
  // [offset, offset + len).
  zx_status_t RequestDirtyTransition(PageRequest* request, uint64_t offset, uint64_t len,
                                     VmoDebugInfo vmo_debug_info) {
    return PopulateRequest(request, offset, len, vmo_debug_info, page_request_type::DIRTY);
  }

  // Updates the request tracking metadata to account for pages [offset, offset + len) having
  // been dirtied in the owning VMO.
  //
  // Note that the range [offset, offset + len) should not have been previously dirtied. The page
  // request tracking in PageSource works by tracking only a fulfilled length, and not exact
  // fulfilled offsets, to save on memory required for metadata. So in order to prevent
  // over-accounting errors, the caller must ensure that they are only calling OnPagesDirtied for
  // newly dirtied ranges.
  // TODO(rashaeqbal): Consider relaxing this constraint by more precise tracking of fulfilled
  // offsets with a bitmap. Might require capping the max permissible length of a page request.
  void OnPagesDirtied(uint64_t offset, uint64_t len);

  // Detaches the source from the VMO. All future calls into the page source will fail. All
  // pending read transactions are aborted. Pending flush transactions will still
  // be serviced.
  void Detach();

  // Closes the source. Will call Detach() if the source is not already detached. All pending
  // transactions will be aborted and all future calls will fail.
  void Close();

  // The returned properties will last at least until Detach() or Close().
  const PageSourceProperties& properties() const { return page_provider_properties_; }

  // Prints state of the page source and any pending requests. The maximum number of requests
  // printed is capped by |max_items|.
  void Dump(uint depth, uint32_t max_items) const;
  // Similar to Dump, but only dumps information about this exact object, and will not forward the
  // Dump request to the related PageProvider.
  void DumpSelf(uint depth, uint32_t max_items) const;

  bool is_detached() const {
    Guard<Mutex> guard{&page_source_mtx_};
    return detached_;
  }

  // Method for the VmCowPages to retrieve the lock for paged VMOs.
  // See VmCowPages::DeferredOps.
  Lock<Mutex>* paged_vmo_lock() { return &paged_vmo_mutex_; }

 protected:
  // destructor should only be invoked from RefPtr
  virtual ~PageSource();
  friend fbl::RefPtr<PageSource>;

 private:
  fbl::Canary<fbl::magic("VMPS")> canary_;

  // Lock used by the VMO to perform synchronization across its hierarchy. This lock does not
  // strictly belong here, but this is a convenient and efficient place to put it.
  // See VmCowPages::DeferredOps for more.
  DECLARE_MUTEX(PageSource) paged_vmo_mutex_;

  mutable DECLARE_MUTEX(PageSource) page_source_mtx_;
  bool detached_ TA_GUARDED(page_source_mtx_) = false;
  bool closed_ TA_GUARDED(page_source_mtx_) = false;
  // We cache the immutable page_provider_->properties() to avoid many virtual calls.
  const PageSourceProperties page_provider_properties_;

  // Trees of outstanding requests which have been sent to the PageProvider, one for each supported
  // page request type. These lists are keyed by the end offset of the requests (not the start
  // offsets).
  fbl::WAVLTree<uint64_t, PageRequest*> outstanding_requests_[page_request_type::COUNT] TA_GUARDED(
      page_source_mtx_);

  // PageProvider instance that will provide pages asynchronously (e.g. a userspace pager, see
  // PagerProxy for details).
  const fbl::RefPtr<PageProvider> page_provider_;

  // Helper that adds the span of |len| pages at |offset| to |request| and forwards it to the
  // provider. |request| must already be initialized, and its page_request_type must be set to
  // |type|. |offset| must be page-aligned.
  //
  // This function will always return |ZX_ERR_SHOULD_WAIT|.
  zx_status_t PopulateRequestLocked(PageRequest* request, uint64_t offset, uint64_t len,
                                    VmoDebugInfo vmo_debug_info, page_request_type type)
      TA_REQ(page_source_mtx_);

  // Sends a request to the backing source, or adds the request to the overlap_ list if
  // the needed region has already been requested from the source.
  void SendRequestToProviderLocked(PageRequest* request) TA_REQ(page_source_mtx_);

  // Wakes up the given PageRequest and all overlapping requests.
  void CompleteRequestLocked(PageRequest* request) TA_REQ(page_source_mtx_);

  // Helper that updates request tracking metadata to resolve requests of |type| in the range
  // [offset, offset + len).
  void ResolveRequestsLocked(page_request_type type, uint64_t offset, uint64_t len,
                             zx_status_t error_status) TA_REQ(page_source_mtx_);

  // Helper to perform early waking on a request and any overlapping requests. The provided range
  // should be in local request space, and this method is only valid to be called if
  // |request->wake_offset_ == req_start|.
  void EarlyWakeRequestLocked(PageRequest* request, uint64_t req_start, uint64_t req_end)
      TA_REQ(page_source_mtx_);

  // Removes |request| from any internal tracking. Called by a PageRequest if
  // it needs to abort itself.
  void CancelRequest(PageRequest* request) override TA_EXCL(page_source_mtx_);

  void CancelRequestLocked(PageRequest* request) TA_REQ(page_source_mtx_);

  zx_status_t PopulateRequest(PageRequest* request, uint64_t offset, uint64_t len,
                              VmoDebugInfo vmo_debug_info, page_request_type type);

  zx_status_t WaitOnRequest(PageRequest* request, bool suspendable) override;

  // Helper that takes an existing request and a new request range and returns whether the new
  // range is any kind of continuation of the existing request. This is used for a mixture of
  // correctness validation and supporting early wake requests.
  enum class ContinuationType {
    NotContinuation,
    SameRequest,
    SameSource,
  };
  ContinuationType RequestContinuationTypeLocked(const PageRequest* request, uint64_t offset,
                                                 uint64_t len, page_request_type type)
      TA_REQ(page_source_mtx_);
};

// The PageRequest provides the ability to be in two difference linked list. One owned by the page
// source (for overlapping requests), and one owned by the page provider (for tracking outstanding
// requests). These tags provide a way to distinguish between the two containers.
struct PageSourceTag;
struct PageProviderTag;
// Object which is used to make delayed page requests to a PageSource
class PageRequest : public fbl::WAVLTreeContainable<PageRequest*>,
                    public fbl::ContainableBaseClasses<
                        fbl::TaggedDoublyLinkedListable<PageRequest*, PageSourceTag>,
                        fbl::TaggedDoublyLinkedListable<PageRequest*, PageProviderTag>> {
 public:
  PageRequest() : PageRequest(false) {}
  // If early_wake is true then the caller is asking to be woken up once some of the request is
  // satisfied, potentially before all of it is satisfied. This is intended to allow users to
  // process partial amounts of data as they come in before continuing to Wait for the rest with
  // only a single PageRequest sent to the PageSource.
  explicit PageRequest(bool early_wake) : early_wake_(early_wake) {}
  ~PageRequest();

  // Returns ZX_OK on success, or a permitted error code if the backing page provider explicitly
  // failed this page request. Returns ZX_ERR_INTERNAL_INTR_KILLED if the thread was killed.
  // Returns ZX_ERR_INTERNAL_INTR_RETRY if |suspendable| is true and the thread was suspended; the
  // thread cannot be suspended in the wait if |suspendable| is false.
  // If this page requested is allowed to early wake then this can return success with the request
  // still active and queued with a PageSource. In this case it is invalid to attempt to use this
  // request with any other PageSource or for any other range without first doing CancelRequest.
  zx_status_t Wait(bool suspendable);

  // Asks the underlying PageRequestInterface to abort this request, by calling
  // PageRequestInterface::CancelRequest. As this can be called from non PageSource paths, and hence
  // without the PageSource lock held, the PageRequestInterface must always be invoked to
  // synchronize with this request being completed by another thread.
  // This method is not thread safe and cannot be called in parallel with Init.
  void CancelRequest();

  DISALLOW_COPY_ASSIGN_AND_MOVE(PageRequest);

 private:
  // TODO: PageSource and AnonymousPageRequest should not have direct access, but should rather have
  // their access mediate by the PageRequestInterface class that they derive from.
  friend PageSource;
  friend AnonymousPageRequester;

  friend PageProvider;
  friend fbl::DefaultKeyedObjectTraits<uint64_t, PageRequest>;

  // PageRequests are initialized separately to being constructed to facilitate any PageSource
  // specific logic. This method makes three assumptions on how it is called:
  //  1. If previously initialized it has been separately uninitialized via `CancelRequest` or
  //     similar.
  //  2. It is invoked under the src lock.
  //  3. It is called on the thread that owns the PageRequest and is not thread safe with parallel
  //     invocations of CancelRequest.
  void Init(fbl::RefPtr<PageRequestInterface> src, uint64_t offset, page_request_type type,
            VmoDebugInfo vmo_debug_info);

  bool IsInitialized() const { return offset_ != UINT64_MAX; }

  uint64_t GetEnd() const {
    // Assert on overflow, since it means vmobject made an out-of-bounds request.
    uint64_t unused;
    DEBUG_ASSERT(!add_overflow(offset_, len_, &unused));

    return offset_ + len_;
  }

  uint64_t GetKey() const { return GetEnd(); }

  bool RangeOverlaps(uint64_t start, uint64_t end) const {
    return end > offset_ && start < GetEnd();
  }

  // Converts a [start,end) range in provider (aka VMO) space to the sub range that overlaps with
  // this request and returns it relative to this requests offset_.
  ktl::pair<uint64_t, uint64_t> TrimRangeToRequestSpace(uint64_t start, uint64_t end) const;

  // The type of the page request.
  page_request_type type_;

  // PageRequests are active if offset_ is not UINT64_MAX. In an inactive request, the
  // only other valid field is src_. Whilst a request is with a PageProvider (i.e. SendAsyncRequest
  // has been called), these fields must be kept constant so the PageProvider can read them. Once
  // the request has been cleared either by SwapAsyncRequest or ClearAsyncRequest they can be
  // modified again. The provider_owned_ bool is used for assertions to validate this flow, but
  // otherwise has no functional affect.
  bool provider_owned_ = false;

  // Set on construction if the user of the PageRequest supports, and wants to be, woken early. If
  // this is true then wake_offset_ will be set to zero when a request is initialized. Early waking
  // is intended to allow for an optimization under the assumption that large requests will be
  // filled sequentially, allowing for a single request to be made to the underlying page source,
  // but processing being able to start before the entire request has been completed.
  // When an early_wake_ request is signaled the user cannot assume that the request is fully
  // complete, and as a consequence must not attempt to use the PageRequest in a new context without
  // first cancelling it.
  const bool early_wake_;

  // The offset into the request at which the event_ should next be signaled. This is request
  // relative, so a value of 0 indicates that it should be signaled when the page at offset_ is
  // provided. After being triggered, the wake_offset_ increments by the amount provided so that it
  // can potentially get triggered again.
  uint64_t wake_offset_ = UINT64_MAX;

  // Tracks any error that we will send to the waiter when the request is completed. This allows for
  // partial failure of a request, where we report the status of the first page in the request so
  // that any partially provided pages can be processed.
  zx_status_t complete_status_ = ZX_OK;

  // The page source this request is currently associated with. This may only be modified by Init
  // and must otherwise be constant, allowing the PageRequest to safely inspect this value without
  // races.
  fbl::RefPtr<PageRequestInterface> src_;
  // Event signaled when the request is fulfilled.
  AutounsignalEvent event_;
  uint64_t offset_ = UINT64_MAX;
  // The total length of the request.
  uint64_t len_ = 0;
  // The vmobject this page request is for.
  VmoDebugInfo vmo_debug_info_ = {};

  // Keeps track of the size of the request that still needs to be fulfilled. This
  // can become incorrect if some pages get supplied, decommitted, and then
  // re-supplied. If that happens, then it will cause the page request to complete
  // prematurely. However, page source clients should be operating in a loop to handle
  // evictions, so this will simply result in some redundant read requests to the
  // page source. Given the rarity in which this situation should arise, it's not
  // worth the complexity of tracking it.
  uint64_t pending_size_ = 0;

  // Linked list for overlapping requests.
  fbl::TaggedDoublyLinkedList<PageRequest*, PageSourceTag> overlap_;
};

// Declare page provider helpers inline now that PageRequest has been defined.
inline page_request_type PageProvider::GetRequestType(const PageRequest* request) {
  DEBUG_ASSERT(request->provider_owned_);
  return request->type_;
}
inline uint64_t PageProvider::GetRequestOffset(const PageRequest* request) {
  DEBUG_ASSERT(request->provider_owned_);
  return request->offset_;
}
inline uint64_t PageProvider::GetRequestLen(const PageRequest* request) {
  DEBUG_ASSERT(request->provider_owned_);
  return request->len_;
}
inline uint64_t PageProvider::GetRequestVmoId(const PageRequest* request) {
  DEBUG_ASSERT(request->provider_owned_);
  return request->vmo_debug_info_.vmo_id;
}

// Wrapper around PageRequest that performs construction on first access. This is useful when a
// PageRequest needs to be allocated eagerly in case it is used, even if the common case is that it
// will not be needed.
class LazyPageRequest {
 public:
  // Construct a page request that does not support early waking.
  LazyPageRequest() : LazyPageRequest(false) {}
  // Construct a page request that optionally supports early waking. See PageRequest constructor.
  explicit LazyPageRequest(bool early_wake) : early_wake_(early_wake) {}
  ~LazyPageRequest() = default;

  // Initialize and return the internal PageRequest.
  PageRequest* get();

  PageRequest* operator->() { return get(); }

  PageRequest& operator*() { return *get(); }

  bool is_initialized() const { return request_.has_value(); }

 private:
  // Early wake parameter to be passed onto the PageRequest constructor.
  bool early_wake_;
  ktl::optional<PageRequest> request_ = ktl::nullopt;
};

// Wrapper around tracking multiple different page requests that might need waiting. Only one
// individual request is allowed to considered 'active' at a time as the one that next needs waiting
// on. Tracking whether a request is active is, depending on the request type, partially automatic
// and partially requiring additional input from the user.
// The PageRequest and LazyPageRequest access methods do not currently have a way to enforce that
// those specific types of requests are made with the returned objects, however this could change
// and callers are expected to use the correct method.
// TODO(adanis): Implement an enforcement strategy.
class MultiPageRequest {
 public:
  MultiPageRequest() = default;
  explicit MultiPageRequest(bool early_wake) : page_request_(early_wake) {}

  // Wait on the currently active page request. The waiting thread is suspendable by default.
  zx_status_t Wait(bool suspendable = true);

  // Retrieve the anonymous page request. The caller may or may not arm the AnonymousPageRequest, if
  // it does the anonymous request becomes considered active and no other request may be retrieved.
  AnonymousPageRequest* GetAnonymous() {
    DEBUG_ASSERT(NoRequestActive());
    return &anonymous_;
  }

  // Retrieve and commit to initializing the page request for read. After calling this it is assumed
  // that the page request will be made waitable and no other request may be retrieved.
  PageRequest* GetReadRequest() {
    DEBUG_ASSERT(NoRequestActive());
    read_active_ = true;
    return page_request_.get();
  }

  // Retrieve a lazy accessor to page request. If a dirty request is generated the caller must
  // then call MadeDirtyRequest so that this helper knows that the page request is active and
  // should be waited on.
  LazyPageRequest* GetLazyDirtyRequest() {
    DEBUG_ASSERT(NoRequestActive());
    return &page_request_;
  }

  // Indicate that the page request retrieved by GetLazyDirtyRequest was used and should be waited
  // on.
  void MadeDirtyRequest() {
    DEBUG_ASSERT(NoRequestActive());
    dirty_active_ = true;
  }

  // Cancel all requests and have no active request.
  void CancelRequests();

 private:
  bool NoRequestActive() const {
    return !anonymous_.is_active() && !read_active_ && !dirty_active_;
  }
  // Track which request is active. This is multiple bools for consistency since the anonymous
  // request being active is tracked directly in the AnonymousPageRequest and could not be part of
  // an enum.
  bool read_active_ = false;
  bool dirty_active_ = false;
  AnonymousPageRequest anonymous_;
  LazyPageRequest page_request_;
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_H_
