/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderDMABUFTextureHost.h"

#include "GLContextEGL.h"
#include "ScopedGLHelpers.h"
#include "mozilla/gfx/Logging.h"
#ifdef XP_LINUX
#  include "mozilla/gfx/FileHandleWrapper.h"
#  include "mozilla/layers/LayersSurfaces.h"
#  include "mozilla/webrender/RenderThread.h"
#endif

namespace mozilla::wr {

#ifdef XP_LINUX
class RenderDMABUFTextureHost::VulkanState {
 public:
  explicit VulkanState(DMABufSurface* aSurface) : mSurface(aSurface) {}
  ~VulkanState() {
    MOZ_ASSERT(RenderThread::IsInRenderThread());
    if (mAccessLocked) {
      Fail();
    }
  }

  WrExternalImage Lock(uint8_t aChannel, WrVulkanExternalImages* aImages) {
    MOZ_ASSERT(RenderThread::IsInRenderThread());
    ++mLocks;
    if (mFailed || aChannel > 1 || !aImages || !mSurface->ForeignRGBUsable()) {
      return InvalidToWrExternalImage();
    }
    if (mImage &&
        !wr_vulkan_foreign_rgb_matches_context(mImage.get(), aImages)) {
      if (mAcquired || mPending || mLocks != 1) {
        return InvalidToWrExternalImage();
      }
      mImage.reset();
      mReleased.reset();
      mOpaque = {};
      mLastIssued = 0;
    }
    if (!mAccessLocked) {
      if (!mSurface->TryLockForeignRGB()) {
        return InvalidToWrExternalImage();
      }
      mAccessLocked = true;
    }
    const auto& descriptor = *mSurface->GetForeignRGBDescriptor();
    if (!mImage) {
      WrVulkanForeignRgbDescriptor image{};
      image.fd = descriptor.fds()[0]->GetHandle();
      image.width = descriptor.width()[0];
      image.height = descriptor.height()[0];
      image.fourcc = descriptor.fourccFormat();
      image.modifier = descriptor.modifier()[0];
      image.offset = descriptor.offsets()[0];
      image.stride = descriptor.strides()[0];
      mImage.reset(wr_vulkan_foreign_rgb_import(aImages, &image));
      mReleased.reset(wr_vulkan_timeline_new(aImages));
      if (!mImage || !mReleased) {
        Fail();
        return InvalidToWrExternalImage();
      }
    }
    if (!mAcquired) {
      if (!wr_vulkan_foreign_rgb_acquire(
              mImage.get(), descriptor.fence()[0]->GetHandle(), &mHandle)) {
        Fail();
        return InvalidToWrExternalImage();
      }
      mAcquired = true;
    }
    if (aChannel == 1 && !mOpaque._0 &&
        !wr_vulkan_foreign_rgb_opaque_view(mImage.get(), &mOpaque)) {
      Fail();
      return InvalidToWrExternalImage();
    }
    return NativeTextureToWrExternalImage(
        (aChannel == 1 ? mOpaque : mHandle)._0, 0, 0,
        float(descriptor.width()[0]), float(descriptor.height()[0]));
  }

  Maybe<VulkanImageRelease> Unlock() {
    MOZ_ASSERT(RenderThread::IsInRenderThread());
    MOZ_ASSERT(mLocks);
    if (!mLocks || --mLocks) {
      return Nothing();
    }
    if (mFailed || !mAcquired) {
      UnlockAccessIfIdle();
      return Nothing();
    }
    if (mLastIssued == UINT64_MAX) {
      Fail();
      return Nothing();
    }
    mAcquired = false;
    ++mPending;
    auto value = ++mLastIssued;
    auto* receipt =
        wr_vulkan_foreign_rgb_release(mImage.get(), mReleased.get(), value);
    return Some(VulkanImageRelease(receipt, value));
  }

  void Notify(uint64_t aValue, WrVulkanReleaseStatus aStatus) {
    MOZ_ASSERT(RenderThread::IsInRenderThread());
    if (!mPending || !aValue || aValue > mLastIssued) {
      Fail();
      return;
    }
    --mPending;
    if (aStatus != WrVulkanReleaseStatus::Complete) {
      Fail();
    }
    UnlockAccessIfIdle();
  }

 private:
  void Fail() {
    mFailed = true;
    if (mAccessLocked) {
      mSurface->UnlockForeignRGB(true);
      mAccessLocked = false;
    }
  }

  void UnlockAccessIfIdle() {
    if (mAccessLocked && !mAcquired && !mPending && !mLocks) {
      mSurface->UnlockForeignRGB();
      mAccessLocked = false;
    }
  }

  struct ImageDeleter {
    void operator()(WrVulkanForeignRgbImage* aImage) {
      wr_vulkan_foreign_rgb_delete(aImage);
    }
  };
  struct TimelineDeleter {
    void operator()(WrVulkanTimeline* aTimeline) {
      wr_vulkan_timeline_delete(aTimeline);
    }
  };
  RefPtr<DMABufSurface> mSurface;
  UniquePtr<WrVulkanForeignRgbImage, ImageDeleter> mImage;
  UniquePtr<WrVulkanTimeline, TimelineDeleter> mReleased;
  ExternalTextureHandle mHandle{};
  ExternalTextureHandle mOpaque{};
  uint64_t mLastIssued = 0;
  size_t mPending = 0;
  size_t mLocks = 0;
  bool mAcquired = false;
  bool mAccessLocked = false;
  bool mFailed = false;
};

WrExternalImage RenderDMABUFTextureHost::LockVulkan(
    uint8_t aChannelIndex, WrVulkanExternalImages* aImages) {
  if (!mSurface->GetForeignRGBDescriptor()) {
    return InvalidToWrExternalImage();
  }
  if (!mVulkan) {
    mVulkan = MakeUnique<VulkanState>(mSurface);
  }
  return mVulkan->Lock(aChannelIndex, aImages);
}

Maybe<VulkanImageRelease> RenderDMABUFTextureHost::UnlockVulkan(
    WrVulkanExternalImages*) {
  return mVulkan ? mVulkan->Unlock() : Nothing();
}

void RenderDMABUFTextureHost::NotifyVulkanRelease(
    uint64_t aValue, WrVulkanReleaseStatus aStatus) {
  MOZ_ASSERT(mVulkan);
  mVulkan->Notify(aValue, aStatus);
}
#endif

RenderDMABUFTextureHost::RenderDMABUFTextureHost(DMABufSurface* aSurface)
    : mSurface(aSurface) {
  MOZ_COUNT_CTOR_INHERITED(RenderDMABUFTextureHost, RenderTextureHost);
}

RenderDMABUFTextureHost::~RenderDMABUFTextureHost() {
  MOZ_COUNT_DTOR_INHERITED(RenderDMABUFTextureHost, RenderTextureHost);
  DeleteTextureHandle();
}

wr::WrExternalImage RenderDMABUFTextureHost::Lock(uint8_t aChannelIndex,
                                                  gl::GLContext* aGL) {
  const gfx::IntSize size(mSurface->GetWidth(aChannelIndex),
                          mSurface->GetHeight(aChannelIndex));

  // Wayland native compositor doesn't use textures so pass zero
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

  if (auto texture = mSurface->GetTexture(aChannelIndex)) {
    mSurface->MaybeSemaphoreWait(texture);
  }

  return NativeTextureToWrExternalImage(
      mSurface->GetTexture(aChannelIndex), 0.0, 0.0,
      static_cast<float>(size.width), static_cast<float>(size.height));
}

gfx::IntSize RenderDMABUFTextureHost::GetSize(uint8_t aChannelIndex) const {
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

void RenderDMABUFTextureHost::Unlock() {}

void RenderDMABUFTextureHost::DeleteTextureHandle() {
  mSurface->ReleaseTextures();
}

void RenderDMABUFTextureHost::ClearCachedResources() {
  DeleteTextureHandle();
  mGL = nullptr;
}

bool RenderDMABUFTextureHost::MapPlane(RenderCompositor* aCompositor,
                                       uint8_t aChannelIndex,
                                       PlaneInfo& aPlaneInfo) {
  if (mSurface->GetAsDMABufSurfaceYUV()) {
    // DMABufSurfaceYUV is not supported.
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

void RenderDMABUFTextureHost::UnmapPlanes() {
  if (mReadback) {
    mReadback->Unmap();
    mReadback = nullptr;
  }
}

}  // namespace mozilla::wr
