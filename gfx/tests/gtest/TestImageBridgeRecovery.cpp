/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "GPUVideoImage.h"
#include "ImageContainer.h"
#include "gfxPlatform.h"
#include "gtest/gtest.h"
#include "mozilla/gfx/2D.h"
#include "mozilla/gfx/GPUProcessManager.h"
#include "mozilla/layers/ImageBridgeChild.h"
#include "mozilla/layers/ImageClient.h"
#include "mozilla/layers/SynchronousTask.h"
#include "nsThreadUtils.h"

namespace mozilla::layers {
namespace {

class RecoveryImage final : public Image {
 public:
  explicit RecoveryImage(bool aReadable = true)
      : Image(nullptr, ImageFormat::MOZ2D_SURFACE) {
    if (aReadable) {
      mSurface = gfx::Factory::CreateDataSourceSurface(
          gfx::IntSize(2, 2), gfx::SurfaceFormat::B8G8R8A8, true);
    }
  }
  gfx::IntSize GetSize() const override { return gfx::IntSize(2, 2); }
  already_AddRefed<gfx::SourceSurface> GetAsSourceSurface() override {
    ++mReadbacks;
    return do_AddRef(mSurface);
  }
  size_t mReadbacks = 0;

 private:
  ~RecoveryImage() override = default;
  RefPtr<gfx::DataSourceSurface> mSurface;
};

class RecoveryVideoManager final : public IGPUVideoSurfaceManager {
 public:
  NS_INLINE_DECL_THREADSAFE_REFCOUNTING(RecoveryVideoManager, override)
  already_AddRefed<gfx::SourceSurface> Readback(
      const SurfaceDescriptorGPUVideo&) override {
    ++mReadbacks;
    return nullptr;
  }
  already_AddRefed<Image> TransferToImage(const SurfaceDescriptorGPUVideo&,
                                          const gfx::IntSize&,
                                          const gfx::ColorDepth&,
                                          gfx::YUVColorSpace, gfx::ColorSpace2,
                                          gfx::TransferFunction,
                                          gfx::ColorRange) override {
    return nullptr;
  }
  void DeallocateSurfaceDescriptor(const SurfaceDescriptorGPUVideo&) override {}
  void OnSetCurrent(const SurfaceDescriptorGPUVideo&) override {}
  size_t mReadbacks = 0;

 private:
  ~RecoveryVideoManager() override = default;
};

class ImageBridgeRecovery : public testing::Test {
 public:
  static void SetUpTestSuite() {
    gfxPlatform::GetPlatform();
    gfxPlatform::InitLayersIPC();
    if (!ImageBridgeChild::GetSingleton()) {
      ImageBridgeChild::InitSameProcess(
          gfx::GPUProcessManager::Get()->AllocateNamespace());
      sCreatedBridge = true;
    }
  }
  static void TearDownTestSuite() {
    if (sCreatedBridge) {
      ImageBridgeChild::ShutDown();
      sCreatedBridge = false;
    }
  }

 protected:
  void SetUp() override {
    mBridge = ImageBridgeChild::GetSingleton();
    ASSERT_TRUE(mBridge);
    OnBridge([&] { ASSERT_TRUE(mBridge->IPCOpen()); });
  }
  template <typename F>
  void OnBridge(F&& aTest) {
    SynchronousTask task("ImageBridge recovery test");
    ASSERT_EQ(NS_OK, mBridge->GetThread()->Dispatch(NS_NewRunnableFunction(
                         "ImageBridge recovery test", [&] {
                           AutoCompleteTask complete(&task);
                           aTest();
                         })));
    task.Wait();
  }
  RefPtr<ImageBridgeChild> mBridge;

 private:
  inline static bool sCreatedBridge = false;
};

TEST_F(ImageBridgeRecovery, RecreatedClientRepublishesRetainedImage) {
  RefPtr<ImageContainer> container;
  RefPtr<RecoveryImage> image;
  RefPtr<ImageClient> original, replacement;
  uint32_t generation = 0;
  OnBridge([&] {
    container = new ImageContainer(ImageUsageType::VideoFrameContainer,
                                   ImageContainer::ASYNCHRONOUS);
    image = new RecoveryImage;
    AutoTArray<ImageContainer::NonOwningImage, 1> images;
    images.AppendElement(ImageContainer::NonOwningImage(image));
    container->SetCurrentImages(images);
    mBridge->UpdateImageClient(container);
    original = container->GetImageClient();
    ASSERT_TRUE(original);
    EXPECT_TRUE(original->IsConnected());
    AutoTArray<ImageContainer::OwningImage, 1> retained;
    container->GetCurrentImages(&retained, &generation);
    EXPECT_EQ(image->mReadbacks, 1u);
    EXPECT_EQ(original->GetLastUpdateGenerationCounter(), generation);
    container->DropImageClient();
    replacement = container->GetImageClient();
    ASSERT_TRUE(replacement);
    EXPECT_NE(original, replacement);
    EXPECT_NE(replacement->GetLastUpdateGenerationCounter(), generation);
    EXPECT_EQ(image->mReadbacks, 1u);
  });
  OnBridge([&] {
    ASSERT_TRUE(replacement);
    EXPECT_EQ(replacement->GetLastUpdateGenerationCounter(), generation);
    EXPECT_EQ(image->mReadbacks, 2u);
    container = nullptr;
    original = replacement = nullptr;
    image = nullptr;
  });
}

TEST_F(ImageBridgeRecovery, DestroyedVideoClientDoesNotExposeItsDescriptor) {
  OnBridge([&] {
    RefPtr<RecoveryVideoManager> manager = new RecoveryVideoManager;
    RefPtr<GPUVideoImage> image = new GPUVideoImage(
        manager, SurfaceDescriptorGPUVideo(), gfx::IntSize(2, 2),
        gfx::ColorDepth::COLOR_8, gfx::YUVColorSpace::BT709,
        gfx::ColorSpace2::BT709, gfx::TransferFunction::BT709,
        gfx::ColorRange::LIMITED);
    RefPtr<TextureClient> texture = image->GetTextureClient(mBridge);
    ASSERT_TRUE(texture);
    EXPECT_TRUE(image->GetDesc());
    texture->Destroy();
    EXPECT_FALSE(image->GetTextureClient(mBridge));
    EXPECT_FALSE(image->GetDesc());
    RefPtr<TextureClient> replacement =
        ImageClient::CreateTextureClientForImage(image, mBridge);
    EXPECT_FALSE(replacement);
    EXPECT_EQ(manager->mReadbacks, 1u);
  });
}

TEST_F(ImageBridgeRecovery, MissingReadbackDoesNotCreateTexture) {
  OnBridge([&] {
    RefPtr<RecoveryImage> image = new RecoveryImage(false);
    RefPtr<TextureClient> texture =
        ImageClient::CreateTextureClientForImage(image, mBridge);
    EXPECT_FALSE(texture);
    EXPECT_EQ(image->mReadbacks, 1u);
  });
}

}  // namespace
}  // namespace mozilla::layers
