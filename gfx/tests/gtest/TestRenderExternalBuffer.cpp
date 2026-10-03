/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <array>
#include <cstring>

#include "gfxPlatform.h"
#include "gtest/gtest.h"
#include "mozilla/ScopeExit.h"
#include "mozilla/gfx/GPUProcessManager.h"
#include "mozilla/layers/SourceSurfaceSharedData.h"
#include "mozilla/layers/SynchronousTask.h"
#include "mozilla/webrender/RenderBufferTextureHost.h"
#include "mozilla/webrender/RenderSharedSurfaceTextureHost.h"
#include "mozilla/webrender/RenderTextureHostWrapper.h"
#include "mozilla/webrender/RenderThread.h"
#include "nsThreadUtils.h"

namespace mozilla::wr {

using namespace gfx;

namespace {

class RenderExternalBuffer : public ::testing::Test {
 protected:
  void SetUp() override {
    gfxPlatform::GetPlatform();
    gfxPlatform::InitLayersIPC();
    if (!RenderThread::Get()) {
      GTEST_SKIP() << "Requires an in-process render thread";
    }
  }

  template <typename F>
  void OnRenderThread(F&& aTest) {
    layers::SynchronousTask task("RenderExternalBuffer test");
    nsCOMPtr<nsIThread> thread = RenderThread::GetRenderThread();
    ASSERT_EQ(NS_OK,
              thread->Dispatch(NS_NewRunnableFunction(
                                   "RenderExternalBuffer test",
                                   [test = std::move(aTest), &task] {
                                     layers::AutoCompleteTask complete(&task);
                                     test();
                                   }),
                               NS_DISPATCH_NORMAL));
    task.Wait();
  }
};

layers::BufferDescriptor BufferDescriptorForTest() {
  return layers::RGBDescriptor(IntSize(3, 2), SurfaceFormat::B8G8R8A8,
                               ColorSpace2::SRGB, TransferFunction::SRGB);
}

class ContextOnlyTexture final : public RenderTextureHost {
 public:
  WrExternalImage Lock(uint8_t, gl::GLContext*) override {
    mUsedContextPath = true;
    return NativeTextureToWrExternalImage(1, 0, 0, 1, 1);
  }
  void Unlock() override { mUsedContextPath = true; }
  size_t Bytes() override { return 0; }
  bool mUsedContextPath = false;

 private:
  ~ContextOnlyTexture() override = default;
};

}  // namespace

TEST_F(RenderExternalBuffer, BufferMappingAndRelock) {
  OnRenderThread([] {
    std::array<uint8_t, 24> bytes{};
    bytes.fill(0x35);
    RefPtr<RenderTextureHost> host =
        new RenderBufferTextureHost(bytes.data(), BufferDescriptorForTest());
    EXPECT_EQ(host->LockExternalBuffer(1).image_type,
              WrExternalImageType::Invalid);
    host->UnlockExternalBuffer();
    for (uint8_t value : {0x35, 0x9a}) {
      bytes[0] = value;
      auto image = host->LockExternalBuffer(0);
      auto unlock = MakeScopeExit([&] { host->UnlockExternalBuffer(); });
      ASSERT_EQ(image.image_type, WrExternalImageType::RawData);
      ASSERT_EQ(image.buff, bytes.data());
      ASSERT_EQ(image.size, bytes.size());
      EXPECT_EQ(image.buff[0], value);
      EXPECT_EQ(image.buff[23], 0x35);
    }
    host->Destroy();
    EXPECT_EQ(host->LockExternalBuffer(0).image_type,
              WrExternalImageType::Invalid);
    host->UnlockExternalBuffer();
  });
}

TEST_F(RenderExternalBuffer, SharedSurfaceKeepsStorageAndStride) {
  OnRenderThread([] {
    RefPtr<SourceSurfaceSharedData> source = new SourceSurfaceSharedData;
    ASSERT_TRUE(
        source->Init(IntSize(2, 2), 16, SurfaceFormat::B8G8R8A8, false));
    auto* data = source->GetData();
    std::memset(data, 0x72, 32);
    RefPtr<SourceSurfaceSharedDataWrapper> surface =
        new SourceSurfaceSharedDataWrapper;
    surface->Init(source);
    RefPtr<RenderTextureHost> host =
        new RenderSharedSurfaceTextureHost(surface);
    source = nullptr;
    surface = nullptr;
    EXPECT_EQ(host->LockExternalBuffer(1).image_type,
              WrExternalImageType::Invalid);
    for (int i = 0; i < 2; ++i) {
      auto image = host->LockExternalBuffer(0);
      auto unlock = MakeScopeExit([&] { host->UnlockExternalBuffer(); });
      ASSERT_EQ(image.image_type, WrExternalImageType::RawData);
      ASSERT_EQ(image.buff, data);
      ASSERT_EQ(image.size, 32U);
      EXPECT_EQ(image.buff[0], 0x72);
      EXPECT_EQ(image.buff[31], 0x72);
      EXPECT_EQ(host->LockExternalBuffer(0).buff, data);
    }
  });
}

TEST_F(RenderExternalBuffer, ContextTexturesDeclineBufferAccess) {
  OnRenderThread([] {
    RefPtr<ContextOnlyTexture> host = new ContextOnlyTexture;
    EXPECT_EQ(host->LockExternalBuffer(0).image_type,
              WrExternalImageType::Invalid);
    host->UnlockExternalBuffer();
    EXPECT_FALSE(host->mUsedContextPath);
  });
}

TEST_F(RenderExternalBuffer, WrapperRetainsAndForwardsBuffer) {
  ExternalImageId id{uint64_t(GPUProcessManager::Get()->AllocateNamespace())
                     << 32};
  OnRenderThread([id] {
    std::array<uint8_t, 24> bytes{};
    RefPtr<RenderTextureHost> host =
        new RenderBufferTextureHost(bytes.data(), BufferDescriptorForTest());
    bool destroyed = false;
    host->SetDestroyedCallback([&] { destroyed = true; });
    RenderThread::Get()->RegisterExternalImage(id, host.forget());
    RefPtr<RenderTextureHost> wrapper = new RenderTextureHostWrapper(id);
    RenderThread::Get()->UnregisterExternalImage(id);
    EXPECT_FALSE(destroyed);
    {
      auto image = wrapper->LockExternalBuffer(0);
      auto unlock = MakeScopeExit([&] { wrapper->UnlockExternalBuffer(); });
      ASSERT_EQ(image.image_type, WrExternalImageType::RawData);
      ASSERT_EQ(image.buff, bytes.data());
      ASSERT_EQ(image.size, bytes.size());
    }
    wrapper = nullptr;
    EXPECT_TRUE(destroyed);
  });
}

}  // namespace mozilla::wr
