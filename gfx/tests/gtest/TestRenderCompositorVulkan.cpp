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
      mozilla::gfx::VulkanVideoFormat(0, 4096, 2160, 1ULL << 32));
  capabilities.formats().AppendElement(mozilla::gfx::VulkanVideoFormat(
      0x0100000000000002, 8192, 4320, 1ULL << 34));
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
