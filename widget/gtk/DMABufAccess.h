/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef mozilla_widget_DMABufAccess_h
#define mozilla_widget_DMABufAccess_h

#include "mozilla/UniquePtr.h"
#include "mozilla/gfx/FileHandleWrapper.h"

namespace mozilla::widget {

// Each mapping is used by one serialized caller; distinct mappings synchronize
// peers.
class DMABufAccess final {
 public:
  enum class LockResult { Acquired, Busy, Stale, Unusable };
  static UniquePtr<DMABufAccess> Create();
  static UniquePtr<DMABufAccess> Import(gfx::FileHandleWrapper*);
  ~DMABufAccess();
  DMABufAccess(const DMABufAccess&) = delete;
  DMABufAccess& operator=(const DMABufAccess&) = delete;

  gfx::FileHandleWrapper* Handle() const { return mHandle; }
  bool IsUsable() const;
  LockResult TryLock(uint64_t aGeneration);
  LockResult WaitLock(uint64_t aGeneration, uint32_t aTimeoutMs);
  bool TryBeginWrite();
  bool Publish(uint64_t& aGeneration);
  void CancelWrite();
  bool TryRetire();
  void Unlock(bool aAbandon = false);

 private:
  struct SharedState;
  DMABufAccess(gfx::FileHandleWrapper*, SharedState*);
  RefPtr<gfx::FileHandleWrapper> mHandle;
  SharedState* const mShared;
  uint32_t mHeld = 0;
};

}  // namespace mozilla::widget
#endif
