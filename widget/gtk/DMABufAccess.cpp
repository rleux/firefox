/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "DMABufAccess.h"

#include <fcntl.h>
#include <linux/futex.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <climits>

#include "base/linux_memfd_defs.h"

namespace mozilla::widget {

enum AccessState : uint32_t { Idle, Reading, Writing, Abandoned, Retired };
static_assert(__atomic_always_lock_free(sizeof(uint32_t), nullptr));
struct DMABufAccess::SharedState {
  uint32_t state;
  uint32_t pad;
  // Protected by state, including on 32-bit systems.
  uint64_t generation;
};

UniquePtr<DMABufAccess> DMABufAccess::Create() {
  UniqueFileHandle fd(syscall(SYS_memfd_create, "wr-dmabuf-access",
                              MFD_CLOEXEC | MFD_ALLOW_SEALING));
  if (!fd || ftruncate(fd.get(), sizeof(SharedState)) ||
      fcntl(fd.get(), F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL)) {
    return nullptr;
  }
  RefPtr<gfx::FileHandleWrapper> handle =
      new gfx::FileHandleWrapper(std::move(fd));
  return Import(handle);
}

UniquePtr<DMABufAccess> DMABufAccess::Import(gfx::FileHandleWrapper* aHandle) {
  if (!aHandle || !FileHandleIsValid(aHandle->GetHandle())) {
    return nullptr;
  }
  const int seals = fcntl(aHandle->GetHandle(), F_GET_SEALS);
  if (seals < 0 || (seals & (F_SEAL_SHRINK | F_SEAL_GROW)) !=
                       (F_SEAL_SHRINK | F_SEAL_GROW)) {
    return nullptr;
  }
  struct stat info;
  if (fstat(aHandle->GetHandle(), &info) ||
      info.st_size != sizeof(SharedState)) {
    return nullptr;
  }
  auto* mapping = mmap(nullptr, sizeof(SharedState), PROT_READ | PROT_WRITE,
                       MAP_SHARED, aHandle->GetHandle(), 0);
  if (mapping == MAP_FAILED) {
    return nullptr;
  }
  return UniquePtr<DMABufAccess>(
      new DMABufAccess(aHandle, static_cast<SharedState*>(mapping)));
}

DMABufAccess::DMABufAccess(gfx::FileHandleWrapper* aHandle,
                           SharedState* aShared)
    : mHandle(aHandle), mShared(aShared) {}
DMABufAccess::~DMABufAccess() {
  Unlock(true);
  munmap(mShared, sizeof(SharedState));
}

bool DMABufAccess::IsUsable() const {
  return __atomic_load_n(&mShared->state, __ATOMIC_ACQUIRE) <= Writing;
}

DMABufAccess::LockResult DMABufAccess::TryLock(uint64_t aGeneration) {
  if (mHeld != Idle) {
    return LockResult::Unusable;
  }
  uint32_t expected = Idle;
  if (!__atomic_compare_exchange_n(&mShared->state, &expected, Reading, false,
                                   __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
    if (expected == Writing) {
      return LockResult::Stale;
    }
    return expected == Reading ? LockResult::Busy : LockResult::Unusable;
  }
  mHeld = Reading;
  if (!aGeneration || mShared->generation != aGeneration) {
    Unlock();
    return LockResult::Stale;
  }
  return LockResult::Acquired;
}

DMABufAccess::LockResult DMABufAccess::WaitLock(uint64_t aGeneration,
                                                uint32_t aTimeoutMs) {
  const auto deadline =
      std::chrono::steady_clock::now() + std::chrono::milliseconds(aTimeoutMs);
  for (;;) {
    const auto result = TryLock(aGeneration);
    if (result != LockResult::Busy) {
      return result;
    }
    const auto remaining = std::chrono::duration_cast<std::chrono::nanoseconds>(
                               deadline - std::chrono::steady_clock::now())
                               .count();
    if (remaining <= 0) {
      return LockResult::Busy;
    }
    const timespec timeout{static_cast<time_t>(remaining / 1000000000),
                           static_cast<long>(remaining % 1000000000)};
    if (syscall(SYS_futex, &mShared->state, FUTEX_WAIT, Reading, &timeout,
                nullptr, 0) < 0 &&
        errno != EAGAIN && errno != EINTR && errno != ETIMEDOUT) {
      return LockResult::Unusable;
    }
  }
}

bool DMABufAccess::TryBeginWrite() {
  if (mHeld != Idle) {
    return false;
  }
  uint32_t expected = Idle;
  if (!__atomic_compare_exchange_n(&mShared->state, &expected, Writing, false,
                                   __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
    return false;
  }
  mHeld = Writing;
  if (mShared->generation == UINT64_MAX) {
    Unlock(true);
    return false;
  }
  return true;
}

bool DMABufAccess::Publish(uint64_t& aGeneration) {
  if (mHeld != Writing ||
      __atomic_load_n(&mShared->state, __ATOMIC_ACQUIRE) != Writing) {
    return false;
  }
  if (mShared->generation == UINT64_MAX) {
    Unlock(true);
    return false;
  }
  aGeneration = ++mShared->generation;
  Unlock();
  return true;
}

void DMABufAccess::CancelWrite() {
  if (mHeld == Writing) {
    Unlock();
  }
}

bool DMABufAccess::TryRetire() {
  if (mHeld != Idle) {
    return false;
  }
  uint32_t expected = Idle;
  const bool retired =
      __atomic_compare_exchange_n(&mShared->state, &expected, Retired, false,
                                  __ATOMIC_ACQ_REL, __ATOMIC_RELAXED);
  if (retired) {
    syscall(SYS_futex, &mShared->state, FUTEX_WAKE, INT_MAX, nullptr, nullptr,
            0);
  }
  return retired;
}

void DMABufAccess::Unlock(bool aAbandon) {
  if (mHeld != Reading && mHeld != Writing) {
    return;
  }
  uint32_t expected = mHeld;
  __atomic_compare_exchange_n(&mShared->state, &expected,
                              aAbandon ? Abandoned : Idle, false,
                              __ATOMIC_RELEASE, __ATOMIC_RELAXED);
  mHeld = Idle;
  syscall(SYS_futex, &mShared->state, FUTEX_WAKE, INT_MAX, nullptr, nullptr, 0);
}

}  // namespace mozilla::widget
