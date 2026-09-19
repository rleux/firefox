/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <cstdlib>

#include "chrome/common/ipc_message.h"
#include "chrome/common/ipc_message_utils.h"
#include "gtest/gtest.h"
#include "mozilla/ScopeExit.h"
#include "mozilla/gfx/gfxVars.h"
#include "mozilla/webrender/RenderCompositorVulkan.h"
#include "nsString.h"

extern "C" void* wr_vulkan_register_dmabuf_device(
    const uint8_t*, const uint8_t*, const uint64_t*, size_t, const uint64_t*,
    size_t, const mozilla::wr::WrHalVideoCapabilities*);
extern "C" void wr_vulkan_unregister_dmabuf_device(void*);

static constexpr uint32_t MakeFourcc(char a, char b, char c, char d) {
  return uint32_t(a) | (uint32_t(b) << 8) | (uint32_t(c) << 16) |
         (uint32_t(d) << 24);
}
static constexpr uint32_t kFourccNV12 = MakeFourcc('N', 'V', '1', '2');
static constexpr uint32_t kFourccP010 = MakeFourcc('P', '0', '1', '0');

TEST(RenderCompositorVulkan, VideoRequiresEveryLiveDeviceToMatchProbe)
{
  using namespace mozilla;
  using gfx::gfxVars;
  using wr::RenderCompositorVulkan;
  gfxVars::Initialize();
  const auto saved = gfxVars::WebRenderVulkanVideoCapabilities();
  const bool video = gfxVars::UseWebRenderVulkanVideo();
  const bool vulkan = gfxVars::UseWebRenderVulkan();
  const bool software = gfxVars::UseSoftwareWebRender();
  auto restore = MakeScopeExit([&] {
    gfxVars::SetWebRenderVulkanVideoCapabilities(saved);
    gfxVars::SetUseWebRenderVulkanVideo(video);
    gfxVars::SetUseWebRenderVulkan(vulkan);
    gfxVars::SetUseSoftwareWebRender(software);
  });
  gfxVars::SetUseWebRenderVulkan(true);
  gfxVars::SetUseSoftwareWebRender(false);
  gfxVars::SetUseWebRenderVulkanVideo(true);
  gfx::VulkanVideoCapabilities expected;
  expected.drmMajor() = 226;
  expected.drmMinor() = 128;
  for (size_t i = 0; i < 16; ++i) {
    expected.deviceUUID().AppendElement(0);
    expected.driverUUID().AppendElement(0);
  }
  expected.deviceUUID()[0] = 1;
  expected.driverUUID()[0] = 2;
  expected.formats().AppendElement(
      gfx::VulkanVideoFormat(kFourccNV12, 0, 4096, 2160, 1ULL << 32));
  gfxVars::SetWebRenderVulkanVideoCapabilities(expected);
  EXPECT_FALSE(RenderCompositorVulkan::SupportsVideo());
  wr::WrHalVideoCapabilities actual{};
  actual.drm_node[0] = 226;
  actual.drm_node[1] = 128;
  actual.device_uuid[0] = 1;
  actual.driver_uuid[0] = 2;
  actual.format_count = 1;
  actual.formats[0] = {kFourccNV12, 0, 4096, 2160, 1ULL << 32};
  const uint64_t modifier = 0;
  const auto add = [&](const wr::WrHalVideoCapabilities& aCaps) {
    return wr_vulkan_register_dmabuf_device(aCaps.device_uuid,
                                            aCaps.driver_uuid, &modifier, 1,
                                            &modifier, 1, &aCaps);
  };
  void* first = add(actual);
  ASSERT_TRUE(first);
  auto unregister =
      MakeScopeExit([&] { wr_vulkan_unregister_dmabuf_device(first); });
  EXPECT_TRUE(RenderCompositorVulkan::SupportsVideo());
  for (int i = 0; i < 9; ++i) {
    SCOPED_TRACE(i);
    auto changed = actual;
    switch (i) {
      case 0:
        changed.drm_node[1]++;
        break;
      case 1:
        changed.device_uuid[0]++;
        break;
      case 2:
        changed.driver_uuid[0]++;
        break;
      case 3:
        changed.format_count = 0;
        break;
      case 4:
        changed.formats[0].modifier++;
        break;
      case 5:
        changed.formats[0].max_width--;
        break;
      case 6:
        changed.formats[0].max_height--;
        break;
      case 7:
        changed.formats[0].max_allocation_size--;
        break;
      case 8:
        changed.formats[0].fourcc = kFourccP010;
        break;
    }
    void* second = add(changed);
    EXPECT_FALSE(RenderCompositorVulkan::SupportsVideo());
    wr_vulkan_unregister_dmabuf_device(second);
    EXPECT_TRUE(RenderCompositorVulkan::SupportsVideo());
  }
  gfxVars::SetUseWebRenderVulkanVideo(false);
  EXPECT_FALSE(RenderCompositorVulkan::SupportsVideo());
}

TEST(RenderCompositorVulkan, VideoFailureRevokesCapabilityForSession)
{
  using namespace mozilla;
  using gfx::gfxVars;
  gfxVars::Initialize();
  const bool vulkan = gfxVars::UseWebRenderVulkan();
  const bool software = gfxVars::UseSoftwareWebRender();
  auto restore = MakeScopeExit([&] {
    gfxVars::SetUseWebRenderVulkan(vulkan);
    gfxVars::SetUseSoftwareWebRender(software);
  });
  gfxVars::SetUseWebRenderVulkan(true);
  gfxVars::SetUseSoftwareWebRender(false);
  gfxVars::SetUseWebRenderVulkanVideo(true);
  gfx::VulkanVideoCapabilities capabilities;
  capabilities.formats().AppendElement(
      gfx::VulkanVideoFormat(kFourccNV12, 0, 128, 128, 24576));
  gfxVars::SetWebRenderVulkanVideoCapabilities(capabilities);
  wr::RenderCompositorVulkan::DisableVideo();
  EXPECT_FALSE(gfxVars::UseWebRenderVulkanVideo());
  EXPECT_TRUE(gfxVars::WebRenderVulkanVideoCapabilities().formats().IsEmpty());
  EXPECT_TRUE(
      wr::RenderCompositorVulkan::ProbeVideoCapabilities().formats().IsEmpty());
}

TEST(RenderCompositorVulkan, SelectionIsExplicitAndSoftwareTakesPrecedence)
{
  mozilla::gfx::gfxVars::Initialize();
  const bool software = mozilla::gfx::gfxVars::UseSoftwareWebRender();
  const bool vulkan = mozilla::gfx::gfxVars::UseWebRenderVulkan();
  const char* backend = getenv("MOZ_WR_BACKEND");
  const bool hadBackend = backend != nullptr;
  const nsCString savedBackend(backend);
  auto restore = mozilla::MakeScopeExit([&] {
    if (hadBackend) {
      setenv("MOZ_WR_BACKEND", savedBackend.get(), 1);
    } else {
      unsetenv("MOZ_WR_BACKEND");
    }
    mozilla::gfx::gfxVars::SetUseSoftwareWebRender(software);
    mozilla::gfx::gfxVars::SetUseWebRenderVulkan(vulkan);
  });

  ASSERT_EQ(setenv("MOZ_WR_BACKEND", "vulkan", 1), 0);
  mozilla::gfx::gfxVars::SetUseSoftwareWebRender(false);
  mozilla::gfx::gfxVars::SetUseWebRenderVulkan(false);
  EXPECT_FALSE(mozilla::wr::RenderCompositorVulkan::IsRequested());
  EXPECT_TRUE(mozilla::wr::RenderCompositorVulkan::ProbeVideoCapabilities()
                  .formats()
                  .IsEmpty());
  mozilla::gfx::gfxVars::SetUseWebRenderVulkan(true);
  EXPECT_TRUE(mozilla::wr::RenderCompositorVulkan::IsRequested());
  mozilla::gfx::gfxVars::SetUseSoftwareWebRender(true);
  EXPECT_FALSE(mozilla::wr::RenderCompositorVulkan::IsRequested());
  EXPECT_TRUE(mozilla::wr::RenderCompositorVulkan::ProbeVideoCapabilities()
                  .formats()
                  .IsEmpty());
  mozilla::gfx::gfxVars::SetUseSoftwareWebRender(false);
  EXPECT_TRUE(mozilla::wr::RenderCompositorVulkan::IsRequested());
  mozilla::gfx::gfxVars::SetUseWebRenderVulkan(false);
  EXPECT_FALSE(mozilla::wr::RenderCompositorVulkan::IsRequested());
}

TEST(RenderCompositorVulkan, VideoCapabilitiesSurviveGfxVarIPC)
{
  mozilla::gfx::VulkanVideoCapabilities capabilities;
  capabilities.drmMajor() = 226;
  capabilities.drmMinor() = 128;
  for (uint8_t i = 0; i < 16; ++i) {
    capabilities.deviceUUID().AppendElement(i);
    capabilities.driverUUID().AppendElement(31 - i);
  }
  capabilities.formats().AppendElement(
      mozilla::gfx::VulkanVideoFormat(kFourccNV12, 0, 4096, 2160, 1ULL << 32));
  capabilities.formats().AppendElement(mozilla::gfx::VulkanVideoFormat(
      kFourccP010, 0x0100000000000002, 8192, 4320, 1ULL << 34));
  mozilla::gfx::GfxVarValue input(capabilities);
  IPC::Message message(MSG_ROUTING_NONE, 0);
  {
    IPC::MessageWriter writer(message);
    IPC::WriteParam(&writer, input);
  }
  IPC::MessageReader reader(message);
  mozilla::gfx::GfxVarValue output;
  ASSERT_TRUE(IPC::ReadParam(&reader, &output));
  ASSERT_EQ(output.type(), mozilla::gfx::GfxVarValue::TVulkanVideoCapabilities);
  EXPECT_EQ(output.get_VulkanVideoCapabilities(), capabilities);
}
