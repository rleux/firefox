/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <fcntl.h>
#include <gbm.h>
#include <unistd.h>
#ifdef XP_LINUX
#  include <sys/eventfd.h>
#  include <sys/syscall.h>

#  include <chrono>
#  include <future>

#  include "base/linux_memfd_defs.h"
#  include "mozilla/webgpu/SharedTextureDMABuf.h"
#endif

#include "gtest/gtest.h"
#include "mozilla/NotNull.h"
#include "mozilla/gfx/FileHandleWrapper.h"
#include "mozilla/ipc/FileDescriptor.h"
#include "mozilla/layers/LayersSurfaces.h"
#include "mozilla/widget/DMABufSurface.h"
#include "nsTArray.h"

using namespace mozilla;
using namespace mozilla::gfx;
using namespace mozilla::layers;

// These tests verify that DMABufSurface descriptors and surfaces round-trip
// stably through serialization and import.

static RefPtr<FileHandleWrapper> MakeFd() {
  int fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
  if (fd == -1) return nullptr;
  return new FileHandleWrapper(UniqueFileHandle(fd));
}

// Matches what DMABufSurfaceRGBA::Serialize() produces for a single-plane
// 128×128 RGBA surface.
static SurfaceDescriptor MakeRGBADescriptor(RefPtr<FileHandleWrapper> fd) {
  AutoTArray<NotNull<RefPtr<FileHandleWrapper>>, 4> fds;
  fds.AppendElement(WrapNotNull(fd));
  AutoTArray<uint64_t, 1> modifiers = {0};
  AutoTArray<uint32_t, 4> width = {128}, height = {128}, strides = {512},
                          offsets = {0};
  AutoTArray<uint32_t, 1>
      format;  // intentionally empty; Serialize() leaves this unset for RGBA
  AutoTArray<NotNull<RefPtr<FileHandleWrapper>>, 1> fence;
  AutoTArray<ipc::FileDescriptor, 1> refCount;
  return SurfaceDescriptor(SurfaceDescriptorDMABuf(
      DMABufSurface::SURFACE_RGBA, GBM_FORMAT_ARGB8888, modifiers, 0, fds,
      width, height, width, height, format, strides, offsets,
      gfx::YUVColorSpace::BT601, gfx::ColorRange::LIMITED,
      gfx::ColorSpace2::UNKNOWN, gfx::TransferFunction::Default, 0, fence, 1, 0,
      refCount, nullptr, false, gfx::HDRMetadata(), Nothing(), Nothing()));
}

// Matches what DMABufSurfaceYUV::Serialize() produces for a two-plane 128×128
// (NV12-style, 4:2:0) YUV surface.
static SurfaceDescriptor MakeYUVDescriptor(RefPtr<FileHandleWrapper> fd0,
                                           RefPtr<FileHandleWrapper> fd1) {
  AutoTArray<NotNull<RefPtr<FileHandleWrapper>>, 4> fds;
  fds.AppendElement(WrapNotNull(fd0));
  fds.AppendElement(WrapNotNull(fd1));
  AutoTArray<uint64_t, 4> modifiers = {0, 0};
  // Y plane full size, UV plane half size (4:2:0 chroma).
  AutoTArray<uint32_t, 4> width = {128, 64}, height = {128, 64},
                          widthAligned = {128, 64}, heightAligned = {128, 64},
                          format = {0, 0}, strides = {128, 128},
                          offsets = {0, 0};
  AutoTArray<NotNull<RefPtr<FileHandleWrapper>>, 1> fence;
  AutoTArray<ipc::FileDescriptor, 1> refCount;
  return SurfaceDescriptor(SurfaceDescriptorDMABuf(
      DMABufSurface::SURFACE_YUV, VA_FOURCC_NV12, modifiers, 0, fds, width,
      height, widthAligned, heightAligned, format, strides, offsets,
      gfx::YUVColorSpace::BT601, gfx::ColorRange::LIMITED,
      gfx::ColorSpace2::UNKNOWN, gfx::TransferFunction::Default, 0, fence, 1, 0,
      refCount, nullptr, false, gfx::HDRMetadata(), Nothing(), Nothing()));
}

// Run 3 serialize → import cycles for a single-plane RGBA surface.
static void RGBARoundtrip(RefPtr<DMABufSurface> surface) {
  SurfaceDescriptor desc;
  for (int i = 0; i < 3; i++) {
    ASSERT_TRUE(surface->Serialize(desc));
    const auto& d = desc.get_SurfaceDescriptorDMABuf();
    EXPECT_EQ(d.bufferType(), uint32_t{DMABufSurface::SURFACE_RGBA});
    EXPECT_EQ(d.fds().Length(), 1u);
    EXPECT_EQ(d.width()[0], 128u);
    EXPECT_EQ(d.height()[0], 128u);
    EXPECT_EQ(d.strides()[0], 512u);
    EXPECT_EQ(d.modifier()[0], uint64_t{0});
    surface = DMABufSurface::CreateDMABufSurface(desc);
    ASSERT_NE(surface, nullptr);
    EXPECT_EQ(surface->GetWidth(), 128);
    EXPECT_EQ(surface->GetHeight(), 128);
  }
}

// Run 3 serialize → import cycles for a two-plane YUV surface.
static void YUVRoundtrip(RefPtr<DMABufSurface> surface) {
  SurfaceDescriptor desc;
  for (int i = 0; i < 3; i++) {
    ASSERT_TRUE(surface->Serialize(desc));
    const auto& d = desc.get_SurfaceDescriptorDMABuf();
    EXPECT_EQ(d.bufferType(), uint32_t{DMABufSurface::SURFACE_YUV});
    EXPECT_EQ(d.fds().Length(), 2u);
    EXPECT_EQ(d.width()[0], 128u);
    EXPECT_EQ(d.height()[0], 128u);
    EXPECT_EQ(d.width()[1], 64u);
    EXPECT_EQ(d.height()[1], 64u);
    EXPECT_EQ(d.format().Length(), 2u);
    EXPECT_EQ(d.modifier().Length(), 2u);
    surface = DMABufSurface::CreateDMABufSurface(desc);
    ASSERT_NE(surface, nullptr);
    EXPECT_EQ(surface->GetWidth(0), 128);
    EXPECT_EQ(surface->GetHeight(0), 128);
    EXPECT_EQ(surface->GetWidth(1), 64);
    EXPECT_EQ(surface->GetHeight(1), 64);
  }
}

// Test both starting points for RGBA: from a descriptor (compositor side,
// verify initial import) and from a surface (GPU process side, no descriptor
// needed thanks to WGPUDMABufInfo).
TEST(DMABufSurface, RGBARoundtrip)
{
  {
    RefPtr<FileHandleWrapper> fd = MakeFd();
    ASSERT_NE(fd, nullptr);
    RefPtr<DMABufSurface> surface =
        DMABufSurface::CreateDMABufSurface(MakeRGBADescriptor(fd));
    ASSERT_NE(surface, nullptr);
    EXPECT_EQ(surface->GetWidth(), 128);
    EXPECT_EQ(surface->GetHeight(), 128);
    RGBARoundtrip(surface);
  }
  {
    RefPtr<FileHandleWrapper> fd = MakeFd();
    ASSERT_NE(fd, nullptr);
    webgpu::ffi::WGPUDMABufInfo info{};
    info.is_valid = true;
    info.is_rgba = true;
    info.plane_count = 1;
    info.strides[0] = 512;
    RefPtr<DMABufSurface> surface =
        DMABufSurfaceRGBA::CreateDMABufSurface(std::move(fd), info, 128, 128);
    ASSERT_NE(surface, nullptr);
    EXPECT_EQ(surface->GetWidth(), 128);
    EXPECT_EQ(surface->GetHeight(), 128);
    RGBARoundtrip(surface);
  }
}

// YUV surfaces require VA-API hardware to create from scratch, so the surface
// is always bootstrapped via descriptor import.  Verify that import then run
// 3 serialize/import cycles from the surface side.
TEST(DMABufSurface, YUVRoundtrip)
{
  RefPtr<FileHandleWrapper> fd0 = MakeFd(), fd1 = MakeFd();
  ASSERT_NE(fd0, nullptr);
  ASSERT_NE(fd1, nullptr);
  RefPtr<DMABufSurface> surface =
      DMABufSurface::CreateDMABufSurface(MakeYUVDescriptor(fd0, fd1));
  ASSERT_NE(surface, nullptr);
  EXPECT_EQ(surface->GetWidth(0), 128);
  EXPECT_EQ(surface->GetHeight(0), 128);
  EXPECT_EQ(surface->GetWidth(1), 64);
  EXPECT_EQ(surface->GetHeight(1), 64);
  YUVRoundtrip(surface);
}

#ifdef XP_LINUX
static void AddVulkanState(SurfaceDescriptor& aDescriptor,
                           RefPtr<FileHandleWrapper> aAccessLock) {
  AutoTArray<uint8_t, 16> identity = {0, 0, 0, 0, 0, 0, 0, 0,
                                      0, 0, 0, 0, 0, 0, 0, 0};
  auto& image = aDescriptor.get_SurfaceDescriptorDMABuf();
  image.semaphoreFdIsSyncFd() = true;
  image.vulkanImageState() =
      Some(VulkanImageState(identity, identity, 1, WrapNotNull(aAccessLock)));
}

TEST(DMABufSurface, VulkanRejectsInvalidAccessLock)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  auto descriptor = MakeRGBADescriptor(fd);
  AddVulkanState(descriptor, fd);
  RefPtr<DMABufSurface> rejected =
      DMABufSurface::CreateDMABufSurface(descriptor);
  EXPECT_FALSE(rejected);
}

TEST(DMABufSurface, VulkanSnapshotSerializesCompositorAccess)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  auto descriptor = MakeRGBADescriptor(fd);
  RefPtr<DMABufSurface> snapshot =
      DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(snapshot);
  ASSERT_TRUE(snapshot->CreateAccessLock());
  AddVulkanState(descriptor, snapshot->GetAccessLockFd());
  RefPtr<DMABufSurface> imported =
      DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(imported);
  SurfaceDescriptor forwarded;
  ASSERT_TRUE(imported->Serialize(forwarded));
  RefPtr<DMABufSurface> compositor =
      DMABufSurface::CreateDMABufSurface(forwarded);
  ASSERT_TRUE(compositor);

  ASSERT_TRUE(snapshot->LockAccess());
  std::promise<void> started;
  auto acquired = std::async(std::launch::async, [&] {
    started.set_value();
    const bool locked = compositor->LockAccess();
    if (locked) {
      compositor->UnlockAccess();
    }
    return locked;
  });
  started.get_future().wait();
  EXPECT_EQ(acquired.wait_for(std::chrono::milliseconds(100)),
            std::future_status::timeout);
  snapshot->UnlockAccess();
  EXPECT_TRUE(acquired.get());
  ASSERT_TRUE(snapshot->LockAccess());
  snapshot->UnlockAccess();
}

TEST(DMABufSurface, VulkanAbandonmentPreventsProducerRecycling)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  auto descriptor = MakeRGBADescriptor(fd);
  RefPtr<DMABufSurface> producer =
      DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(producer);
  ASSERT_TRUE(producer->CreateAccessLock());
  AddVulkanState(descriptor, producer->GetAccessLockFd());
  RefPtr<DMABufSurface> compositor =
      DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(compositor);

  webgpu::ffi::WGPUTextureFormat format{};
  format.tag = webgpu::ffi::WGPUTextureFormat_Bgra8Unorm;
  webgpu::ffi::WGPUDMABufInfo info{};
  info.is_valid = true;
  info.for_webrender = true;
  webgpu::SharedTextureDMABuf texture(
      128, 128, format, {}, RefPtr<DMABufSurface>(producer),
      descriptor.get_SurfaceDescriptorDMABuf(), info);
  EXPECT_TRUE(texture.GetDMABufInfo().is_valid);

  ASSERT_TRUE(compositor->LockAccess());
  compositor->UnlockAccess();
  EXPECT_TRUE(producer->AccessLockUsable());
  ASSERT_TRUE(compositor->LockAccess());
  compositor->UnlockAccess(true);
  EXPECT_FALSE(producer->AccessLockUsable());
  EXPECT_FALSE(producer->LockAccess());
  EXPECT_FALSE(compositor->LockAccess());
  EXPECT_FALSE(texture.GetDMABufInfo().is_valid);
  EXPECT_FALSE(texture.CloneDmaBufFd());
  texture.CleanForRecycling();
  EXPECT_FALSE(texture.GetDMABufInfo().is_valid);
  EXPECT_FALSE(producer->CreateAccessLock());
  producer->UnlockAccess();
  EXPECT_FALSE(producer->AccessLockUsable());
}

TEST(DMABufSurface, ForeignRGBRejectsMissingAccessLock)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  auto descriptor = MakeRGBADescriptor(fd);
  auto& image = descriptor.get_SurfaceDescriptorDMABuf();
  image.fence().AppendElement(WrapNotNull(fd));
  image.refCount().AppendElement(ipc::FileDescriptor(fd->GetHandle()));
  image.foreignRGBImageState() = Some(ForeignRGBImageState(1, WrapNotNull(fd)));
  RefPtr<DMABufSurface> rejected =
      DMABufSurface::CreateDMABufSurface(descriptor);
  EXPECT_FALSE(rejected);
}
#endif

#ifdef XP_LINUX
TEST(DMABufSurface, ForeignRGBAccessLockIsSharedAndAbandonmentPersists)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  int rawLock = syscall(SYS_memfd_create, "foreign-rgb-test",
                        MFD_CLOEXEC | MFD_ALLOW_SEALING);
  ASSERT_GE(rawLock, 0);
  RefPtr<FileHandleWrapper> lock =
      new FileHandleWrapper(UniqueFileHandle(rawLock));
  ASSERT_EQ(ftruncate(rawLock, sizeof(uint32_t)), 0);
  ASSERT_EQ(
      fcntl(rawLock, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL),
      0);
  int rawRef = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK | EFD_SEMAPHORE);
  ASSERT_GE(rawRef, 0);
  RefPtr<FileHandleWrapper> refs =
      new FileHandleWrapper(UniqueFileHandle(rawRef));
  auto descriptor = MakeRGBADescriptor(fd);
  auto& image = descriptor.get_SurfaceDescriptorDMABuf();
  image.fence().AppendElement(WrapNotNull(fd));
  image.refCount().AppendElement(ipc::FileDescriptor(rawRef));
  image.foreignRGBImageState() =
      Some(ForeignRGBImageState(1, WrapNotNull(lock)));
  RefPtr<DMABufSurface> first = DMABufSurface::CreateDMABufSurface(descriptor);
  RefPtr<DMABufSurface> second = DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(first);
  ASSERT_TRUE(second);
  ASSERT_TRUE(first->LockForeignRGB());
  first->UnlockForeignRGB();
  ASSERT_TRUE(second->LockForeignRGB());
  second->UnlockForeignRGB(true);
  EXPECT_FALSE(first->ForeignRGBUsable());
  EXPECT_FALSE(first->LockForeignRGB());
  first->UnlockForeignRGB();
  EXPECT_FALSE(second->ForeignRGBUsable());
}
#endif
