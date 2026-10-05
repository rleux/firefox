/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "DMABufAccess.h"

#include <cerrno>
#include <chrono>
#include <climits>
#include <fcntl.h>
#include <linux/futex.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include "base/linux_memfd_defs.h"

namespace mozilla::widget {

enum class AccessState : uint32_t { Idle, Locked, Abandoned, Retired };
static_assert(__atomic_always_lock_free(sizeof(uint32_t), nullptr));

UniquePtr<DMABufAccess> DMABufAccess::Create() {
  UniqueFileHandle fd(syscall(SYS_memfd_create, "wr-dmabuf-access",
                              MFD_CLOEXEC | MFD_ALLOW_SEALING));
  if (!fd || ftruncate(fd.get(), sizeof(uint32_t)) ||
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
  // Seals make the following size check stable.
  struct stat info;
  if (fstat(aHandle->GetHandle(), &info) || info.st_size != sizeof(uint32_t)) {
    return nullptr;
  }
  auto* mapping = mmap(nullptr, sizeof(uint32_t), PROT_READ | PROT_WRITE,
                       MAP_SHARED, aHandle->GetHandle(), 0);
  if (mapping == MAP_FAILED) {
    return nullptr;
  }
  return UniquePtr<DMABufAccess>(
      new DMABufAccess(aHandle, static_cast<uint32_t*>(mapping)));
}

DMABufAccess::DMABufAccess(gfx::FileHandleWrapper* aHandle, uint32_t* aState)
    : mHandle(aHandle), mState(aState) {}

DMABufAccess::~DMABufAccess() { munmap(mState, sizeof(uint32_t)); }

bool DMABufAccess::IsUsable() const {
  return __atomic_load_n(mState, __ATOMIC_ACQUIRE) <=
         uint32_t(AccessState::Locked);
}

bool DMABufAccess::TryLock() {
  uint32_t expected = uint32_t(AccessState::Idle);
  return __atomic_compare_exchange_n(mState, &expected,
                                     uint32_t(AccessState::Locked), false,
                                     __ATOMIC_ACQUIRE, __ATOMIC_RELAXED);
}

bool DMABufAccess::WaitLock(uint32_t aTimeoutMs) {
  const auto deadline =
      std::chrono::steady_clock::now() + std::chrono::milliseconds(aTimeoutMs);
  for (;;) {
    uint32_t expected = uint32_t(AccessState::Idle);
    if (__atomic_compare_exchange_n(mState, &expected,
                                    uint32_t(AccessState::Locked), false,
                                    __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
      return true;
    }
    if (expected != uint32_t(AccessState::Locked)) {
      return false;
    }
    auto remaining = std::chrono::duration_cast<std::chrono::nanoseconds>(
                         deadline - std::chrono::steady_clock::now())
                         .count();
    if (remaining <= 0) {
      return false;
    }
    const timespec timeout{static_cast<time_t>(remaining / 1000000000),
                           static_cast<long>(remaining % 1000000000)};
    if (syscall(SYS_futex, mState, FUTEX_WAIT, uint32_t(AccessState::Locked),
                &timeout, nullptr, 0) < 0 &&
        errno != EAGAIN && errno != EINTR && errno != ETIMEDOUT) {
      return false;
    }
  }
}

bool DMABufAccess::TryRetire() {
  uint32_t expected = uint32_t(AccessState::Idle);
  if (!__atomic_compare_exchange_n(mState, &expected,
                                   uint32_t(AccessState::Retired), false,
                                   __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) {
    return false;
  }
  syscall(SYS_futex, mState, FUTEX_WAKE, INT_MAX, nullptr, nullptr, 0);
  return true;
}

void DMABufAccess::Unlock(bool aAbandon) {
  if (aAbandon) {
    __atomic_store_n(mState, uint32_t(AccessState::Abandoned),
                     __ATOMIC_RELEASE);
  } else {
    uint32_t expected = uint32_t(AccessState::Locked);
    __atomic_compare_exchange_n(mState, &expected, uint32_t(AccessState::Idle),
                                false, __ATOMIC_RELEASE, __ATOMIC_RELAXED);
  }
  syscall(SYS_futex, mState, FUTEX_WAKE, INT_MAX, nullptr, nullptr, 0);
}

}  // namespace mozilla::widget
