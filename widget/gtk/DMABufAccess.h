/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef mozilla_widget_DMABufAccess_h
#define mozilla_widget_DMABufAccess_h

#include "mozilla/UniquePtr.h"
#include "mozilla/gfx/FileHandleWrapper.h"

namespace mozilla::widget {

class DMABufAccess final {
 public:
  static UniquePtr<DMABufAccess> Create();
  static UniquePtr<DMABufAccess> Import(gfx::FileHandleWrapper*);
  ~DMABufAccess();
  DMABufAccess(const DMABufAccess&) = delete;
  DMABufAccess& operator=(const DMABufAccess&) = delete;

  gfx::FileHandleWrapper* Handle() const { return mHandle; }
  bool IsUsable() const;
  bool TryLock();
  bool WaitLock(uint32_t aTimeoutMs);
  bool TryRetire();
  void Unlock(bool aAbandon = false);

 private:
  DMABufAccess(gfx::FileHandleWrapper*, uint32_t*);
  RefPtr<gfx::FileHandleWrapper> mHandle;
  // Shared mapping owned until destruction; the sealed FD prevents resizing.
  uint32_t* const mState;
};

}  // namespace mozilla::widget

#endif
