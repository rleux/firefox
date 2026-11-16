/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderLinuxTextureHost.h"

#include "GLContextEGL.h"
#include "RenderThread.h"
#include "ScopedGLHelpers.h"
#include "VulkanImageRetirement.h"
#include "mozilla/CheckedInt.h"
#include "mozilla/gfx/Logging.h"
#include "mozilla/widget/DMABufSurface.h"
#include "mozilla/widget/SHMBufSurface.h"

namespace mozilla::wr {

#ifdef XP_LINUX
class RenderLinuxTextureHost::VulkanState {
 public:
  explicit VulkanState(DMABufSurface* aSurface) : mSurface(aSurface) {}
  void Abandon() { Fail(); }
  ~VulkanState() {
    MOZ_ASSERT(RenderThread::IsInRenderThread());
    if (!mAccessLocked) {
      return;
    }
    auto surface = mSurface;
    const bool abandon = mFailed || mLocks != 0;
    auto* raw = !abandon && mBorrowed ? CurrentReturn() : nullptr;
    mAccessLocked = false;
    if (raw) {
      const auto value = wr_vulkan_release_value(raw);
      RenderThread::Get()->NativeImages().Add(
          VulkanImageRelease(raw, value),
          [surface](uint64_t, WrVulkanReleaseStatus status) {
            surface->UnlockForeignRGB(status !=
                                      WrVulkanReleaseStatus::Complete);
          });
    } else {
      RenderThread::Get()->NativeImages().Defer(
          [surface, abandon = abandon || mBorrowed || mPending != 0] {
            surface->UnlockForeignRGB(abandon);
          });
    }
  }

  WrExternalImage Lock(uint8_t aChannel, WrVulkanExternalImages* aImages) {
    MOZ_ASSERT(RenderThread::IsInRenderThread());
    ++mLocks;
    if (mFailed || aChannel > 1 || !aImages || !mSurface->ForeignRGBUsable()) {
      Fail();
      return InvalidToWrExternalImage();
    }
    if (mImage &&
        !wr_vulkan_foreign_rgb_matches_context(mImage.get(), aImages)) {
      if (!SwitchContext()) {
        return mFailed ? InvalidToWrExternalImage()
                       : PendingToWrExternalImage();
      }
    }
    if (!mAccessLocked) {
      if (!mSurface->TryLockForeignRGB()) {
        return mSurface->ForeignRGBUsable() ? PendingToWrExternalImage()
                                            : InvalidToWrExternalImage();
      }
      mAccessLocked = true;
    }
    const auto& descriptor = *mSurface->GetForeignRGBDescriptor();
    if (!mImage) {
      WrVulkanForeignRgbDescriptor image{};
      image.fd = descriptor.fds()[0]->GetHandle();
      image.render_node_fd =
          descriptor.foreignRGBImageState()->renderNode()->GetHandle();
      image.width = descriptor.width()[0];
      image.height = descriptor.height()[0];
      image.fourcc = descriptor.fourccFormat();
      image.modifier = descriptor.modifier()[0];
      image.offset = descriptor.offsets()[0];
      image.stride = descriptor.strides()[0];
      mImage.reset(wr_vulkan_foreign_rgb_import(aImages, &image));
      if (!mImage) {
        Fail();
        return InvalidToWrExternalImage();
      }
    }
    if (!mAcquired) {
      const auto status = wr_vulkan_foreign_rgb_acquire(
          mImage.get(), descriptor.fence()[0]->GetHandle(), &mHandle);
      mBorrowed = wr_vulkan_foreign_rgb_has_access(mImage.get());
      if (status == WrVulkanAcquireStatus::Failed) {
        Fail();
        return InvalidToWrExternalImage();
      }
      if (status == WrVulkanAcquireStatus::Pending) {
        return PendingToWrExternalImage();
      }
      mAcquired = true;
    }
    if (aChannel == 1 && !mOpaque._0 &&
        !wr_vulkan_foreign_rgb_opaque_view(mImage.get(), &mOpaque)) {
      Fail();
      return InvalidToWrExternalImage();
    }
    return NativeTextureToWrExternalImage((aChannel ? mOpaque : mHandle)._0, 0,
                                          0, float(descriptor.width()[0]),
                                          float(descriptor.height()[0]));
  }

  Maybe<VulkanImageRelease> Unlock() {
    MOZ_ASSERT(RenderThread::IsInRenderThread());
    if (!mLocks) {
      Fail();
      return Nothing();
    }
    if (--mLocks) {
      return Nothing();
    }
    if (mFailed || !mAcquired) {
      UnlockAccessIfIdle();
      return Nothing();
    }
    auto* receipt = wr_vulkan_foreign_rgb_release(mImage.get());
    const auto value = wr_vulkan_release_value(receipt);
    if (!receipt || value <= mLastIssued) {
      wr_vulkan_release_delete(receipt);
      Fail();
      return Nothing();
    }
    mAcquired = false;
    mBorrowed = false;
    mLastIssued = value;
    ++mPending;
    return Some(VulkanImageRelease(receipt, value));
  }

  void Notify(uint64_t aValue, WrVulkanReleaseStatus aStatus) {
    MOZ_ASSERT(RenderThread::IsInRenderThread());
    if (!mPending || !aValue || aValue <= mLastNotified ||
        aValue > mLastIssued) {
      Fail();
      return;
    }
    --mPending;
    mLastNotified = aValue;
    if (aStatus != WrVulkanReleaseStatus::Complete) {
      Fail();
    }
    UnlockAccessIfIdle();
  }

 private:
  WrVulkanRelease* CurrentReturn() {
    auto* receipt = wr_vulkan_foreign_rgb_last_release(mImage.get());
    if (mBorrowed &&
        (!receipt || wr_vulkan_release_value(receipt) <= mLastIssued)) {
      wr_vulkan_release_delete(receipt);
      receipt = wr_vulkan_foreign_rgb_release(mImage.get());
      if (!receipt) {
        receipt = wr_vulkan_foreign_rgb_last_release(mImage.get());
        if (receipt && wr_vulkan_release_value(receipt) <= mLastIssued) {
          wr_vulkan_release_delete(receipt);
          receipt = nullptr;
        }
      }
    }
    return receipt;
  }
  bool SwitchContext() {
    if (mLocks != 1 || mAcquired || mPending) {
      return false;
    }
    if (mBorrowed) {
      auto* raw = CurrentReturn();
      if (!raw) {
        Fail();
        return false;
      }
      VulkanImageRelease receipt(raw, wr_vulkan_release_value(raw));
      const auto status = receipt.GetStatus();
      if (status == WrVulkanReleaseStatus::Pending) {
        return false;
      }
      if (status != WrVulkanReleaseStatus::Complete) {
        Fail();
        return false;
      }
      mBorrowed = false;
    }
    mImage.reset();
    mHandle = {};
    mOpaque = {};
    mLastIssued = 0;
    mLastNotified = 0;
    return true;
  }
  void Fail() {
    mFailed = true;
    if (mAccessLocked) {
      mSurface->UnlockForeignRGB(true);
      mAccessLocked = false;
    }
  }
  void UnlockAccessIfIdle() {
    if (mAccessLocked && !mAcquired && !mBorrowed && !mPending && !mLocks) {
      mSurface->UnlockForeignRGB();
      mAccessLocked = false;
    }
  }
  struct ImageDeleter {
    void operator()(WrVulkanForeignRgbImage* image) {
      wr_vulkan_foreign_rgb_delete(image);
    }
  };
  RefPtr<DMABufSurface> mSurface;
  UniquePtr<WrVulkanForeignRgbImage, ImageDeleter> mImage;
  ExternalTextureHandle mHandle{}, mOpaque{};
  uint64_t mLastIssued = 0, mLastNotified = 0;
  size_t mPending = 0, mLocks = 0;
  bool mAcquired = false, mBorrowed = false, mAccessLocked = false,
       mFailed = false;
};

WrExternalImage RenderLinuxTextureHost::LockVulkan(
    uint8_t channel, WrVulkanExternalImages* images) {
  auto* surface = mSurface->GetAsDMABufSurface();
  if (!surface || !surface->GetForeignRGBDescriptor()) {
    return LockExternalBuffer(channel);
  }
  if (!mVulkan) {
    mVulkan = std::make_unique<VulkanState>(surface);
  }
  return mVulkan->Lock(channel, images);
}
Maybe<VulkanImageRelease> RenderLinuxTextureHost::UnlockVulkan(
    WrVulkanExternalImages*) {
  if (!mVulkan) {
    UnlockExternalBuffer();
    return Nothing();
  }
  return mVulkan->Unlock();
}
void RenderLinuxTextureHost::NotifyVulkanRelease(uint64_t value,
                                                 WrVulkanReleaseStatus status) {
  MOZ_ASSERT(mVulkan);
  mVulkan->Notify(value, status);
}
#endif

// Derived from
RenderLinuxTextureHost::RenderLinuxTextureHost(BufferSurface* aSurface)
    : mSurface(aSurface) {
  if (!aSurface->GetAsDMABufSurface()) {
    NS_WARNING("Direct import is used for DMABufSurfaces only!");
    mSurface = nullptr;
  }
  MOZ_COUNT_CTOR_INHERITED(RenderLinuxTextureHost, RenderTextureHost);
}

RenderLinuxTextureHost::RenderLinuxTextureHost(
    uint8_t* aBuffer, const layers::BufferDescriptor& aDescriptor) {
  MOZ_COUNT_CTOR_INHERITED(RenderLinuxTextureHost, RenderTextureHost);
  mSurface = SHMBufSurface::Create(aBuffer, aDescriptor);
}

RenderLinuxTextureHost::~RenderLinuxTextureHost() {
  MOZ_COUNT_DTOR_INHERITED(RenderLinuxTextureHost, RenderTextureHost);
  DeleteTextureHandle();
}

void RenderLinuxTextureHost::PrepareForUse() {
  // We don't use texture recycling for dmabuf, but we may
  // implement it for WebGL for instance.
  if (mSurface->GetAsSHMBufSurface()) {
    mSurface->MarkTextureDirty();
  }
}

wr::WrExternalImage RenderLinuxTextureHost::Lock(uint8_t aChannelIndex,
                                                 gl::GLContext* aGL) {
#ifdef XP_LINUX
  if (auto* surface = mSurface->GetAsDMABufSurface();
      surface && surface->IsForeignRGB()) {
    // Pending release callbacks retain this host and still need its Vulkan
    // state.
    if (mVulkan) {
      mVulkan->Abandon();
    } else {
      surface->UnlockForeignRGB(true);
    }
    return InvalidToWrExternalImage();
  }
#endif
  const gfx::IntSize size(mSurface->GetWidth(aChannelIndex),
                          mSurface->GetHeight(aChannelIndex));
  // Wayland native compositor may not use textures so pass zero
  // there. It saves GPU resources.
  if (!aGL) {
    return NativeTextureToWrExternalImage(0, 0.0, 0.0,
                                          static_cast<float>(size.width),
                                          static_cast<float>(size.height));
  }

  if (mGL.get() != aGL) {
    if (mGL) {
      // This should not happen. EGLImage is created only in
      // parent process.
      MOZ_ASSERT_UNREACHABLE("Unexpected GL context");
      return InvalidToWrExternalImage();
    }
    mGL = aGL;
  }

  if (!mGL || !mGL->MakeCurrent()) {
    return InvalidToWrExternalImage();
  }

  if (!mSurface->GetTexture(aChannelIndex)) {
    if (!mSurface->CreateTextures(mGL)) {
      return InvalidToWrExternalImage();
    }
    ActivateBindAndTexParameteri(mGL, LOCAL_GL_TEXTURE0, LOCAL_GL_TEXTURE_2D,
                                 mSurface->GetTexture(aChannelIndex));
  }

  auto texture = mSurface->GetTexture(aChannelIndex);
  if (auto* dmabuf = mSurface->GetAsDMABufSurface()) {
    dmabuf->MaybeSemaphoreWait(texture);
  }

  return NativeTextureToWrExternalImage(texture, 0.0, 0.0,
                                        static_cast<float>(size.width),
                                        static_cast<float>(size.height));
}

gfx::IntSize RenderLinuxTextureHost::GetSize(uint8_t aChannelIndex) const {
  MOZ_ASSERT(mSurface);
  MOZ_ASSERT((mSurface->GetTextureCount() == 0)
                 ? (aChannelIndex == mSurface->GetTextureCount())
                 : (aChannelIndex < mSurface->GetTextureCount()));

  if (!mSurface) {
    return gfx::IntSize();
  }
  return gfx::IntSize(mSurface->GetWidth(aChannelIndex),
                      mSurface->GetHeight(aChannelIndex));
}

WrExternalImage RenderLinuxTextureHost::LockExternalBuffer(
    uint8_t aChannelIndex) {
  auto* shm = mSurface ? mSurface->GetAsSHMBufSurface() : nullptr;
  if (!shm || aChannelIndex >= GetPlaneCount()) {
    return InvalidToWrExternalImage();
  }
  int32_t stride = 0;
  gfx::IntSize size;
  auto* data = shm->MapPlane(aChannelIndex, stride, size);
  if (!data || size.IsEmpty() || stride <= 0) {
    return InvalidToWrExternalImage();
  }
  const auto length = CheckedInt<size_t>(stride) * size.height;
  if (!length.isValid()) {
    return InvalidToWrExternalImage();
  }
  return RawDataToWrExternalImage(data, length.value());
}

void RenderLinuxTextureHost::Unlock() {}

void RenderLinuxTextureHost::DeleteTextureHandle() {
  mSurface->ReleaseTextures();
}

void RenderLinuxTextureHost::ClearCachedResources() {
  DeleteTextureHandle();
  mGL = nullptr;
}

bool RenderLinuxTextureHost::MapPlane(RenderCompositor* aCompositor,
                                      uint8_t aChannelIndex,
                                      PlaneInfo& aPlaneInfo) {
  // SHM surfaces are directly mapped
  if (auto* shm = mSurface->GetAsSHMBufSurface()) {
    aPlaneInfo.mData =
        shm->MapPlane(aChannelIndex, aPlaneInfo.mStride, aPlaneInfo.mSize);
    return !!aPlaneInfo.mData;
  }

#ifdef XP_LINUX
  if (auto* surface = mSurface->GetAsDMABufSurface();
      surface && surface->IsForeignRGB()) {
    // Pending release callbacks retain this host and still need its Vulkan
    // state.
    if (mVulkan) {
      mVulkan->Abandon();
    } else {
      surface->UnlockForeignRGB(true);
    }
    return false;
  }
#endif
  if (mSurface->GetAsDMABufSurfaceYUV()) {
    // DMABufSurfaceYUV mapping is not supported yet. Do we need it?
    return false;
  }

  const RefPtr<gfx::SourceSurface> surface = mSurface->GetAsSourceSurface();
  if (!surface) {
    return false;
  }

  const RefPtr<gfx::DataSourceSurface> dataSurface = surface->GetDataSurface();
  if (!dataSurface) {
    return false;
  }

  gfx::DataSourceSurface::MappedSurface map;
  if (!dataSurface->Map(gfx::DataSourceSurface::MapType::READ, &map)) {
    return false;
  }

  mReadback = dataSurface;
  aPlaneInfo.mSize = gfx::IntSize(mSurface->GetWidth(), mSurface->GetHeight());
  aPlaneInfo.mStride = map.mStride;
  aPlaneInfo.mData = map.mData;

  return true;
}

void RenderLinuxTextureHost::UnmapPlanes() {
  if (mReadback) {
    mReadback->Unmap();
    mReadback = nullptr;
  }
}

}  // namespace mozilla::wr
