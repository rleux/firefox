/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <fcntl.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <limits>

#include "base/linux_memfd_defs.h"
#include "gtest/gtest.h"
#include "mozilla/widget/DMABufAccess.h"

namespace mozilla::widget {
namespace {
using LockResult = DMABufAccess::LockResult;

RefPtr<gfx::FileHandleWrapper> AccessFile(size_t size, int seals) {
  UniqueFileHandle fd(syscall(SYS_memfd_create, "dmabuf-access-test",
                              MFD_CLOEXEC | MFD_ALLOW_SEALING));
  if (!fd || ftruncate(fd.get(), size) ||
      (seals && fcntl(fd.get(), F_ADD_SEALS, seals))) {
    return nullptr;
  }
  return new gfx::FileHandleWrapper(std::move(fd));
}
}  // namespace

TEST(DMABufAccess, WriterAndReaderExcludeEachOther)
{
  auto producer = DMABufAccess::Create();
  ASSERT_TRUE(producer);
  auto reader = DMABufAccess::Import(producer->Handle());
  auto other = DMABufAccess::Import(producer->Handle());
  ASSERT_TRUE(reader);
  ASSERT_TRUE(other);
  uint64_t generation = 0;
  ASSERT_TRUE(producer->TryBeginWrite());
  EXPECT_TRUE(producer->IsUsable());
  EXPECT_TRUE(reader->IsUsable());
  EXPECT_EQ(reader->TryLock(1), LockResult::Stale);
  EXPECT_EQ(reader->WaitLock(1, 0), LockResult::Stale);
  ASSERT_TRUE(producer->Publish(generation));
  ASSERT_EQ(generation, 1U);
  ASSERT_EQ(reader->TryLock(generation), LockResult::Acquired);
  EXPECT_TRUE(reader->IsUsable());
  EXPECT_FALSE(producer->TryBeginWrite());
  EXPECT_EQ(other->TryLock(generation), LockResult::Busy);
  EXPECT_EQ(other->WaitLock(generation, 0), LockResult::Busy);
  reader->Unlock();
  ASSERT_EQ(other->WaitLock(generation, 1), LockResult::Acquired);
  EXPECT_FALSE(producer->TryBeginWrite());
  other->Unlock();
  ASSERT_TRUE(producer->TryBeginWrite());
  producer->CancelWrite();
}

TEST(DMABufAccess, DelayedGenerationCannotAcquireOrPoisonNewPublication)
{
  auto producer = DMABufAccess::Create();
  ASSERT_TRUE(producer);
  auto delayed = DMABufAccess::Import(producer->Handle());
  auto current = DMABufAccess::Import(producer->Handle());
  ASSERT_TRUE(delayed);
  ASSERT_TRUE(current);
  uint64_t first = 0, second = 0;
  ASSERT_TRUE(producer->TryBeginWrite());
  ASSERT_TRUE(producer->Publish(first));
  ASSERT_EQ(delayed->TryLock(first), LockResult::Acquired);
  delayed->Unlock();
  ASSERT_TRUE(producer->TryBeginWrite());
  EXPECT_EQ(delayed->TryLock(first), LockResult::Stale);
  delayed->Unlock(true);
  EXPECT_TRUE(producer->IsUsable());
  ASSERT_TRUE(producer->Publish(second));
  ASSERT_EQ(second, first + 1);
  EXPECT_EQ(delayed->TryLock(first), LockResult::Stale);
  EXPECT_EQ(delayed->WaitLock(first, 1), LockResult::Stale);
  ASSERT_EQ(current->TryLock(second), LockResult::Acquired);
  delayed->Unlock(true);
  EXPECT_TRUE(current->IsUsable());
  EXPECT_FALSE(producer->TryBeginWrite());
  current->Unlock();
  ASSERT_TRUE(producer->TryBeginWrite());
  producer->CancelWrite();
}

TEST(DMABufAccess, CancelledWriterPreservesPublishedGeneration)
{
  auto producer = DMABufAccess::Create();
  ASSERT_TRUE(producer);
  auto reader = DMABufAccess::Import(producer->Handle());
  ASSERT_TRUE(reader);
  uint64_t generation = 0;
  ASSERT_TRUE(producer->TryBeginWrite());
  ASSERT_TRUE(producer->Publish(generation));
  ASSERT_EQ(generation, 1U);
  ASSERT_TRUE(producer->TryBeginWrite());
  EXPECT_EQ(reader->TryLock(generation), LockResult::Stale);
  producer->CancelWrite();
  ASSERT_EQ(reader->TryLock(generation), LockResult::Acquired);
  reader->Unlock();
  ASSERT_TRUE(producer->TryBeginWrite());
  ASSERT_TRUE(producer->Publish(generation));
  EXPECT_EQ(generation, 2U);
}

TEST(DMABufAccess, OwnedReadOrFailedWriteAbandonsWithoutReopening)
{
  for (bool abandonWriter : {false, true}) {
    auto producer = DMABufAccess::Create();
    ASSERT_TRUE(producer);
    auto reader = DMABufAccess::Import(producer->Handle());
    ASSERT_TRUE(reader);
    uint64_t generation = 0;
    ASSERT_TRUE(producer->TryBeginWrite());
    ASSERT_TRUE(producer->Publish(generation));
    if (abandonWriter) {
      ASSERT_TRUE(producer->TryBeginWrite());
      producer->Unlock(true);
    } else {
      ASSERT_EQ(reader->TryLock(generation), LockResult::Acquired);
      reader->Unlock(true);
    }
    EXPECT_FALSE(producer->IsUsable());
    EXPECT_FALSE(reader->IsUsable());
    EXPECT_FALSE(producer->TryBeginWrite());
    EXPECT_EQ(reader->TryLock(generation), LockResult::Unusable);
    EXPECT_EQ(reader->WaitLock(generation, 1), LockResult::Unusable);
    EXPECT_FALSE(producer->Publish(generation));
    producer->CancelWrite();
    reader->Unlock();
    EXPECT_FALSE(producer->IsUsable());
  }
}

TEST(DMABufAccess, RetiredAllocationCannotBeReadOrWritten)
{
  auto producer = DMABufAccess::Create();
  ASSERT_TRUE(producer);
  auto reader = DMABufAccess::Import(producer->Handle());
  ASSERT_TRUE(reader);
  uint64_t generation = 0;
  ASSERT_TRUE(producer->TryBeginWrite());
  EXPECT_FALSE(reader->TryRetire());
  ASSERT_TRUE(producer->Publish(generation));
  ASSERT_EQ(reader->TryLock(generation), LockResult::Acquired);
  EXPECT_FALSE(producer->TryRetire());
  reader->Unlock();
  ASSERT_TRUE(producer->TryRetire());
  EXPECT_FALSE(producer->IsUsable());
  EXPECT_FALSE(producer->TryBeginWrite());
  EXPECT_EQ(reader->TryLock(generation), LockResult::Unusable);
  reader->Unlock(true);
  EXPECT_FALSE(producer->IsUsable());
}

TEST(DMABufAccess, GenerationOverflowAbandonsFailedPublication)
{
  auto producer = DMABufAccess::Create();
  ASSERT_TRUE(producer);
  ASSERT_TRUE(producer->TryBeginWrite());
  const uint64_t maximum = std::numeric_limits<uint64_t>::max();
  // Exercise the generation limit in the shared 16-byte protocol while owned.
  ASSERT_EQ(
      pwrite(producer->Handle()->GetHandle(), &maximum, sizeof(maximum), 8),
      ssize_t(sizeof(maximum)));
  uint64_t generation = 0;
  EXPECT_FALSE(producer->Publish(generation));
  EXPECT_FALSE(producer->IsUsable());
  producer->CancelWrite();
  EXPECT_FALSE(producer->TryBeginWrite());
  EXPECT_EQ(producer->TryLock(maximum), LockResult::Unusable);
}

TEST(DMABufAccess, ImportRejectsWrongSizeOrMissingResizeSeals)
{
  EXPECT_FALSE(DMABufAccess::Import(nullptr));
  for (size_t size : {0, 4, 8, 15, 17, 32}) {
    auto file = AccessFile(size, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL);
    ASSERT_TRUE(file);
    EXPECT_FALSE(DMABufAccess::Import(file));
  }
  for (int seals : {0, F_SEAL_SHRINK, F_SEAL_GROW}) {
    auto file = AccessFile(16, seals);
    ASSERT_TRUE(file);
    EXPECT_FALSE(DMABufAccess::Import(file));
  }
  auto valid = DMABufAccess::Create();
  ASSERT_TRUE(valid);
  EXPECT_TRUE(DMABufAccess::Import(valid->Handle()));
}
}  // namespace mozilla::widget
